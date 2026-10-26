# Local development

The public UI is Rust Leptos, server-rendered with Wasm hydration. There is no Node production runtime or npm build step. Run all commands below from the workspace root; scripts resolve their workspace paths independently of the current directory.

## Prerequisites

- Python 3.9+.
- Rust/Cargo and the `wasm32-unknown-unknown` target.
- `wasm-bindgen-cli` **at exactly the wasm-bindgen version in Cargo.lock**.
- For real authentication and persistence: Docker with a running local daemon, and a current stable Supabase CLI supporting `status --output json`. The CLI may be on PATH or at `.tools/bin/supabase` (executable).
- A browser, such as Google Chrome.

The repository-local wasm-bindgen CLI is installed at version `0.2.129`, and the web build has succeeded. Supabase CLI `2.122.0` (the latest stable release at this check) is installed only at `.tools/bin/supabase`, from the official GitHub Darwin ARM64 release archive after verifying its SHA-256 against the release API digest. Its `--version` check passed. A status probe with an explicit isolated `--workdir` reached the expected Docker-unavailable failure without a config parse error. Docker remains a user-installed prerequisite: no Docker daemon or authenticated stack was started, and live auth/database flows remain unverified. The config uses the documented current CLI tables (`inbucket` is the CLI table for the mail UI); old CLI versions may need upgrading.

If `.tools/cargo` and `.tools/rustup` exist, each script sets absolute `CARGO_HOME`/`RUSTUP_HOME` and prepends `.tools/cargo/bin` to the child PATH. Otherwise it uses the system toolchain. No script installs tools automatically. With the repository-local toolchain, run explicit installation commands in a shell configured for that toolchain, for example:

```sh
export CARGO_HOME="$PWD/.tools/cargo"
export RUSTUP_HOME="$PWD/.tools/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
rustup target add wasm32-unknown-unknown
# Currently locked version; recheck Cargo.lock if dependencies change.
cargo install wasm-bindgen-cli --version 0.2.129 --locked
```

Use your normal system toolchain instead if no repository-local toolchain is installed. Installation and first-time Cargo builds may download dependencies; the first Supabase start downloads Docker images. Install Docker and Supabase CLI explicitly using their official installation instructions. No hosted Supabase account, access token, project link, service-role key, or production environment file is needed.

## Check and build

```sh
python3 scripts/local.py check
sh scripts/build-web.sh
```

`check` reports missing executables and, when available, probes Docker without starting anything. Missing tools yield a nonzero exit code. It does not install tools, print environment values, or prove the wasm target is installed.

The build also creates deterministic gzip siblings for WASM, JS, CSS, fonts and SVG when smaller. Axum serves those precompressed files to supported browsers, avoiding runtime compression work. Photos are not recompressed. WASM transfer size varies with the current build; measure the generated assets rather than relying on a fixed size claim. See [Performance](performance.md) for dated local observations and their limitations, not production latency guarantees.

The build runs `cargo build --locked --release --target wasm32-unknown-unknown -p debarred-web --no-default-features --features hydrate`, then `wasm-bindgen --target web --out-dir crates/web/dist/assets --out-name app` on `target/wasm32-unknown-unknown/release/debarred_web.wasm`. It verifies the CLI version against Cargo.lock before compiling. All entries under `crates/web/public/` are copied recursively to `dist/assets/`, including `app.css`, `boot.js`, photos, and fonts when present. `wasm-bindgen` is found on PATH, including the local `.tools/cargo/bin` fallback. A missing target, CLI, or version mismatch fails with setup guidance, not an automatic install. Generated `dist` is ignored.

## Device-only preview (no Docker, Supabase, or database)

```sh
sh scripts/build-web.sh
python3 scripts/local.py preview
```

This invokes `cargo run -p debarred-server -- --preview` in the foreground. Open **http://localhost:3000** in Chrome. The server binds only to `127.0.0.1:3000`. Preview is a device-only sandbox, not an authenticated account or database-backed workflow. It supplies no database URL, Supabase key, or session encryption key. It does not seed, migrate, or start Supabase. The backend's `--preview` implementation supplies the sandbox behavior; building assets is a separate step. Use Ctrl-C to stop the server.

You can inspect command help without starting a server:

```sh
python3 scripts/local.py preview --help
```

## Actual local authentication and persistence

With Docker running and Supabase CLI installed:

```sh
python3 scripts/local.py start
python3 scripts/local.py status
python3 scripts/local.py migrate
python3 scripts/local.py seed
sh scripts/build-web.sh
python3 scripts/local.py run
```

Every Supabase CLI `start`, `status`, and `stop` invocation explicitly passes `--workdir` pointing to the absolute path of `.tools/local/supabase-project` and also uses that directory as its subprocess working directory. Before each invocation, the helper creates this ignored project and refreshes only `supabase/config.toml` from the tracked workspace config. It never copies environment files, root project links, or migrations. Edit the tracked config, not the generated copy. Dotenv files and symlinks in the generated project's top level or `supabase/` directory cause a refusal before launching the CLI; do not put secrets there. This prevents CLI auto-loading from the repository root or its `supabase/` directory. Generated CLI metadata remains under `.tools/`, already ignored.

`start` requires both CLI and reachable Docker daemon before invoking Supabase. CLI start/status/stop output is captured because it can contain secrets. `status` displays only fixed local endpoints, never the full JSON or keys. `run` first runs `cargo run -p debarred-server -- --migrate`, then starts the normal server in the foreground; `migrate` runs only that migration command. These use the server's SQL migrations, not hosted CLI migrations. Ctrl-C stops the foreground server. Stop Supabase separately while retaining its data:

```sh
python3 scripts/local.py stop
```

| Service | Local address |
| --- | --- |
| App | http://localhost:3000 |
| Supabase API | http://127.0.0.1:54321 |
| PostgreSQL | 127.0.0.1:54322 |
| Studio | http://localhost:54323 |
| Mail UI | http://localhost:54324 |

Project ID is `vitattendance-local`. Do not link it to a hosted project. Keep Docker-published development ports private to your machine; the app's loopback binding does not itself firewall Docker's ports. Supabase owns its local data volumes; `stop` does not reset them. No reset/delete command is provided by this helper.

### Disposable local account

`seed` sends an actual `POST /auth/v1/signup` to the verified local Auth API with the local anon/publishable key. Email signup is enabled and confirmations are disabled. It never writes `auth.users` stubs, uses an admin/service key, or imports production users. A repeated signup that reports an existing account is verified via a real password-grant login; response bodies/tokens are not printed.

**Local-only, public test credentials — never reuse outside this disposable stack:**

- Email: `dev@vitattendance.test`
- Password: `LocalAttendance123!`

Use the local password login UI backed by the backend's `POST /auth/local/login` endpoint. That endpoint requires the backend's local-auth guard and matching exact local Origin and sets the session cookie; authenticated mutations then require the session CSRF token. In other words, an Auth token alone is not an app session. Opening the app at `http://localhost:3000` keeps Origin consistent. Seeding creates an Auth identity, not attendance fixtures. Preview does not exercise this path.

### Optional Google OAuth

Google is **disabled by default**; password signup/login needs no Google credentials. To test Google, use a dedicated development OAuth client, export `LOCAL_GOOGLE_CLIENT_ID` and `LOCAL_GOOGLE_CLIENT_SECRET` in your shell, and change only `[auth.external.google].enabled` to `true` in `supabase/config.toml`. The TOML references `env(LOCAL_GOOGLE_CLIENT_ID)` and `env(LOCAL_GOOGLE_CLIENT_SECRET)`; never paste credentials into tracked files. Restart the local stack after config changes.

Register `http://127.0.0.1:54321/auth/v1/callback` as Google's authorized redirect URI for this configuration. The app callback is a different URL: `http://localhost:3000/auth/callback`, explicitly allowed in local Auth. Google login contacts Google and is not an offline feature. Do not use production OAuth credentials. The default local UI prioritizes password sign-in and hides the Google button. To exercise optional Google after configuring it, navigate directly to http://localhost:3000/auth/login. Production, where local password login is disabled, retains the Google button.

## Safety boundaries

- The helper never reads, sources, or evaluates `.env` files and never writes production files. It uses subprocess argument arrays, never shell/eval. Supabase CLI runs only from the generated isolated project with explicit `--workdir`, never with the repository root as cwd. Only the tracked TOML config is copied; repository `.env`, `production.env`, and `supabase/.env` are neither read nor copied. No `link`, `db push`, hosted migrations, or admin-key operations are exposed. This is a workdir/config-discovery boundary, not an OS filesystem sandbox.
- Child environments use an allowlist, not a copy of the parent environment. Inherited database/Supabase/AI credentials, `PG*`, proxy settings, trusted proxies, and arbitrary Rust flags are not forwarded. Only the two explicit local Google variables are additionally forwarded to Supabase CLI. AI is disabled with `AI_API_KEY=""`; unused AI/proxy settings are removed.
- Authenticated run/migrate/seed require local CLI status JSON. The API must be HTTP on a loopback host at port 54321. The database must be PostgreSQL on a loopback host at port 54322; URL query/fragment overrides and ambiguous paths are rejected. Valid URLs are normalized to `127.0.0.1`. Seed disables proxies and HTTP redirects.
- The app receives `PUBLIC_ORIGIN=http://localhost:3000`, `BIND` and `BIND_ADDRESS=127.0.0.1:3000`, `SUPABASE_URL=http://127.0.0.1:54321`, only the local anon/publishable key, the verified local `DATABASE_URL`, and `LOCAL_AUTH_ENABLED=true`. Preview instead disables local auth and omits all DB/Supabase credentials.
- Authenticated run/migrate creates a random persistent base64 32-byte AES session key in `.tools/local/session.key` with mode `0600`. Existing keys are never overwritten; malformed or insecure files fail closed. Keep this local key to preserve local sessions. `.tools/` is already ignored.
- CLI failures suppress raw output, Auth failures suppress response bodies, and Cargo output redacts known local credentials. Avoid enabling unrelated debug logging that dumps configuration or tokens.

## Drafts and saving

The browser calculates projections locally with the Rust domain engine. Cloud saves are debounced and serialized, carrying both the expected database revision and an `X-Session-User` identity precondition. A changed browser account cannot receive an older tab's plan. Failed initial loads never become empty-plan writes; expired sessions and revision conflicts pause saves.

Device drafts are user-, semester-, and document-scoped. Every document, including a reload or duplicated tab, writes a fresh unique namespace; it never takes over another document's key. A per-tab sessionStorage pointer restores that tab's previous copy on reload. After all tabs close, a fresh sandbox discovers the latest edited device copy. Archived copies remain discoverable under **Device drafts**, where restoration requires confirmation. A fresh authenticated tab loads the server first; archived account drafts require explicit restoration rather than automatically saving another tab's unsaved edits. Legacy shared draft keys are read-only recovery sources.

Conflict drafts keep their original base revision across repeated reloads. Draft restoration happens before sandbox editing is enabled and does not wait for server availability. Merely reloading a copy does not make it newer than another tab's actual edit. If storage is unavailable/full, export the plan as JSON before closing the tab. Clearing browser site data removes these device copies; they are not backups outside this browser.

Cloud reload requires an explicit discard confirmation. Signing out warns about unsaved changes and clears the current document's account draft only; archived or other-document drafts are not silently deleted. Never treat local storage as a secure vault on a shared computer: use export/private devices or clear browser site data when appropriate. No session/CSRF/provider tokens are persisted with drafts.

Image extraction returns a reviewable draft. Applying it explicitly replaces the timetable and resets attendance/leave, with a notice to re-enter counts. The helper's structured status/max-skip paths are deterministic and free; the sandbox makes no cloud helper/import/save requests.

## Account, privacy, and operator controls (Phase 4)

`/account` provides signed-in profile information, account JSON export, and a
confirmed deletion request; `/privacy` and `/terms` are public draft policy pages
requiring operator review before launch. Public policy reads do not load private
drafts or trigger cloud saves. In the sandbox, account export/deletion is disabled:
export device drafts separately and use browser site-data cleanup to remove them.
Optional AI sends the selected image or routed question/context to the configured
provider; normal editing/calculation needs no AI. The local helper disables AI.

The server reads `SUPPORT_EMAIL` from its process environment, sanitizes it, and
publishes the accepted contact in SSR and `/api/runtime`. It is public contact
information, not a secret. Missing/invalid values show “Support contact must be
configured before launch”; no real contact has been verified by this documentation
update. Configure a monitored operator address and review the policies before
publishing. `scripts/local.py` deliberately does **not** forward `SUPPORT_EMAIL`,
`HTTP_MAX_IN_FLIGHT`, or `METRICS_BEARER_TOKEN` from the parent environment; merely
exporting them before invoking the helper does not configure its child server.
Use a separately controlled process environment for explicit configuration tests,
without sourcing production environment files or weakening the helper allowlist.

Normal executable startup wires admission control (`HTTP_MAX_IN_FLIGHT`, default
32) and `/internal/metrics`. Metrics are disabled (`404`) without
`METRICS_BEARER_TOKEN`; a configured token must be 32–256 visible ASCII bytes and
requests require the exact operator bearer, not a user session. Empty tokens are
invalid. Keep tokens out of browser storage, URLs, logs, and command arguments;
restrict scrapes to a private network and use TLS off loopback. Metrics use bounded
route/method/status labels, not personal identifiers, but remain operationally
sensitive. Credential-free preview does not exercise authenticated account routes
or normal operations wiring. See [Operations](operations.md) for limits and the
operator checklist; current startup is wired despite that guide's original
module-scoped integration notes.

### Deletion and persistent device-cache tombstones

Deletion requires the exact `DELETE MY ACCOUNT` phrase, a second UI confirmation,
and a server-checked interactive login within five minutes. Refresh is not a fresh
login. A successful HTTP 202 erases owned plans, retained AI usage, migration audit
rows, and all app sessions in one transaction, then reports
`pending_identity_deletion`, **not complete identity erasure**. An error/timeout is
not proof of deletion. The old cookie may remain inert; server session revocation
is authoritative. See [Account lifecycle](account-lifecycle.md) for the contract.

On success, the browser writes a persistent localStorage tombstone named
`vitattendance:v1:account-deleted:<user UUID>` before removing that user's
per-document drafts and this tab's sessionStorage pointers. Other open tabs receive
the storage event. The tombstone prevents frozen tabs or late callbacks from
recreating account drafts; it intentionally survives reloads and later sign-in.
Other users' drafts and sandbox copies are not targeted. If browser storage is
unavailable or cleanup fails, use browser site-data cleanup manually; server
success does not guarantee every device copy was erased.

There is no automatic cache reactivation. If an operator intentionally reactivates
a disposable test identity outside the normal deletion workflow, signing in again
does not remove the browser tombstone. Close all app tabs first, export any other
needed device drafts, clear this origin's site data in browser settings (including
local/session storage), then reopen and sign in. This removes **all** app drafts
for that origin in this browser, not just the reactivated user's copies. Browser
cleanup does not reactivate the server identity or clear its pending queue marker;
do not delete queue markers merely to bypass pending-deletion protections.

### Manual identity work and backup limits

Supabase identity deletion remains a manual, authorized operator action in the
correct project's Authentication Dashboard, matched by queued UUID, never by
email. While pending, the server refuses new app sessions/writes. Verify app-data
erasure and the identity operation, then verify the queue row cascaded away after
`auth.users` deletion; do not manually drop the marker as a completion shortcut.
This does not delete a Google account, revoke Google authorization, or force a
Google sign-out. Use only disposable identities when rehearsing locally; the real
local Supabase/browser journey remains unverified here.

Downloaded exports, other devices, logs, provider retention, and backups are
separate copies with separate handling. Account deletion does not erase them.
Operators must define retention and replay deletion requests after a restore so
old backups cannot resurrect erased data or reopen pending accounts. Keep the
nonidentifying global batch audit; erasing user-linked migration rows can
intentionally block later whole-batch rollback. A native synthetic dump/restore
drill tests limited data/security integrity, not usable provider sessions, key
recovery, production backups, PITR, or a real Supabase restore. See
[Migration](migration.md), [Release](release.md), and [Performance](performance.md).
No migration, deployment, official-calendar verification, or real-provider
validation was performed for this documentation update; launch remains blocked.

## Browser regression tests

Node and Playwright are development-only dependencies. In this checkout Playwright is installed under ignored `.tools/browser`; Chrome is used from `/Applications/Google Chrome.app`. On other machines, install Playwright and its Chromium runtime explicitly:

```sh
npm install --prefix .tools/browser --no-audit --no-fund playwright@1.64.1
.tools/browser/node_modules/.bin/playwright install chromium
```

Build the web assets and native server, then run:

```sh
sh scripts/build-web.sh
cargo build -p debarred-server --locked
node scripts/browser-tests.cjs
```

The runner starts its own bounded preview server on a random loopback port, uses a sanitized environment, and stops both browser and server afterward. It tests actual SSR/WASM hydration, leave math, reload persistence, invalid inputs, mobile keyboard/dialog behavior, export, recovery from missing WASM, draft isolation, and save ordering/conflicts/expiry. Screenshots are written to `.tools/screenshots/`.

**Authentication browser scenarios are explicit network-contract mocks**, not evidence of a live Supabase login. Separate Rust tests exercise real disposable Postgres and mock provider exchanges. Full local Supabase + browser sign-in remains blocked until Docker is available. No test runner reads the production `.env`.

## Safety tests

```sh
python3 -m unittest discover -s scripts -p test_local.py -v
```

These tests use only the Python standard library, temporary files, and mocks. They verify URI guards (including remote DB query overrides), environment isolation, private persistent keys, public-key selection, error suppression, and preview's lack of stack dependencies. They also assert that start/status/stop use both the explicit isolated `--workdir` and isolated cwd, only the config is copied, and synthetic root `.env`, `production.env`, and `supabase/.env` fixtures are never accessed. Generated-project dotenv/symlink rejection is tested before any CLI subprocess can launch. They do not start servers, touch databases, or inspect real production environment files.
