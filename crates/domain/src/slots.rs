//! FFCS presentation metadata shared by future native/server and WASM clients.
use crate::{GridDay, SLOT_COUNT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSlot {
    pub id: &'static str,
    pub start: &'static str,
    pub end: &'static str,
}

/// Lunch is presentation-only and does not consume a slot index.
pub const LUNCH_AFTER_INDEX: usize = 2;
pub const TIME_SLOTS: [TimeSlot; SLOT_COUNT] = [
    TimeSlot {
        id: "s1",
        start: "08:30",
        end: "10:00",
    },
    TimeSlot {
        id: "s2",
        start: "10:05",
        end: "11:35",
    },
    TimeSlot {
        id: "s3",
        start: "11:40",
        end: "13:10",
    },
    TimeSlot {
        id: "s4",
        start: "13:15",
        end: "14:45",
    },
    TimeSlot {
        id: "s5",
        start: "14:50",
        end: "16:20",
    },
    TimeSlot {
        id: "s6",
        start: "16:25",
        end: "17:55",
    },
    TimeSlot {
        id: "s7",
        start: "18:00",
        end: "19:30",
    },
];

pub fn slot_codes(day: GridDay) -> [&'static str; SLOT_COUNT] {
    match day {
        GridDay::Mon => ["A11", "B11", "C11", "A21", "A14", "B21", "C21"],
        GridDay::Tue => ["D11", "E11", "F11", "D21", "E14", "E21", "F21"],
        GridDay::Wed => ["A12", "B12", "C12", "A22", "B14", "B22", "A24"],
        GridDay::Thu => ["D12", "E12", "F12", "D22", "F14", "E22", "F22"],
        GridDay::Fri => ["A13", "B13", "C13", "A23", "C14", "B23", "B24"],
        GridDay::Sat => ["D13", "E13", "F13", "D23", "D14", "D24", "E23"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn slot_metadata_is_ordered_unique_and_excludes_lunch() {
        assert_eq!(TIME_SLOTS.len(), 7);
        assert_eq!(LUNCH_AFTER_INDEX, 2);
        let mut codes = BTreeSet::new();
        for day in [
            GridDay::Mon,
            GridDay::Tue,
            GridDay::Wed,
            GridDay::Thu,
            GridDay::Fri,
            GridDay::Sat,
        ] {
            for code in slot_codes(day) {
                assert!(codes.insert(code));
            }
        }
        for pair in TIME_SLOTS.windows(2) {
            assert!(pair[0].start < pair[0].end);
            assert!(pair[0].end <= pair[1].start);
        }
        assert!(TIME_SLOTS[6].start < TIME_SLOTS[6].end);
        assert_eq!(codes.len(), 42);
    }
}
