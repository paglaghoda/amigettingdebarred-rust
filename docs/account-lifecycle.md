# Account lifecycle (phase 4 backend)

## Integration boundary

`crates/server/src/account.rs` exposes `routes() -> Router<AppState>` and the
application router merges these routes before `.with_state(state)`. The scoped
account changes do **not** edit `lib.rs`, `main.rs`, configuration, dependencies,
or web code. The integration tests also mount the exact module directly; no
duplicate handlers are used. Apply the normal SQLx migrations, including
`0002_account_lifecycle.sql` and `0003_migration_audit.sql`, before serving the
new backend. No provider admin/service key is required.

## Export

`GET /api/account/export` requires the existing opaque session cookie. Optional
`X-Session-User` is a stale-page precondition only: if supplied it must be a single
matching UUID. It never supplies the identity for a query.

The JSON format is version 1:

- `version`: `1`
- `generated_at`: database transaction timestamp
- `profile`: **only** `user_id`, `email`, `display_name`, from authenticated context
- `plans`: every owned plan, each containing `plan`, `revision`, `updated_at`
- `ai_usage`: every retained owned usage row, each containing `day`, `kind`, `used`
- `migration_history`: owned migration-row metadata, containing **only**
  `semester_id`, `inserted_revision`, `plan_storage_sha256`, and the joined batch's
  `applied_at` and `status`. There are no batch IDs, other-user row counts,
  source/mapping hashes, batch fingerprints, or whole-batch objects.

Plans, usage, and migration history are read in one read-only REPEATABLE READ
transaction, ordered by semester, day/kind, and batch application time/batch ID/
semester respectively. Batch ID is only an internal join/sort key, not exported. Profile is the authenticated session's profile,
not a separate provider lookup. Export is not an archive of already expired usage
removed by the existing retention job. Neither sessions nor OAuth flows are
exported. There are no cookies, CSRF tokens, token hashes, encrypted token blobs,
provider access/refresh tokens, or provider keys in the response. Explicit field
allowlists prevent serializing `AuthUser` or a session row. Responses, including
errors, carry `Cache-Control: no-store`, `Pragma: no-cache`, and
`Referrer-Policy: no-referrer`. Exported attendance/profile data is still private;
users should store downloaded exports securely.

## Request deletion

`POST /api/account/deletion` accepts exactly:

```json
{"confirmation":"DELETE MY ACCOUNT"}
```

Unknown fields and any other confirmation fail without deleting data. The normal
`Authenticated` extractor checks the cookie, exact Origin, CSRF header, and optional
`X-Session-User` precondition. No body/query parameter chooses a user.

The same cookie session must have an interactive login no more than five minutes
ago and still be valid. If it is too old, sign out and sign in again within five
minutes before retrying. Access-token refresh does **not** reset this age. The
migration assigns existing sessions `authenticated_at = '-infinity'`, then changes
the insertion default to `now()`; these legacy sessions must sign in again. Both
successful PKCE and local interactive login explicitly record the current database
time. Refresh SQL intentionally never writes `authenticated_at`.

A single database transaction:

1. Takes the per-user transaction advisory lock, keyed by
   `hashtextextended(user_uuid::text, 0)`.
2. Queues the UUID in `app_private.account_deletion_requests`. The only other
   column is `requested_at`; no email, provider identifiers, or tokens are copied.
3. Deletes all owned plans, AI usage, migration audit rows (`migration_rows`),
   and **all** application sessions under the same advisory lock.
4. Commits before acknowledging success. The deletion response sends **no
   Set-Cookie header**, including no cookie-expiration header.

Deleting all owned server sessions is the authoritative revocation. The old opaque
cookie is inert and can remain in the browser until a subsequent login replaces
it. A delayed deletion response for account A must not expire the shared cookie
after account B has signed in while A's request was waiting for a database lock.
Normal logout retains its separate cookie behavior; deletion does not call the
cookie-clearing helper.

Success is HTTP **202**, not a claim of completed identity erasure:

```json
{"status":"pending_identity_deletion","app_data_deleted":true}
```

The whole route is bounded to 30 seconds, deletion transaction work to 25 seconds,
with shorter database lock/statement timeouts. An error, cancellation, or timeout
rolls back unfinished work: never treat an error as confirmation that data was
erased. If a commit succeeds but the response is lost, the old session will no
longer work; an operator can check the UUID queue. The endpoint deliberately does
not offer an unauthenticated status lookup or automatic retry login.

The global `migration_batches` audit is retained unchanged: it has no individual
user identifiers. Removing an owned migration row also removes its UUID, semester,
and plan hashes from that audit. This intentionally makes a later whole-batch
rollback fail its incomplete-audit check rather than reconstruct deleted personal
metadata or silently perform a partial rollback. Privacy erasure takes precedence;
do not restore audit rows to make rollback succeed.

## Pending is not complete

**This does not delete the Supabase identity or the Google account.** It does not
call a provider admin API or revoke upstream Google authorization. Supabase
identity deletion is a later, manual operator action in the project's Supabase
Dashboard. Deleting the Supabase identity is also not deletion of a Google account.
User-facing copy must not say “account fully deleted” for a 202 response.

While pending, provider login can succeed upstream, but local session persistence
returns Forbidden and issues no cookie. The account cannot recreate app data.
The pending row stays until the identity is deleted; its foreign key cascades on
`auth.users` deletion. Do not remove the pending marker to mark a task complete
while retaining the identity: that would reopen login and writes.

## Concurrency contract

Guaranteed server paths use READ COMMITTED transactions and take
`app_private.lock_account_write(uuid)` **before any row DML** for plan saves,
quota consumption, and session insertion. It takes the user advisory lock and
checks pending state with a fresh snapshot after waiting. Quota consumption
retains its one atomic conditional UPSERT, inside the lock transaction. Plan
revision checks remain conditional; there is no lost-update read/modify/write.
Deletion explicitly sets `SET TRANSACTION ISOLATION LEVEL READ COMMITTED`
immediately after BEGIN, before any snapshot-taking query, then takes the same
lock before queuing and deleting. It must not inherit a REPEATABLE READ default:
the advisory-lock SELECT could otherwise pin a snapshot before its lock wait,
hiding rows that the preceding writer commits during that wait. Consequently a writer
that wins first commits before deletion erases its writes; a writer that waits
behind deletion sees pending and fails. This includes an old handler that already
authenticated before deletion.

Row triggers on plan/usage INSERT and UPDATE repeat the guard. Ownership updates
are rejected. Session INSERT has the same guard; session UPDATE deliberately does
**not** acquire the advisory lock. Refresh already holds its session row lock over
a provider call (bounded to 20 seconds); deletion may wait for it, then removes
that row. Refresh cannot insert a replacement session, restore a deleted row, or
extend the interactive-login timestamp. This avoids a refresh-row -> advisory /
delete-advisory -> row deadlock.

Triggers alone do not establish correct lock order for arbitrary direct plan/usage
UPDATEs: PostgreSQL can lock the row before invoking a row trigger. All future
server writers must use the helper before DML, not merely rely on triggers. An
administrator or other out-of-contract writer can still cause a detected deadlock;
PostgreSQL aborts a transaction, and deletion must fail closed rather than report
partial success. Do not add a session UPDATE advisory trigger. Cleanup deletes do
not recreate data and do not take advisory locks. SQL errors sent to clients are
sanitized; pending session persistence uses the existing Forbidden error without
raw provider response details.

## Least-privilege database roles

`app_private.lock_account_write(uuid)` is a narrowly scoped **SECURITY DEFINER**
function owned by the trusted migration role, which also owns the deletion queue.
It only takes a transaction advisory lock, checks for a pending marker, and raises
an error; it performs no privileged table writes and returns no queue contents.
The queue is fully qualified, builtins are qualified with `pg_catalog`, and the
function fixes `search_path` to `pg_catalog, app_private`. Do not grant runtime or
API roles CREATE in either trusted schema or ownership of this function/queue.

Definer rights are necessary even when a restricted backend has SELECT on the
queue: without a matching RLS policy, an invoker-rights EXISTS could see zero rows
and fail open for a pending account. The migration owner sees the marker regardless
of the runtime role's policies. Keep that owner trusted and able to read all queue
rows (table ownership bypasses RLS unless FORCE ROW LEVEL SECURITY is enabled).
Do not change queue ownership/RLS behavior without preserving that guarantee.
The trigger function remains invoker-rights and calls the same definer helper;
reentrant acquisition preserves the existing server lock order.

EXECUTE remains revoked from PUBLIC, `anon`, and `authenticated`; migrations do
not invent or grant a backend role. For a separately provisioned, trusted server
runtime role, an authorized operator must grant only the helper execution needed
by its server paths, in addition to its existing private schema/table grants and
appropriate RLS policies. For example, replace `backend_runtime` with the actual
server-only role before running:

```sql
GRANT EXECUTE ON FUNCTION app_private.lock_account_write(uuid) TO backend_runtime;
```

Do not grant this to browser/API roles, expose it as a public RPC, or grant direct
EXECUTE on the trigger function. Do not grant broad BYPASSRLS or ownership to the
backend just to make this guard work. This helper grant does not replace the
separate least-privilege grants/policies needed for exports and transactional
queue insertion/deletion. No provider service key is involved.

For migration-history export and erasure, the server-only runtime additionally
needs SELECT and DELETE on `migration_rows`, **not INSERT or UPDATE**. Batch access
must be column-limited to the join key, application time, and status; there is no
runtime need to read global counts, source/mapping hashes, or fingerprints:

```sql
GRANT SELECT, DELETE ON app_private.migration_rows TO backend_runtime;
GRANT SELECT (batch_id, applied_at, status)
  ON app_private.migration_batches TO backend_runtime;
```

These are operator provisioning examples, not automatic grants. Both tables have
RLS enabled, so the runtime also needs appropriate SELECT policies on both tables
and a DELETE policy on `migration_rows`. Grants alone are insufficient: missing
policies can silently hide export rows or filter deletion to zero rows. Verify
visibility and deletion under the actual runtime role before deployment. Do not
grant batch DELETE/INSERT/UPDATE or whole-table batch SELECT to support this API;
keep migration import/rollback privileges separate from the application runtime.

## Manual operator review

Use the existing authenticated browser flow to request deletion. Do not request
cookies, CSRF values, passwords, or provider tokens through support. Review only
UUIDs and timestamps in an authorized operator SQL session:

```sql
SELECT user_id, requested_at
FROM app_private.account_deletion_requests
ORDER BY requested_at;

-- Bind a reviewed queue UUID as $1. All four counts should be zero.
SELECT
  (SELECT count(*) FROM app_private.student_plans WHERE user_id = $1) AS plans,
  (SELECT count(*) FROM app_private.ai_usage WHERE user_id = $1) AS usage,
  (SELECT count(*) FROM app_private.auth_sessions WHERE user_id = $1) AS sessions,
  (SELECT count(*) FROM app_private.migration_rows WHERE user_id = $1) AS migration_rows;
```

Review the matching **UUID** in the correct Supabase project's Authentication
Dashboard and perform its identity-deletion operation under your organization's
approval/retention policy. Do not map accounts automatically by email: emails can
change, be shared, or refer to different identities. No application production
code writes `auth.users`. No new provider administrator client or service key is
introduced. After confirmed Dashboard deletion, verify that the queue UUID has
cascaded away and the identity is absent. Investigate failures rather than
manually dropping queue rows. Backups, database/provider retention, logs, and
previous user-downloaded exports have their own retention policies; this endpoint
does not promise to erase those copies.

## Validation

The new tests are ignored by default and require an explicitly selected disposable
PostgreSQL database. They bootstrap minimal `auth.users` **only in tests**, apply
the real migrations, seed real AES-GCM session ciphertext bound to cookie hashes,
and use a loopback mock provider (no external credentials or provider calls).
Never point `TEST_DATABASE_URL` at production. The restricted-role regression
test additionally requires permission to create and SET ROLE to a NOLOGIN test
role. Its role, policies, grants, and fixture rows are created inside one transaction
and rolled back; it verifies the role no longer exists afterward.

```sh
cargo test -p debarred-server --test account
# Set TEST_DATABASE_URL to a disposable local database first:
cargo test -p debarred-server --test account -- --ignored --test-threads=1
```

Run the ignored suite serially: its snapshot test intentionally takes a table lock
to prove export cannot mix plan and usage versions. Tests cover export allowlists,
owner isolation, auth preconditions, confirmation rejection, legacy/expired login
age, refresh not extending freshness, interactive login recording freshness,
owned erasure/other-user preservation, pending login and raw insert guards, both
write/deletion orderings, refresh-row/deletion lock ordering, and a restricted
runtime role whose write policies allow plans/sessions but whose queue RLS hides
pending rows. Active-account inserts succeed under that role; pending-account
inserts must still raise the guard's permission error. Additional regressions use
a pool with `after_connect` setting a REPEATABLE READ default to verify deletion
erases plan, quota, and session inserts committed while it waits, and verify that
a delayed deletion response sends no Set-Cookie while another account's new
interactive-login session remains valid. A shared-batch audit regression verifies
that export includes only the authenticated owner's allowlisted migration metadata,
deletion removes that owner's audit rows, and another owner's rows and the global
batch remain unchanged.
