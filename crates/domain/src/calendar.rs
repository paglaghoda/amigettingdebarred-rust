//! Deterministic semester calendars; all dates and rules come from versioned data.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const DEFAULT_SEMESTER_ID: &str = "winter-2026";
const WINTER_2026: &str = include_str!("../data/winter-2026.v1.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GridDay {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
}

impl GridDay {
    pub fn from_weekday(weekday: Weekday) -> Option<Self> {
        match weekday {
            Weekday::Mon => Some(Self::Mon),
            Weekday::Tue => Some(Self::Tue),
            Weekday::Wed => Some(Self::Wed),
            Weekday::Thu => Some(Self::Thu),
            Weekday::Fri => Some(Self::Fri),
            Weekday::Sat => Some(Self::Sat),
            Weekday::Sun => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemesterConfig {
    pub schema_version: u32,
    /// Content revision, independent of the schema's format version.
    pub revision: u32,
    pub id: String,
    pub label: String,
    pub batch_label: String,
    pub batch_description: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub last_instructional_day: NaiveDate,
    pub minimum_percentage: u8,
    pub holidays: BTreeMap<NaiveDate, String>,
    pub working_saturdays: BTreeMap<NaiveDate, GridDay>,
    pub day_notes: BTreeMap<NaiveDate, String>,
    pub exam_windows: Vec<ExamWindow>,
    pub checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub id: String,
    pub label: String,
    pub start_date: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExamWindow {
    pub checkpoint_id: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DayKind {
    Instructional,
    WorkingSaturday,
    Holiday,
    Exam,
    NoInstruction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarDay {
    pub date: NaiveDate,
    pub kind: DayKind,
    pub follows_weekday: Option<GridDay>,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Calendar {
    config: SemesterConfig,
    days: Vec<CalendarDay>,
}

impl Calendar {
    pub fn new(config: SemesterConfig) -> Result<Self> {
        validate(&config)?;
        let count = (config.end_date - config.start_date).num_days() + 1;
        let mut days = Vec::with_capacity(count as usize);
        let mut date = config.start_date;
        loop {
            let weekday = date.weekday();
            let exam = config
                .exam_windows
                .iter()
                .find(|window| window.from <= date && date <= window.to);
            let extra_note = config.day_notes.get(&date);
            let mut day = CalendarDay {
                date,
                kind: DayKind::NoInstruction,
                follows_weekday: None,
                note: Some("No classes".into()),
            };
            // Precedence mirrors the published calendar, including exam Saturdays
            // but excluding Sundays from exam windows.
            if let Some(holiday) = config.holidays.get(&date) {
                day.kind = DayKind::Holiday;
                day.note = Some(holiday.clone());
            } else if let Some(window) = exam.filter(|_| weekday != Weekday::Sun) {
                day.kind = DayKind::Exam;
                day.note = Some(window.note.clone());
            } else if let Some(follows) = config.working_saturdays.get(&date) {
                day.kind = DayKind::WorkingSaturday;
                day.follows_weekday = Some(*follows);
                let prefix = extra_note
                    .filter(|note| !note.is_empty())
                    .map(|note| format!("{note} · "))
                    .unwrap_or_default();
                day.note = Some(format!("{prefix}Follows {follows:?} timetable"));
            } else if weekday != Weekday::Sat
                && weekday != Weekday::Sun
                && date <= config.last_instructional_day
            {
                day.kind = DayKind::Instructional;
                day.note = extra_note.filter(|note| !note.is_empty()).cloned();
            }
            days.push(day);
            if date == config.end_date {
                break;
            }
            // The validated end date guarantees a successor before the last day.
            date = date
                .succ_opt()
                .ok_or_else(|| Error::Invalid("calendar date exceeds supported range".into()))?;
        }
        Ok(Self { config, days })
    }

    pub fn winter_2026() -> Result<Self> {
        let config = serde_json::from_str(WINTER_2026).map_err(Error::Json)?;
        Self::new(config)
    }

    pub fn config(&self) -> &SemesterConfig {
        &self.config
    }

    pub fn days(&self) -> &[CalendarDay] {
        &self.days
    }

    pub fn day(&self, date: NaiveDate) -> Option<&CalendarDay> {
        if date < self.config.start_date || date > self.config.end_date {
            return None;
        }
        self.days
            .get((date - self.config.start_date).num_days() as usize)
    }

    pub fn effective_weekday(&self, date: NaiveDate) -> Option<GridDay> {
        let day = self.day(date)?;
        match day.kind {
            DayKind::Instructional => GridDay::from_weekday(date.weekday()),
            DayKind::WorkingSaturday => day.follows_weekday,
            _ => None,
        }
    }
}

/// Load only explicitly supported semesters; stale or misspelled IDs are errors.
pub fn load_semester(id: &str) -> Result<Calendar> {
    match id {
        DEFAULT_SEMESTER_ID => Calendar::winter_2026(),
        _ => Err(Error::UnknownSemester(id.into())),
    }
}

fn validate(config: &SemesterConfig) -> Result<()> {
    let invalid = |message: &str| Error::Invalid(message.into());
    if config.schema_version != 1 {
        return Err(invalid("unsupported semester schema_version (expected 1)"));
    }
    if config.revision == 0 {
        return Err(invalid("semester revision must be greater than zero"));
    }
    if config.start_date > config.end_date
        || (config.end_date - config.start_date).num_days() >= 730
    {
        return Err(invalid(
            "semester must contain between 1 and 730 calendar days",
        ));
    }
    let in_term = |date: NaiveDate| config.start_date <= date && date <= config.end_date;
    if !in_term(config.last_instructional_day) {
        return Err(invalid(
            "last_instructional_day must be inside the semester",
        ));
    }
    if !(1..=100).contains(&config.minimum_percentage) {
        return Err(invalid("minimum_percentage must be between 1 and 100"));
    }
    for date in config.holidays.keys().chain(config.day_notes.keys()) {
        if !in_term(*date) {
            return Err(Error::Invalid(format!(
                "holiday or day note {date} is outside the semester"
            )));
        }
    }
    for (date, follows) in &config.working_saturdays {
        if !in_term(*date)
            || *date > config.last_instructional_day
            || date.weekday() != Weekday::Sat
            || *follows == GridDay::Sat
        {
            return Err(Error::Invalid(format!(
                "working Saturday {date} must be a Saturday inside the instructional period following Mon–Fri"
            )));
        }
    }
    let mut checkpoint_ids = BTreeSet::new();
    let mut previous_date = None;
    for checkpoint in &config.checkpoints {
        if !checkpoint_ids.insert(checkpoint.id.as_str()) {
            return Err(Error::Invalid(format!(
                "duplicate checkpoint ID: {}",
                checkpoint.id
            )));
        }
        if !in_term(checkpoint.start_date)
            || previous_date.is_some_and(|previous| checkpoint.start_date <= previous)
        {
            return Err(invalid(
                "checkpoint dates must be inside the semester, unique, and strictly increasing",
            ));
        }
        previous_date = Some(checkpoint.start_date);
    }
    for (index, window) in config.exam_windows.iter().enumerate() {
        if !checkpoint_ids.contains(window.checkpoint_id.as_str()) {
            return Err(Error::Invalid(format!(
                "unknown exam checkpoint ID: {}",
                window.checkpoint_id
            )));
        }
        if window.from > window.to || !in_term(window.from) || !in_term(window.to) {
            return Err(invalid(
                "exam windows must be ordered ranges inside the semester",
            ));
        }
        if config.exam_windows[..index]
            .iter()
            .any(|other| window.from <= other.to && other.from <= window.to)
        {
            return Err(invalid("exam windows must not overlap"));
        }
    }
    for checkpoint in &config.checkpoints {
        let first_exam_date = config
            .exam_windows
            .iter()
            .filter(|window| window.checkpoint_id == checkpoint.id)
            .map(|window| window.from)
            .min();
        if first_exam_date.is_some_and(|date| date != checkpoint.start_date) {
            return Err(Error::Invalid(format!(
                "checkpoint {} must start on its first exam-window date",
                checkpoint.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn config() -> SemesterConfig {
        serde_json::from_str(WINTER_2026).unwrap()
    }

    #[test]
    fn inclusive_boundaries_and_lookup() {
        let calendar = Calendar::winter_2026().unwrap();
        assert_eq!(calendar.days().len(), 136);
        assert_eq!(calendar.days().first().unwrap().date, date("2026-11-16"));
        assert_eq!(calendar.days().last().unwrap().date, date("2027-03-31"));
        for pair in calendar.days().windows(2) {
            assert_eq!(pair[0].date.succ_opt(), Some(pair[1].date));
        }
        for day in calendar.days() {
            assert_eq!(calendar.day(day.date), Some(day));
        }
        for outside in ["2026-11-15", "2027-04-01"] {
            assert!(calendar.day(date(outside)).is_none());
            assert!(calendar.effective_weekday(date(outside)).is_none());
        }
        assert_eq!(
            calendar.effective_weekday(date("2026-11-16")),
            Some(GridDay::Mon)
        );
        assert_eq!(
            calendar.day(date("2027-03-09")).unwrap().kind,
            DayKind::Instructional
        );
        assert_eq!(
            calendar.day(date("2027-03-10")).unwrap().kind,
            DayKind::NoInstruction
        );
    }

    #[test]
    fn preserves_all_holidays_notes_and_working_saturdays() {
        let calendar = Calendar::winter_2026().unwrap();
        assert_eq!(calendar.config().holidays.len(), 12);
        assert_eq!(calendar.config().day_notes.len(), 4);
        for (date, note) in &calendar.config().holidays {
            let day = calendar.day(*date).unwrap();
            assert_eq!(day.kind, DayKind::Holiday);
            assert_eq!(day.note.as_ref(), Some(note));
            assert_eq!(calendar.effective_weekday(*date), None);
        }
        for value in ["2026-11-21", "2026-12-12"] {
            let day = calendar.day(date(value)).unwrap();
            assert_eq!(day.kind, DayKind::WorkingSaturday);
            assert_eq!(day.follows_weekday, Some(GridDay::Fri));
            assert_eq!(calendar.effective_weekday(day.date), Some(GridDay::Fri));
            assert_eq!(
                day.note.as_deref(),
                Some("Remote learning (online) · Follows Fri timetable")
            );
        }
        for (value, note) in [
            ("2026-11-24", "Guru Nanak Jayanti (sectional holiday)"),
            ("2027-03-05", "Remote learning (online)"),
        ] {
            let day = calendar.day(date(value)).unwrap();
            assert_eq!(day.kind, DayKind::Instructional);
            assert_eq!(day.note.as_deref(), Some(note));
        }
        for value in ["2026-11-22", "2026-11-28", "2027-03-10"] {
            let day = calendar.day(date(value)).unwrap();
            assert_eq!(day.kind, DayKind::NoInstruction);
            assert_eq!(day.note.as_deref(), Some("No classes"));
        }
    }

    #[test]
    fn exam_ranges_include_saturdays_but_not_sundays() {
        let calendar = Calendar::winter_2026().unwrap();
        for window in &calendar.config().exam_windows {
            let mut current = window.from;
            loop {
                let day = calendar.day(current).unwrap();
                if current.weekday() == Weekday::Sun {
                    assert_eq!(day.kind, DayKind::NoInstruction);
                    assert_eq!(day.note.as_deref(), Some("No classes"));
                } else {
                    assert_eq!(day.kind, DayKind::Exam);
                    assert_eq!(day.note.as_ref(), Some(&window.note));
                }
                assert_eq!(calendar.effective_weekday(current), None);
                if current == window.to {
                    break;
                }
                current = current.succ_opt().unwrap();
            }
        }
    }

    #[test]
    fn holiday_then_exam_then_working_saturday_precedence() {
        let mut cfg = config();
        let saturday = date("2026-12-19");
        cfg.working_saturdays.insert(saturday, GridDay::Mon);
        cfg.day_notes.insert(saturday, "Ignored extra note".into());
        let calendar = Calendar::new(cfg.clone()).unwrap();
        assert_eq!(calendar.day(saturday).unwrap().kind, DayKind::Exam);
        assert_eq!(calendar.day(saturday).unwrap().follows_weekday, None);
        cfg.holidays.insert(saturday, "Holiday wins".into());
        cfg.holidays
            .insert(date("2026-12-20"), "Sunday holiday".into());
        let calendar = Calendar::new(cfg).unwrap();
        assert_eq!(calendar.day(saturday).unwrap().kind, DayKind::Holiday);
        assert_eq!(
            calendar.day(saturday).unwrap().note.as_deref(),
            Some("Holiday wins")
        );
        assert_eq!(
            calendar.day(date("2026-12-20")).unwrap().kind,
            DayKind::Holiday
        );
    }

    #[test]
    fn unknown_semesters_never_fall_back() {
        assert_eq!(
            load_semester(DEFAULT_SEMESTER_ID).unwrap().config(),
            &config()
        );
        for id in ["", "winter-2025", "Winter-2026", " winter-2026"] {
            assert!(matches!(load_semester(id), Err(Error::UnknownSemester(value)) if value == id));
        }
    }

    #[test]
    fn rejects_invalid_configuration() {
        type InvalidCase = (&'static str, fn(&mut SemesterConfig));
        let cases: &[InvalidCase] = &[
            ("schema", |c| c.schema_version = 2),
            ("zero revision", |c| c.revision = 0),
            ("reversed term", |c| {
                c.end_date = c.start_date.pred_opt().unwrap()
            }),
            ("too long", |c| {
                c.end_date = c.start_date + Duration::days(730)
            }),
            ("last day before term", |c| {
                c.last_instructional_day = c.start_date.pred_opt().unwrap()
            }),
            ("last day after term", |c| {
                c.last_instructional_day = c.end_date.succ_opt().unwrap()
            }),
            ("zero threshold", |c| c.minimum_percentage = 0),
            ("large threshold", |c| c.minimum_percentage = 101),
            ("holiday outside", |c| {
                c.holidays
                    .insert(c.start_date.pred_opt().unwrap(), "bad".into());
            }),
            ("note outside", |c| {
                c.day_notes
                    .insert(c.end_date.succ_opt().unwrap(), "bad".into());
            }),
            ("Saturday outside", |c| {
                c.working_saturdays.insert(date("2026-11-14"), GridDay::Mon);
            }),
            ("Saturday after instruction", |c| {
                c.working_saturdays.insert(date("2027-03-13"), GridDay::Mon);
            }),
            ("not Saturday", |c| {
                c.working_saturdays.insert(c.start_date, GridDay::Mon);
            }),
            ("Saturday follows Saturday", |c| {
                c.working_saturdays.insert(date("2026-11-21"), GridDay::Sat);
            }),
            ("duplicate checkpoint ID", |c| {
                c.checkpoints[1].id = c.checkpoints[0].id.clone()
            }),
            ("duplicate checkpoint date", |c| {
                c.checkpoints[1].start_date = c.checkpoints[0].start_date
            }),
            ("checkpoint order", |c| c.checkpoints.swap(0, 1)),
            ("checkpoint outside", |c| {
                c.checkpoints[0].start_date = c.start_date.pred_opt().unwrap()
            }),
            ("checkpoint before exam", |c| {
                c.checkpoints[0].start_date = c.exam_windows[0].from.pred_opt().unwrap()
            }),
            ("checkpoint after exam start", |c| {
                c.checkpoints[0].start_date = c.exam_windows[0].from.succ_opt().unwrap()
            }),
            ("unknown checkpoint", |c| {
                c.exam_windows[0].checkpoint_id = "unknown".into()
            }),
            ("reversed exam", |c| {
                c.exam_windows[0].to = c.exam_windows[0].from.pred_opt().unwrap()
            }),
            ("exam before term", |c| {
                c.exam_windows[0].from = c.start_date.pred_opt().unwrap()
            }),
            ("exam after term", |c| {
                c.exam_windows[2].to = c.end_date.succ_opt().unwrap()
            }),
            ("overlapping exams", |c| {
                c.exam_windows[1].from = c.exam_windows[0].to
            }),
        ];
        for (label, mutate) in cases {
            let mut cfg = config();
            mutate(&mut cfg);
            assert!(
                matches!(Calendar::new(cfg), Err(Error::Invalid(_))),
                "{label}"
            );
        }
    }

    #[test]
    fn content_revision_is_independent_of_schema_version() {
        for revision in [1, 2, u32::MAX] {
            let mut cfg = config();
            assert_eq!(cfg.schema_version, 1);
            assert_eq!(cfg.revision, 1);
            cfg.revision = revision;
            let calendar = Calendar::new(cfg).unwrap();
            assert_eq!(calendar.config().schema_version, 1);
            assert_eq!(calendar.config().revision, revision);
        }
    }

    #[test]
    fn accepts_threshold_and_calendar_length_limits() {
        for threshold in [1, 100] {
            let mut cfg = config();
            cfg.minimum_percentage = threshold;
            cfg.end_date = cfg.start_date + Duration::days(729);
            assert_eq!(Calendar::new(cfg).unwrap().days().len(), 730);
        }
        let mut cfg = config();
        cfg.end_date = cfg.start_date;
        cfg.last_instructional_day = cfg.start_date;
        cfg.holidays.clear();
        cfg.day_notes.clear();
        cfg.working_saturdays.clear();
        cfg.checkpoints.clear();
        cfg.exam_windows.clear();
        assert_eq!(Calendar::new(cfg).unwrap().days().len(), 1);
    }

    #[test]
    fn serde_round_trips_and_exact_enum_names() {
        let cfg = config();
        assert_eq!(
            serde_json::from_str::<SemesterConfig>(&serde_json::to_string(&cfg).unwrap()).unwrap(),
            cfg
        );
        let calendar = Calendar::new(cfg).unwrap();
        let encoded = serde_json::to_string(calendar.days()).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<CalendarDay>>(&encoded).unwrap(),
            calendar.days()
        );
        for (weekday, grid, name) in [
            (Weekday::Mon, GridDay::Mon, "Mon"),
            (Weekday::Tue, GridDay::Tue, "Tue"),
            (Weekday::Wed, GridDay::Wed, "Wed"),
            (Weekday::Thu, GridDay::Thu, "Thu"),
            (Weekday::Fri, GridDay::Fri, "Fri"),
            (Weekday::Sat, GridDay::Sat, "Sat"),
        ] {
            assert_eq!(GridDay::from_weekday(weekday), Some(grid));
            assert_eq!(serde_json::to_value(grid).unwrap(), serde_json::json!(name));
        }
        assert_eq!(GridDay::from_weekday(Weekday::Sun), None);
        for (kind, name) in [
            (DayKind::Instructional, "instructional"),
            (DayKind::WorkingSaturday, "working_saturday"),
            (DayKind::Holiday, "holiday"),
            (DayKind::Exam, "exam"),
            (DayKind::NoInstruction, "no_instruction"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(name));
        }
    }

    #[test]
    fn serde_rejects_unknown_fields_and_invalid_dates_or_enums() {
        let original: serde_json::Value = serde_json::from_str(WINTER_2026).unwrap();
        for pointer in ["", "/checkpoints/0", "/exam_windows/0"] {
            let mut value = original.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unexpected".into(), serde_json::json!(true));
            assert!(serde_json::from_value::<SemesterConfig>(value).is_err());
        }
        for (key, bad) in [("start_date", "2026-02-30"), ("working_saturdays", "Sun")] {
            let mut value = original.clone();
            if key == "working_saturdays" {
                value[key]["2026-11-21"] = serde_json::json!(bad);
            } else {
                value[key] = serde_json::json!(bad);
            }
            assert!(serde_json::from_value::<SemesterConfig>(value).is_err());
        }
        assert!(serde_json::from_str::<DayKind>("\"no_instructional\"").is_err());
        assert!(serde_json::from_str::<GridDay>("\"mon\"").is_err());
    }
}
