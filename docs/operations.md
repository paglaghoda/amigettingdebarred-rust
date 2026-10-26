# Operations: admission control, metrics, and deployment

## Status and integration boundary

This change supplies `crates/server/src/ops.rs` and native, database-free tests. It does **not** wire the running executable, change health checks, provision infrastructure, or demonstrate a production deployment. `lib.rs`, `main.rs`, configuration, dependencies, and error types are intentionally unchanged. Until the integration below is made, existing startup does not activate these controls.

Exact public API in `ops`:

```rust
#[derive(Clone)] // deliberately NOT Debug
pub struct OpsConfig {
    pub max_in_flight: usize,
    pub metrics_token: Option<String>,
}
// Default: max_in_flight = 32, metrics_token = None
impl OpsConfig {
    pub fn from_env() -> crate::error::Result<Self>;
    pub fn from_values(max_in_flight: Option<&str>, metrics_token: Option<&str>)
        -> crate::error::Result<Self>;
    pub fn validate(&self) -> crate::error::Result<()>;
}
pub fn protect<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>, config: OpsConfig,
) -> axum::Router<S>;
```

Required follow-up wiring (owned by the main/router integration, not this patch):

1. Export `pub mod ops;` from the library.
2. Expose `pub fn router_with_ops(state: AppState, ops: OpsConfig) -> Router` and have legacy `router(state: AppState) -> Router` call it with `OpsConfig::default()`. Do not make library router construction depend on environment.
3. In normal executable startup, call `OpsConfig::from_env()?` before binding/serving, and pass the explicit result to `router_with_ops`. Neither this module nor startup should load a root `.env` file.
4. Build application/auth routes and their route-specific auth/body-limit middleware first. Call `ops::protect(router, ops)` **once**, then apply the existing request timeout and response/security policy **outside** it. Keep timeout outside so dropping the handler future releases its permit, and response policy outside so busy responses and metrics receive server-generated request IDs and security headers. Metrics must not inherit user/session authentication, CSRF, or CORS middleware. Do not trust incoming `X-Request-ID`; existing response policy generates the ID.
5. Supply `AppState` as before; `protect` captures its own shared `Arc`, not an additional application-state field. Reserve `/internal/metrics`; a duplicate route registration is an Axum configuration error. Protecting clones of one router shares one budget; separate calls to `protect` create separate budgets.
6. Credential-free browser preview may stay unchanged or use `protect(preview_router(), OpsConfig::default())`. It does not need metrics credentials, database access, or normal startup configuration.

Invalid explicitly constructed configurations cause `protect` to panic with a fixed, secret-free message. Validate them first when accepting fallible input. Startup parsing returns existing `AppError` errors without echoing values.

## Configuration and secrets

| Process variable | Absent | Accepted explicit value |
| --- | --- | --- |
| `HTTP_MAX_IN_FLIGHT` | `32` | Decimal digits, integer `1..=1024`; no sign or whitespace |
| `METRICS_BEARER_TOKEN` | Metrics unavailable (`404`) | `32..=256` visible ASCII bytes (`!` through `~`), no spaces/control characters |

An empty token is invalid, not an alias for absent. Non-Unicode environment values are errors. Never derive `Debug` for secret-bearing configuration or log its fields.

Generate a unique random operator token in an approved secret manager (at least 32 random bytes encoded as hex is suitable). Inject it into the server environment and the monitoring system's secret-backed bearer configuration. There is intentionally no working/example key in this guide. Keep it out of Git, images, command-line arguments, query strings, browser storage, shell history, dashboards, and access logs. Use a dedicated metrics credential, not a user cookie, provider key, or database password. Rotation requires replacing the process configuration and restarting/rolling the server; coordinate scraper rollout. Only one token is accepted per instance.

## Admission and failure semantics

- Immediate `try_acquire_owned` admission; no application semaphore queue. The budget is per protected router/process, not a distributed rate limit or per-user quota. Size it against database pool capacity, provider concurrency, memory, and load-test evidence.
- The exact `/health/live` path bypasses admission, including query strings. Readiness, unknown paths, and metrics do not bypass. Configure ingress to route that exact liveness URL to the actual liveness handler, not a slow fallback.
- Saturation returns `503`, `Retry-After: 1`, and the existing JSON error `{ "error": { "code": "unavailable", "message": "server busy, retry shortly" } }`. Clients should back off with jitter; do not blindly replay non-idempotent writes.
- A permit covers the downstream handler until response headers are returned, **not streaming body transmission**. Errors, cancellation, panic unwinding, and outer timeout drops release it through RAII. The in-flight gauge decrements on drop. Bound slow uploads/downloads and active connections at the ingress as well.
- The existing outer timeout produces `408`; since it cancels the inner future, ops records a cancellation, not a fabricated handler `408`. A timeout inside ops would instead count the response status. In-flight work is not completed merely because a scrape occurs.
- Liveness bypass is essential but not an OS/CPU starvation guarantee. Keep its handler cheap and dependency-free.

## Operator metrics

`GET /internal/metrics` (Axum also supports HEAD) is available only with the exact single header `Authorization: Bearer <operator token>`. Missing, malformed, duplicate, or wrong credentials return **403** when configured; an absent configured token returns **404** regardless of credentials. Saturation takes precedence and may return `503` before credential handling. Comparisons use SHA-256 fixed-length digests and `subtle` constant-time equality; malformed syntax/length is rejected before comparison. Authorization is operator-bearer-only: user sessions confer no access. No CORS support is added.

Successful response: `Content-Type: text/plain; version=0.0.4; charset=utf-8`. Metrics-handler success and denial responses use `Cache-Control: no-store`. Neither tokens nor headers nor raw URIs are reflected or logged by this module. Metrics labels never include user/session IDs, query strings, path parameter values, request IDs, IPs, or other PII. Scrape bodies remain operationally sensitive and should not be public.

Metrics reset on process restart:

| Metric | Meaning |
| --- | --- |
| `requests_total{method,route,status_class}` | Completed responses, including busy responses, with `1xx` through `5xx` classes |
| `admission_rejected_total{method,route}` | Failed immediate acquisitions, also counted in `requests_total` as `5xx` |
| `requests_cancelled_total{method,route}` | Handler futures dropped before a response, not included in response-status counters |
| `requests_in_flight` | Current admitted handler futures; excludes liveness bypass |
| `request_duration_seconds_{bucket,count,sum}{method,route}` | Time to headers, rejection, or cancellation; cumulative buckets with additional `le` label |

Bucket upper bounds (seconds): `0.005`, `0.01`, `0.025`, `0.05`, `0.1`, `0.25`, `0.5`, `1`, `5`, `+Inf`. `+Inf` equals count; sum is in seconds. All observed requests, including health/metrics/rejections/cancellations, enter the histogram. A scrape observes its own in-flight gauge but its response counter/duration completes only after its snapshot.

Labels use Axum `MatchedPath` server-defined templates, not raw URLs. Missing templates use `unmatched`; templates over 256 bytes also use `unmatched`. Methods are restricted to GET/HEAD/POST/PUT/PATCH/DELETE/OPTIONS/CONNECT/TRACE or OTHER. At most **128 route/method keys** exist per process: the first 127 observed keys plus shared `(OTHER, overflow)` for additional keys. This hard cap also bounds series under extension-method attacks, arbitrary unmatched URLs, and large server route sets. Status classes and histogram bounds are static. Templates are Prometheus-escaped for quotes, backslashes, and newlines. Stored labels come only from server routing definitions; do not register routes dynamically from user input. Snapshots clone bounded counters under a short standard mutex; no lock is held across an await. Gauges and snapshots are not a transactionally consistent global view.

Monitor rates of rejections/cancellations, `5xx`, readiness failures, sustained in-flight pressure, and latency. Use histogram rates/quantiles, not averages alone. Overflow signals incomplete route attribution, not discarded request accounting. Server-local metrics complement rather than replace ingress connection and end-to-end response metrics.

## Ingress and TLS

- Terminate public HTTPS at a maintained ingress with certificate renewal/expiry alerts. Restrict backend binding/network exposure so clients cannot bypass ingress controls. Encrypt and authenticate remote upstream connections as appropriate to the network trust model; remote PostgreSQL must use verified TLS (`sslmode=verify-full` with trusted CA).
- Deny public access to `/internal/metrics` at ingress; permit only the monitoring network/identity **and** require the application operator bearer. Use TLS for non-loopback scrapes; a bearer sent over plaintext can be stolen. Disable authorization/cookie/query logging and ensure observability tooling redacts headers. Never put credentials in a URL.
- Enforce connection counts, request/header/body sizes, header/read/write/idle timeouts, slow-client protections, and distributed rate limits at ingress. Keep application body limits, the existing timeout, auth limits, and AI concurrency/quota controls. This semaphore does not replace them.
- Restrict trusted-proxy IPs and overwrite forwarded headers at the trusted edge. Incoming request IDs are untrusted; the server-generated value is authoritative.

## Public health and readiness

`/health/live` is public and cheap: `200` means the process can answer HTTP, not that persistence or providers work. Do not include secrets or dependency details. A failing readiness probe should remove the instance from service; liveness should not restart instances simply because dependencies are temporarily down.

**Current code** checks readiness with `SELECT 1`. That proves connectivity only, not schema presence, runtime-role access to the plan store, or successful plan operations. The startup schema probe is also not a substitute for ongoing readiness. The integration owner should change readiness to a bounded, non-mutating query of the actual `app_private.student_plans` schema using the runtime role (for example selecting required columns with `LIMIT 0`) and validate the privileges needed for plan reads/writes without modifying student data. Monitor required write privileges separately or include a non-mutating privilege check; a readable table alone does not prove writes are permitted. Return `200` only when the chosen readiness contract holds and `503` when unavailable, without leaking SQL/schema diagnostics. Admission saturation appropriately yields `503` readiness even with a healthy database. Tune probe thresholds to avoid rolling restart storms.

## Deployment checklist (not evidence of deployment)

- [ ] Complete and review the explicit wiring above, including outer timeout/security policy and plan-store readiness. Run tests and build the pinned release and browser assets; verify lockfile/toolchain consistency.
- [ ] Rehearse against isolated staging with disposable data: auth/CSRF, plan read/write/conflict, role permissions, disabled provider behavior, asset serving, real ingress/TLS, saturated admission, healthy liveness, unavailable readiness, cancelled/time-limited requests, token denial and authenticated scrape.
- [ ] Set per-instance limits and replica/database budgets from measurements. Verify edge protections and request/secret redaction. Confirm metrics are private and alerts/scrapes work without an application user session.
- [ ] Inventory schema migrations and old/new binary compatibility. Use a separately authorized migration role where appropriate; do not expand runtime privileges to mask readiness failures.
- [ ] Take an approved encrypted backup/snapshot before migration, record its timestamp/recovery point and retention, and verify a restore drill. Ensure session encryption and required operational secrets have separately controlled recovery paths.
- [ ] Roll out gradually with a known previous image/config available. Verify readiness through the actual ingress, synthetic disposable-account persistence, bounded errors/latency, metrics, and graceful draining. A green liveness endpoint alone is insufficient.
- [ ] Record actual rollout status, observations, version, and owner. Do not call this deployed or backup-verified based on unit tests.

## Rollback and backup/restore checklist

- [ ] Stop rollout or route traffic away if readiness, error rate, cancellation/rejection rate, or latency regresses. Preserve redacted diagnostic evidence and communicate impact.
- [ ] Roll back image/config only if the current schema is compatible. Do not blindly run down-migrations or restore over live writes. Drain instances and coordinate token/config changes with scrapers.
- [ ] If data recovery is required, explicitly approve the recovery point and expected lost writes (RPO), restore to an **isolated** database first, and check schema/migration history, runtime privileges, representative plan revisions/ownership, and auth/session dependencies.
- [ ] Rehearse restoring encrypted backups with verified tooling and access controls; measure recovery time (RTO). Account for external auth identities and session encryption keys rather than assuming a plan-table backup restores the whole service.
- [ ] Validate health, readiness, authorized reads/writes, isolation, and metrics before cutover. Revoke/rotate compromised credentials and retain audit records. Document actual restore results, remaining risk, and incident follow-up.

## Local verification

From the workspace root, with the configured Rust toolchain:

```sh
cargo test -p debarred-server --test ops
```

This target includes `../src/ops.rs` directly and re-exports the existing application error module, so it runs before library wiring. It uses native `oneshot` requests, no database, no environment mutation, no running server, and no production reads. It covers three blocked handlers, busy retry responses, responsive liveness, readiness admission, cancellation/outer timeout release, error responses, token access, generic state, bounded labels, escaping, and exact cumulative histogram boundaries. Once the library exports ops, the integration owner may switch it to a normal public import (the embedded histogram unit test can then run under library tests).
