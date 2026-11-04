# Database Debug Queries

Sample SQL for poking at the database directly — useful when an API response looks wrong and you
want to check what's actually in Postgres, bypassing the API (and the Hazelcast cache) entirely.
`messagedb` belongs to message-service and lives on a Cloud SQL for PostgreSQL instance provisioned
by Crossplane (see [README.md](README.md#architecture)).

## Connecting

The instance has **no public IP** - only its private IP inside the VPC - so there's nothing to
port-forward to directly, and the `default` namespace's NetworkPolicies only let the app and
postgres-exporter reach it. Use a throwaway pod in a namespace of its own (no NetworkPolicies, not
covered by the Kyverno enforce policies), and delete the namespace afterwards:

```bash
DB_IP=$(kubectl get secret cloudsql-connection -o jsonpath='{.data.privateIP}' | base64 -d)
kubectl create namespace db-shell

# psql prompts for the password - it never ends up in a pod spec that `view` users can read.
# Print it (then paste at the prompt) with:
#   kubectl get secret postgres-credentials -o jsonpath='{.data.DB_PASSWORD}' | base64 -d; echo
kubectl run psql -n db-shell --rm -it --restart=Never --image=postgres:16-alpine -- \
  psql "host=$DB_IP port=5432 dbname=messagedb user=message_app sslmode=require"

kubectl delete namespace db-shell
```

**DataGrip / other local clients** - relay through a pod, then port-forward to it:

```bash
kubectl run pg-relay -n db-shell --restart=Never --image=alpine/socat -- \
  tcp-listen:5432,fork,reuseaddr "tcp-connect:$DB_IP:5432"
kubectl port-forward -n db-shell pod/pg-relay 5433:5432
```

```
jdbc:postgresql://localhost:5433/messagedb?sslmode=require      # user message_app
```

(TLS is required - the instance is `ENCRYPTED_ONLY`. `sslmode=require` encrypts without verifying
the server certificate; the server CA is in the `cloudsql-connection` Secret for `verify-ca`.)

---

## message-service (`messagedb`)

Tables: `authors`, `messages` (see `app/models.py` and `migrations/versions/`), plus Alembic's own
`alembic_version`.

### 1. List all authors

```sql
SELECT id, name, email, created_at FROM authors ORDER BY created_at;
```

### 2. List all messages

```sql
SELECT id, title, author_id, version, created_at FROM messages ORDER BY created_at DESC;
```

### 3. Row counts

```sql
SELECT (SELECT count(*) FROM authors) AS authors, (SELECT count(*) FROM messages) AS messages;
```

### 4. A message with its author, joined

```sql
SELECT m.id, m.title, m.version, a.name AS author_name, a.email AS author_email
FROM messages m
JOIN authors a ON a.id = m.author_id
ORDER BY m.created_at DESC
LIMIT 20;
```

### 5. Messages per author (matches `GET /authors/{id}?include=messages`)

```sql
SELECT a.name, count(m.id) AS message_count
FROM authors a
LEFT JOIN messages m ON m.author_id = a.id
GROUP BY a.id, a.name
ORDER BY message_count DESC;
```

### 6. Check a message's current `version` (debugging a `409` from `PATCH /messages/{id}`)

The `409` means the client's `version` no longer matches this row — this is the authoritative value
(`GET /messages/{id}` is served from the Hazelcast cache, see [README.md's Architecture
section](README.md#architecture), so it can briefly lag the row after a burst of writes; see
[Concurrency & Transaction Isolation](README.md#concurrency--transaction-isolation)):

```sql
SELECT id, version, created_at FROM messages WHERE id = '<MESSAGE_ID>';
```

### 7. Find an author with no messages (safe to `DELETE /authors/{id}`)

`DELETE /authors/{id}` answers `409` while messages still reference the author — this finds candidates:

```sql
SELECT a.id, a.name
FROM authors a
LEFT JOIN messages m ON m.author_id = a.id
WHERE m.id IS NULL;
```

### 8. Reproduce the `GET /messages?limit&offset` pagination order

Same `ORDER BY` the API uses (`created_at DESC, id DESC`, served by the `ix_messages_created_at_id`
index) — useful for confirming a page boundary by hand:

```sql
SELECT id, title, created_at
FROM messages
ORDER BY created_at DESC, id DESC
LIMIT 10 OFFSET 20;
```

### 9. Duplicate/near-duplicate emails (shouldn't exist — `email` is unique)

```sql
SELECT email, count(*) FROM authors GROUP BY email HAVING count(*) > 1;
```

### 10. Messages created in the last hour, with author

Useful right after running a k6 script or a batch of manual creates:

```sql
SELECT m.id, m.title, a.name, m.created_at
FROM messages m
JOIN authors a ON a.id = m.author_id
WHERE m.created_at > now() - interval '1 hour'
ORDER BY m.created_at DESC;
```

### 11. Which migration is applied

```sql
SELECT version_num FROM alembic_version;
```

One row, the current Alembic revision (`0001` for the initial schema). No row or an error means
`alembic upgrade head` hasn't run against this database yet.
