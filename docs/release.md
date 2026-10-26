# Offline release gate (phase 4)

**Launch status: BLOCKED.** No real contact email or completed production/manual
attestations have been provided. The local working tree is dirty. Docker is not
available on the measured machine; native PostgreSQL tests do not substitute for
a real Supabase Auth journey or local-stack validation. Nothing in these scripts
logs into a provider, calls a hosted endpoint, deploys, pushes, commits, or changes
production configuration. A successful benchmark is **not** release approval.

The only approved public origin for this gate is exactly:

```text
https://vitattendance.rasmalai.tech
```

There is no trailing slash, alternate hostname, or HTTP fallback in evidence.
The gate does not read `production.env`, discover dotenv files, scan the workspace
for secrets, read Git diffs, or auto-generate privacy/terms/legal claims.

## Prerequisites and operator workflow

1. Build and test the intended revision with the installed toolchain. Explicitly
   build the release server and public assets using the existing tooling:

   ```sh
   cargo build --offline --locked --release -p debarred-server
   sh scripts/build-web.sh
   python3 -m unittest discover -s scripts -p test_release.py -v
   python3 -m unittest discover -s scripts -p test_backup_drill.py -v
   ```

   Use the existing absolute local Cargo/Rustup environment where required (see
   `docs/local-development.md`). This gate does not install dependencies or
   validate that an old binary was built from the current source: the operator
   must build/test the intended revision **before** manifest generation.
2. Run the local benchmark as described in `docs/performance.md`, and the synthetic
   native backup drill below. These establish local regression/recovery properties,
   not provider or production readiness. The production backup/restore attestation
   still requires operator review and an appropriate real-backup rehearsal.
3. Have an operator review the intended source changes and obtain a clean Git
   tree through their normal workflow. The gate will not commit, stash, revert,
   or create a branch. A dirty-tree check is an expected **BLOCKED** result,
   not a failure of the fixture test suite.
4. Generate the manifest after the final artifact build:

   ```sh
   python3 scripts/release.py manifest --output .tools/release/manifest.json
   ```

5. Independently perform the manual reviews below for that artifact set. Create
   the explicit operator file `.tools/release/evidence.json` with accurate
   attestations, a real monitored contact email, and the manifest fingerprint.
   Do not fill in passing evidence based only on automated tests. The script
   intentionally does not manufacture an evidence file.
6. Run the gate:

   ```sh
   python3 scripts/release.py check --manifest .tools/release/manifest.json --evidence .tools/release/evidence.json
   ```

   Exit `0` means hashes, clean-tree check, and supplied attestations passed the
   schema/checks. Exit `1` means **BLOCKED**. Even `READY` means an **offline gate**
   passed, not that deployment occurred or that the script independently proved
   a human's assertions. Deployment remains a separate operator decision.

Only explicit `.json` input/output paths under the repository's `.tools/` are
accepted by `release.py`; parent traversal and symlinks are rejected. No private
file is discovered by convention. Do not put secrets, tokens, passwords, database
URLs, customer data, or private export data in evidence or public dist. Store
underlying review records privately elsewhere and use nonsecret ticket IDs here.

## Artifact manifest API

The manifest is deterministic JSON `version: 1`, `algorithm: sha256`, with:

- `inputs[]`: repository-relative POSIX `path`, file `bytes`, and `sha256`.
- `fingerprint`: SHA-256 of UTF-8 JSON for the sorted input list using sorted keys
  and compact separators (`,`, `:`). Names and byte counts participate, not just
  concatenated file hashes. There are no timestamps, environment values, or
  machine-specific absolute paths.

Declared inputs are **only**:

- `target/release/debarred-server` (the release binary);
- **all files** in `crates/web/dist/`, recursively, including JS, WASM, CSS, fonts,
  images, type declarations, and `.gz` siblings;
- `crates/domain/data/winter-2026.v1.json`;
- **all files** in `crates/server/migrations/`, recursively;
- `Cargo.lock`.

Gzip files are not excluded as duplicates: the server actually serves them, so a
stale compressed sibling must invalidate the manifest. Added/removed assets,
calendar/migration changes, binary rebuilds, and lockfile changes all invalidate
the current artifact set. Missing directories/required JS or WASM, symlinked
inputs, and hidden/dotenv-named artifacts are rejected. The script never follows
symlinks to read files outside the declarations.

`check` reconstructs this exact manifest and requires full equality with the
saved manifest. Evidence must also match the **current** fingerprint. After any
artifact change, regenerate the manifest and have the affected reviews repeated
and all attestations reaffirmed for the new fingerprint; do not silently reuse
old evidence. SHA-256 binding is not a signature or proof of reviewer identity.

## Evidence schema (manual, nonsecret)

This is an intentionally **incomplete/BLOCKED template**, not real evidence.
Replace the fingerprint with the manifest's current 64-character hex value and
supply a real contact. Leave each `passed` false until its work is completed.
Every listed field and attestation is required; unknown fields are rejected.

```json
{
  "version": 1,
  "artifact_fingerprint": "COPY_CURRENT_MANIFEST_FINGERPRINT",
  "public_origin": "https://vitattendance.rasmalai.tech",
  "contact_email": "",
  "attestations": {
    "official_calendar_review": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "live_google_non_test_login": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "real_supabase_auth_journey": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "backup_restore_drill": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "identity_migration_review": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "identity_cutover_plan": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "rollback_review": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "account_deletion_process": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""},
    "local_supabase_stack_validation": {"passed": false, "reviewer": "", "reviewed_at": "", "reference": ""}
  }
}
```

Each completed attestation requires `passed: true`, reviewer and reference IDs of
1–64 ASCII letters/digits/underscores/hyphens, and a timezone-aware ISO-8601
`reviewed_at` no later than the current time. Example ID forms: `operator-1`,
`OPS-123`; these are not actual reviewers/reviews. No arbitrary notes/URLs are
accepted. The contact must be syntactically email-shaped and not a reserved test
or example address. Syntax does **not** prove mailbox ownership or monitoring;
operators must verify that separately. No real contact is supplied by this work.

Required review meaning:

| Attestation | Required operator evidence |
| --- | --- |
| `official_calendar_review` | Verify bundled semester dates, day types, checkpoints, holidays, and revision against the institution's official source; resolve discrepancies. |
| `live_google_non_test_login` | Verify the intended Google consent/client configuration using a real non-test account on the exact public origin, not only a development/test-user allowlist. Do not record user identity/tokens here. |
| `real_supabase_auth_journey` | Exercise actual Supabase-backed login, callback, authenticated read/write, CSRF/Origin enforcement, refresh, logout/revocation, and failure handling in the intended environment. Synthetic benchmark sessions do not count. |
| `backup_restore_drill` | Restore a consistent database backup into an isolated environment, verify schema/data integrity and app compatibility, document recovery time and responsible operator. Never restore over production merely to satisfy this gate. |
| `identity_migration_review` | Review UUID ownership and mapping, collision/conflict handling, migration audit/results, rejected records, and identity boundaries; see `docs/migration.md`. No attendance reassignment based solely on matching display names or unverified email. |
| `identity_cutover_plan` | Named owner, freeze/write-routing rules, dry-run/reconciliation, staged switch, abort criteria, and treatment of writes during/after cutover. |
| `rollback_review` | Verify retained prior binary/assets, backup reference, schema compatibility, routing reversal, and data reconciliation procedure. |
| `account_deletion_process` | Named operations owner and monitored contact; review pending-identity queue, verify application route integration and user-facing pending semantics, and document manual Supabase identity deletion and verification. |
| `local_supabase_stack_validation` | Validate the real local Supabase stack with Docker/CLI and application migrations/Auth integration. If Docker/stack is absent, this remains pending; native PostgreSQL or mock-provider tests are not substitutes. |

The current server router mounts `account::routes()`; this drill does not exercise
those HTTP endpoints or provider identity deletion. Operators must verify the
integration and manual operations before attesting to the deletion process. The
deletion queue's app-data erasure response is not completed identity erasure: a responsible
operator must process the pending Supabase identities, verify completion, and
preserve the pending marker until identity deletion cascades it. Never claim that
a queued request deletes a Google account. Refer to the existing lifecycle guide;
this document is not generated legal policy.

## Gate report API

The JSON report has `version: 1`, `status: BLOCKED|READY`, current
`artifact_fingerprint` when available, `automated`,
`manual_attestations_not_automated_tests`, `local_docker_cli`, `failures[]`, and
`deployment_performed: false`.

Automated checks cover current file hashes/fingerprint and `git status
--porcelain=v1 --untracked-files=all`. Git status is bounded to ten seconds and
reports only aggregate two-column statuses, **never filenames or diff contents**.
The gate uses a sanitized child environment and never inspects Git file content.
Manual report values are schema/binding/attestation checks, not remote test results;
contact email, reviewer IDs, references, and review-file contents are not echoed.
Failures are fixed sanitized reason identifiers, not exception dumps or environment
values. Docker CLI detection is availability only; the script does not run it or
claim the daemon/stack was validated.

## Staged rollout and rollback (operator actions only)

1. **Prepare:** resolve every blocker; identify monitored contact and on-call
   owner; retain previous binary **and its matching dist assets**; take/verify a
   consistent backup; capture schema version, current manifest, and evidence.
2. **Isolated rehearsal:** apply the normal additive migrations to a restored or
   disposable environment, verify real Auth/identity flows, data migration results,
   deletion operations, and rollback compatibility. Do not use production data in
   the local benchmark.
3. **Controlled cutover:** follow the reviewed freeze/routing plan, migrate
   additively, deploy the exact reviewed binary/calendar/migrations/assets, begin
   with a small controlled audience, and observe readiness, errors, revision
   conflicts, sessions, and deletion queue. Verify public asset/content-encoding
   integrity, origin/cookie settings, and actual Auth callback behavior. Expand
   only after operator approval.
4. **Abort/rollback:** stop expanding traffic and restrict writes if ownership or
   integrity is uncertain. Route traffic back to the retained compatible prior
   release with its matching assets. **Do not downgrade migrations, drop schemas,
   drop columns, or delete identity/attendance records as a rollback shortcut.**
   Additive schema stays in place; use the rehearsed reconciliation/forward-fix
   plan for writes made after cutover. A destructive restore is a separately
   authorized recovery operation, not this script's rollback mechanism.
5. **Re-review:** any rebuilt artifact set needs a new manifest and matching
   evidence. Keep backups and prior artifacts until the reviewed observation and
   retention windows are complete. These scripts never execute these steps.

## Validation observed locally

Final validation reported by the operator passed: the full pure Python suite
(**42 tests**), **17 browser scenarios**, workspace native PostgreSQL tests with
`--include-ignored`, formatting, native and hydrate Clippy, and release builds.
These results are operator-reported, not fields in the benchmark/backup reports
or tests rerun for this documentation update. Browser auth scenarios use mocks;
native PostgreSQL is not Docker or a real Supabase stack. No real Supabase/Docker
validation or release approval is implied.

`python3 -m unittest discover -s scripts -p test_release.py -v` passed all 12 pure
fixture tests. Hash mutation tests cover every declared input plus asset
addition/removal; other tests cover missing/stale evidence, dirty/unavailable Git,
narrow evidence schema, operator-path restrictions, private/symlinked assets, and
numeric-loopback benchmark URL rejection. Git behavior is mocked; fixtures never
alter the real repository state and use no provider keys or remote calls.

The actual local manifest/check was run: artifact hashes matched, while the check
returned **BLOCKED** for the dirty tree and missing manual evidence. Docker CLI
was absent. The synthetic native backup/restore described below was subsequently
performed; no production backup restore, login, official-calendar approval,
identity cutover, monitored contact, or deployment was invented or performed.
The release binary was explicitly rebuilt for the drill, so earlier release
manifests/evidence must be regenerated/reviewed for the new artifact fingerprint.
The drill does not rewrite the release manifest or evidence.

## Executable synthetic native backup/restore drill

`scripts/backup-drill.py` performs an actual `pg_dump`/`pg_restore` round trip,
not a checklist-only attestation. It requires an **already running** native local
PostgreSQL cluster and a role allowed to create/drop databases and apply the
server migrations. It never starts a cluster, installs tools, builds a binary,
creates roles, or connects to a provider. Tool discovery uses PATH, with
`/opt/homebrew/opt/postgresql@17/bin` as a fallback for `psql`, `createdb`, `dropdb`,
`pg_dump`, and `pg_restore`.

Explicitly build the latest release server first so its embedded migrations match
the current files. The script verifies every embedded migration ledger checksum
(SHA-384) against `crates/server/migrations/*.sql`, including 0002 and 0003; an old
binary is rejected rather than being treated as a valid schema rehearsal.

```sh
cargo build --offline --locked --release -p debarred-server
python3 -m unittest discover -s scripts -p test_backup_drill.py -v
DRILL_ADMIN_DATABASE_URL=postgresql://phase2_test@127.0.0.1:6543/phase4_validation20261026 python3 scripts/backup-drill.py
```

The maintenance database in that URI must already exist. It is used only as the
maintenance connection for creating/dropping the two drill databases: **it is not
dumped, seeded, migrated, or dropped**. If that operator database is absent, the
operator can explicitly choose the cluster's `postgres` maintenance database:

```sh
DRILL_ADMIN_DATABASE_URL=postgresql://phase2_test@127.0.0.1:6543/postgres python3 scripts/backup-drill.py
```

`DRILL_ADMIN_DATABASE_URL` is the only input database setting. Numeric IPv4/IPv6
loopback is mandatory; DNS names including `localhost`, remote addresses, sockets,
queries (even empty queries and `host`/`hostaddr`/`options` overrides), fragments,
control characters, and ambiguous host forms are rejected before launching tools.
There is no fallback to an inherited `DATABASE_URL`, PG environment, root dotenv,
or production configuration. Passwords are decoded into a sanitized child's
`PGPASSWORD`, never command arguments, logged URIs, or reports. Native clients
receive discrete PGHOST/PGPORT/PGUSER values, disabled password prompts, five-second
connect timeouts, and `/dev/null` password/service files. `psql -X` ignores local
psql startup files. The one generated `DATABASE_URL` passed to `--migrate` is an
**ephemeral test config**, not an input read from the shell or any file.

`--binary` may select an existing executable strictly under workspace
`target/release/` (default `target/release/debarred-server`). Outside paths, debug
paths, traversal, and symlinks are rejected. The report records its SHA-256.

### Isolation, seed, and archive

Every invocation generates two fresh names:

```text
vitattendance_drill_<32 lowercase UUID hex>_source
vitattendance_drill_<32 lowercase UUID hex>_restore
```

Creation uses `template0`. Operations require the generated-name whitelist,
current invocation's name set, and its successfully-created list. Existing name
collisions fail creation; the collided database is never adopted or dropped.
Only these new databases are ever dumped, restored into, or dropped. Source
bootstrap creates the minimal test-only `auth.users(id uuid PRIMARY KEY)`, then
runs the release server's real `--migrate` entrypoint with a random session key,
loopback public origin, unused loopback Auth URL on port 1, local password Auth
disabled, and AI disabled. Captured command output is never echoed.

The seed contains two synthetic UUID users; the active user owns a standalone
two-course sample plan, one session byte blob, one quota row, and one migration
audit row in one batch. The second user has a pending-deletion marker and no plan.
There is also one OAuth-flow byte blob. `plan_storage_sha256` is checked against
the JSONB read back from PostgreSQL using compact recursively-key-sorted JSON.
The synthetic audit row is not evidence of an actual identity migration.

**The session and OAuth byte blobs are random bytea fixtures, not valid Auth
ciphertexts.** This drill validates byte preservation only; it does not decrypt
sessions, validate key recovery, or establish provider login/refresh correctness.

The source is dumped in custom format with ACLs intact: neither `--no-acl` nor
`--no-privileges` is used. Restore targets the other empty, fresh database with
`--no-owner --exit-on-error --single-transaction`; it does not use `--clean`.
This is a same-cluster/same-role drill: any existing `anon`/`authenticated` roles
are reused, and no global roles are created or backed up. If absent, the role
checks still verify PUBLIC revokes but do not pretend those roles were tested.

Archives remain under `.tools/release/backup_<uuid>.dump` with mode **0600**.
The JSON report is atomically replaced at `.tools/release/backup-drill.json`, also
0600. Symlinked output directories/destinations and archive overwrites are refused.
Failed runs can leave a partial synthetic archive, also private. Archives contain
synthetic rows; they are not production backups and should not be published.

### Integrity and security checks

The script discovers **all** ordinary/partitioned `app_private` tables and also
checks `auth.users` and `public._sqlx_migrations`. It orders each table's complete
JSON rows by their JSONB text, canonicalizes the resulting JSON array, and compares
per-table counts and SHA-256 fingerprints. No row data, user IDs, blobs, keys,
passwords, URLs, or raw security definitions appear in the report.

A separate ordered security fingerprint compares table RLS/forced-RLS, owners,
schema/table/function ACL text, default privileges, function definitions,
`prosecdef` and configured search paths, trigger definitions/enabled state, and
available API-role attributes. Implicit NULL schema/table ACLs are expanded with
`acldefault` before text comparison: pg_restore can materialize those default
rights explicitly. Actual grants/revokes are not discarded or ignored.

Both source and restore must independently meet the expected security properties:
all private tables have RLS; PUBLIC/available API-role schema, table, function,
and default-ACL privileges are revoked; available API roles have no effective
private access (including inherited privileges), superuser, or BYPASSRLS rights;
`lock_account_write` remains SECURITY DEFINER with exactly
`search_path=pg_catalog, app_private`; all three account-write guards remain enabled.

Both databases are behaviorally probed: the active user's lock check must succeed,
and a plan INSERT for the pending user must raise SQLSTATE 42501 **with the
expected pending-identity message**, not fail due to some unrelated error. The
probe runs in a PL/pgSQL exception subtransaction, and post-probe fingerprints must
remain unchanged on both databases. This is a database-owner probe of the guard,
not a substitute for application-role or HTTP authorization tests.

### Report, bounds, cleanup, and limitations

JSON `version: 1` includes `synthetic: true`, `status: PASSED|FAILED`, per-table
`source`/`restored` counts and hashes, source/restored security hashes and expected-
property results, `data_pass`, `security_pass`, `pending_guard_pass`, probe side-
effect status, archive bytes/hash/mode, binary hash, dump/restore/total seconds,
cleanup status/counts, and fixed sanitized failure identifiers. Exit 0 requires
the round trip, probes, security checks **and cleanup** to succeed. The explicit
fields `key_decryption_tested`, `local_supabase_tested`, and
`production_backup_tested` remain **false** even on success.

Native subprocesses are bounded to at most 60 seconds, SQL statements to 45
seconds and lock waits to five seconds. The work deadline is 240 seconds; cleanup
reserves up to 15 seconds for each of the two database drops. A `finally` block
attempts cleanup after success, errors, timeouts, or a normal interrupt. A hard
process kill, machine/cluster failure, or lost creation acknowledgement can leave
an orphan: an operator must inspect randomized drill names on that local cluster.
The script never widens cleanup to prefix scans or drops other invocations' DBs.

A passing synthetic drill proves this local schema/data/security round trip. It
**does not prove production backup completeness, PITR/WAL recovery, external role
recreation, encryption-key restoration, hosted Supabase compatibility, or an
operator's production recovery-time objective**. It never writes release evidence
or sets `backup_restore_drill: passed` in an attestation. Those reviews remain
blocked until honestly completed against the intended recovery procedure.

### Actual run

The latest `.tools/release/backup-drill.json` records **PASSED** at
**2026-10-26T05:35:40Z** for the rebuilt release binary's synthetic native
PostgreSQL drill. The operator reports native PostgreSQL 17 on `127.0.0.1:6543`;
the report itself does not record the port or maintenance database name.

The run compared **nine tables / twelve rows**: seven private tables with one row
each, two auth users, and three successful migration ledger rows. Source/restored
data fingerprints and normalized security fingerprints matched, both pending-user
guard checks passed, and probe fingerprints remained unchanged. The report records
successful cleanup, **two databases created and zero remaining**. No independent
catalog check was performed for this run.

Dump: **0.043763 s**; restore: **0.038783 s**; total including setup/checks/cleanup:
**1.141081 s**. The archive was **22,466 bytes**, mode 0600. These are single-run
synthetic timings, not production recovery guarantees. `key_decryption_tested`,
`local_supabase_tested`, and `production_backup_tested` remain **false**; matching
synthetic blobs prove byte integrity, not usable login sessions or key recovery.
No release attestation was created or marked passed by the drill.
