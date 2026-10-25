# AmIGettingDebarred — Rust rebuild

Phase 1: a deterministic, validated attendance engine shared by the future Rust
server and Rust/WASM frontend. The existing app in `amigettingdebarred-main/` is
unchanged and remains the design/behaviour reference.

**This is not yet a replacement web app.** Authentication, persistence, HTTP
endpoints, and the new UI belong to later phases. This crate does not depend on
Clerk, Supabase, Lovable, React, a system clock, or any network service.

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

docs/phase-1.md                       Rules, deliberate changes, Phase 2 handoff
```

## Run

Requires stable Rust and its `rustfmt`/`clippy` components. Node is only needed to
execute the development WASM smoke test; it is not used by the engine itself.
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
and property tests run natively. This is not a frontend or a public JS API; Leptos
will consume the library directly in Phase 3.

GitHub Actions repeats formatting, Clippy, native tests, WASM compilation, and
the WASM runtime smoke test. `Cargo.lock` is checked in for reproducible builds.

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
