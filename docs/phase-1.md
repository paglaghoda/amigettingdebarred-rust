# Phase 1: rules, contracts, and handoff

## Scope and architecture

The Cargo workspace currently contains only `debarred-domain`. Axum/SQLx/auth
and Leptos should be added as separate crates in Phases 2 and 3; Phase 1 does not
pretend to implement those services. The original TypeScript application is
untouched.

The domain library performs no I/O, reads no environment variables, calls no
system clock, and makes no auth or AI calls. Its runtime dependencies are
`chrono` (without clock/default features), `serde`, `serde_json`, and `thiserror`.
`proptest` is a native-only test dependency. The Node file only runs a compiled
WASM test harness and is not a production requirement.

`Calendar::new` validates and expands one calendar. `Engine::new` validates one
immutable plan and canonicalizes its leave selections. It builds meeting lists
and per-course held/skipped prefix counts once. Repeated checkpoint projections
use those counts instead of reparsing leave or scanning the entire semester for
each result. Do not cache an engine across changes to its plan or calendar.

## Calendar provenance and versioning

`crates/domain/data/winter-2026.v1.json` transcribes the checked-in source at
`amigettingdebarred-main/src/lib/academic-calendar.ts`; this is **not** an
independent verification of the university's published calendar.

- Semester ID: `winter-2026`, all batches.
- Term: November 16, 2026–March 31, 2027, inclusive (136 dates).
- Last instructional day: March 9, 2027.
- CAT-1: December 15–23; CAT-2: February 5–13; finals: March 11–31.
- Working Saturdays: November 21 and December 12, both following Friday.
- All twelve holiday entries and four informational day notes are preserved.
- The sectional holiday note does not automatically cancel classes for everyone.
- Slot IDs/times and all 42 FFCS cell codes are preserved in `slots.rs`.

`schema_version` identifies the JSON shape; `revision` identifies calendar
content. Plans pin `calendar_revision`. Changing calendar content must create a
new revision and trigger an explicit plan reconciliation; it must not silently
reinterpret attendance. `load_semester` rejects unsupported IDs, including old
`fall-2026` values. No historical calendar was invented.

Configuration validation checks schema/revision, a 1–730 day term, threshold
1–100%, in-term holidays/notes, valid working Saturdays, ordered unique
checkpoints, valid nonoverlapping exam windows, and exam/checkpoint consistency.
A linked checkpoint starts on its earliest exam-window date; checkpoints without
exam windows remain supported for other assessment rules.

### Calendar precedence

1. Explicit holiday: no meetings, even during an exam window.
2. Exam window: no meetings; Sundays remain noninstructional.
3. Working Saturday: use its configured Monday–Friday timetable.
4. Other weekends: no meetings.
5. Weekdays through the last instructional date: normal timetable.
6. Later dates: no meetings.

The Saturday timetable row can be stored/displayed but is not counted directly.
Every occupied slot counts as one meeting, even if a course repeats that day.
Lunch is not a slot. Partial-day suspensions are not inferred from free-text
notes; supporting them later requires explicit structured calendar data.

## Attendance and time

`attendance.as_of` means **through the end of this campus date**, inclusively.
The supplied per-course counts must include exactly that completed period.
Missing registered course counts mean zero. Counts above meetings held, unknown
courses, negative JSON values, and fractional JSON counts are rejected.

Time helpers accept an injected UTC instant and derive modern India Standard
Time (`Asia/Kolkata`, UTC+05:30). The shared engine never chooses its own "today".
`last_completed_campus_date` supplies yesterday in campus time for a UI's default
snapshot cutoff. Range overflow returns an error, not a panic.

The snapshot date can be before the term (zero held classes) or after it. It is
not clamped to the first instructional date. Nothing automatically advances a
snapshot at midnight: **the caller must update the date and counts together**.
A stale snapshot remains a forecast from that date, not current factual
attendance. In a future UI, show the snapshot date prominently and offer an
explicit refresh/update flow.

Meetings after `as_of` are forecast, not observed. All are assumed attended
unless leave is selected. The engine does not infer past attendance from leave,
and does not claim that future classes have already been attended.

## Leave

The v1 JSON contract uses tagged entries:

```json
[
  { "kind": "day", "date": "2026-11-21" },
  { "kind": "slot", "date": "2026-12-12", "slot": 3 }
]
```

Slots are zero-based integers 0–6. `SlotIndex` prevents out-of-range construction.
A selected date must have a meeting in this timetable, and an individual slot
must be occupied. Out-of-term dates, holidays, exams, free days, and free slots
are rejected rather than silently ignored.

Selections form a set of actual meetings. Duplicates and ordering do not matter;
whole-day leave includes all occupied slots and dominates partial selections.
A past or snapshot-date selection may remain stored but cannot alter recorded
attendance. `leave_summary` counts only leave strictly after the snapshot date.

`LeaveEntry::from_legacy` and `to_legacy` explicitly handle the old date / date#slot
encoding. This is only a syntax adapter; `Engine::new` still validates the date
and meeting. Migrating a whole legacy plan requires more than parsing strings.

## Projection arithmetic

Only checkpoints whose start date is strictly after the snapshot are projected.
For one course and one upcoming checkpoint:

```text
held_so_far       = meetings on or before as_of
future_held       = meetings after as_of and before checkpoint.start_date
future_skipped    = selected meetings in that same future interval
total_held        = held_so_far + future_held
total_attended    = snapshot.attended + future_held - future_skipped
minimum_required  = ceil(total_held * minimum_percentage / 100)
safe              = total_attended >= minimum_required
```

The ceiling uses widened integer arithmetic. Display percentage is rounded to
one decimal but never used in the safety comparison: a result displaying 75.0%
can still be below 75% before rounding. Zero total meetings retain the old 100%
convention and `safe=true`, but expose `has_classes=false`; the UI must say "no
classes" rather than present an affirmative eligibility verdict.

`project_course` returns `HistoricalCheckpoint` when the checkpoint has started.
`project_all` returns a tagged `historical` record with no fabricated course
results. Aggregate attendance cannot reconstruct historical eligibility. Daily
attendance events or saved checkpoint snapshots would be needed to add that
feature truthfully.

### Skip allowance

```text
baseline_attended      = snapshot.attended + future_held
 total_future_allowance = min(future_held,
                              max(baseline_attended - minimum_required, 0))
already_planned        = future_skipped
additional             = max(total_future_allowance - already_planned, 0)
shortfall              = max(minimum_required - total_attended, 0)
```

The allowance is a count of meetings, not whole days. A zero allowance does not
mean the student is safe; inspect `safe` and `shortfall`. `shortfall` is a deficit
against the checkpoint's scheduled denominator, not a claim that new meetings
can be added outside the calendar.

## Deliberate changes from the old app

| Old behaviour | New contract |
|---|---|
| Today's entire timetable is counted as held | Explicit completed-day snapshot; default helper chooses yesterday |
| Date clamped into the semester | Dates remain explicit; before-term held count is zero |
| Any semester ID resolves to winter | Unknown IDs fail |
| Attendance above held meetings is silently capped | Invalid counts fail validation |
| Today’s selected leave may cap already-entered attendance | Snapshot attendance is never implicitly altered |
| Past checkpoint uses today's aggregate counts | Historical eligibility unavailable |
| Maximum skips ignores existing leave | Separate total allowance, selected skips, additional allowance, shortfall |
| Invalid leave strings/slots can be ignored | Strict syntax and semantic validation |
| Held breakdown's class-day count includes unrelated courses | Counts only dates on which that course actually met |

Unambiguous existing behaviour is retained: repeated slots, calendar precedence,
working Saturday substitution, checkpoint-day exclusion, one-decimal display,
75% equality, and the zero-meeting display convention. The fixed winter fixtures
and manually derived counts cover this parity; ambiguous behaviour has explicit
new tests instead of blindly copying the TypeScript implementation.

## Validation and test boundaries

Native tests cover calendar validation/serde, end-of-day snapshots, thresholds
1/67/75/100, exact equality, rounding without false safety, repeated meetings,
whole/partial/duplicate leave, holidays/exams/Saturdays, pre/post-term dates,
historical checkpoints, skip allowance/shortfall, range counts, and held
breakdowns. Independent integer oracles exhaust small states and property-test
arbitrary leave subsets, monotonicity, and allowance maximality.

A versioned JSON plan fixture is used by a native integration test, the runnable
example, and the compiled WASM runtime smoke test. The entire native property
test suite is not executed inside WASM. CI builds the same domain library for
`wasm32-unknown-unknown` and runs its smoke harness in Node.

These tests establish rules and portability, not production throughput, page
speed, or official eligibility. No web app or database benchmarks exist yet.

## Phase 2 integration requirements

1. Authenticate with Supabase; derive user ownership on the server. Domain plans
   intentionally contain no trusted user identity.
2. Bound HTTP bodies before deserialization, then use `Engine::new` before
   accepting/persisting inputs. Domain collection bounds are not HTTP size limits.
3. Persist a plan revision for concurrency separately from plan schema version
   and calendar content revision. Those are three different concepts.
4. Store counts and their snapshot date atomically. Never advance only the date.
5. Inventory legacy identity tables, semester IDs, and invalid/stale selections.
   Request explicit user reconciliation where data cannot be interpreted safely;
   do not invent a snapshot date, change semesters, or silently discard data.
6. Verify the university calendar before production. Preserve old revisions when
   dates/rules change, and explain any resulting count changes to users.
7. Keep image parsing and optional AI outside this crate. Validate parsed data
   through the same types/engine, and never let AI perform eligibility arithmetic.
8. Retain forecast assumptions and the no-classes/historical states in the UI.
   Safety flags are projections, not official university decisions.
