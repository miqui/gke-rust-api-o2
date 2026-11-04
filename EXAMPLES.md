# API Examples

Example [`grpcurl`](https://github.com/fullstorydev/grpcurl) calls against the message-service gRPC
API, ordered simple to complex. All of them target the public endpoint `grpc.miqui.dev:443` (TLS); for
a local `./mvnw spring-boot:run` use `localhost:9090` with `-plaintext`. The schema comes from server
reflection, so no `.proto` file is needed. See [README.md](README.md#grpc-api) for the RPC summary and
[API-DESIGN.md](API-DESIGN.md) for the status codes, validation rules and error model. `./test-api.sh`
runs most of these as an automated smoke test.

Replace placeholder IDs (`<AUTHOR_ID>`, `<MESSAGE_ID>`) with real ones from your own data - run
example 1 first to find some. Every example uses `jq` where it helps; drop `| jq` if you don't have it.

```bash
ADDR=grpc.miqui.dev:443          # local: ADDR=localhost:9090 and add -plaintext to every grpcurl
M=message.v1.MessageService
A=message.v1.AuthorService
```

`grpcurl` prints the proto field names (`created_at`, `total_count`) and omits fields at their default
value (a message at `version` 0 has no `version` key) unless you add `-emit-defaults`. `int64` values
such as `total_count` are strings in JSON.

---

### 1. Discover the API

```bash
grpcurl $ADDR list                       # services
grpcurl $ADDR list $M                    # methods of one service
grpcurl $ADDR describe $M.CreateMessage  # a method and its request/response types
grpcurl $ADDR describe message.v1.CreateMessageRequest
```

```
grpc.health.v1.Health
grpc.reflection.v1.ServerReflection
message.v1.AuthorService
message.v1.MessageService
```

### 2. Health

```bash
grpcurl $ADDR grpc.health.v1.Health/Check
```

```json
{ "status": "SERVING" }
```

### 3. List authors

```bash
grpcurl $ADDR $A/ListAuthors
```

```json
{
  "items": [
    { "id": "20a55a7e-528b-4459-a0ce-482d5b9489f7", "name": "system",
      "email": "system@message-service.local", "created_at": "2026-09-30T16:40:06.283066Z" }
  ],
  "total_count": "1"
}
```

Authors are paginated like messages (`limit` 50 and `offset` 0 by default), oldest first.

### 4. Get a single author or message by ID

```bash
grpcurl -d '{"id": "<AUTHOR_ID>"}'  $ADDR $A/GetAuthor
grpcurl -d '{"id": "<MESSAGE_ID>"}' $ADDR $M/GetMessage
```

`GetMessage` embeds the author. The first read of a message loads it from Postgres and populates the
Hazelcast cache; later reads are served from the cache until the message is updated or deleted.

### 5. Create an author

```bash
grpcurl -d '{"name": "Ada Lovelace", "email": "ada@example.com"}' $ADDR $A/CreateAuthor
```

Answers with the created author. A second author with the same email is `ALREADY_EXISTS`
(`ErrorInfo.reason` `CONFLICT`).

### 6. Create a message

```bash
grpcurl -d '{"title": "Hello", "content": "First message", "author_id": "<AUTHOR_ID>"}' $ADDR $M/CreateMessage
```

The new message is at `version` 0 (omitted from the JSON; add `-emit-defaults` to see it). An unknown
`author_id` is `NOT_FOUND`.

### 7. Paginate messages - first page

```bash
grpcurl -d '{"limit": 5, "offset": 0}' $ADDR $M/ListMessages | jq '{total_count, ids: [.items[].id]}'
```

Newest first. `total_count` is the total number of messages, independent of `limit`/`offset`.

### 8. Paginate messages - next page

```bash
grpcurl -d '{"limit": 5, "offset": 5}' $ADDR $M/ListMessages | jq '{total_count, ids: [.items[].id]}'
```

`limit` must be `1`-`200` and `offset` must be `>= 0`; anything else is `INVALID_ARGUMENT` (see
example 11).

### 9. Update a message with optimistic locking

Send back the `version` you read. `title` is optional; `content` and `version` are required.

```bash
grpcurl -emit-defaults -d '{"id": "<MESSAGE_ID>", "title": "Hello (edited)", "content": "Edited content", "version": 0}' \
  $ADDR $M/UpdateMessage | jq '{version, title}'
```

```json
{ "version": 1, "title": "Hello (edited)" }
```

Repeat the same request: the row is now at version 1, so version 0 is stale and the update is refused
without touching the row.

```bash
grpcurl -d '{"id": "<MESSAGE_ID>", "content": "Stale write", "version": 0}' $ADDR $M/UpdateMessage
```

```
ERROR:
  Code: Aborted
  Message: Message with ID '...' has changed since version 0 was read; refetch and retry.
  Details:
  1)	{
    	  "@type": "type.googleapis.com/google.rpc.ErrorInfo",
    	  "domain": "message-service.miqui.dev",
    	  "reason": "CONFLICT"
    	}
```

The fix is the client's read-again-and-retry: `GetMessage`, then resend with the new `version`.

### 10. An author with their messages

```bash
grpcurl -d '{"id": "<AUTHOR_ID>", "include_messages": true, "messages_limit": 50, "messages_offset": "0"}' \
  $ADDR $A/GetAuthor | jq '{name: .author.name, messages: [.messages[].title], next_messages_offset}'
```

This returns one page (default 50 messages, maximum 200), not the entire collection. If
`next_messages_offset` is present, request the next page using that value:

```bash
grpcurl -d '{"id": "<AUTHOR_ID>", "include_messages": true, "messages_limit": 50, "messages_offset": "<NEXT_MESSAGES_OFFSET>"}' \
  $ADDR $A/GetAuthor
```

Both offsets are `int64`, represented as strings in proto3 JSON. Continue until
`next_messages_offset` is absent. Pages are newest first; concurrent writes can
shift offset-based page boundaries. Without `include_messages` the `messages` list is empty and
there is no continuation. The embedded messages don't repeat the author, so there is no nesting.

### 11. Error responses

Every error is a `google.rpc.Status` whose `ErrorInfo.reason` is a stable code. Validation failures
list every bad field at once in a `BadRequest` detail:

```bash
grpcurl -d '{"title": "", "content": "", "author_id": "not-a-uuid"}' $ADDR $M/CreateMessage
```

```
ERROR:
  Code: InvalidArgument
  Message: The request content was invalid or failed validation constraints.
  Details:
  1)	{ "@type": "type.googleapis.com/google.rpc.ErrorInfo",
    	  "domain": "message-service.miqui.dev", "reason": "BAD_USER_INPUT" }
  2)	{ "@type": "type.googleapis.com/google.rpc.BadRequest",
    	  "fieldViolations": [
    	    { "field": "title",     "description": "title is required and cannot be blank" },
    	    { "field": "content",   "description": "content is required and cannot be blank" },
    	    { "field": "author_id", "description": "author_id must be a valid UUID" } ] }
```

Other cases:

```bash
# NOT_FOUND - a well-formed id that doesn't exist
grpcurl -d '{"id": "00000000-0000-0000-0000-000000000000"}' $ADDR $M/GetMessage

# INVALID_ARGUMENT / BAD_USER_INPUT - a malformed id (the violation names "id")
grpcurl -d '{"id": "not-a-uuid"}' $ADDR $M/GetMessage

# INVALID_ARGUMENT / BAD_USER_INPUT - out-of-range pagination (never silently clamped)
grpcurl -d '{"limit": 500, "offset": -1}' $ADDR $M/ListMessages

# INVALID_ARGUMENT / BAD_USER_INPUT - PostgreSQL cannot store NUL in a text field
grpcurl -d '{"title": "Hello\u0000world", "content": "Body", "author_id": "<AUTHOR_ID>"}' \
  $ADDR $M/CreateMessage

# FAILED_PRECONDITION (reason CONFLICT) - deleting an author that still has messages
grpcurl -d '{"id": "<AUTHOR_ID>"}' $ADDR $A/DeleteAuthor
```

In a script, read the exit code (`grpcurl` exits non-zero on any non-OK status, printing the error to
stderr) and parse `"reason"` from the details - see `reason()` in [`test-api.sh`](test-api.sh).

### 12. Full lifecycle in one script

```bash
#!/usr/bin/env bash
set -euo pipefail
ADDR=grpc.miqui.dev:443
M=message.v1.MessageService
A=message.v1.AuthorService

# Create an author and a message.
AUTHOR_ID=$(grpcurl -d "{\"name\": \"Demo\", \"email\": \"demo-$(date +%s)@example.com\"}" $ADDR $A/CreateAuthor | jq -r .id)
MSG_ID=$(grpcurl -d "{\"title\": \"Demo\", \"content\": \"Hello\", \"author_id\": \"$AUTHOR_ID\"}" $ADDR $M/CreateMessage | jq -r .id)

# Read it, update it (version 0 -> 1), read it again.
grpcurl -d "{\"id\": \"$MSG_ID\"}" $ADDR $M/GetMessage | jq '{title, version}'
grpcurl -emit-defaults -d "{\"id\": \"$MSG_ID\", \"content\": \"Hello again\", \"version\": 0}" $ADDR $M/UpdateMessage | jq '{content, version}'
grpcurl -d "{\"id\": \"$MSG_ID\"}" $ADDR $M/GetMessage | jq '{content, version}'

# Walk every page of messages.
OFFSET=0; LIMIT=50
while :; do
  PAGE=$(grpcurl -d "{\"limit\": $LIMIT, \"offset\": $OFFSET}" $ADDR $M/ListMessages)
  echo "$PAGE" | jq -r '.items[].title'
  TOTAL=$(echo "$PAGE" | jq -r .total_count)
  OFFSET=$((OFFSET + LIMIT))
  [ "$OFFSET" -ge "$TOTAL" ] && break
done

# Clean up: the message first, then its author (FAILED_PRECONDITION while any message remains).
grpcurl -d "{\"id\": \"$MSG_ID\"}" $ADDR $M/DeleteMessage
grpcurl -d "{\"id\": \"$AUTHOR_ID\"}" $ADDR $A/DeleteAuthor
```
