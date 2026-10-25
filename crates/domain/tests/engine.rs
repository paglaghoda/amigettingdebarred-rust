//! Independent contract tests: expected counts come from explicit fixtures or
//! integer arithmetic, never from another engine projection.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use debarred_domain::{
    calendar::{Checkpoint, GridDay},
    AttendanceSnapshot, Calendar, CheckpointProjection, Course, Engine, Error, LeaveEntry, Plan,
    SlotIndex, Timetable,
};
use proptest::prelude::*;

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn calendar() -> Calendar {
    let mut config = Calendar::winter_2026().unwrap().config().clone();
    config.id = "independent-test-week".into();
    config.start_date = date("2026-11-16");
    config.end_date = date("2026-11-23");
    config.last_instructional_day = date("2026-11-20");
    config.minimum_percentage = 75;
    config.holidays.clear();
    config.day_notes.clear();
    config.working_saturdays.clear();
    config.exam_windows.clear();
    config.checkpoints = vec![Checkpoint {
        id: "cat1".into(),
        label: "Test checkpoint".into(),
        start_date: date("2026-11-23"),
    }];
    Calendar::new(config).unwrap()
}

fn row(codes: &[Option<&str>]) -> [Option<String>; 7] {
    assert!(codes.len() <= 7);
    std::array::from_fn(|i| codes.get(i).copied().flatten().map(str::to_owned))
}

fn plan(calendar: &Calendar, as_of: &str) -> Plan {
    Plan {
        schema_version: 1,
        semester_id: calendar.config().id.clone(),
        calendar_revision: calendar.config().revision,
        timetable: Timetable {
            courses: vec![
                Course {
                    code: "A".into(),
                    name: "Algebra".into(),
                },
                Course {
                    code: "B".into(),
                    name: "Biology".into(),
                },
                Course {
                    code: "EMPTY".into(),
                    name: "No meetings".into(),
                },
            ],
            grid: BTreeMap::from([
                (GridDay::Mon, row(&[Some("A"), Some("A"), Some("B")])),
                (GridDay::Tue, row(&[Some("A"), Some("B")])),
                (GridDay::Wed, row(&[Some("A")])),
                (GridDay::Thu, row(&[Some("A")])),
                (GridDay::Fri, row(&[Some("A"), Some("B")])),
                (GridDay::Sat, row(&[Some("B"); 7])),
            ]),
        },
        attendance: AttendanceSnapshot {
            as_of: date(as_of),
            attended: BTreeMap::new(),
        },
        leave: vec![],
    }
}

fn day(s: &str) -> LeaveEntry {
    LeaveEntry::Day { date: date(s) }
}

fn slot(s: &str, index: u8) -> LeaveEntry {
    LeaveEntry::Slot {
        date: date(s),
        slot: SlotIndex::new(index).unwrap(),
    }
}

#[test]
fn repeated_slots_and_end_of_day_snapshot_are_counted_individually() {
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-16");
    plan.attendance.attended.insert("A".into(), 1);
    plan.attendance.attended.insert("B".into(), 1);
    let engine = Engine::new(&calendar, &plan).unwrap();
    assert_eq!(engine.held_so_far("A").unwrap(), 2);
    let meetings = engine.meetings_on(date("2026-11-16"));
    assert_eq!(meetings.len(), 3);
    assert_eq!(meetings[0].slot, SlotIndex::new(0).unwrap());
    assert_eq!(meetings[1].slot, SlotIndex::new(1).unwrap());
    assert_eq!(meetings[0].course_code, "A");
    assert_eq!(meetings[1].course_code, "A");
    let p = engine.project_course("A", "cat1").unwrap();
    assert_eq!((p.held_so_far, p.attended_so_far), (2, 1));
    assert_eq!((p.future_held, p.future_skipped), (4, 0));
    assert_eq!((p.total_held, p.total_attended), (6, 5));
    assert_eq!(p.percentage, 83.3);
    assert!(p.safe && p.has_classes);
}

#[test]
fn exactly_seventy_five_is_safe_but_one_less_is_not() {
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-15");
    plan.timetable.grid = BTreeMap::from([(GridDay::Mon, row(&[Some("A"); 4]))]);
    plan.leave = vec![slot("2026-11-16", 0)];
    let p = Engine::new(&calendar, &plan)
        .unwrap()
        .project_course("A", "cat1")
        .unwrap();
    assert_eq!((p.total_held, p.total_attended, p.percentage), (4, 3, 75.0));
    assert!(p.safe);
    plan.leave.push(slot("2026-11-16", 1));
    let p = Engine::new(&calendar, &plan)
        .unwrap()
        .project_course("A", "cat1")
        .unwrap();
    assert_eq!((p.total_attended, p.percentage), (2, 50.0));
    assert!(!p.safe);
}

#[test]
fn percentages_round_to_one_decimal_without_changing_safety() {
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-15");
    plan.timetable.grid = BTreeMap::from([(GridDay::Mon, row(&[Some("A"); 3]))]);
    plan.leave = vec![slot("2026-11-16", 0)];
    let p = Engine::new(&calendar, &plan)
        .unwrap()
        .project_course("A", "cat1")
        .unwrap();
    assert_eq!((p.total_held, p.total_attended, p.percentage), (3, 2, 66.7));
    assert!(!p.safe);
}

#[test]
fn rounded_seventy_five_does_not_make_an_under_threshold_ratio_safe() {
    let mut config = calendar().config().clone();
    config.start_date = date("2026-01-01");
    config.end_date = date("2027-12-31");
    config.last_instructional_day = date("2027-12-30");
    config.checkpoints[0].start_date = date("2027-12-31");
    let calendar = Calendar::new(config).unwrap();
    let mut plan = plan(&calendar, "2027-12-30");
    plan.timetable.grid = [
        GridDay::Mon,
        GridDay::Tue,
        GridDay::Wed,
        GridDay::Thu,
        GridDay::Fri,
    ]
    .into_iter()
    .map(|d| (d, row(&[Some("A"); 7])))
    .collect();
    // Jan 1, 2026 through Dec 30, 2027: 104 weeks plus one Thursday.
    // 521 instructional weekdays * 7 = 3647; 2735/3647 rounds to 75.0.
    plan.attendance.attended.insert("A".into(), 2735);
    let engine = Engine::new(&calendar, &plan).unwrap();
    let p = engine.project_course("A", "cat1").unwrap();
    assert_eq!(
        (p.total_held, p.total_attended, p.percentage),
        (3647, 2735, 75.0)
    );
    assert!(!p.safe);
    assert_eq!(engine.skip_allowance("A", "cat1").unwrap().shortfall, 1);
}

#[test]
fn empty_course_and_snapshot_before_term() {
    let calendar = calendar();
    let plan = plan(&calendar, "2026-11-01");
    let engine = Engine::new(&calendar, &plan).unwrap();
    let p = engine.project_course("A", "cat1").unwrap();
    assert_eq!((p.held_so_far, p.attended_so_far, p.future_held), (0, 0, 6));
    assert_eq!((p.total_attended, p.percentage), (6, 100.0));
    let p = engine.project_course("EMPTY", "cat1").unwrap();
    assert_eq!(
        (p.total_held, p.total_attended, p.percentage),
        (0, 0, 100.0)
    );
    assert!(p.safe);
    assert!(!p.has_classes);
    let a = engine.skip_allowance("EMPTY", "cat1").unwrap();
    assert_eq!(
        (
            a.total_future_allowance,
            a.already_planned,
            a.additional,
            a.shortfall
        ),
        (0, 0, 0, 0)
    );
    assert!(engine.meetings_on(date("2026-11-15")).is_empty());
    assert!(engine.meetings_on(date("2026-11-21")).is_empty());
    assert!(engine.meetings_on(date("2026-11-22")).is_empty());
    assert!(engine.meetings_on(date("2026-11-24")).is_empty());
}

#[test]
fn full_day_dominates_partial_leave_and_order_and_duplicates_do_not_matter() {
    let calendar = calendar();
    let base = plan(&calendar, "2026-11-15");
    let entries = [
        slot("2026-11-16", 0),
        day("2026-11-16"),
        slot("2026-11-16", 1),
    ];
    for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2], [0, 2, 1]] {
        let mut plan = base.clone();
        plan.leave = order
            .iter()
            .flat_map(|&i| [entries[i], entries[i]])
            .collect();
        let engine = Engine::new(&calendar, &plan).unwrap();
        assert_eq!(
            engine.project_course("A", "cat1").unwrap().future_skipped,
            2
        );
        assert_eq!(
            engine.project_course("B", "cat1").unwrap().future_skipped,
            1
        );
        let summary = engine.leave_summary();
        assert_eq!((summary.days, summary.classes), (1, 3));
    }
    let mut partial = base;
    partial.leave = vec![slot("2026-11-16", 0), slot("2026-11-16", 0)];
    let engine = Engine::new(&calendar, &partial).unwrap();
    assert_eq!(
        engine.project_course("A", "cat1").unwrap().future_skipped,
        1
    );
    assert_eq!(
        engine.project_course("B", "cat1").unwrap().future_skipped,
        0
    );
    let summary = engine.leave_summary();
    assert_eq!((summary.days, summary.classes), (1, 1));
}

#[test]
fn past_and_snapshot_day_leaves_never_subtract_recorded_attendance() {
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-17");
    plan.attendance.attended.insert("A".into(), 2);
    plan.attendance.attended.insert("B".into(), 1);
    plan.leave = vec![
        day("2026-11-16"),
        day("2026-11-17"),
        slot("2026-11-17", 0),
        day("2026-11-20"),
    ];
    let engine = Engine::new(&calendar, &plan).unwrap();
    let p = engine.project_course("A", "cat1").unwrap();
    assert_eq!((p.held_so_far, p.attended_so_far), (3, 2));
    assert_eq!(
        (p.future_held, p.future_skipped, p.total_attended),
        (3, 1, 4)
    );
    let summary = engine.leave_summary();
    assert_eq!((summary.days, summary.classes), (1, 2));
}

#[test]
fn checkpoint_day_is_excluded_even_when_it_has_instruction() {
    let mut config = calendar().config().clone();
    config.last_instructional_day = date("2026-11-23");
    let calendar = Calendar::new(config).unwrap();
    let mut plan = plan(&calendar, "2026-11-15");
    plan.leave = vec![day("2026-11-23")];
    let engine = Engine::new(&calendar, &plan).unwrap();
    assert_eq!(engine.meetings_on(date("2026-11-23")).len(), 3);
    let p = engine.project_course("A", "cat1").unwrap();
    assert_eq!((p.future_held, p.future_skipped, p.total_held), (6, 0, 6));
    let allowance = engine.skip_allowance("A", "cat1").unwrap();
    assert_eq!(allowance.already_planned, 0);
    assert_eq!(engine.leave_summary().classes, 3);
}

#[test]
fn historical_checkpoint_on_cutoff_and_after_term_cannot_be_reconstructed() {
    let calendar = calendar();
    for cutoff in ["2026-11-23", "2027-01-01"] {
        let mut plan = plan(&calendar, cutoff);
        plan.attendance.attended.insert("A".into(), 6);
        let engine = Engine::new(&calendar, &plan).unwrap();
        assert_eq!(engine.held_so_far("A").unwrap(), 6);
        assert!(matches!(
            engine.project_course("A", "cat1"),
            Err(Error::HistoricalCheckpoint(_))
        ));
        assert!(matches!(
            engine.skip_allowance("A", "cat1"),
            Err(Error::HistoricalCheckpoint(_))
        ));
        let all = engine.project_all();
        assert_eq!(all.len(), 1);
        assert!(
            matches!(&all[0], CheckpointProjection::Historical { checkpoint } if checkpoint.id == "cat1")
        );
    }
}

#[test]
fn project_all_marks_mixed_history_and_upcoming_without_dropping_courses() {
    let mut config = calendar().config().clone();
    config.checkpoints.insert(
        0,
        Checkpoint {
            id: "early".into(),
            label: "Early".into(),
            start_date: date("2026-11-17"),
        },
    );
    let calendar = Calendar::new(config).unwrap();
    let plan = plan(&calendar, "2026-11-17");
    let engine = Engine::new(&calendar, &plan).unwrap();
    let all = engine.project_all();
    assert_eq!(all.len(), 2);
    assert!(
        matches!(&all[0], CheckpointProjection::Historical { checkpoint } if checkpoint.id == "early")
    );
    match &all[1] {
        CheckpointProjection::Upcoming {
            checkpoint,
            courses,
        } => {
            assert_eq!(checkpoint.id, "cat1");
            assert_eq!(courses.len(), 3);
        }
        _ => panic!("cat1 must still be upcoming"),
    }
}

#[test]
fn skip_allowance_accounts_for_existing_absence_plans_and_shortfall() {
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-16");
    plan.attendance.attended.insert("A".into(), 2);
    // Six total meetings require five attended. Only one future skip is safe.
    for (leaves, expected) in [
        (vec![], (1, 0, 1, 0)),
        (vec![day("2026-11-17")], (1, 1, 0, 0)),
        (vec![day("2026-11-17"), day("2026-11-18")], (1, 2, 0, 1)),
    ] {
        plan.leave = leaves;
        let a = Engine::new(&calendar, &plan)
            .unwrap()
            .skip_allowance("A", "cat1")
            .unwrap();
        assert_eq!(
            (
                a.total_future_allowance,
                a.already_planned,
                a.additional,
                a.shortfall
            ),
            expected
        );
    }
    plan.attendance.attended.insert("A".into(), 0);
    plan.leave.clear();
    let a = Engine::new(&calendar, &plan)
        .unwrap()
        .skip_allowance("A", "cat1")
        .unwrap();
    assert_eq!(
        (a.total_future_allowance, a.additional, a.shortfall),
        (0, 0, 1)
    );
}

#[test]
fn invalid_plans_and_unknown_queries_are_rejected() {
    let calendar = calendar();
    let base = plan(&calendar, "2026-11-16");
    let mut invalid = vec![];
    let mut p = base.clone();
    p.semester_id = "not-a-semester".into();
    invalid.push(p);
    let mut p = base.clone();
    p.calendar_revision += 1;
    invalid.push(p);
    let mut p = base.clone();
    p.schema_version = 2;
    invalid.push(p);
    let mut p = base.clone();
    p.attendance.attended.insert("A".into(), 3);
    invalid.push(p);
    let mut p = base.clone();
    p.attendance.attended.insert("UNKNOWN".into(), 0);
    invalid.push(p);
    let mut p = base.clone();
    p.timetable.grid.get_mut(&GridDay::Mon).unwrap()[0] = Some("UNKNOWN".into());
    invalid.push(p);
    let mut p = base.clone();
    p.timetable.courses.push(p.timetable.courses[0].clone());
    invalid.push(p);
    for leave in [
        day("2026-11-15"),
        day("2026-11-24"),
        slot("2026-11-21", 0),
        slot("2026-11-22", 0),
        slot("2026-11-18", 6),
    ] {
        let mut p = base.clone();
        p.leave.push(leave);
        invalid.push(p);
    }
    for (i, p) in invalid.iter().enumerate() {
        assert!(
            Engine::new(&calendar, p).is_err(),
            "invalid plan case {i} was accepted"
        );
    }
    let engine = Engine::new(&calendar, &base).unwrap();
    assert!(matches!(
        engine.held_so_far("UNKNOWN"),
        Err(Error::UnknownCourse(_))
    ));
    assert!(matches!(
        engine.project_course("UNKNOWN", "cat1"),
        Err(Error::UnknownCourse(_))
    ));
    assert!(matches!(
        engine.project_course("A", "unknown"),
        Err(Error::UnknownCheckpoint(_))
    ));
    assert!(engine.skip_allowance("UNKNOWN", "cat1").is_err());
    assert!(engine.skip_allowance("A", "unknown").is_err());
    assert!(SlotIndex::new(7).is_err());
    assert!(SlotIndex::new(u8::MAX).is_err());
}

#[test]
fn legacy_leave_and_plan_serde_round_trip_and_malformed_input() {
    for text in ["2026-11-16", "2026-11-16#0", "2026-11-16#6"] {
        let entry = LeaveEntry::from_legacy(text).unwrap();
        assert_eq!(entry.to_legacy(), text);
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: LeaveEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.to_legacy(), text);
    }
    for text in [
        "",
        "2026-02-30",
        "2026-11-16#7",
        "2026-11-16#-1",
        "2026-11-16#",
        "2026-11-16#1#2",
        "not-a-date",
    ] {
        assert!(LeaveEntry::from_legacy(text).is_err(), "accepted {text:?}");
    }
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-16");
    plan.attendance.attended.insert("A".into(), 2);
    plan.leave = vec![day("2026-11-17"), slot("2026-11-20", 0)];
    let encoded = serde_json::to_value(&plan).unwrap();
    let decoded: Plan = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
    assert!(Engine::new(&calendar, &decoded).is_ok());
    for json in ["", "{", "null", "[]", "{}", "{\"schema_version\":1,}"] {
        assert!(serde_json::from_str::<Plan>(json).is_err());
    }
    let mut negative = encoded.clone();
    negative["attendance"]["attended"]["A"] = serde_json::json!(-1);
    assert!(serde_json::from_value::<Plan>(negative).is_err());
    let mut overflow = encoded.clone();
    overflow["attendance"]["attended"]["A"] = serde_json::json!(4294967296_u64);
    assert!(serde_json::from_value::<Plan>(overflow).is_err());
    let mut invalid_slot = encoded.clone();
    invalid_slot["leave"][1]["slot"] = serde_json::json!(7);
    assert!(serde_json::from_value::<Plan>(invalid_slot).is_err());
    let mut bad_date = encoded.clone();
    bad_date["attendance"]["as_of"] = serde_json::json!("2026-02-30");
    assert!(serde_json::from_value::<Plan>(bad_date).is_err());
    let mut short_row = encoded;
    short_row["timetable"]["grid"]["Mon"] = serde_json::json!(["A"]);
    assert!(serde_json::from_value::<Plan>(short_row).is_err());
}

#[test]
fn winter_fixture_matches_published_weekday_counts_and_saturday_substitution() {
    let calendar = Calendar::winter_2026().unwrap();
    let mut plan = plan(&calendar, "2026-11-15");
    let weekdays = [
        GridDay::Mon,
        GridDay::Tue,
        GridDay::Wed,
        GridDay::Thu,
        GridDay::Fri,
    ];
    plan.timetable.courses = ["MON", "TUE", "WED", "THU", "FRI", "SAT"]
        .map(|code| Course {
            code: code.into(),
            name: code.into(),
        })
        .to_vec();
    plan.timetable.grid = weekdays
        .into_iter()
        .zip(["MON", "TUE", "WED", "THU", "FRI"])
        .map(|(day, code)| (day, row(&[Some(code)])))
        .collect();
    plan.timetable
        .grid
        .insert(GridDay::Sat, row(&[Some("SAT"); 7]));
    let engine = Engine::new(&calendar, &plan).unwrap();
    // Published TS dates, counted by weekday with holidays and exam windows
    // removed; Nov 21 and Dec 12 add Friday meetings, not Saturday meetings.
    for (checkpoint, expected) in [
        ("cat1", [5, 4, 4, 4, 6]),
        ("cat2", [10, 8, 9, 10, 9]),
        ("fat", [14, 12, 12, 12, 11]),
    ] {
        for (code, count) in ["MON", "TUE", "WED", "THU", "FRI"]
            .into_iter()
            .zip(expected)
        {
            let p = engine.project_course(code, checkpoint).unwrap();
            assert_eq!(
                (p.total_held, p.total_attended),
                (count, count),
                "{code}/{checkpoint}"
            );
        }
        assert_eq!(
            engine.project_course("SAT", checkpoint).unwrap().total_held,
            0
        );
    }
    for s in ["2026-11-21", "2026-12-12"] {
        let meetings = engine.meetings_on(date(s));
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].course_code, "FRI");
    }
    for s in [
        "2026-12-25",
        "2027-01-01",
        "2027-01-11",
        "2027-01-15",
        "2027-01-26",
        "2027-02-18",
        "2027-02-19",
        "2027-02-20",
        "2027-03-06",
        "2026-12-15",
        "2026-12-23",
        "2027-02-05",
        "2027-02-13",
        "2027-03-10",
        "2027-03-11",
        "2027-03-31",
    ] {
        assert!(
            engine.meetings_on(date(s)).is_empty(),
            "unexpected meetings on {s}"
        );
    }
    for leave in [slot("2026-12-25", 0), slot("2026-12-15", 0)] {
        let mut invalid = plan.clone();
        invalid.leave = vec![leave];
        assert!(Engine::new(&calendar, &invalid).is_err());
    }
    // Notes alone must not remove otherwise instructional dates.
    assert_eq!(engine.meetings_on(date("2026-11-24"))[0].course_code, "TUE");
    assert_eq!(engine.meetings_on(date("2027-03-05"))[0].course_code, "FRI");
}

#[test]
fn three_meetings_at_sixty_seven_percent_require_all_three() {
    let calendar = threshold_calendar(67);
    // One meeting per date allows every split between recorded and future attendance.
    for completed in 0..=3_u32 {
        for attended in 0..=completed {
            for skipped in 0..=3 - completed {
                let mut plan = plan(&calendar, "2026-11-15");
                plan.timetable.grid = [GridDay::Mon, GridDay::Tue, GridDay::Wed]
                    .into_iter()
                    .map(|d| (d, row(&[Some("A")])))
                    .collect();
                plan.attendance.as_of =
                    date("2026-11-15") + chrono::Duration::days(i64::from(completed));
                plan.attendance.attended.insert("A".into(), attended);
                for i in completed..completed + skipped {
                    plan.leave.push(LeaveEntry::Day {
                        date: date("2026-11-16") + chrono::Duration::days(i64::from(i)),
                    });
                }
                let engine = Engine::new(&calendar, &plan).unwrap();
                let p = engine.project_course("A", "cat1").unwrap();
                let projected = attended + 3 - completed - skipped;
                assert_eq!((p.total_held, p.total_attended), (3, projected));
                assert_eq!(p.safe, projected == 3);
                if projected == 2 {
                    assert_eq!(p.percentage, 66.7);
                    assert!(!p.safe);
                }
                let allowance = engine.skip_allowance("A", "cat1").unwrap();
                assert_eq!(allowance.total_future_allowance, 0);
                assert_eq!(allowance.already_planned, skipped);
                assert_eq!(allowance.additional, 0);
                assert_eq!(allowance.shortfall, 3 - projected);
                // Even when attendance is perfect so far, one more skip fails.
                if p.safe && completed < 3 {
                    assert!((projected - 1) * 100 < 3 * 67);
                }
            }
        }
    }
}

#[test]
fn classes_held_between_is_half_open_and_independent_of_snapshot_and_leave() {
    let calendar = calendar();
    let mut plan = plan(&calendar, "2026-11-16");
    plan.leave = vec![day("2026-11-16"), day("2026-11-20")];
    let engine = Engine::new(&calendar, &plan).unwrap();
    for (from, to, expected) in [
        ("2026-11-16", "2026-11-17", 2),
        ("2026-11-17", "2026-11-18", 1),
        ("2026-11-16", "2026-11-20", 5),
        ("2026-11-20", "2026-11-21", 1),
        ("2026-11-16", "2026-11-24", 6),
        ("2026-11-16", "2026-11-16", 0),
        ("2026-11-18", "2026-11-18", 0),
        ("2026-11-01", "2026-11-01", 0),
        ("2026-12-01", "2026-12-01", 0),
        ("2026-11-01", "2026-11-16", 0),
        ("2026-11-24", "2026-12-01", 0),
        ("2026-11-01", "2026-11-17", 2),
        ("2026-11-20", "2026-12-01", 1),
        ("2026-11-01", "2026-12-01", 6),
        ("2026-11-21", "2026-11-24", 0),
    ] {
        assert_eq!(
            engine
                .classes_held_between("A", date(from), date(to))
                .unwrap(),
            expected,
            "range [{from}, {to})"
        );
    }
    assert_eq!(
        engine
            .classes_held_between("B", date("2026-11-16"), date("2026-11-24"))
            .unwrap(),
        3
    );
    assert_eq!(
        engine
            .classes_held_between("EMPTY", date("2026-11-01"), date("2026-12-01"))
            .unwrap(),
        0
    );
    for (from, to) in [
        ("2026-11-20", "2026-11-16"),
        ("2026-11-02", "2026-11-01"),
        ("2026-12-02", "2026-12-01"),
    ] {
        assert!(matches!(
            engine.classes_held_between("A", date(from), date(to)),
            Err(Error::Invalid(_))
        ));
    }
    for (from, to) in [
        ("2026-11-16", "2026-11-20"),
        ("2026-11-16", "2026-11-16"),
        ("2026-12-01", "2026-12-02"),
    ] {
        assert!(matches!(
            engine.classes_held_between("UNKNOWN", date(from), date(to)),
            Err(Error::UnknownCourse(_))
        ));
    }
}

#[test]
fn held_breakdown_counts_distinct_dates_and_includes_snapshot_working_saturday() {
    let mut config = calendar().config().clone();
    config.last_instructional_day = date("2026-11-21");
    config
        .working_saturdays
        .insert(date("2026-11-21"), GridDay::Fri);
    let calendar = Calendar::new(config).unwrap();
    for (cutoff, a_expected, b_expected) in [
        ("2026-11-15", (0, 0, 0, 0), (0, 0, 0, 0)),
        ("2026-11-16", (2, 0, 0, 1), (1, 0, 0, 1)),
        ("2026-11-20", (7, 0, 0, 5), (3, 0, 0, 3)),
        ("2026-11-21", (9, 2, 1, 6), (4, 1, 1, 4)),
        ("2026-11-22", (9, 2, 1, 6), (4, 1, 1, 4)),
        ("2026-12-01", (9, 2, 1, 6), (4, 1, 1, 4)),
    ] {
        let mut plan = plan(&calendar, cutoff);
        // Friday and its substitute Saturday each have two A meetings;
        // the stored Saturday row has seven B meetings and must be ignored.
        plan.timetable
            .grid
            .insert(GridDay::Fri, row(&[Some("A"), Some("A"), Some("B")]));
        plan.leave = vec![day("2026-11-16"), day("2026-11-21")];
        let engine = Engine::new(&calendar, &plan).unwrap();
        for (code, expected) in [
            ("A", a_expected),
            ("B", b_expected),
            ("EMPTY", (0, 0, 0, 0)),
        ] {
            let breakdown = engine.held_breakdown(code).unwrap();
            assert_eq!(
                (
                    breakdown.total,
                    breakdown.saturday_classes,
                    breakdown.saturday_days,
                    breakdown.class_days
                ),
                expected,
                "{code} at {cutoff}"
            );
            assert_eq!(breakdown.total, engine.held_so_far(code).unwrap());
        }
        assert!(matches!(
            engine.held_breakdown("UNKNOWN"),
            Err(Error::UnknownCourse(_))
        ));
    }
}

#[test]
fn held_breakdown_counts_multiple_working_saturdays_as_distinct_dates() {
    let calendar = Calendar::winter_2026().unwrap();
    let mut plan = plan(&calendar, "2026-12-12");
    plan.timetable.grid = BTreeMap::from([
        (GridDay::Fri, row(&[Some("A"), Some("A")])),
        (GridDay::Sat, row(&[Some("B"); 7])),
    ]);
    let engine = Engine::new(&calendar, &plan).unwrap();
    let breakdown = engine.held_breakdown("A").unwrap();
    // Four normal Fridays plus the Nov 21 and Dec 12 Friday substitutes.
    assert_eq!(
        (
            breakdown.total,
            breakdown.saturday_classes,
            breakdown.saturday_days,
            breakdown.class_days
        ),
        (12, 4, 2, 6)
    );
    assert_eq!(breakdown.total, engine.held_so_far("A").unwrap());
    let absent = engine.held_breakdown("B").unwrap();
    assert_eq!(
        (
            absent.total,
            absent.saturday_classes,
            absent.saturday_days,
            absent.class_days
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(absent.total, engine.held_so_far("B").unwrap());
    assert_eq!(
        engine
            .classes_held_between("A", date("2026-11-21"), date("2026-11-22"))
            .unwrap(),
        2
    );
}

// Ten meetings, two per weekday. Cutoff is a count of completed weekdays.
fn small_plan(calendar: &Calendar, completed: u32, attended: u32, mask: u16) -> Plan {
    let mut plan = plan(calendar, "2026-11-15");
    plan.attendance.as_of = date("2026-11-15") + chrono::Duration::days(i64::from(completed));
    plan.attendance.attended.insert("A".into(), attended);
    plan.timetable.grid = [
        GridDay::Mon,
        GridDay::Tue,
        GridDay::Wed,
        GridDay::Thu,
        GridDay::Fri,
    ]
    .into_iter()
    .map(|d| (d, row(&[Some("A"), Some("A")])))
    .collect();
    for i in 0..10_u32 {
        if mask & (1 << i) != 0 {
            plan.leave.push(LeaveEntry::Slot {
                date: date("2026-11-16") + chrono::Duration::days(i64::from(i / 2)),
                slot: SlotIndex::new((i % 2) as u8).unwrap(),
            });
        }
    }
    plan
}

fn threshold_calendar(threshold: u8) -> Calendar {
    let mut config = calendar().config().clone();
    config.minimum_percentage = threshold;
    Calendar::new(config).unwrap()
}

fn check_small_oracle(completed: u32, attended: u32, mask: u16, threshold: u8) {
    let calendar = threshold_calendar(threshold);
    let plan = small_plan(&calendar, completed, attended, mask);
    let engine = Engine::new(&calendar, &plan).unwrap();
    let held = completed * 2;
    let future = 10 - held;
    let skipped = (held..10).filter(|i| mask & (1 << i) != 0).count() as u32;
    let projected = attended + future - skipped;
    let p = engine.project_course("A", "cat1").unwrap();
    assert_eq!((p.held_so_far, p.attended_so_far), (held, attended));
    assert_eq!((p.future_held, p.future_skipped), (future, skipped));
    assert_eq!((p.total_held, p.total_attended), (10, projected));
    assert_eq!(p.percentage, f64::from(projected) * 10.0);
    let target = 10 * u32::from(threshold);
    let required = (0..=10).find(|n| n * 100 >= target).unwrap();
    assert_eq!(p.safe, projected * 100 >= target);
    let a = engine.skip_allowance("A", "cat1").unwrap();
    // Enumerate possibilities instead of reproducing the allowance formula.
    let total_allowance = (0..=future)
        .filter(|s| (attended + future - s) * 100 >= target)
        .max()
        .unwrap_or(0);
    let additional = (0..=future - skipped)
        .filter(|s| (projected - s) * 100 >= target)
        .max()
        .unwrap_or(0);
    assert_eq!(a.total_future_allowance, total_allowance);
    assert_eq!(a.already_planned, skipped);
    assert_eq!(a.additional, additional);
    assert_eq!(a.shortfall, required.saturating_sub(projected));
    if (attended + future) * 100 >= target {
        assert!((attended + future - a.total_future_allowance) * 100 >= target);
        if a.total_future_allowance < future {
            assert!((attended + future - a.total_future_allowance - 1) * 100 < target);
        }
    }
    if p.safe {
        assert!((projected - a.additional) * 100 >= target);
        if a.additional < future - skipped {
            assert!((projected - a.additional - 1) * 100 < target);
        }
    } else {
        assert_eq!(a.additional, 0);
    }
}

#[test]
fn exhaustive_small_attendance_and_future_skip_counts_match_integer_oracle() {
    for threshold in [1, 67, 75, 100] {
        for completed in 0..=5 {
            let held = completed * 2;
            for attended in 0..=held {
                for skipped in 0..=10 - held {
                    let mask = (((1_u32 << skipped) - 1) << held) as u16;
                    check_small_oracle(completed, attended, mask, threshold);
                }
            }
        }
    }
}

proptest! {
    #[test]
    fn arbitrary_leave_subsets_match_oracle_and_safety_is_monotone(
        completed in 0_u32..=5,
        attendance_seed in 0_u32..=10,
        mask in 0_u16..1024,
        threshold in prop::sample::select(vec![1_u8, 67, 75, 100]),
    ) {
        let attended = attendance_seed % (completed * 2 + 1);
        check_small_oracle(completed, attended, mask, threshold);
        let calendar = threshold_calendar(threshold);
        let plan = small_plan(&calendar, completed, attended, mask);
        let engine = Engine::new(&calendar, &plan).unwrap();
        let before = engine.project_course("A", "cat1").unwrap();
        if attended < completed * 2 {
            let more_attendance = small_plan(&calendar, completed, attended + 1, mask);
            let improved = Engine::new(&calendar, &more_attendance).unwrap().project_course("A", "cat1").unwrap();
            prop_assert!(improved.percentage >= before.percentage);
            prop_assert!(!before.safe || improved.safe);
        }
        for i in completed * 2..10 {
            let more_leave = small_plan(&calendar, completed, attended, mask | (1 << i));
            let reduced = Engine::new(&calendar, &more_leave).unwrap().project_course("A", "cat1").unwrap();
            prop_assert!(reduced.percentage <= before.percentage);
            prop_assert!(!reduced.safe || before.safe);
        }
    }
}
