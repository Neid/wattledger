//! Local time: everything user-facing is Europe/Warsaw; storage is UTC.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

pub const TZ: Tz = chrono_tz::Europe::Warsaw;
/// RCE prices (and our energy accounting) use 15-minute slots.
pub const SLOT: Duration = Duration::minutes(15);

/// UTC instant of local midnight at the start of `date`.
pub fn day_start(date: NaiveDate) -> DateTime<Utc> {
    // Polish midnight always exists (DST switches happen at 02:00/03:00).
    TZ.from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("valid time"))
        .earliest()
        .expect("local midnight exists")
        .with_timezone(&Utc)
}

/// Start and end (exclusive) of a local day: 23, 24 or 25 hours long.
pub fn day_bounds(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let next = date.succ_opt().expect("date in range");
    (day_start(date), day_start(next))
}

pub fn local_date(t: DateTime<Utc>) -> NaiveDate {
    t.with_timezone(&TZ).date_naive()
}

/// Number of 15-minute slots in a local day (92, 96 or 100).
pub fn slots_in_day(date: NaiveDate) -> usize {
    let (start, end) = day_bounds(date);
    ((end - start).num_minutes() / SLOT.num_minutes()) as usize
}

/// Start of the 15-minute slot containing `t`.
pub fn floor_slot(t: DateTime<Utc>) -> DateTime<Utc> {
    // Warsaw's offset is whole hours, so UTC slots are local slots.
    let secs = t.timestamp();
    let slot = SLOT.num_seconds();
    DateTime::from_timestamp(secs - secs.rem_euclid(slot), 0).expect("in range")
}

/// Interprets a local wall-clock time (e.g. from a datetime-local input).
/// An ambiguous time (the repeated hour in October) takes the earlier one;
/// a time that does not exist (the skipped hour in March) is an error.
pub fn from_local(t: NaiveDateTime) -> Option<DateTime<Utc>> {
    TZ.from_local_datetime(&t)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveTime, Timelike};

    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn ordinary_day_is_24_hours() {
        let (start, end) = day_bounds(d(2026, 11, 5));
        assert_eq!(start, utc("2026-11-04T23:00:00Z"));
        assert_eq!(end, utc("2026-11-05T23:00:00Z"));
        assert_eq!(slots_in_day(d(2026, 11, 5)), 96);
    }

    #[test]
    fn dst_days() {
        // Clocks go back: 25 hours.
        let (start, end) = day_bounds(d(2026, 10, 25));
        assert_eq!(start, utc("2026-10-24T22:00:00Z"));
        assert_eq!(end, utc("2026-10-25T23:00:00Z"));
        assert_eq!(slots_in_day(d(2026, 10, 25)), 100);
        // Clocks go forward: 23 hours.
        assert_eq!(slots_in_day(d(2026, 3, 29)), 92);
    }

    #[test]
    fn local_dates() {
        assert_eq!(local_date(utc("2026-11-04T23:30:00Z")), d(2026, 11, 5));
        assert_eq!(local_date(utc("2026-11-04T22:59:59Z")), d(2026, 11, 4));
    }

    #[test]
    fn slots() {
        assert_eq!(
            floor_slot(utc("2026-11-05T10:14:59Z")),
            utc("2026-11-05T10:00:00Z")
        );
        assert_eq!(
            floor_slot(utc("2026-11-05T10:15:00Z")),
            utc("2026-11-05T10:15:00Z")
        );
    }

    #[test]
    fn local_input_times() {
        let t = |date, h, m| NaiveDateTime::new(date, NaiveTime::from_hms_opt(h, m, 0).unwrap());
        assert_eq!(
            from_local(t(d(2026, 11, 5), 18, 0)),
            Some(utc("2026-11-05T17:00:00Z"))
        );
        // 02:30 happens twice on 2026-10-25: take the first.
        assert_eq!(
            from_local(t(d(2026, 10, 25), 2, 30)),
            Some(utc("2026-10-25T00:30:00Z"))
        );
        // 02:30 does not exist on 2026-03-29.
        assert_eq!(from_local(t(d(2026, 3, 29), 2, 30)), None);
        assert_eq!(from_local(t(d(2026, 3, 29), 3, 0)).unwrap().hour(), 1);
    }
}
