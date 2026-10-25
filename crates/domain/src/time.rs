//! Campus dates are derived from an injected UTC instant; never from the host timezone.
use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::{Error, Result};

pub const CAMPUS_TIMEZONE: &str = "Asia/Kolkata";

/// India uses UTC+05:30 year-round for the semesters supported by this engine.
/// No timezone database, browser clock, or OS clock is needed by the shared crate.
pub fn campus_date(instant: DateTime<Utc>) -> Result<NaiveDate> {
    instant
        .naive_utc()
        .checked_add_signed(Duration::minutes(330))
        .map(|local| local.date())
        .ok_or_else(|| Error::Invalid("instant is outside the supported campus-date range".into()))
}

/// Default snapshot cutoff for a daily UI: yesterday in campus time. The current
/// day is not complete, so a UI must not silently count all its meetings as held.
pub fn last_completed_campus_date(instant: DateTime<Utc>) -> Result<NaiveDate> {
    campus_date(instant)?
        .pred_opt()
        .ok_or_else(|| Error::Invalid("campus date has no representable previous day".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campus_midnight_and_completed_day_are_explicit() {
        let before: DateTime<Utc> = "2026-11-16T18:29:59Z".parse().unwrap();
        let after: DateTime<Utc> = "2026-11-16T18:30:00Z".parse().unwrap();
        assert_eq!(campus_date(before).unwrap().to_string(), "2026-11-16");
        assert_eq!(campus_date(after).unwrap().to_string(), "2026-11-17");
        assert_eq!(
            last_completed_campus_date(before).unwrap().to_string(),
            "2026-11-15"
        );
        assert_eq!(
            last_completed_campus_date(after).unwrap().to_string(),
            "2026-11-16"
        );
    }

    #[test]
    fn month_and_leap_day_rollover() {
        let leap: DateTime<Utc> = "2028-02-29T20:00:00Z".parse().unwrap();
        assert_eq!(campus_date(leap).unwrap().to_string(), "2028-03-01");
        assert_eq!(
            last_completed_campus_date(leap).unwrap().to_string(),
            "2028-02-29"
        );
    }

    #[test]
    fn extreme_inputs_fail_without_panicking() {
        assert!(campus_date(DateTime::<Utc>::MIN_UTC).is_ok());
        assert!(last_completed_campus_date(DateTime::<Utc>::MIN_UTC).is_err());
        assert!(campus_date(DateTime::<Utc>::MAX_UTC).is_err());
        assert!(last_completed_campus_date(DateTime::<Utc>::MAX_UTC).is_err());
    }
}
