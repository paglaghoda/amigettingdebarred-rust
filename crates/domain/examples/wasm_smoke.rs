//! Test-only WASM entry point, intentionally independent of any frontend bindings.
use debarred_domain::{load_semester, time, Engine, LeaveEntry, Plan};

/// Returns zero after the same fixture, leave semantics, and timezone checks
/// succeed in a WASM runtime. Any assertion failure traps the host invocation.
#[no_mangle]
pub extern "C" fn run_contract_checks() -> u32 {
    let mut plan = Plan::from_json(include_str!("../tests/fixtures/winter-plan.v1.json")).unwrap();
    let calendar = load_semester(&plan.semester_id).unwrap();
    {
        let engine = Engine::new(&calendar, &plan).unwrap();
        let projection = engine.project_course("CSE1001", "cat1").unwrap();
        assert_eq!((projection.total_held, projection.total_attended), (19, 18));
        assert_eq!(projection.percentage, 94.7);
        assert!(projection.safe);
        assert_eq!(
            engine.skip_allowance("CSE1001", "cat1").unwrap().additional,
            3
        );
        assert_eq!(engine.project_all().len(), 3);
        let encoded = serde_json::to_string(&engine.project_all()).unwrap();
        assert!(encoded.contains("upcoming"));
    }
    // A duplicated selection must not count twice in the browser either.
    plan.leave.push(plan.leave[0]);
    let day = LeaveEntry::from_legacy("2026-11-21").unwrap();
    plan.leave.push(day);
    let engine = Engine::new(&calendar, &plan).unwrap();
    assert_eq!(engine.leave_summary().classes, 2);
    assert_eq!(
        engine
            .project_course("MAT1001", "cat1")
            .unwrap()
            .total_attended,
        14
    );
    let midnight = "2026-11-16T18:30:00Z".parse().unwrap();
    assert_eq!(
        time::campus_date(midnight).unwrap().to_string(),
        "2026-11-17"
    );
    assert_eq!(
        time::last_completed_campus_date(midnight)
            .unwrap()
            .to_string(),
        "2026-11-16"
    );
    0
}
