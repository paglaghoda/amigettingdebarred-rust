# Offline legacy plan migration (phase 4)

`debarred-migrate` imports **operator-provided offline exports only**. It does not call Clerk, Supabase APIs, or any hosted service to discover identities or export data. No actual export or identity mapping ships with this tool. All examples/tests are synthetic.

## Wiring and prerequisites

`crates/migrate` is a root workspace member. Package/binary: `debarred-migrate`. The server's existing `sqlx::migrate!` directory embedding picks up `crates/server/migrations/0003_migration_audit.sql`; no server library changes are needed. Rebuild the server and deploy migrations through the existing privileged migration process **before** apply/rollback. This CLI never creates tables or runs migrations itself.

Offline validation needs no app configuration, authentication, database, or environment variables. Database commands require a privileged PostgreSQL role with access to `auth.users` and the private tables (including RLS bypass, normally the migration owner). Never grant this tool to anonymous/authenticated API roles or expose it through a web endpoint.

## Exact input format

Export file (UTF-8 JSON):

```json
{
  "source_kind": "Clerk",
  "records": [
    {
      "source_user_id": "user_synthetic_student",
      "semester_id": "winter-2026",
      "timetable": {
        "courses": [{"code": "CS101", "name": "Synthetic course"}],
        "grid": {"Mon": ["CS101", null, null, null, null, null, null]}
      },
      "attended": {"CS101": 1},
      "leave_dates": ["2026-11-23", "2026-11-30#0"]
    }
  ]
}
```

`source_kind` is exactly `Clerk` or `Supabase`. Clerk IDs must be `user_` followed by at least one character; source identifiers permit only ASCII alphanumerics, `_`, and `-`, maximum 256 bytes. Supabase source IDs must be non-nil, canonical lowercase hyphenated UUIDs. IDs are matched byte-for-byte; no trimming, case folding, UUID aliasing, email matching, or provider lookup occurs. Export different source kinds in separate batches.

`timetable`, `attended`, and `leave_dates` accept their shown object/array form **or one JSON string encoding of that exact form**. No recursive string decoding, null/default substitution, or permissive schema coercion. This matches the legacy `attendance-engine.ts` timetable (`courses`, `grid`), `plan.functions.ts` JSON blobs, and stored string-array leaves. Grid keys are `Mon`, `Tue`, `Wed`, `Thu`, `Fri`, `Sat`; provided rows have exactly seven course-code-or-null slots, excluding lunch. Missing grid rows mean no classes. Counts are unsigned integers; unknown courses and counts exceeding held classes at the confirmed snapshot fail.

Separate identity manifest:

```json
{
  "mappings": [
    {
      "source_user_id": "user_synthetic_student",
      "target_user_id": "00000000-0000-4000-8000-000000000001",
      "semester_id": "winter-2026",
      "snapshot_as_of": "2026-11-16",
      "verified_by": "Synthetic operator",
      "verification_reference": "Synthetic manually reviewed evidence 1"
    }
  ]
}
```

Every field is mandatory. `target_user_id` must be a non-nil UUID belonging to an existing destination `auth.users` account. Confirmation strings must be nonblank, control-character-free, and at most 1024 bytes. `snapshot_as_of` is the human-confirmed inclusive end-of-day attendance cutoff, **not** export time, `created_at`, login time, or today's date. The domain permits a cutoff before/after semester boundaries; the operator must establish its actual meaning. It is never inferred.

**A valid manifest or report is not proof of identity.** The operator must independently verify each source-to-destination association and date with appropriate account-owner evidence, record who verified it and a reference in the separate manifest, and review reconciliation before applying. Do not place emails, passwords, provider keys, or evidence documents in these fields. Reports and hashes cannot authenticate whoever created a manifest.

### Reconciliation rules (fail closed)

- Unknown fields and duplicate JSON object keys anywhere (also in string-encoded fields) are errors. Strip unrelated legacy export columns explicitly during operator preparation; do not feed raw database dumps containing email or timestamp columns.
- Exactly one record and one explicit mapping per source ID. Duplicate source IDs fail even across semesters; split those into independently reviewed batches. Duplicate destination `(user UUID, semester)` pairs, unmapped records, extra mappings, unsupported/ambiguous IDs, and empty batches fail the entire batch.
- Source and manifest semester IDs must match exactly `winter-2026`. No `fall-*` to winter aliases, calendar rewriting, or automatic recovery. Fall plans need a separately designed compatible calendar/migration, not renaming.
- Legacy `YYYY-MM-DD` leaves mean all that day's meetings; `YYYY-MM-DD#0` through `#6` mean a zero-based individual FFCS slot. The strict domain legacy parser and `Engine::new` validate **every** entry. Invalid dates, non-class days, free slots, out-of-semester dates, invalid timetable references, and invalid counts abort validation. Nothing is filtered out or silently repaired.
- The prepared plan preserves leave order and duplicates. Canonical semantic hashing expands whole-day leaves into their actual slots, sorts/deduplicates those slots, sorts courses by code, and recursively sorts JSON object keys. Attendance cutoff, counts, course names, timetable, schema, and calendar revision remain committed. This is the version-1 semantic canonical algorithm for reconciliation. Rollback additionally requires a **lossless storage commitment** (`plan_storage_sha256`) that normalizes only JSON object-key order. It preserves every array's order and duplicates, course order, and Day/Slot representations; semantically equivalent edits still block rollback. Retain both hash implementations for rollback compatibility.
- Apply refuses a target that already has **any semester plan**, not merely the incoming semester. Existing data requires human reconciliation outside this tool. There is no upsert, overwrite, merge, partial apply, or automatic identity linking.

## Dry-run first

Use a private operator directory and restrictive umask (`umask 077`). Keep exports/manifests/report paths outside the repository. The following names refer to files in that directory:

```sh
cargo run -p debarred-migrate -- validate \
  --input export.json --mapping identities.json --report review.json
```

Omitting `validate` selects the same offline dry-run:

```sh
cargo run -p debarred-migrate -- \
  --input export.json --mapping identities.json --report review-default.json
```

Validation **never reads database configuration or connects**, even if `MIGRATION_DATABASE_URL` or `DATABASE_URL` points to a remote server. Invalid data produces a private JSON report and a nonzero exit; no partial prepared batch is returned.

Reports contain `report_version`, raw-file `source_sha256` / `mapping_sha256`, record/mapping/prepared counts, operator-private source IDs, fixed error codes with source IDs where available, and `prepared_digest` (null unless the entire batch validates). They contain no timetable, attendance counts, leave dates, verification strings, or destination UUIDs. Malformed/ambiguous JSON yields a fixed error rather than echoing its content. Source IDs still identify people: **reports are not public artifacts**. `prepared_count` counts individually prepared records even when errors elsewhere invalidate the batch.

`prepared_digest` commits to version, both raw-file SHA-256 values, and sorted commitments to every source ID, target UUID, semester, semantic canonical plan SHA-256, and lossless storage SHA-256. Editing whitespace in either input changes the confirmation digest. Review both files and counts, archive the exact reviewed bytes, and preserve the digest.

For an explicit full-data preparation artifact only:

```sh
cargo run -p debarred-migrate -- validate \
  --input export.json --mapping identities.json --report review-bundle.json \
  --prepared-bundle prepared-private.json
```

Bundles include source IDs, target UUIDs, all prepared plans, their hashes, and the fingerprint. They are sensitive; they are never written by default or accepted as trusted apply input. Apply always revalidates the original export and manifest. Both reports and prepared bundles use `create_new`: output files must be new, existing operator paths are never overwritten, and no overwrite flag exists. They are created mode `0600` on Unix and synced before success. Secure the containing directory/backups; non-Unix permissions must be managed by the operator.

## Apply

Explicitly supply **only** `MIGRATION_DATABASE_URL` via your secure operator environment; `DATABASE_URL` is never a fallback. Do not put credentials on a command line or commit them. The examples below deliberately omit secrets and actual connection URLs.

```sh
cargo run -p debarred-migrate -- apply \
  --input export.json --mapping identities.json --report apply-review.json \
  --confirm-digest EXACT_PREPARED_DIGEST_FROM_REVIEW
```

`apply` itself is the explicit mutation opt-in; no redundant `--apply` flag exists. A fresh report is written, confirmation must exactly match the freshly validated digest, and only then is database configuration read. Success prints `batch_id` and `prepared_digest`; retain both in the operator record. CLI errors are fixed codes, never raw parser/domain/SQL errors, database URLs, passwords, or provider keys. Protect the shell/process environment separately.

Local hosts are restricted to `localhost`, `127.0.0.1`, `::1`, or Unix socket paths, checked after SQLx parses the effective host. Other hosts require `--allow-remote`; remote TLS is forcibly `VerifyFull` even when the URL requests weaker TLS. Use an appropriate trusted server certificate/CA and DNS name. This task does not execute remote migration calls. A local tunnel/proxy counts as local to the CLI; the operator is responsible for its upstream destination.

One READ COMMITTED transaction acquires deduplicated per-user advisory locks in sorted UUID order using the exact 0002 lock key (`pg_advisory_xact_lock(hashtextextended(uuid::text,0))`). Separate pending-deletion checks after lock acquisition see newly committed deletions; key-share locks on every mapped `auth.users` row prevent concurrent account deletion/cascade through commit. Lock timeout is 15 seconds and statement timeout 60 seconds. Every account must exist and have no deletion request or existing plan. The tool inserts only revision-1 plans and private audit rows; a unique constraint or any race/failure aborts **all** writes. Retry by validating/reviewing again, not by relaxing safeguards.

`app_private.migration_batches` stores UUID, report version, fingerprint, source/map hashes, original row count, status, and timestamps. `migration_rows` stores only batch ID, destination user/semester, inserted revision 1, semantic canonical plan hash (`plan_sha256`), and lossless JSON-value hash (`plan_storage_sha256`). RLS is enabled without API policies and privileges revoked from PUBLIC/anon/authenticated. No plan copies, source IDs, legacy emails, or verification evidence enter the audit tables. User deletion cascades audit rows for privacy; the nonidentifying batch summary remains.

## Rollback

```sh
cargo run -p debarred-migrate -- rollback \
  --batch-id BATCH_UUID_FROM_APPLY \
  --confirm-digest EXACT_PREPARED_DIGEST_FROM_REVIEW
```

This also requires `MIGRATION_DATABASE_URL`; the same local-only/`--allow-remote`/VerifyFull restrictions apply. It does not restore legacy exports, accounts, or overwritten data (none were overwritten).

Rollback locks the applied batch, validates its version/fingerprint and original audit-row count, acquires the same sorted account locks and pending/account checks, then locks **all** imported plans. Every plan must still exist at revision 1 with both the same lossless stored-JSON hash and semantic canonical hash. The storage hash is checked directly on the JSON value read from PostgreSQL, before typed plan deserialization; only object-key ordering is normalized. Course-array reordering, Day-to-equivalent-Slot conversion, leave-array reordering, or duplicate changes without a revision bump all abort rollback and leave every row intact. A changed revision, changed data even at revision 1, deleted plan/account, pending deletion, missing audit row, or already-rolled-back batch aborts the entire rollback. Only after every check passes are precisely the audited plans deleted and status/timestamp marked `rolled_back` in the same transaction. Audit rows remain until account deletion. No drop, truncate, bulk user cleanup, unrelated plan deletion, or force-rollback option exists. Keep the original export and manifest in a separately secured operator backup; audit tables cannot reconstruct plans.

## Tests and library API

```sh
cargo test -p debarred-migrate
# With TEST_DATABASE_URL explicitly set to a disposable LOCAL PostgreSQL database:
cargo test -p debarred-migrate --test postgres -- --ignored --test-threads=1
```

No Docker is required. Ignored tests bootstrap minimal `auth.users` only in that disposable test database and run the same SQLx migration directory embedded by the server, including 0003. They create random synthetic accounts and test apply atomicity, competing writes/applies, pending/missing accounts, conservative rollback, and audit status. Never point them at a real user database. Pure tests cover strict legacy parsing, lossless leaves, canonical hashes, missing confirmation/snapshot, duplicates, invalid counts/leaves/semesters, private reports, remote TLS policy, and default dry-run with deliberately remote environment URLs and no app configuration.

Public integration API:

- `validate(source_bytes, mapping_bytes) -> Validation`: offline; `report: Report`, `prepared: Option<PreparedBatch>`.
- `PreparedBatch::{digest,len,is_empty}`; batch is serializable for explicit bundles, but not deserializable/publicly constructible.
- `canonical_plan_sha256(&Plan) -> Result<String>` for semantic reconciliation; `plan_storage_sha256(&serde_json::Value) -> Result<String>` for lossless rollback; `sha256(&[u8]) -> String`, `REPORT_VERSION`.
- `storage::connection_options(url, allow_remote)`, `storage::connect(url, allow_remote)` enforce CLI connection policy. Library callers supplying their own `PgPool` must enforce equivalent destination/TLS restrictions.
- `storage::apply(&PgPool, &PreparedBatch, confirm_digest) -> Result<Uuid>`.
- `storage::rollback(&PgPool, batch_id, confirm_digest) -> Result<()>`.
- `Error` displays a fixed safe code. Do not replace it with raw SQLx/serde/domain debug output in wrappers.
