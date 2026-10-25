//! Shared native/WASM domain engine. No system clock, I/O, authentication, or AI.
//!
//! Attendance snapshots describe completed dates. Projections assume attendance
//! at every later meeting except those explicitly selected as leave.

pub mod calendar;
pub mod engine;
pub mod model;
pub mod slots;
pub mod time;

pub use calendar::{load_semester, Calendar, Checkpoint, GridDay};
pub use engine::{
    CheckpointProjection, CourseProjection, Engine, HeldBreakdown, LeaveSummary, Meeting,
    SkipAllowance,
};
pub use model::{AttendanceSnapshot, Course, LeaveEntry, Plan, SlotIndex, Timetable, SLOT_COUNT};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid domain input: {0}")]
    Invalid(String),
    #[error("unknown semester: {0}")]
    UnknownSemester(String),
    #[error("unknown course: {0}")]
    UnknownCourse(String),
    #[error("unknown checkpoint: {0}")]
    UnknownCheckpoint(String),
    #[error("checkpoint {0} is historical; aggregate attendance cannot reconstruct it")]
    HistoricalCheckpoint(String),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
