import { check, sleep } from 'k6';
import { randomIntBetween } from 'https://jslib.k6.io/k6-utils/1.4.0/index.js';
import { createAuthor, grpc, MESSAGES, rpc, versionOf } from './k6-common.js';

export const options = {
  vus: __ENV.VUS ? parseInt(__ENV.VUS, 10) : 10,
  duration: __ENV.DURATION || '10s',
  thresholds: {
    checks: ['rate>0.99'],
    grpc_req_duration: ['p(95)<500'],  // 95% of calls below 500ms
  },
};

export function setup() {
  return { authorId: createAuthor('k6-message-lifecycle') };
}

export default function (data) {
  // Create
  const createRes = rpc(`${MESSAGES}/CreateMessage`, {
    title: `Lifecycle message ${randomIntBetween(1, 1000000)}`,
    content: 'Created by k6-message-lifecycle.js',
    authorId: data.authorId,
  }, 'CreateMessage');
  const created = check(createRes, {
    'create: status is OK': (r) => r.status === grpc.StatusOK,
  });
  if (!created) {
    sleep(randomIntBetween(1, 3) * 0.1);
    return;
  }
  const id = createRes.message.id;

  // Read (cache-aside: a miss, then populated)
  const getRes = rpc(`${MESSAGES}/GetMessage`, { id }, 'GetMessage');
  check(getRes, {
    'read: status is OK': (r) => r.status === grpc.StatusOK,
    'read: id matches': (r) => !!r.message && r.message.id === id,
  });

  // Update - version 0 matches the just-created message; a stale version would be ABORTED.
  const updateRes = rpc(`${MESSAGES}/UpdateMessage`, {
    id,
    title: 'Updated by k6-message-lifecycle.js',
    content: 'Updated content',
    version: 0,
  }, 'UpdateMessage');
  check(updateRes, {
    'update: status is OK': (r) => r.status === grpc.StatusOK,
    'update: version bumped to 1': (r) => !!r.message && versionOf(r.message) === 1,
  });

  // Delete
  const deleteRes = rpc(`${MESSAGES}/DeleteMessage`, { id }, 'DeleteMessage');
  check(deleteRes, {
    'delete: status is OK': (r) => r.status === grpc.StatusOK,
  });

  // Simulate think time between 100ms and 300ms using k6-utils
  sleep(randomIntBetween(1, 3) * 0.1);
}
