# Phase 2 — Rust backend

## What is built

A single Axum/Tokio process provides Supabase Google authentication, opaque
server-side sessions, per-semester saved plans, domain projections, deterministic
helper answers, optional AI intent extraction/image import, and daily quotas.
SQLx connects directly to Supabase Postgres. There are no Clerk/Lovable services,
JavaScript server functions, Redis, background job platform, or Node runtime in
the replacement backend.

The old app remains untouched as a reference. Legacy Clerk accounts/plans have
**not** been migrated. Phase 3 now adds a server-rendered Leptos frontend, including
`/setup` and `/planner`, plus an isolated device-only `/demo`. For local-only
workflows that do not read production credentials, use
[Local development](local-development.md) instead of the hosted setup below.

## Setup

### 1. Supabase and Google

Use a Supabase project you control. Obtain its project URL, publishable/anon key,
and Postgres connection credentials. The Rust server does not need or expose a
Supabase service-role API key.

Enable Google under Supabase Authentication providers and configure its Google
OAuth client ID/secret in Supabase, not in this repository. In Google Cloud, the
provider callback is Supabase's `https://YOUR_PROJECT.supabase.co/auth/v1/callback`.
In Supabase's **redirect allowlist**, add the application's exact callback:

```text
http://localhost:3000/auth/callback
https://YOUR_APP_HOST/auth/callback
```

Set the production Supabase site URL to the real application origin. Redirects
are server-selected; there is no user-controlled `next`/redirect URL.

Use a direct database or **session pooler** connection. The current SQLx prepared
statements/session usage is not configured for transaction-pooler compatibility.
Keep application and database in nearby regions.

### 2. Environment

Copy `.env.example` to the ignored `.env` and replace every required placeholder.
Generate the session encryption key independently for each deployment:

```sh
openssl rand -base64 32
```

`SESSION_ENCRYPTION_KEY` is standard base64 for exactly 32 random bytes. All-zero
keys are rejected. Keep it in a secrets manager in production. Changing this key
invalidates existing encrypted sessions/flows; there is no key-rotation keyring.

`.env` is **not loaded automatically**. For the shell-compatible values in the
example, export it before running Cargo:

```sh
set -a
. ./.env
set +a
```

Percent-encode reserved characters in the database password. If adding additional
URL query parameters containing `&`, quote that assignment when sourcing it as a
shell file. Docker `--env-file` expects literal unquoted values instead.

Required variables:

| Variable | Purpose |
|---|---|
| `PUBLIC_ORIGIN` | Exact browser origin, without a path/query; HTTPS except loopback development |
| `SUPABASE_URL` | Project origin, not an auth endpoint |
| `SUPABASE_ANON_KEY` | Publishable/anon project key for Supabase Auth calls |
| `DATABASE_URL` | Postgres connection; remote hosts require `sslmode=verify-full` |
| `SESSION_ENCRYPTION_KEY` | Random 32-byte base64 AES key |

HTTP provider URLs reject credentials in URLs and insecure remote HTTP. Local
HTTP exceptions are limited to loopback development. Remote Postgres requires
certificate **and hostname** verification, not `sslmode=prefer` or merely
`require`. Supply a trusted `sslrootcert` if your database requires a project CA;
do not solve certificate failures by disabling verification. Unix sockets and
loopback databases may use plaintext for local development. TLS checks examine
the driver's parsed connection options, including host overrides.

Other limits have validated bounds; defaults are in `.env.example`. Choose
`DATABASE_MAX_CONNECTIONS` within the database's connection budget, multiplied
by the number of application replicas.

### 3. Migrate and run

From the workspace root with a Rust toolchain active:

```sh
cargo run -p debarred-server --locked -- --migrate
cargo run -p debarred-server --locked
```

`--migrate` applies embedded SQLx migrations and exits. Normal startup does not
mutate schemas and fails if storage has not been initialized. Run migrations
with an appropriate administrative deployment credential; the normal runtime
can use a narrower backend-only database role as described below.

There is no production bootstrap of `auth.users`: Supabase already owns that
table. The new migration creates only `app_private` tables. It does not modify
legacy `student_plans`, `clerk_student_plans`, legacy quotas, or auth accounts.

The default listener is `127.0.0.1:3000`. Set `BIND_ADDRESS=0.0.0.0:3000` inside a
container. `/health/live` checks the process, and `/health/ready` checks the DB.
Ctrl-C/SIGTERM initiates graceful shutdown. A minute-based maintenance task
removes expired sessions/flows and quota records older than 90 days.

## Authentication and browser contract

1. Navigate to `GET /auth/login`.
2. The server generates a random PKCE verifier and an opaque browser flow cookie.
   The DB stores a SHA-256 cookie hash and an AES-256-GCM encrypted verifier for
   ten minutes. The cookie is HttpOnly, host-only, and SameSite=Lax.
3. The browser visits Supabase Google authorization. Supabase returns a one-time
   code to `/auth/callback`; only the browser with the matching flow cookie can
   exchange it. The flow is consumed atomically, including failed callbacks.
4. The server exchanges the code with Supabase and creates a fresh random
   256-bit session cookie. Access/refresh/CSRF credentials stay encrypted in the
   private database; no Supabase bearer token is sent to browser JavaScript.
5. `GET /api/me` returns the cached user ID/profile and the session CSRF token.
   The future UI should keep that token in memory and send `X-CSRF-Token` on
   mutations, along with the cookie and the exact `Origin`.
6. `POST /auth/logout` removes the local session and clears its cookie even if
   Supabase is unavailable. This is local-app logout, not global Supabase logout
   or revocation of sessions in other browsers/applications.

Production cookies use `__Host-` names, `Secure`, HttpOnly, Path=/, no Domain.
Development uses `session` / `oauth-flow` without Secure only on the configured
HTTP loopback origin. Sessions have an absolute lifetime (30 days by default);
refresh does not extend it.

Fresh authentication makes one indexed local session read, not an external user
profile lookup. Within 60 seconds of access-token expiry, a refresh revalidates
with Supabase. Per-session in-process locks coalesce refresh waiters **before**
DB checkout; global refresh admission is bounded to a quarter of the configured
pool (minimum one). A transactional `FOR UPDATE` re-read serializes rotation
across replicas, with a two-second lock wait bound. A single slow refresh cannot
fill a normal-sized pool with same-session lock waiters. Deploy with more than
one connection if unrelated users must remain responsive during refresh.
Logout uses the same per-session guard and bounded admission before its delete
checks out a connection, and its cross-replica lock waits are also bounded. Retry
logout if admission or database availability returns 429/503; a failed logout
must not be presented as a completed revocation. Expired-session rejection does
not queue a blocking delete; expiration maintenance removes those rows.

Invalid refresh credentials or a mismatched replacement user revoke the local
session. Provider outages fail closed without discarding valid refresh state.
Upstream changes such as Supabase bans/sign-outs are observed on refresh, not by
an external lookup on every request. Supabase account deletion cascades local
records immediately through foreign keys.

All application API/auth responses are no-store and include defensive headers.
No permissive CORS policy is installed: this is a same-origin cookie API. Use a
same-origin dev proxy rather than enabling arbitrary credentialed origins.
HTML now uses a separate same-origin CSP with the narrowly scoped
`'wasm-unsafe-eval'` token required for Wasm compilation, but no JavaScript
`'unsafe-eval'` or `'unsafe-inline'`. JSON APIs retain the restrictive policy.

### Reverse proxies and rate limits

Auth login/callback share a bounded per-IP budget (20 requests/minute by default),
including **HEAD** requests because Axum executes GET handlers for HEAD. Both
steps count; adjust `AUTH_REQUESTS_PER_MINUTE` to your workload.

By default, only the actual TCP peer IP is used. Behind a reverse proxy, configure
`TRUSTED_PROXY_IPS` with that proxy's exact IP addresses; otherwise all its clients
would share one budget. Never add arbitrary public client ranges. Only trusted
TCP peers may supply one valid `X-Forwarded-For` header. Resolve right-to-left,
removing trusted hops and using the nearest untrusted address; spoofable entries
to its left cannot reset limits. Missing/malformed forwarding from a configured
proxy is rejected rather than silently sharing a proxy-wide budget.

Forwarded headers from all other peers are ignored. IP lists do not support
CIDRs or automatic discovery. Restrict direct ingress to the proxy and make sure
the proxy appends/sets the real client address. Multiple replicas additionally
need an edge-level distributed rate limit, body-size limit, and connection
limits. The in-process limiter is not a substitute for DDoS protection.

Request logs use generated request IDs, method, matched route, status, and elapsed
time, never raw callback URIs or auth headers. Configure your ingress/access logs
to redact OAuth query codes and cookies too. Do not use verbose network logging
with production credentials.

## API

Every route below except health, calendar reads, login, and callback requires a
valid cookie. All authenticated mutations additionally require exact Origin and
`X-CSRF-Token`.

| Method | Path | Contract |
|---|---|---|
| GET | `/health/live` | Process health |
| GET | `/health/ready` | Database connectivity |
| GET | `/api/semesters` | Supported ID/label/revision list |
| GET | `/api/semesters/{id}` | Config and expanded calendar days |
| GET | `/api/me` | User profile and browser CSRF token; no bearer tokens |
| GET | `/api/plans/{id}` | Saved `{plan, revision, updated_at}`; 404 if absent |
| PUT | `/api/plans/{id}` | `{expected_revision, plan}` → saved version |
| POST | `/api/projections` | `{plan}` → `as_of`, projections, allowances, leave summary; never saves |
| GET | `/api/quota` | Current IST date and separate helper/image quotas |
| POST | `/api/helper` | `{plan, intent}` or `{plan, question}`; not both |
| POST | `/api/timetable/import` | `{image_data_url}` → validated timetable draft and quota; never saves |
| GET | `/auth/login` | Start Google PKCE flow |
| GET | `/auth/callback` | Consume browser-bound code flow |
| POST | `/auth/logout` | Delete local session, clear cookie |

Plan data uses the Phase 1 contract; see
`crates/domain/tests/fixtures/winter-plan.v1.json`. To create, wrap it in:

```json
{ "expected_revision": 0, "plan": { "schema_version": 1, "...": "see the complete fixture" } }
```

This snippet is illustrative, not a complete valid plan. Use the complete
fixture or a domain-produced plan. The route semester must match `plan.semester_id`.
A user ID is never accepted in the save payload: ownership comes from the
verified server session. Only supported calendar IDs/revisions are accepted.
Counts and their dated snapshot are validated and written together.

### Safe saving and errors

Revision 0 means **create only**, returning revision 1. Subsequent writes supply
the last loaded positive revision and atomically increment it. Concurrent writers
cannot both succeed. A stale/retried write returns **409** and never overwrites a
newer plan. The client must reload, merge/review its local changes, and retry with
the new revision. Debounce/batch saves in Phase 3; the backend does not silently
combine conflicting documents or guarantee retry success through idempotency keys.

A missing row is 404, not a successful empty plan. Database failures/corrupt stored
plans are 503, not 404/null. Never treat read failures as permission to overwrite.
Other statuses: 400 semantic validation, 401 invalid/expired login, 403 CSRF/origin,
413 payload too large, 422 malformed/schema-invalid JSON, 429 quota/admission/rate
limit, 503 unavailable provider. Most application errors are
`{"error":{"code":"...","message":"..."}}`; Axum extractor errors and timeouts
may use its default response format. Database/provider details and credentials
are not reflected in application error bodies.

## Helper and optional AI

Structured helper intents are free and deterministic:

```json
{
  "plan": { "schema_version": 1, "...": "use a complete valid plan" },
  "intent": {
    "action": "max_skips",
    "course_codes": ["CSE1001"],
    "checkpoint": "cat1",
    "dates": []
  }
}
```

Actions are `status`, `max_skips`, and `skip_dates`. Empty courses select all;
null/omitted checkpoint selects all. `skip_dates` requires unique YYYY-MM-DD
dates after the snapshot with actual selected-course meetings. Other actions
must have no dates. The skip scenario preserves existing leave, affects only
selected courses, and is not saved. Exact questions `status` and `max skips`
also use the free path.

Other natural-language questions (at most 1,000 bytes) require `AI_API_KEY` and
`AI_MODEL`. Configure an OpenAI-compatible provider supporting JSON-object output;
image import additionally requires a vision-capable model. The endpoint base URL
is explicit and defaults to OpenAI, but no paid provider is enabled by default.
No Lovable gateway is used.

Natural questions use **one** provider call to extract a strict intent, supplying
only the question, course codes, calendar, snapshot date, and injected current
IST date for relative-date interpretation. Attendance counts and course names
are not needed by that call. The Rust engine computes every figure and generates
the final answer; there is no second AI explanation call or client-provided facts.
Images and questions are shared with the configured provider: disclose that in
the frontend and use an appropriate provider retention policy.

Limits:

- Ordinary JSON bodies: 128 KiB; image-import JSON: 3 MiB.
- Inline base64 PNG/JPEG/WebP only, matching magic bytes, decoded maximum 2 MiB.
  Images are not stored locally or automatically applied to a plan.
- Provider responses: at most 64 KiB, 2,048 output tokens, 20-second operation
  timeout; redirects disabled, no automatic retries.
- Shared immediate-rejection concurrency semaphore (default 4 per process),
  rather than an unbounded waiting queue.
- Separate helper/image budgets (default 3 each), atomically consumed per user,
  category, and **Asia/Kolkata calendar day**. There is no fail-open quota read.
- Requests that reach the provider consume quota even on failure/invalid output.
  Invalid input, disabled providers, deterministic intents, and concurrency
  rejection do not consume provider quota.

Image import returns a structurally/semantically validated **draft**, not a claim
of OCR accuracy. The UI must require review before saving.

## Database authorization

Tables live in unexposed `app_private`, with RLS enabled and all schema/table
privileges revoked from PUBLIC and existing Supabase `anon`/`authenticated` roles.
Plans, usage, and sessions reference `auth.users(id)` with cascading deletion.
Provider credentials are authenticated-encrypted with fresh nonces and bound to
the hashed session/flow identity, preventing ciphertext swapping across rows.

**RLS does not establish end-user ownership for a privileged server connection.**
Every plan query explicitly predicates on the authenticated user ID and semester;
quota queries predicate on user/day/category. These are tested authorization
boundaries. Do not expose the backend's Postgres credential or this schema through
a browser/Data API.

For a least-privilege runtime role, create a dedicated login role through your
secure deployment tooling (do not store its password in SQL/source). After applying
migrations as an administrator, grant that backend-only role schema usage and
explicit table/column privileges and backend-role RLS policies. The example below
covers migrations `0001_private_storage.sql`, `0002_account_lifecycle.sql`, and
`0003_migration_audit.sql`; `debarred_backend` denotes your separately provisioned,
trusted server-only role, not a browser identity. These are provisioning examples,
not grants made automatically by migrations:

```sql
GRANT USAGE ON SCHEMA app_private TO debarred_backend;
GRANT SELECT, INSERT, UPDATE, DELETE ON
  app_private.student_plans, app_private.auth_sessions, app_private.ai_usage
  TO debarred_backend;
GRANT SELECT, INSERT, DELETE ON app_private.oauth_flows TO debarred_backend;
GRANT SELECT, INSERT ON app_private.account_deletion_requests TO debarred_backend;
GRANT EXECUTE ON FUNCTION app_private.lock_account_write(uuid) TO debarred_backend;
GRANT SELECT, DELETE ON app_private.migration_rows TO debarred_backend;
GRANT SELECT (batch_id, applied_at, status)
  ON app_private.migration_batches TO debarred_backend;
CREATE POLICY backend_access ON app_private.student_plans
  TO debarred_backend USING (true) WITH CHECK (true);
CREATE POLICY backend_access ON app_private.auth_sessions
  TO debarred_backend USING (true) WITH CHECK (true);
CREATE POLICY backend_access ON app_private.oauth_flows
  TO debarred_backend USING (true) WITH CHECK (true);
CREATE POLICY backend_access ON app_private.ai_usage
  TO debarred_backend USING (true) WITH CHECK (true);
CREATE POLICY backend_select ON app_private.account_deletion_requests
  FOR SELECT TO debarred_backend USING (true);
CREATE POLICY backend_insert ON app_private.account_deletion_requests
  FOR INSERT TO debarred_backend WITH CHECK (true);
CREATE POLICY backend_select ON app_private.migration_rows
  FOR SELECT TO debarred_backend USING (true);
CREATE POLICY backend_delete ON app_private.migration_rows
  FOR DELETE TO debarred_backend USING (true);
CREATE POLICY backend_select ON app_private.migration_batches
  FOR SELECT TO debarred_backend USING (true);
```

Never grant this role, schema/table privileges, or helper execution to `anon` or
`authenticated`; PUBLIC must retain no access. These policies permit trusted
server operations whose handlers explicitly scope personal data by authenticated
UUID; they are not end-user policies. Grants alone are insufficient under RLS:
missing SELECT/DELETE policies can silently hide export rows or erase zero audit
rows. Verify export visibility and erasure under the actual runtime role.

`app_private.lock_account_write(uuid)` is **SECURITY DEFINER**, with fixed
`search_path = pg_catalog, app_private`, qualified builtins and queue reference.
Its trusted migration-role owner must remain able to see all pending queue rows;
otherwise a hidden marker could fail open. Do not grant runtime/API roles CREATE
in the trusted schemas, ownership of the helper/queue, BYPASSRLS, or direct EXECUTE
on `app_private.guard_account_write()`. Do not change owner/RLS behavior (including
FORCE ROW LEVEL SECURITY) without preserving queue visibility. Writers call the
helper before row DML in READ COMMITTED transactions; triggers alone do not
establish safe lock order. See [Account lifecycle](account-lifecycle.md).

Queue access is INSERT/SELECT only: runtime must not UPDATE/DELETE pending markers
to reopen login. `migration_rows` needs SELECT+DELETE for owned export/erasure,
**not INSERT/UPDATE**. `migration_batches` SELECT is limited to `batch_id`,
`applied_at`, and `status`, never whole-table SELECT or batch writes. Global batch
counts, source/mapping hashes, and fingerprints are not needed by the API.
Import/rollback privileges belong to a separate authorized operator, not the app
runtime; see [Migration](migration.md). Do not use blanket ALL TABLES or default
runtime grants for future tables. If upgrading an existing role with broader or
inherited grants, revoke those first through reviewed provisioning: these narrower
GRANT statements do not remove prior privileges.

## Tests and validation boundaries

Default fast suite:

```sh
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Database tests are deliberately `#[ignore]` so a missing test database cannot
silently be reported as exercised. Run them explicitly against **a disposable
PostgreSQL database only**, never a Supabase project containing real users:

```sh
export TEST_DATABASE_URL=postgresql://postgres:postgres@127.0.0.1:5432/debarred_test
cargo test -p debarred-server --locked -- --ignored --test-threads=1
```

Tests create a minimal `auth.users` stand-in in that disposable DB. This is test
bootstrap only, not a Supabase migration. Optionally create `anon` and
`authenticated` roles beforehand to exercise explicit privilege revocation too.
SQLx migrations plus actual Postgres constraints/row locking/upserts are exercised,
not replaced by an in-memory repository. Single-threaded test scheduling limits
fixture connection usage; the race tests themselves spawn concurrent operations.

Auth/provider tests use local HTTP mocks; they do not call Google, Supabase, or
paid AI APIs. Coverage includes PKCE/replay, refresh rotation/admission/lock waits,
wrong-user and expired/tampered sessions, CSRF, cross-user isolation, concurrent
saves/quotas, failure-vs-empty reads, real router body limits, HEAD/proxy rate-limit
handling, and deterministic projection parity. GitHub Actions runs these tests
with a PostgreSQL 17 service and retains the native/WASM domain checks.

The implementation was tested with disposable local PostgreSQL, not your real
Supabase project. Live Google login, project settings, production certificates,
load testing, and Docker execution still need deployment-specific verification.
No production latency/memory improvement is claimed without benchmarks.

## Container

The multi-stage `Dockerfile` produces a non-root Debian-based runtime with the
release Rust binary and CA certificates—no Node, Rust toolchain, or old app.

```sh
docker build -t debarred-api .
docker run --rm --env-file .env debarred-api --migrate
docker run --rm --env-file .env -e BIND_ADDRESS=0.0.0.0:3000 -p 3000:3000 debarred-api
```

Use secret injection and an HTTPS reverse proxy in production. Configure trusted
proxy IPs deliberately; the server does not infer a public origin from forwarded
headers. If using a custom DB CA path, mount its certificate read-only where the
unprivileged runtime can read it. `.dockerignore` excludes the old app, local
build/toolchain directories, Git data, and all environment files.

## Phases 3–4 and remaining release work

Phase 3 supplies the Leptos frontend, local shared-engine recalculation, debounced
saves, and explicit conflict handling. Phase 4 adds account export/deletion and
privacy UI, offline migration/rollback, operations controls, local benchmarks,
and release tooling. Current router/startup wires account routes and operations;
readiness calls the storage schema/privilege probe rather than only `SELECT 1`.
See [Account lifecycle](account-lifecycle.md), [Migration](migration.md),
[Operations](operations.md), [Release](release.md), and [Performance](performance.md).
The operations guide's original module integration notes are not deployment evidence.

Launch remains blocked pending official calendar review, live Google and real
Supabase/local-stack validation, reviewed identity mapping/cutover, production
backup/restore and rollback evidence, policy/contact approval, and staged rollout.
Native synthetic tests/benchmarks/drills do not satisfy those manual attestations.
No legacy identity/plan migration has been performed; nothing automatically links
accounts by email, deletes legacy data, or retires old infrastructure.
