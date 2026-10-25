use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::{calendar::GridDay, Error, Result};

pub const SLOT_COUNT: usize = 7;
pub const PLAN_SCHEMA_VERSION: u32 = 1;
pub const MAX_COURSES: usize = 64;
pub const MAX_LEAVE_ENTRIES: usize = 8192;

/// A zero-based slot in the FFCS matrix, excluding lunch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct SlotIndex(u8);

impl SlotIndex {
    pub fn new(value: u8) -> Result<Self> {
        if usize::from(value) < SLOT_COUNT {
            Ok(Self(value))
        } else {
            Err(Error::Invalid(format!(
                "slot index {value} must be between 0 and 6"
            )))
        }
    }

    pub fn get(self) -> usize {
        usize::from(self.0)
    }
}

impl TryFrom<u8> for SlotIndex {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self> {
        Self::new(value)
    }
}

impl From<SlotIndex> for u8 {
    fn from(value: SlotIndex) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Course {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timetable {
    pub courses: Vec<Course>,
    /// Missing rows are free days. Every provided row has exactly seven slots.
    pub grid: BTreeMap<GridDay, [Option<String>; SLOT_COUNT]>,
}

impl Timetable {
    pub fn validate(&self) -> Result<()> {
        if self.courses.len() > MAX_COURSES {
            return Err(Error::Invalid(format!(
                "at most {MAX_COURSES} courses are allowed"
            )));
        }
        let mut codes = BTreeSet::new();
        for course in &self.courses {
            if course.code.is_empty()
                || course.code.len() > 32
                || !course
                    .code
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err(Error::Invalid(format!(
                    "invalid course code {:?}; use 1–32 uppercase letters, digits, '-' or '_'",
                    course.code
                )));
            }
            if course.name.len() > 160 || course.name.chars().any(char::is_control) {
                return Err(Error::Invalid(format!("invalid name for {}", course.code)));
            }
            if !codes.insert(course.code.as_str()) {
                return Err(Error::Invalid(format!("duplicate course {}", course.code)));
            }
        }
        for code in self.grid.values().flatten().flatten() {
            if !codes.contains(code.as_str()) {
                return Err(Error::UnknownCourse(code.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttendanceSnapshot {
    /// Inclusive end-of-day cutoff, not a live wall-clock date. Leave at or
    /// before this date cannot change the recorded attendance.
    pub as_of: NaiveDate,
    /// Missing registered courses mean zero attendance; unknown courses fail validation.
    pub attended: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LeaveEntry {
    Day { date: NaiveDate },
    Slot { date: NaiveDate, slot: SlotIndex },
}

impl LeaveEntry {
    pub fn date(self) -> NaiveDate {
        match self {
            Self::Day { date } | Self::Slot { date, .. } => date,
        }
    }

    /// Explicit adapter for the old YYYY-MM-DD / YYYY-MM-DD#slot encoding.
    /// It parses only; Engine::new checks the date and actual meeting.
    pub fn from_legacy(value: &str) -> Result<Self> {
        let (date, slot) = match value.split_once('#') {
            Some((date, slot)) => (date, Some(slot)),
            None => (value, None),
        };
        if date.len() != 10 {
            return Err(Error::Invalid(format!("invalid leave date {date:?}")));
        }
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| Error::Invalid(format!("invalid leave date {date:?}")))?;
        match slot {
            None => Ok(Self::Day { date }),
            Some(slot) if slot.len() == 1 && slot.as_bytes()[0].is_ascii_digit() => {
                Ok(Self::Slot {
                    date,
                    slot: SlotIndex::new(slot.as_bytes()[0] - b'0')?,
                })
            }
            Some(_) => Err(Error::Invalid(format!("invalid legacy leave {value:?}"))),
        }
    }

    pub fn to_legacy(self) -> String {
        match self {
            Self::Day { date } => date.to_string(),
            Self::Slot { date, slot } => format!("{date}#{}", slot.get()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema_version: u32,
    pub semester_id: String,
    /// Pins the calendar content, independently of its JSON schema version.
    pub calendar_revision: u32,
    pub timetable: Timetable,
    pub attendance: AttendanceSnapshot,
    pub leave: Vec<LeaveEntry>,
}

impl Plan {
    /// Structural deserialization is followed by semantic validation in Engine::new.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }
}
