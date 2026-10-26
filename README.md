# AmIGettingDebarred — Rust rebuild

Phases 1–4: a deterministic shared attendance engine, Rust HTTP backend,
server-rendered Leptos frontend with browser Wasm hydration, and account lifecycle,
offline migration, operations, performance, and release tooling. Supabase handles
authentication; Postgres stores versioned plans; optional AI only extracts intent
or timetable drafts. The existing app in `amigettingdebarred-main/` is unchanged and
remains the design/behaviour reference; it was not included in the Phase 1 Git
commit.

The replacement uses no Clerk, Lovable gateway, React, or Node production runtime.
The domain crate remains pure and network-free; the same Rust calculations run
on the server and in the browser. Existing production credentials/data are not
needed for local development.

## Phase status and guides

- **Phase 1 — domain engine:** deterministic attendance calculations, versioned
  calendar/plan contracts, and native/WASM parity. [Rules and handoff](docs/phase-1.md).
- **Phase 2 — backend:** Supabase sessions, private Postgres persistence, projections,
  quotas, and optional AI. [Backend guide](docs/phase-2.md).
- **Phase 3 — frontend:** Leptos SSR/hydration, device drafts, conflict-aware cloud
  saves, and a credential-free sandbox. [Local development](docs/local-development.md).
- **Phase 4 — launch preparation:** account export/deletion and privacy UI,
  operator-reviewed offline import/rollback, admission control/private metrics,
  local benchmarks, and an offline release gate. See
  [Account lifecycle](docs/account-lifecycle.md), [Migration](docs/migration.md),
  [Operations](docs/operations.md), [Release](docs/release.md), and
  [Performance](docs/performance.md).

**Launch remains blocked, not production-verified.** Local synthetic tests,
benchmarks, and backup/restore drills do not verify the official calendar, live
Google login, a real Supabase Auth journey/local stack, production backups, or a
production deployment. Manual evidence and operator review are still required;
no legacy identity/plan migration has been performed. Phase 4 tooling is not
release approval. The operations guide retains module-level integration notes;
the current executable wires account routes and operations controls, and readiness
uses the storage schema/privilege probe.

## Try the app locally

Frontend assets have been built in this checkout. To rebuild and start the
credential-free, device-only preview:

```sh
sh scripts/build-web.sh
python3 scripts/local.py preview
```

Open **http://localhost:3000** and choose **Try local sandbox**. It is clearly
labelled: drafts stay on this device, with no authenticated cloud saves. Ctrl-C
stops the server. No production `.env` is read by these commands.

For actual local Supabase Auth/Postgres, first install/start Docker, then:

```sh
python3 scripts/local.py start
python3 scripts/local.py seed
python3 scripts/local.py run
```

The local password form uses the disposable account documented in
**[Local development](docs/local-development.md)**. Supabase CLI and wasm-bindgen
are installed under ignored `.tools/` on this checkout; Docker is still missing,
so the real local Supabase stack has not been exercised here. Database/backend
tests use disposable native Postgres and mock auth providers instead.

For backend setup, environment variables, API contracts, database privileges,
and security/deployment notes, see **[Phase 2 guide](docs/phase-2.md)**.

## Layout

```text
Cargo.toml                           Cargo workspace
crates/domain/
  src/calendar.rs                    Calendar expansion and validation
  src/model.rs                       Versioned plans and typed inputs
  src/engine.rs                      Projections, leave, skip allowances, counts
  src/time.rs                        Explicit UTC → Asia/Kolkata dates
  src/slots.rs                       Original seven-slot FFCS metadata
  data/winter-2026.v1.json            Versioned calendar from the existing app
  tests/                             Regression, property, fixture, WASM checks
  examples/project.rs                Runnable projection example
  examples/wasm_smoke.rs              Test-only WASM entry point
  tests/fixtures/winter-plan.v1.json  Example input contract
  tests/wasm-smoke.cjs                Development-only WASM runtime harness

crates/server/
  src/                               Axum API, Supabase sessions, SQLx, helper
  migrations/                        Private tables and access restrictions
  tests/                             Real Postgres, OAuth, and HTTP tests
crates/migrate/                      Offline legacy validation/import/rollback CLI

.env.example                         Required/optional server configuration
crates/web/
  src/                               Leptos views, shared-engine UI state, browser I/O
  public/                            Self-hosted CSS, images, fonts, bootstrap
  dist/                              Generated WASM/JS and precompressed assets (ignored)
supabase/config.toml                 Local-only Supabase project configuration
scripts/local.py                     Isolated local stack / preview workflow
scripts/build-web.sh                 Browser WASM and assets build
scripts/browser-tests.cjs            Chrome/Playwright regression suite
Dockerfile                           Non-root server + frontend image

docs/phase-1.md                       Domain rules and deliberate changes
docs/phase-2.md                       Backend setup, API, auth, persistence
docs/local-development.md             Local workflows, safety, and test boundaries
docs/account-lifecycle.md             Account export, erasure, pending identity work
docs/migration.md                     Offline migration and conservative rollback
docs/operations.md                    Admission, private metrics, deployment checklists
docs/release.md                       Offline release gate and backup drill boundaries
docs/performance.md                   Local measurements, not production guarantees
```

## Run

Requires stable Rust and its `rustfmt`/`clippy` components. Node is only needed to
execute development WASM smoke/browser tests; it is not a production dependency.
Run all commands from this workspace root, **not** from the old app directory.

```sh
cargo test --workspace --locked
cargo run -p debarred-domain --example project --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The example uses a saved attendance snapshot through November 20, 2026 and a
single class off on a working Saturday. Its CAT-1 results are:

- `CSE1001`: 18 / 19 classes, **94.7%**, three additional classes may be skipped.
- `MAT1001`: 15 / 15 classes, **100%**.

### Isolated toolchain on this checkout

During implementation, Rust was installed under ignored `.tools/` because no
global toolchain was available. To use that installation in a terminal session:

```sh
. .tools/cargo/env
export CARGO_HOME=.tools/cargo
export RUSTUP_HOME=.tools/rustup
cargo test --workspace --locked
```

Keep that session's working directory at the workspace root while using these
relative environment paths. Other clones can use a normal Rust installation.
No global shell profile was changed.

## Verify native and WASM portability

```sh
rustup target add wasm32-unknown-unknown
cargo build -p debarred-domain --lib --example wasm_smoke --release --target wasm32-unknown-unknown --locked
node crates/domain/tests/wasm-smoke.cjs
```

The Node harness executes the **compiled Rust WASM**, checking fixture results,
leave deduplication, serialization, and IST midnight behaviour. Full exhaustive
and property tests run natively. This harness is not a frontend or a public JS API;
the Phase 3 Leptos frontend consumes the domain library directly.

GitHub Actions repeats formatting, Clippy, native tests, real PostgreSQL tests,
release server compilation, WASM compilation, and the WASM runtime smoke test.
`Cargo.lock` is checked in for reproducible builds. Database tests are explicitly
ignored by the default test command; the Phase 2 guide explains how to run them.

## Domain API

```rust
use debarred_domain::{load_semester, Engine, Plan};

fn project(saved_json: &str) -> debarred_domain::Result<()> {
    let plan = Plan::from_json(saved_json)?;
    let calendar = load_semester(&plan.semester_id)?;
    let engine = Engine::new(&calendar, &plan)?; // semantic validation boundary
    let result = engine.project_course("CSE1001", "cat1")?;
    let allowance = engine.skip_allowance("CSE1001", "cat1")?;
    println!("{}% — {} more skips", result.percentage, allowance.additional);
    Ok(())
}
```

`Plan::from_json` checks JSON structure; `Engine::new` checks semantic validity.
Do not treat deserialization alone as validation. Reuse an immutable engine for
multiple queries; rebuild it when a plan changes.

## Important rules

- Attendance is an explicitly dated **completed-day snapshot**, not an implicit
  interpretation of the host machine's current date.
- Every subsequent scheduled meeting is presumed attended unless selected as
  leave. Forecasts must display their snapshot date and assumptions.
- Future leave never changes the snapshot. Past leave is not subtracted twice.
- Checkpoint start dates are excluded. Historical checkpoint results cannot be
  reconstructed from aggregate counts and are marked unavailable.
- Threshold comparisons use exact integer arithmetic, not rounded percentages.
- A forecast is an estimate based on the configured calendar, not an official
  university eligibility decision.

See [Phase 1 rules and handoff](docs/phase-1.md) before integrating auth/storage.
