# Local performance benchmark (phase 4)

This is a reproducible **local measurement**, not a comparison with the old app,
a production capacity estimate, or a hosted Supabase latency test. There is no
old-app baseline. Provider login/refresh, Google, AI, TLS, WAN latency, browser
hydration, and rendering in a browser are **not measured**.

## Prerequisites and commands

- Installed Rust toolchain and cached workspace dependencies. The script uses
  `scripts/local.py::child_env()` for the existing absolute `.tools/cargo` and
  `.tools/rustup` paths when available. It does not load dotenv files, application
  credentials, proxies, or `DATABASE_URL`.
- Existing built public assets, including
  `crates/web/dist/assets/app_bg.wasm.gz`. If needed, explicitly run the existing
  `sh scripts/build-web.sh` first; the benchmark does not install tools or rebuild
  public assets for you.
- An **explicitly provisioned, disposable native PostgreSQL database** with a
  migration-capable role. Do not point this at a personal application database,
  production, or a hosted service. Native PostgreSQL is not the Supabase stack.

For example, after installing native PostgreSQL, create a separate cluster (only
if the directory does not already contain one):

```sh
mkdir -p .tools/performance
initdb -D .tools/performance/postgres -U benchmark --auth=trust --no-locale --encoding=UTF8
pg_ctl -D .tools/performance/postgres -l .tools/performance/postgres.log -o '-h 127.0.0.1 -p 6544' -w -t 15 start
createdb -h 127.0.0.1 -p 6544 -U benchmark benchmark
BENCH_DATABASE_URL=postgresql://benchmark@127.0.0.1:6544/benchmark python3 scripts/benchmark.py --rss
BENCH_DATABASE_URL=postgresql://benchmark@127.0.0.1:6544/benchmark python3 scripts/benchmark.py --check --rss --output .tools/performance/report.json
pg_ctl -D .tools/performance/postgres -m fast -w -t 15 stop
```

Trust auth is appropriate only for this disposable, loopback-only cluster on a
trusted development machine. The runner does not start, discover, wipe, or stop
databases automatically. `BENCH_DATABASE_URL` is the **only** database input.
Numeric loopback IPv4/IPv6 is required; `localhost`, remote names/addresses,
query parameters (including `host`, `hostaddr`, `options`, and `sslmode`), fragments,
and Unix sockets are rejected. Native port `6543` is also supported when an
operator explicitly supplies a disposable database there. There is no fallback
to `DATABASE_URL` or remote override.

The Python wrapper builds offline/locked in release mode, bounded to 600 seconds,
then executes `target/release/examples/benchmark` directly, bounded to 60 seconds.
Separating the build keeps Cargo/compiler RSS and compilation time out of runtime
measurements. To run the executor manually with a sanitized environment and the
same explicit database, the equivalent Cargo command is:

```sh
cargo run --offline --locked --release -p debarred-server --example benchmark
```

**Do not benchmark a debug build.** The executor rejects `debug_assertions=true`;
the report records `build_mode: release` and `debug_assertions: false`.

## What is exercised

The example creates the minimal test-only `auth.users(id uuid PRIMARY KEY)` stub
if absent, runs **all** `debarred_server::db::MIGRATOR` migrations, and configures a
private-schema PostgreSQL pool with 12 connections and bounded SQL timeouts. The
SQLx migration ledger stays in `public`, including on repeated runs.

Each run inserts a unique UUID and an opaque application session using the real
session format: random 32-byte cookie, SHA-256 of the encoded cookie as row key
and AES-256-GCM AAD, fresh nonce prepended to ciphertext, and encrypted token JSON
with CSRF. Access/refresh tokens are fake and expire one hour later, avoiding any
external refresh call. The `Config` literal sets HTTP `127.0.0.1`,
`local_auth_enabled=false`, no AI, and a deliberately unreachable loopback Auth
endpoint. No credential, cookie, CSRF value, encryption key, or user UUID appears
in reports or captured diagnostics.

An actual Axum router listens on TCP `127.0.0.1:0` with `ConnectInfo<SocketAddr>`.
Reqwest disables proxies and redirects. Authenticated requests send Cookie,
CSRF, exact Origin, and the seeded user's `X-Session-User`. Public requests are
credential-free. HTTP timing includes request construction, serialization where
applicable, network, handler work, and full response-body consumption.

| Scenario | Method / work | Measured concurrency |
| --- | --- | ---: |
| `authenticated_plan_get` | `GET /api/plans/winter-2026` | 8, bounded batches |
| `authenticated_plan_put` | `PUT /api/plans/winter-2026` | 1 |
| `authenticated_projection_post` | `POST /api/projections` | 1 |
| `public_ssr_get` | `GET /` (SSR response, not browser render) | 1 |
| `gzip_wasm_transfer` | `GET /assets/app_bg.wasm`, `Accept-Encoding: gzip` | 1 |
| `engine_build` | Shared domain `Engine::new` | 1 |
| `engine_project_all` | Shared domain `Engine::project_all`, prebuilt engine | 1 |

Each scenario has 10 warmups and 100 measured samples. GET warmups are sequential;
measured GETs use at most eight simultaneous requests. Writes start after one
seeded revision, remain sequential, and verify every returned revision equals
`expected_revision + 1`, including warmups. No intentional conflicts/errors are
part of the benchmark. The existing two-course domain fixture
`crates/domain/tests/fixtures/winter-plan.v1.json` is used; this is a small workload,
not a large timetable stress test.

The executor allows 48 seconds for migrations/workload, reserves six seconds for
cleanup, and has an outer 60-second timeout. Every HTTP request has a five-second
timeout. Normal completion and workload-error paths delete **only the seeded
user**, cascading its sessions/plans; schemas and other users are never wiped.
A killed process, outer timeout, database outage, or machine crash can interrupt
cleanup: inspect the disposable database before reuse. Do not delete unrelated
users to repair a failed benchmark.

## Report API and interpretation

`--output` defaults to `.tools/performance/report.json`; JSON is also printed.
Default execution reports results without enforcing a performance threshold.
`--check` exits nonzero for any recorded error (including warmups), missing sample
counts, non-release mode, or any p95 **>=250 ms**. This is a deliberately generous
local regression budget, not a production SLA. Build/executor failures exit
nonzero even without `--check`; diagnostic output is suppressed to avoid leaking
credentials.

JSON `version: 1` contains:

- `metadata`: OS/architecture/platform, rustc, timestamp, release/debug status,
  timeout settings, explicit provider-auth/AI exclusions, and optional RSS.
- `scenarios[]`: scenario identifier, `unit: ms`, `p50`, `p95`, `max`,
  `sample_count`, `warmup_count`, concurrency, error count, and the sum of measured
  response-body bytes. Percentiles use nearest rank over measured samples;
  failed requests remain latency samples and contribute to errors. Warmups do
  not contribute to percentiles or byte totals.
- `local_budget`: threshold, whether enforced, and whether the report meets it.

With `--rss`, `/bin/ps` samples **only the executor PID**, approximately every
50 ms, reporting bytes converted from ps KiB. It excludes Cargo, PostgreSQL,
Python, and descendants. The sampled peak can miss short spikes; it is not a
kernel high-water mark. Without RSS or where unavailable the value is `null`.
WASM bytes are the compressed HTTP body, not HTTP/TCP overhead; reqwest has no
gzip decoder enabled. A missing gzip content-encoding is an error.

## Actual local observation

Observed **2026-10-26T05:35:40Z**, macOS 27.0.1 arm64, rustc 1.99.1,
optimized release with debug assertions off, native PostgreSQL 17 on loopback
port 6543 (operator-reported). All seven scenarios had **100 samples, 10 warmups, zero errors**;
`--check --rss` passed. These are one-run observations with warm local caches,
not stable guarantees or improvement claims.

| Scenario | p50 ms | p95 ms | max ms |
| --- | ---: | ---: | ---: |
| Authenticated plan GET (8 concurrent) | 0.644750 | 3.930209 | 5.675750 |
| Authenticated plan PUT | 0.336250 | 0.710167 | 0.735208 |
| Authenticated projection POST | 0.104375 | 0.121375 | 0.140500 |
| Public SSR GET | 0.048208 | 0.058542 | 0.063125 |
| Gzip WASM transfer | 0.280292 | 0.316333 | 0.339000 |
| Engine build | 0.006583 | 0.008375 | 0.010792 |
| Engine project all | 0.000292 | 0.000334 | 0.000375 |

Each gzip WASM body was **987,556 bytes** (98,755,600 over 100 measured transfers).
Sampled executor RSS peaked at **12,861,440 bytes** in this run; sampling variation
can be substantial for a short-lived executor. These sizes describe this build,
not a fixed asset-size or memory guarantee. The benchmark report does not record
post-run catalog cleanup counts; no independent cleanup check is claimed here. `.tools/performance/report.json` holds the machine-readable observation
and is overwritten by later runs. Repeat on the same machine/build before
interpreting small differences. No old-app baseline or hosted performance result
has been collected.

## Separate synthetic backup/restore observation

Backup recovery is not an HTTP benchmark scenario. `scripts/backup-drill.py`
performs a real native `pg_dump` custom-format / `pg_restore` round trip between
**two newly created randomized databases**, never against an existing user's data.
See `docs/release.md` for isolation, prerequisites, ACL/RLS/guard checks, report
schema, cleanup bounds, and the remaining production-recovery limitations.

Build the latest release server explicitly, then supply an already-existing
numeric-loopback maintenance database. The script does not build/start PostgreSQL
or read dotenv/production configuration:

```sh
python3 -m unittest discover -s scripts -p test_backup_drill.py -v
DRILL_ADMIN_DATABASE_URL=postgresql://phase2_test@127.0.0.1:6543/postgres python3 scripts/backup-drill.py
```

Observed **2026-10-26T05:35:40Z**, native PostgreSQL 17 on port 6543, using the
freshly rebuilt release server's embedded migrations:

| Measurement | Observed |
| --- | ---: |
| Custom-format dump | 0.043763 s |
| Restore into empty fresh database | 0.038783 s |
| Entire drill including setup, fingerprints, probes, and cleanup | 1.141081 s |
| Synthetic archive | 22,466 bytes, mode 0600 |
| Compared data | 9 tables, 12 rows |

All source/restored data fingerprints and security fingerprints matched. Source
and restored pending-identity guards rejected the synthetic second user's INSERT
with the expected 42501 error, and active-user checks succeeded. Probe fingerprints
remained unchanged. The report records successful cleanup, two databases created,
and **zero databases remaining**; no independent catalog check was performed for
this run. No existing database was seeded or dumped.

Final validation reported by the operator passed: **42** pure Python tests,
**17** browser scenarios, workspace native PostgreSQL tests with `--include-ignored`,
format checks, native and hydrate Clippy, and release builds. These validation
results are separate from the two measurement reports; browser auth mocks and
native PostgreSQL do not verify Docker or real Supabase.

The JSON result is `.tools/release/backup-drill.json` (0600). Per-run custom
archives remain at `.tools/release/backup_<uuid>.dump` (0600); report fields contain
only counts/hashes and nonsecret check/timing metadata, not row contents or keys.
The synthetic Auth/OAuth blobs test **byte integrity only**, not decryption or
usable login sessions. `key_decryption_tested`, `local_supabase_tested`, and
`production_backup_tested` are explicitly false. These small, warm-local synthetic
timings are not production restore estimates, PITR/WAL tests, provider validation,
or evidence that production backups are complete. No release attestation was
created or marked passed, and no baseline/improvement claim is made.
