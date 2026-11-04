# gRPC API Design

The design of the `message-service` gRPC API: the resources, the conventions every RPC follows, and
the reasoning behind the parts that aren't obvious (optimistic locking, the error model, the cache,
and how the cost of a single request is bounded). The contract is
[`src/main/proto/message/v1/message_service.proto`](src/main/proto/message/v1/message_service.proto)
and is the source of truth for exact shapes; the server serves it through reflection, so
`grpcurl grpc.miqui.dev:443 describe message.v1.MessageService` always shows the running version.
Copy-paste calls live in [EXAMPLES.md](EXAMPLES.md).

## Resources

Two resources, both addressed by UUID string: `Author` and `Message` (a message belongs to exactly
one author). The tables are in
[`V1__initial_schema.sql`](src/main/resources/db/migration/V1__initial_schema.sql); the database stays
`snake_case`, and so do the proto field names.

| RPC | Purpose | Success | Errors (gRPC status) |
| --- | --- | --- | --- |
| `MessageService/ListMessages` | Page of messages, newest first | `{items, total_count}` | `INVALID_ARGUMENT` |
| `MessageService/GetMessage` | One message, author embedded (cache-aside) | `Message` | `INVALID_ARGUMENT`, `NOT_FOUND` |
| `MessageService/CreateMessage` | Create `{title, content, author_id}` | `Message` (`version` 0) | `INVALID_ARGUMENT`, `NOT_FOUND` unknown author |
| `MessageService/UpdateMessage` | Update `{id, title?, content, version}` | `Message` (`version` + 1) | `INVALID_ARGUMENT`, `NOT_FOUND`, **`ABORTED`** stale version |
| `MessageService/DeleteMessage` | Delete | `Empty` | `INVALID_ARGUMENT`, `NOT_FOUND` |
| `AuthorService/ListAuthors` | Page of authors, oldest first | `{items, total_count}` | `INVALID_ARGUMENT` |
| `AuthorService/GetAuthor` | One author, optionally with a page of messages (`include_messages`) | `{author, messages, next_messages_offset?}` | `INVALID_ARGUMENT`, `NOT_FOUND` |
| `AuthorService/CreateAuthor` | Create `{name, email}` | `Author` | `INVALID_ARGUMENT`, **`ALREADY_EXISTS`** duplicate email |
| `AuthorService/UpdateAuthor` | Update `{id, name?, email?}` | `Author` | `INVALID_ARGUMENT`, `NOT_FOUND`, `ALREADY_EXISTS` |
| `AuthorService/DeleteAuthor` | Delete | `Empty` | `INVALID_ARGUMENT`, `NOT_FOUND`, **`FAILED_PRECONDITION`** still has messages |
| `grpc.health.v1.Health/Check` | Standard health check (served by `grpc-services`) | `SERVING` / `NOT_SERVING` | - |
| `grpc.reflection.v1.ServerReflection` | Schema discovery for `grpcurl`, k6 | - | - |

All RPCs are unary. Kubelet probes and the load balancer's health check don't use gRPC: they hit
Spring Boot Actuator on a separate HTTP port (`:8081`, `/actuator/health/liveness` and
`/readiness`), which is not routed publicly.

Updates are partial: a field declared `optional` and **absent** means "unchanged" (proto3 tracks
presence for `optional` fields, so absent and empty are different). A message's `content` and
`version` are always required on update, because the version guard needs the caller to say which
state they read.

## Conventions

- **Status codes are real.** Each outcome has its own gRPC status, so clients, load balancers and the
  `grpc_errors_total{grpc_status_code}` metric all see failures without parsing a body. The code says
  what kind of failure; the `ErrorInfo.reason` (below) is the stable, finer-grained code to switch on.
- **Bad input is `INVALID_ARGUMENT`**, one status across validation, a malformed UUID, and an
  out-of-range pagination bound. A request larger than the server's 16 KB limit is rejected by the
  transport with `RESOURCE_EXHAUSTED` before it reaches the service.
- **Conflicts map to the status that fits the situation**: a stale `version` is `ABORTED` ("the
  operation was aborted because of a concurrent change - retry at a higher level", which is exactly
  read-again-and-retry), a duplicate email is `ALREADY_EXISTS`, and deleting an author who still has
  messages is `FAILED_PRECONDITION`. All three carry `ErrorInfo.reason = CONFLICT`.
- **`include_messages`** on `GetAuthor` embeds the author's messages as `MessageSummary` *without*
  their author. Results are one page, newest first (`created_at DESC, id DESC`): `messages_limit`
  defaults to 50 and is capped at 200, and `messages_offset` defaults to 0. If another page exists,
  `next_messages_offset` is present; pass it as `messages_offset` with the same author id,
  `include_messages: true`, and page size. When it is absent, paging is complete. Without
  `include_messages`, both the messages and continuation are omitted/empty; supplied pagination
  fields are still validated.
- **Timestamps** are `google.protobuf.Timestamp` (RFC 3339 in JSON). **`int64`** values (`total_count`,
  `messages_offset`, `next_messages_offset`) are strings in proto3 JSON, per the spec.
- **Field names.** The proto uses `snake_case`. The standard proto3 JSON mapping (what k6's
  `k6/net/grpc` and most gateways produce) is `lowerCamelCase`; `grpcurl` prints the proto names.
  Parsers accept either spelling on input. A field at its default value (for example `version: 0`) is
  omitted from JSON unless the client asks for defaults (`grpcurl -emit-defaults`).

## Validation

Hand-written in [`Violations.java`](src/main/java/dev/miqui/messageservice/validation/Violations.java)
and called by the gRPC service classes. Every failing field is reported at once, not just the first.

| Field | Rule |
| --- | --- |
| `title` | required, trimmed, non-blank, at most 100 characters |
| `content` | required, trimmed, non-blank, at most 1000 characters |
| `name` | required, trimmed, non-blank, at most 50 characters |
| `email` | required, trimmed, at most 100 characters, matches `^[^\s@]+@[^\s@]+\.[^\s@]+$` |
| `author_id`, `id` | canonical 8-4-4-4-12 UUID (`UUID.fromString` alone also accepts `1-2-3-4-5`) |
| `version` | required on update, `0` to 2,147,483,647 |
| `limit` | `1` to `200` (default `50` when absent) |
| `offset` | `0` or more (default `0` when absent) |
| `messages_limit` | `1` to `200` (default `50` when absent) |
| `messages_offset` | `0` or more, `int64` (default `0` when absent) |

All text fields (`title`, `content`, `name`, `email`) reject NUL (U+0000), which PostgreSQL cannot
store, with a field violation rather than allowing a database error to become `INTERNAL`.

On `UpdateMessage`/`UpdateAuthor`, an optional field that is *present* must pass the same check as on
create - a blank `title` is `INVALID_ARGUMENT`, not "leave it alone". Out-of-range pagination values
are rejected, never silently clamped: a client asking for `limit: 500` should learn that its
assumption is wrong.

## Error model

Every error is a [`google.rpc.Status`](https://cloud.google.com/apis/design/errors) sent in the
`grpc-status-details-bin` trailer (the gRPC "rich error model"), built in one place
([`GrpcErrorHandler`](src/main/java/dev/miqui/messageservice/grpc/GrpcErrorHandler.java)). It carries:

- a **`google.rpc.ErrorInfo`** whose `reason` is the stable machine-readable code clients should
  switch on (the human `message` may change) and `domain` is `message-service.miqui.dev`;
- for `INVALID_ARGUMENT` only, a **`google.rpc.BadRequest`** with one `FieldViolation {field,
  description}` per invalid field.

| `ErrorInfo.reason` | gRPC status | When |
| --- | --- | --- |
| `BAD_USER_INPUT` | `INVALID_ARGUMENT` | Validation failure, bad UUID, out-of-range pagination |
| `NOT_FOUND` | `NOT_FOUND` | The message, author, or (on create) the referenced author doesn't exist |
| `CONFLICT` | `ABORTED`, `ALREADY_EXISTS`, `FAILED_PRECONDITION` | Stale `version`, duplicate author email, deleting an author who still has messages |
| `INTERNAL_SERVER_ERROR` | `INTERNAL` | Anything unhandled. Logged with the trace id; the response never contains a stack trace or exception text |

A status the service didn't produce (for example `RESOURCE_EXHAUSTED` from an oversized message, or
`UNAVAILABLE` from the transport) carries no `ErrorInfo`; `grpc_errors_total{error_code}` records the
bare gRPC code for those. The proto imports `google/rpc/error_details.proto` without using it in any
field, purely so reflection also serves `ErrorInfo` and `BadRequest` and `grpcurl` can decode the
details instead of printing raw bytes. Clients in code read them with the standard helpers (for
example `StatusProto.fromThrowable` in Java, `status.FromError` + `Details()` in Go). See
[`k6-common.js`](k6-common.js) (`reasonOf`, `violationsOf`) for the same thing in k6.

Every error increments `grpc_errors_total{rpc_service, rpc_method, grpc_status_code, error_code}` and
writes one structured log line per RPC (`RpcMetricsInterceptor`).

## Optimistic locking

`Message.version` starts at `0` and every successful `UpdateMessage` adds one. The update is one
statement:

```sql
UPDATE messages SET title = COALESCE(:title, title), content = :content, version = version + 1
WHERE id = :id AND version = :version RETURNING id
```

It uses the version **the client sent**, not one the server re-reads: guarding against the server's
own just-read value only protects the milliseconds between its read and its write, and can't tell
that the client acted on stale data. When no row matches, the service checks whether the id exists:
`NOT_FOUND` if not, `ABORTED` if it does (the row moved on).

`k6-transaction-isolation.js` is the regression test: many clients increment a counter stored in one
message, and it asserts that the final `content` equals the final `version` - which holds only if no
write was applied against a stale read. The integration tests (`MessageServiceTest`, against a real
Postgres via Testcontainers) cover the same guarantee.

## Caching

`GetMessage` is cache-aside against a Hazelcast map (`cache/HazelcastMessageCache.java`): read the
cache, on a miss load from Postgres and populate it. `UpdateMessage` and `DeleteMessage` evict the
entry after their transaction commits (transactions are explicit, via `TransactionOperations`, so the
eviction visibly happens after the commit). Lists and author RPCs don't use the cache. There is no TTL
and no near cache (a near cache went stale across pods).

Cache hits reuse the message fields but reload the embedded author from Postgres. `UpdateAuthor`
therefore takes effect on subsequent message reads without invalidating every message by that
author. A hit costs one author lookup; a miss loads the message and author together.

`GetMessage`, `UpdateMessage`, and `DeleteMessage` share a Hazelcast per-message map lock across
replicas. A reader holds it through cache lookup, DB read, and cache fill; a mutation holds it through
DB commit and eviction. A slow reader cannot repopulate a deleted or obsolete message after an
eviction. Different message ids can proceed independently. Lock acquisition waits at most five
seconds; failure is logged and returned as `INTERNAL`, never treated as an unlocked cache access.
The lock has no lease that could expire mid-operation and is released in `finally`. Stale-version
failures still evict defensively before returning `ABORTED`.

When first deploying this locking protocol, stop all old application replicas and clear the
Hazelcast `messages` map before starting the new replicas: older replicas do not participate in the
locks, and existing stale entries are not repaired by introducing locks. Afterwards every writer
must use the same protocol; direct database edits also require cache invalidation.

The cache is **mandatory**: an unreachable Hazelcast member fails startup rather than running
without it, and a client that gave up reconnecting fails liveness so the container is restarted.

## Bounding the cost of a request

There's no query language to constrain, so the cost of one request is bounded by ordinary limits:

- **Pagination** - `limit` is capped at 200, so no list response is unbounded.
  `GetAuthor(include_messages=true)` also returns at most 200 messages and fetches at most one
  additional row to determine whether a next page exists; it never loads the entire relationship.
- **Field lengths** - per-field maximums (above), so a single message is at most a few KB.
- **Message size** - inbound messages over 16 KB are rejected by the gRPC server
  (`spring.grpc.server.inbound.message.max-size`).
- **Connection pool** - each pod has at most 10 database connections (`DB_POOL_MAX`), so 6 pods stay
  well under Cloud SQL's `max_connections = 100`.
- **Relationships** - `include_messages` returns a bounded page of messages without their author.
  The `(author_id, created_at DESC, id DESC)` index in Flyway migration V2 supports this query.

### Author-message pagination compatibility

`GetAuthor(include_messages=true)` now returns a page rather than every message. Existing requests
still parse and return up to 50 messages by default, but clients requiring the entire collection
must inspect `next_messages_offset` and continue until it is absent. This is an intentional
behavior change to bound response memory and size; the new fields do not reuse existing numbers.
As with the existing offset-based list APIs, concurrent inserts/deletes can shift page boundaries:
this is not a snapshot or a cursor guarantee.

## Exposure

- **Reflection**: on, and public, like everything else - it is what lets `grpcurl` and k6 work
  without the `.proto`. Turn it off with `spring.grpc.server.reflection.enabled=false` anywhere real.
- **Transport**: TLS terminates at the Google load balancer (`grpc.miqui.dev:443`, TLS 1.2+); the
  hop to the pods is cleartext HTTP/2 (h2c) inside the VPC.
- **Browsers**: native gRPC doesn't work from a browser (it needs gRPC-Web or a proxy), and this
  service has none, so there is no CORS configuration. `grpcurl`, k6 and gRPC client libraries are
  unaffected.
- **Authentication**: none. This is a dev-cluster service; a real deployment would put authn/authz in
  front of it (a server interceptor validating a token in the call metadata), and rate limiting,
  which is out of scope here.

## Evolving the API

- Adding an optional response field, an optional request field, or a new RPC is backwards compatible.
  Never reuse or renumber a field number; `reserved` a removed one.
- `ErrorInfo.reason` values are part of the contract: add new ones, don't repurpose existing ones.
- Breaking changes (a renamed or retyped field, a changed status) would go in a new package
  (`message.v2`) served alongside `message.v1`.
- A new table or column is a new Flyway migration (`src/main/resources/db/migration/V<n>__*.sql`).
  Migrations run at every container start under Flyway's Postgres advisory lock, so each must be safe
  to run while the previous version's pods are still serving - add columns as nullable or with a
  default before the code that requires them ships.
