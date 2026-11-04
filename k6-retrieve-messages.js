import { check, sleep } from 'k6';
import { randomIntBetween } from 'https://jslib.k6.io/k6-utils/1.4.0/index.js';
import { grpc, MESSAGES, rpc } from './k6-common.js';

export const options = {
  vus: __ENV.VUS ? parseInt(__ENV.VUS, 10) : 10,
  duration: __ENV.DURATION || '10s',
  thresholds: {
    checks: ['rate>0.99'],
    grpc_req_duration: ['p(95)<500'],  // 95% of calls below 500ms
  },
};

export default function () {
  const res = rpc(`${MESSAGES}/ListMessages`, { limit: 50 }, 'ListMessages');
  check(res, {
    'status is OK': (r) => r.status === grpc.StatusOK,
    'has at least one item': (r) => !!(r.message && r.message.items && r.message.items.length > 0),
    // int64 -> a decimal string in proto3 JSON
    'has totalCount': (r) => !!r.message && !Number.isNaN(parseInt(r.message.totalCount, 10)),
  });

  // Paginate with limit/offset: a small page comes back at most that size (limit is 1-200).
  const pageRes = rpc(`${MESSAGES}/ListMessages`, { limit: 5 }, 'ListMessagesPage');
  check(pageRes, {
    'page: status is OK': (r) => r.status === grpc.StatusOK,
    'page: at most 5 items': (r) => !!r.message && (r.message.items || []).length <= 5,
  });

  // Simulate think time between 100ms and 300ms using k6-utils
  sleep(randomIntBetween(1, 3) * 0.1);
}
