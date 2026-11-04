import { check, sleep } from 'k6';
import { randomIntBetween, uuidv4 } from 'https://jslib.k6.io/k6-utils/1.4.0/index.js';
import { grpc, MESSAGES, reasonOf, rpc, violationsOf } from './k6-common.js';

export const options = {
  vus: __ENV.VUS ? parseInt(__ENV.VUS, 10) : 10,
  duration: __ENV.DURATION || '10s',
  thresholds: {
    // Every call here is meant to fail. `checks` passing means the API returned the *correct*
    // status code and google.rpc error details (ErrorInfo reason, BadRequest violations) every time.
    checks: ['rate>0.99'],
    grpc_req_duration: ['p(95)<500'],  // 95% of calls below 500ms
  },
};

export default function () {
  // Invalid create: blank title/content and a blank authorId -> INVALID_ARGUMENT, BAD_USER_INPUT,
  // one field violation per field.
  const invalidRes = rpc(`${MESSAGES}/CreateMessage`, { title: '', content: ' ', authorId: '' }, 'CreateInvalidMessage');
  check(invalidRes, {
    'invalid create: INVALID_ARGUMENT': (r) => r.status === grpc.StatusInvalidArgument,
    'invalid create: BAD_USER_INPUT': (r) => reasonOf(r) === 'BAD_USER_INPUT',
    'invalid create: all three fields reported': (r) => violationsOf(r).length === 3,
  });

  // Well-formed but non-existent id -> NOT_FOUND.
  const notFoundRes = rpc(`${MESSAGES}/GetMessage`, { id: uuidv4() }, 'GetMissingMessage');
  check(notFoundRes, {
    'not found: NOT_FOUND status': (r) => r.status === grpc.StatusNotFound,
    'not found: NOT_FOUND reason': (r) => reasonOf(r) === 'NOT_FOUND',
  });

  // Malformed id: not a UUID, so validation fails before any lookup - INVALID_ARGUMENT, not NOT_FOUND.
  const badIdRes = rpc(`${MESSAGES}/GetMessage`, { id: `non-existent-${randomIntBetween(1, 1000000)}` }, 'GetMessageBadId');
  check(badIdRes, {
    'bad id: INVALID_ARGUMENT': (r) => r.status === grpc.StatusInvalidArgument,
    'bad id: names the id field': (r) => violationsOf(r).some((v) => v.field === 'id'),
  });

  // Oversized request (> 16 KiB inbound limit): rejected by gRPC before the service runs.
  const bigRes = rpc(`${MESSAGES}/CreateMessage`, { title: 't', content: 'x'.repeat(20000), authorId: uuidv4() }, 'CreateOversizedMessage');
  check(bigRes, {
    'oversized: RESOURCE_EXHAUSTED': (r) => r.status === grpc.StatusResourceExhausted,
  });

  // Simulate think time between 100ms and 300ms using k6-utils
  sleep(randomIntBetween(1, 3) * 0.1);
}
