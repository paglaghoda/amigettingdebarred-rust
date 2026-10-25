use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::Serialize;

use crate::{
    calendar::{Calendar, Checkpoint, DayKind},
    model::{LeaveEntry, Plan, SlotIndex, MAX_LEAVE_ENTRIES, PLAN_SCHEMA_VERSION},
    Error, Result,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meeting {
    pub slot: SlotIndex,
    pub course_code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CourseProjection {
    pub course_code: String,
    pub held_so_far: u32,
    pub attended_so_far: u32,
    pub future_held: u32,
    pub future_skipped: u32,
    pub total_held: u32,
    pub total_attended: u32,
    pub minimum_attended_required: u32,
    /// One decimal for presentation only. Never used to determine safety.
    pub percentage: f64,
    pub safe: bool,
    /// Zero meetings display 100% for compatibility, but are not evidence of eligibility.
    pub has_classes: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckpointProjection {
    Upcoming {
        checkpoint: Checkpoint,
        courses: Vec<CourseProjection>,
    },
    /// Aggregate counts cannot establish attendance at an earlier checkpoint.
    Historical { checkpoint: Checkpoint },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SkipAllowance {
    /// Maximum future skips assuming no future leave has yet been selected.
    pub total_future_allowance: u32,
    pub already_planned: u32,
    /// Additional future skips beyond the existing selections.
    pub additional: u32,
    /// Classes short of the threshold under the current leave scenario.
    pub shortfall: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LeaveSummary {
    pub days: u32,
    pub classes: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HeldBreakdown {
    pub total: u32,
    pub saturday_classes: u32,
    pub saturday_days: u32,
    /// Dates on which this particular course actually met.
    pub class_days: u32,
}

#[derive(Debug, Default)]
struct PrefixCounts {
    held: Vec<u32>,
    skipped: Vec<u32>,
}

/// Validated, immutable view of a scenario. Build once per changed plan.
/// Calendar expansion and leave parsing are not repeated for every projection.
#[derive(Debug)]
pub struct Engine<'a> {
    calendar: &'a Calendar,
    plan: &'a Plan,
    meetings: BTreeMap<NaiveDate, Vec<Meeting>>,
    leave: BTreeMap<NaiveDate, BTreeSet<SlotIndex>>,
    counts: BTreeMap<String, PrefixCounts>,
    cutoff: usize,
}

impl<'a> Engine<'a> {
    pub fn new(calendar: &'a Calendar, plan: &'a Plan) -> Result<Self> {
        let config = calendar.config();
        if plan.schema_version != PLAN_SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported plan schema version {}",
                plan.schema_version
            )));
        }
        if plan.semester_id != config.id {
            return Err(Error::UnknownSemester(plan.semester_id.clone()));
        }
        if plan.calendar_revision != config.revision {
            return Err(Error::Invalid(
                "plan calendar revision does not match loaded calendar".into(),
            ));
        }
        plan.timetable.validate()?;
        if plan.leave.len() > MAX_LEAVE_ENTRIES {
            return Err(Error::Invalid(format!(
                "at most {MAX_LEAVE_ENTRIES} leave entries are allowed"
            )));
        }

        let mut meetings = BTreeMap::new();
        for day in calendar.days() {
            let row = calendar
                .effective_weekday(day.date)
                .and_then(|weekday| plan.timetable.grid.get(&weekday));
            let daily = row
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(index, code)| {
                    code.as_ref().map(|code| Meeting {
                        // Array indices are bounded by the seven-slot timetable type.
                        slot: SlotIndex::new(index as u8).expect("fixed-size timetable slot"),
                        course_code: code.clone(),
                    })
                })
                .collect::<Vec<_>>();
            meetings.insert(day.date, daily);
        }

        let mut leave: BTreeMap<NaiveDate, BTreeSet<SlotIndex>> = BTreeMap::new();
        for entry in &plan.leave {
            let date = entry.date();
            let daily = meetings.get(&date).ok_or_else(|| {
                Error::Invalid(format!("leave date {date} is outside the semester"))
            })?;
            if daily.is_empty() {
                return Err(Error::Invalid(format!(
                    "leave date {date} has no scheduled classes"
                )));
            }
            let slots = leave.entry(date).or_default();
            match entry {
                LeaveEntry::Day { .. } => slots.extend(daily.iter().map(|m| m.slot)),
                LeaveEntry::Slot { slot, .. } => {
                    if !daily.iter().any(|m| m.slot == *slot) {
                        return Err(Error::Invalid(format!(
                            "leave slot {} on {date} has no class",
                            slot.get()
                        )));
                    }
                    slots.insert(*slot);
                }
            }
        }

        let mut counts = BTreeMap::new();
        for course in &plan.timetable.courses {
            let mut prefix = PrefixCounts {
                held: Vec::with_capacity(calendar.days().len() + 1),
                skipped: Vec::with_capacity(calendar.days().len() + 1),
            };
            prefix.held.push(0);
            prefix.skipped.push(0);
            let (mut held, mut skipped) = (0, 0);
            for day in calendar.days() {
                for meeting in &meetings[&day.date] {
                    if meeting.course_code == course.code {
                        held += 1;
                        if leave
                            .get(&day.date)
                            .is_some_and(|slots| slots.contains(&meeting.slot))
                        {
                            skipped += 1;
                        }
                    }
                }
                prefix.held.push(held);
                prefix.skipped.push(skipped);
            }
            counts.insert(course.code.clone(), prefix);
        }
        let cutoff = calendar
            .days()
            .partition_point(|day| day.date <= plan.attendance.as_of);
        for (code, attended) in &plan.attendance.attended {
            let prefix = counts
                .get(code)
                .ok_or_else(|| Error::UnknownCourse(code.clone()))?;
            if *attended > prefix.held[cutoff] {
                return Err(Error::Invalid(format!(
                    "attendance {attended} for {code} exceeds {} held classes as of {}",
                    prefix.held[cutoff], plan.attendance.as_of
                )));
            }
        }
        Ok(Self {
            calendar,
            plan,
            meetings,
            leave,
            counts,
            cutoff,
        })
    }

    pub fn meetings_on(&self, date: NaiveDate) -> &[Meeting] {
        self.meetings.get(&date).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn held_so_far(&self, course: &str) -> Result<u32> {
        Ok(self.course_counts(course)?.held[self.cutoff])
    }

    /// Counts meetings in [from, to), naturally clipped to semester bounds.
    pub fn classes_held_between(
        &self,
        course: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<u32> {
        let counts = self.course_counts(course)?;
        if to < from {
            return Err(Error::Invalid("class range ends before it starts".into()));
        }
        let start = self.calendar.days().partition_point(|day| day.date < from);
        let end = self.calendar.days().partition_point(|day| day.date < to);
        Ok(counts.held[end] - counts.held[start])
    }

    pub fn project_course(&self, course: &str, checkpoint: &str) -> Result<CourseProjection> {
        let counts = self.course_counts(course)?;
        let checkpoint = self
            .calendar
            .config()
            .checkpoints
            .iter()
            .find(|c| c.id == checkpoint)
            .ok_or_else(|| Error::UnknownCheckpoint(checkpoint.into()))?;
        if checkpoint.start_date <= self.plan.attendance.as_of {
            return Err(Error::HistoricalCheckpoint(checkpoint.id.clone()));
        }
        Ok(self.project_known(course, counts, checkpoint))
    }

    pub fn project_all(&self) -> Vec<CheckpointProjection> {
        self.calendar
            .config()
            .checkpoints
            .iter()
            .map(|checkpoint| {
                if checkpoint.start_date <= self.plan.attendance.as_of {
                    CheckpointProjection::Historical {
                        checkpoint: checkpoint.clone(),
                    }
                } else {
                    CheckpointProjection::Upcoming {
                        checkpoint: checkpoint.clone(),
                        courses: self
                            .plan
                            .timetable
                            .courses
                            .iter()
                            .map(|course| {
                                self.project_known(
                                    &course.code,
                                    &self.counts[&course.code],
                                    checkpoint,
                                )
                            })
                            .collect(),
                    }
                }
            })
            .collect()
    }

    pub fn skip_allowance(&self, course: &str, checkpoint: &str) -> Result<SkipAllowance> {
        let projected = self.project_course(course, checkpoint)?;
        let baseline_attended = projected.attended_so_far + projected.future_held;
        let total_future_allowance = baseline_attended
            .saturating_sub(projected.minimum_attended_required)
            .min(projected.future_held);
        Ok(SkipAllowance {
            total_future_allowance,
            already_planned: projected.future_skipped,
            additional: total_future_allowance.saturating_sub(projected.future_skipped),
            shortfall: projected
                .minimum_attended_required
                .saturating_sub(projected.total_attended),
        })
    }

    /// Only forecast leave affects this summary. Past selections remain in the
    /// stored plan but never double-subtract from an attendance snapshot.
    pub fn leave_summary(&self) -> LeaveSummary {
        let mut summary = LeaveSummary::default();
        for (date, slots) in &self.leave {
            if *date > self.plan.attendance.as_of {
                summary.days += 1;
                summary.classes += slots.len() as u32;
            }
        }
        summary
    }

    pub fn held_breakdown(&self, course: &str) -> Result<HeldBreakdown> {
        self.course_counts(course)?;
        let mut breakdown = HeldBreakdown::default();
        for day in &self.calendar.days()[..self.cutoff] {
            let classes = self
                .meetings_on(day.date)
                .iter()
                .filter(|m| m.course_code == course)
                .count() as u32;
            breakdown.total += classes;
            if classes > 0 {
                breakdown.class_days += 1;
                if day.kind == DayKind::WorkingSaturday {
                    breakdown.saturday_classes += classes;
                    breakdown.saturday_days += 1;
                }
            }
        }
        Ok(breakdown)
    }

    fn course_counts(&self, course: &str) -> Result<&PrefixCounts> {
        self.counts
            .get(course)
            .ok_or_else(|| Error::UnknownCourse(course.into()))
    }

    fn project_known(
        &self,
        course: &str,
        counts: &PrefixCounts,
        checkpoint: &Checkpoint,
    ) -> CourseProjection {
        let end = self
            .calendar
            .days()
            .partition_point(|day| day.date < checkpoint.start_date);
        let held_so_far = counts.held[self.cutoff];
        let attended_so_far = self
            .plan
            .attendance
            .attended
            .get(course)
            .copied()
            .unwrap_or(0);
        let future_held = counts.held[end] - held_so_far;
        let future_skipped = counts.skipped[end] - counts.skipped[self.cutoff];
        let total_held = held_so_far + future_held;
        let total_attended = attended_so_far + future_held - future_skipped;
        let threshold = u64::from(self.calendar.config().minimum_percentage);
        let required = (u64::from(total_held) * threshold).div_ceil(100) as u32;
        let percentage = if total_held == 0 {
            100.0
        } else {
            (f64::from(total_attended) * 1000.0 / f64::from(total_held)).round() / 10.0
        };
        CourseProjection {
            course_code: course.into(),
            held_so_far,
            attended_so_far,
            future_held,
            future_skipped,
            total_held,
            total_attended,
            minimum_attended_required: required,
            percentage,
            safe: total_attended >= required,
            has_classes: total_held > 0,
        }
    }
}
