//! Run with: cargo run -p debarred-domain --example project
use debarred_domain::{load_semester, Engine, Plan};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plan = Plan::from_json(include_str!("../tests/fixtures/winter-plan.v1.json"))?;
    let calendar = load_semester(&plan.semester_id)?;
    let engine = Engine::new(&calendar, &plan)?;
    for course in &plan.timetable.courses {
        let projection = engine.project_course(&course.code, "cat1")?;
        let allowance = engine.skip_allowance(&course.code, "cat1")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "projection": projection,
                "skip_allowance": allowance,
            }))?
        );
    }
    Ok(())
}
