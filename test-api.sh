#!/usr/bin/env bash
# Smoke-tests the message gRPC API end to end. Exits non-zero if any check fails.
# Usage: ./test-api.sh                                   (the public GKE endpoint, TLS)
#        GRPC_ADDR=localhost:9090 PLAINTEXT=true ./test-api.sh   (./mvnw spring-boot:run, or a port-forward)
# Needs grpcurl and jq. The service descriptors come from server reflection, so no .proto is needed.
set -euo pipefail

GRPC_ADDR="${GRPC_ADDR:-grpc.miqui.dev:443}"
PLAINTEXT="${PLAINTEXT:-false}"

for tool in grpcurl jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "Error: $tool is required."; exit 1; }
done

OPTS=(-emit-defaults -max-time 15)   # -emit-defaults: a message at version 0 still prints "version": 0
[ "${PLAINTEXT}" = "true" ] && OPTS+=(-plaintext)

echo "=========================================================="
echo " Testing Spring Boot gRPC API (Cloud SQL + Hazelcast)"
echo " Target: ${GRPC_ADDR} (plaintext=${PLAINTEXT})"
echo "=========================================================="

BODY=""      # response message as JSON ("" on error)
CODE=""      # gRPC status name: OK, InvalidArgument, NotFound, Aborted, ...
ERR=""       # grpcurl's error text (status, message, ErrorInfo/BadRequest details)
FAILURES=0

# call METHOD [JSON_REQUEST] - e.g. call message.v1.MessageService/GetMessage '{"id":"..."}'
# Sets CODE, BODY and ERR, and prints the exchange.
call() {
  local method="$1" data="${2:-}" out
  [ -n "${data}" ] || data='{}'
  if out=$(grpcurl "${OPTS[@]}" -d "${data}" "${GRPC_ADDR}" "${method}" 2>"${TMPERR}"); then
    CODE=OK; BODY="${out}"; ERR=""
  else
    BODY=""; ERR=$(cat "${TMPERR}")
    CODE=$(printf '%s\n' "${ERR}" | sed -n 's/^  Code: //p' | head -1)
    CODE="${CODE:-Unavailable}"   # transport failure: no gRPC status was returned
  fi
  echo "${method} -> ${CODE}"
  if [ -n "${BODY}" ]; then echo "${BODY}" | jq . 2>/dev/null || echo "${BODY}"; fi
  if [ -n "${ERR}" ]; then printf '%s\n' "${ERR}"; fi
}
TMPERR=$(mktemp); trap 'rm -f "${TMPERR}"' EXIT

# reason: the google.rpc.ErrorInfo reason from the last error (BAD_USER_INPUT, CONFLICT, ...)
reason() { printf '%s\n' "${ERR}" | sed -n 's/.*"reason": *"\([A-Z_]*\)".*/\1/p' | head -1; }
# violations: how many google.rpc.BadRequest field violations the last error carried
violations() { printf '%s\n' "${ERR}" | grep -c '"field":' || true; }

# expect DESCRIPTION EXPECTED ACTUAL
expect() {
  if [ "$2" = "$3" ]; then
    echo "  ok: $1"
  else
    echo "  FAIL: $1 (expected '$2', got '$3')"
    FAILURES=$((FAILURES + 1))
  fi
}

MSG=message.v1.MessageService
AUT=message.v1.AuthorService

echo -e "\n1. Reflection and health:"
SERVICES=$(grpcurl "${OPTS[@]}" "${GRPC_ADDR}" list 2>&1 || true)
echo "${SERVICES}"
expect "MessageService is listed" 1 "$(echo "${SERVICES}" | grep -c "^${MSG}$" || true)"
expect "AuthorService is listed" 1 "$(echo "${SERVICES}" | grep -c "^${AUT}$" || true)"
call grpc.health.v1.Health/Check
expect "health is SERVING" SERVING "$(echo "${BODY}" | jq -r .status)"

echo -e "\n2. List messages:"
call ${MSG}/ListMessages '{"limit":50,"offset":0}'
expect "list is OK" OK "${CODE}"
expect "list has a numeric total_count" 1 "$(echo "${BODY}" | jq -r '(.total_count | tonumber) >= 0' | grep -c true || true)"

echo -e "\n3. Create an author:"
EMAIL="kubernetes-admin-$(date +%s)@example.com"
call ${AUT}/CreateAuthor "{\"name\":\"kubernetes-admin\",\"email\":\"${EMAIL}\"}"
expect "create author is OK" OK "${CODE}"
AUTHOR_ID=$(echo "${BODY}" | jq -r .id)

echo -e "\n3b. Create an author with the same email (expecting ALREADY_EXISTS):"
call ${AUT}/CreateAuthor "{\"name\":\"duplicate\",\"email\":\"${EMAIL}\"}"
expect "duplicate email is AlreadyExists" AlreadyExists "${CODE}"
expect "reason is CONFLICT" CONFLICT "$(reason)"

echo -e "\n4. Create a valid message:"
call ${MSG}/CreateMessage "{\"title\":\"GKE Deployment\",\"content\":\"Spring Boot gRPC running on GKE!\",\"author_id\":\"${AUTHOR_ID}\"}"
expect "create message is OK" OK "${CODE}"
MSG_ID=$(echo "${BODY}" | jq -r .id)
expect "new message is at version 0" 0 "$(echo "${BODY}" | jq -r .version)"

echo -e "\n5. Create an invalid message (expecting INVALID_ARGUMENT / BAD_USER_INPUT with field violations):"
call ${MSG}/CreateMessage '{"title":"","content":"","author_id":""}'
expect "invalid create is InvalidArgument" InvalidArgument "${CODE}"
expect "reason is BAD_USER_INPUT" BAD_USER_INPUT "$(reason)"
expect "all three fields are reported" 3 "$(violations)"

echo -e "\n5b. Reject a NUL character before reaching PostgreSQL:"
call ${MSG}/CreateMessage "{\"title\":\"bad\\u0000title\",\"content\":\"Body\",\"author_id\":\"${AUTHOR_ID}\"}"
expect "NUL input is InvalidArgument" InvalidArgument "${CODE}"
expect "reason is BAD_USER_INPUT" BAD_USER_INPUT "$(reason)"
expect "title violation is reported" 1 "$(violations)"

echo -e "\n6. Get message by id (${MSG_ID}):"
call ${MSG}/GetMessage "{\"id\":\"${MSG_ID}\"}"
expect "get is OK" OK "${CODE}"
expect "id matches" "${MSG_ID}" "$(echo "${BODY}" | jq -r .id)"
expect "author is embedded" "${AUTHOR_ID}" "$(echo "${BODY}" | jq -r .author.id)"

echo -e "\n7. Update message, version 0:"
call ${MSG}/UpdateMessage "{\"id\":\"${MSG_ID}\",\"title\":\"Updated Title\",\"content\":\"Updated content on GKE\",\"version\":0}"
expect "update is OK" OK "${CODE}"
expect "version is bumped to 1" 1 "$(echo "${BODY}" | jq -r .version)"

echo -e "\n8. Update again with a stale version (expecting ABORTED / CONFLICT):"
call ${MSG}/UpdateMessage "{\"id\":\"${MSG_ID}\",\"content\":\"Stale write\",\"version\":0}"
expect "stale update is Aborted" Aborted "${CODE}"
expect "reason is CONFLICT" CONFLICT "$(reason)"

echo -e "\n9. Unknown id and malformed id:"
call ${MSG}/GetMessage '{"id":"00000000-0000-0000-0000-000000000000"}'
expect "unknown id is NotFound" NotFound "${CODE}"
expect "reason is NOT_FOUND" NOT_FOUND "$(reason)"
call ${MSG}/GetMessage '{"id":"not-a-uuid"}'
expect "malformed id is InvalidArgument" InvalidArgument "${CODE}"

echo -e "\n10. Delete the author while it still has a message (expecting FAILED_PRECONDITION):"
call ${AUT}/DeleteAuthor "{\"id\":\"${AUTHOR_ID}\"}"
expect "delete author with messages is FailedPrecondition" FailedPrecondition "${CODE}"

echo -e "\n11. Get the author with their messages:"
call ${AUT}/GetAuthor "{\"id\":\"${AUTHOR_ID}\",\"include_messages\":true,\"messages_limit\":1,\"messages_offset\":0}"
expect "get author is OK" OK "${CODE}"
expect "author has one message" 1 "$(echo "${BODY}" | jq '.messages | length')"
expect "final page has no continuation" false "$(echo "${BODY}" | jq 'has("next_messages_offset")')"

echo -e "\n12. Delete message (${MSG_ID}):"
call ${MSG}/DeleteMessage "{\"id\":\"${MSG_ID}\"}"
expect "delete message is OK" OK "${CODE}"

echo -e "\n13. Delete author (${AUTHOR_ID}), now that its message is gone:"
call ${AUT}/DeleteAuthor "{\"id\":\"${AUTHOR_ID}\"}"
expect "delete author is OK" OK "${CODE}"

echo -e "\n=========================================================="
if [ "${FAILURES}" -eq 0 ]; then
  echo " API verification passed."
  echo "=========================================================="
else
  echo " API verification FAILED: ${FAILURES} check(s)."
  echo "=========================================================="
  exit 1
fi
