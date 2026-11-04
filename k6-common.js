// Shared by the k6-*.js scenarios (imported, not run on its own).
//
// Target: GRPC_ADDR (default grpc.miqui.dev:443, TLS). For a local server set PLAINTEXT=true,
// e.g. GRPC_ADDR=localhost:9090 PLAINTEXT=true k6 run k6-create-messages.js
//
// The service descriptors come from server reflection, so no .proto files are needed here.
// Responses are proto3 JSON: camelCase fields, int64 as strings (totalCount), and fields at their
// default value omitted (a message at version 0 has no `version` key) - see versionOf().
import grpc from 'k6/net/grpc';

export const GRPC_ADDR = __ENV.GRPC_ADDR || 'grpc.miqui.dev:443';
const PLAINTEXT = __ENV.PLAINTEXT === 'true';

export const MESSAGES = 'message.v1.MessageService';
export const AUTHORS = 'message.v1.AuthorService';

// One client per VU, connected on first use: connect() is only allowed in VU code (setup,
// default, teardown), not in the init context.
const client = new grpc.Client();
let connected = false;

export function rpc(method, request, tag) {
  if (!connected) {
    client.connect(GRPC_ADDR, { plaintext: PLAINTEXT, reflect: true, timeout: '10s' });
    connected = true;
  }
  return client.invoke(method, request, tag ? { tags: { name: tag } } : {});
}

export const versionOf = (message) => message.version || 0;

// The google.rpc.ErrorInfo reason (BAD_USER_INPUT, NOT_FOUND, CONFLICT, ...) of a failed call.
export function reasonOf(response) {
  const details = (response.error && response.error.details) || [];
  const info = details.find((d) => d['@type'] === 'type.googleapis.com/google.rpc.ErrorInfo');
  return info ? info.reason : undefined;
}

// The google.rpc.BadRequest field violations of a failed call.
export function violationsOf(response) {
  const details = (response.error && response.error.details) || [];
  const bad = details.find((d) => d['@type'] === 'type.googleapis.com/google.rpc.BadRequest');
  return bad ? bad.fieldViolations : [];
}

export function createAuthor(name) {
  const res = rpc(`${AUTHORS}/CreateAuthor`, { name, email: `${name}-${Date.now()}@example.com` });
  if (res.status !== grpc.StatusOK) {
    throw new Error(`setup: failed to create author, status ${res.status}, error ${JSON.stringify(res.error)}`);
  }
  return res.message.id;
}

export { grpc };
