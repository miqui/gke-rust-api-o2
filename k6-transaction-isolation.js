import { check, sleep } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';
import { createAuthor, grpc, MESSAGES, rpc, versionOf } from './k6-common.js';

// Proves UpdateMessage's optimistic locking (Messages.update / MessageRepository.updateIfVersion)
// actually closes the lost-update gap for real clients: many VUs race to increment a counter
// stored in one message's `content` field, each doing its own read (content + version) then an
// UpdateMessage that submits content+1 guarded by the version it read. The server answers ABORTED
// whenever the row changed since that version was read - so out of N attempts, some legitimately
// lose the race and get ABORTED (expected, not a bug), but every success must correspond to a
// real, distinct +1.
//
// The invariant checked in teardown: every successful update bumps `version` by exactly 1 and
// sets content = (content it read) + 1. Starting from content "0" / version 0, the two therefore
// stay equal if and only if no write was applied against a stale read. Under a server-side-only
// CAS (re-reading its own "current" version right before writing instead of trusting the version
// the client read) they diverge: the DB's version outruns the counter.
export const options = {
  vus: __ENV.VUS ? parseInt(__ENV.VUS, 10) : 20,
  duration: __ENV.DURATION || '15s',
  thresholds: {
    // ABORTED is an expected outcome, so the check accepts it; anything else fails the check.
    checks: ['rate>0.99'],
    // Teardown: final content == final version. Any lost update fails the run.
    no_lost_updates: ['rate==1'],
  },
};

const successfulIncrements = new Counter('successful_increments');
const writeConflicts = new Counter('write_conflicts');
const finalCounterValue = new Trend('final_counter_value');
const noLostUpdates = new Rate('no_lost_updates');

export function setup() {
  const authorId = createAuthor('k6-isolation-test');
  const res = rpc(`${MESSAGES}/CreateMessage`, { title: 'k6-transaction-isolation counter', content: '0', authorId });
  if (res.status !== grpc.StatusOK) {
    throw new Error(`setup: failed to create counter message, status ${res.status}, error ${JSON.stringify(res.error)}`);
  }
  return { id: res.message.id };
}

export default function (data) {
  const getRes = rpc(`${MESSAGES}/GetMessage`, { id: data.id }, 'ReadCounter');
  const read = check(getRes, {
    'read: status is OK': (r) => r.status === grpc.StatusOK,
  });
  if (!read) return;

  const current = parseInt(getRes.message.content, 10);
  const readVersion = versionOf(getRes.message);

  const updateRes = rpc(`${MESSAGES}/UpdateMessage`, {
    id: data.id,
    content: String(current + 1),
    version: readVersion,
  }, 'IncrementCounter');

  const isSuccess = updateRes.status === grpc.StatusOK;
  const isConflict = updateRes.status === grpc.StatusAborted;

  check(updateRes, {
    'write: OK or ABORTED': () => isSuccess || isConflict,
  });

  if (isSuccess) {
    successfulIncrements.add(1);
  } else if (isConflict) {
    writeConflicts.add(1);
  }
}

export function teardown(data) {
  // Reads are cache-aside. A slow reader can repopulate the cache with a pre-update row right
  // after an update's eviction; the next stale-version ABORTED evicts it again, but no writers are
  // left at the tail of a burst to do that. Poll until two consecutive reads agree so the check
  // runs against settled state rather than a transient stale entry.
  let content = NaN;
  let version = NaN;
  let previous = null;
  for (let attempt = 0; attempt < 20; attempt++) {
    const res = rpc(`${MESSAGES}/GetMessage`, { id: data.id });
    if (res.status === grpc.StatusOK) {
      content = parseInt(res.message.content, 10);
      version = versionOf(res.message);
      const snapshot = `${content}/${version}`;
      if (snapshot === previous) break;
      previous = snapshot;
    }
    sleep(0.3);
  }
  finalCounterValue.add(content);
  noLostUpdates.add(content === version);

  console.log(`\nFinal state: counter (content) = ${content}, version = ${version}.`);
  console.log('They must be equal. Compare both against "successful_increments" in the summary '
    + 'below: also exactly equal. "write_conflicts" (ABORTED) are expected under contention and '
    + 'are not lost updates - the client is told to refetch and retry.');

  rpc(`${MESSAGES}/DeleteMessage`, { id: data.id });
}
