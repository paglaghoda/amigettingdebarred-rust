use debarred_domain::{load_semester, Engine, Plan};

#[test]
fn versioned_example_plan_has_fixed_cat1_results() {
    let plan = Plan::from_json(include_str!("fixtures/winter-plan.v1.json")).unwrap();
    let calendar = load_semester(&plan.semester_id).unwrap();
    let engine = Engine::new(&calendar, &plan).unwrap();
    let programming = engine.project_course("CSE1001", "cat1").unwrap();
    assert_eq!(
        (programming.held_so_far, programming.attended_so_far),
        (4, 4)
    );
    assert_eq!(
        (programming.total_held, programming.total_attended),
        (19, 18)
    );
    assert_eq!((programming.percentage, programming.safe), (94.7, true));
    let allowance = engine.skip_allowance("CSE1001", "cat1").unwrap();
    assert_eq!(
        (
            allowance.total_future_allowance,
            allowance.already_planned,
            allowance.additional
        ),
        (4, 1, 3)
    );
    let maths = engine.project_course("MAT1001", "cat1").unwrap();
    assert_eq!(
        (maths.total_held, maths.total_attended, maths.percentage),
        (15, 15, 100.0)
    );
    assert_eq!(engine.leave_summary().classes, 1);
    assert_eq!(
        plan,
        Plan::from_json(&serde_json::to_string(&plan).unwrap()).unwrap()
    );
}
