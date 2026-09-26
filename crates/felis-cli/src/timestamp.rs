//! Hand-rolled: a date crate would put time zones, parsing, and
//! arithmetic nothing here calls into `cargo deny`'s audit surface for
//! one formatting direction.

use std::time::{SystemTime, UNIX_EPOCH};

/// `t` as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// WHY-NOT clamping a pre-epoch instant to the epoch: the wire admits
/// any `google.protobuf.Timestamp` back to year 0001, and rewriting one
/// to 1970 would publish a value the daemon never sent.
pub(crate) fn rfc3339_utc(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs().min(i64::MAX.cast_unsigned()).cast_signed(),
        Err(e) => {
            let d = e.duration();
            let whole = d.as_secs().min(i64::MAX.cast_unsigned()).cast_signed();
            whole
                .saturating_neg()
                .saturating_sub(i64::from(d.subsec_nanos() > 0))
        }
    };
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400).cast_unsigned();
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day / 60) % 60,
        time_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days since 1970-01-01 to a proleptic-Gregorian civil date: Howard
/// Hinnant's `civil_from_days`
/// (<http://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097).cast_unsigned();
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era.cast_signed() + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(epoch_secs: u64) -> String {
        rfc3339_utc(UNIX_EPOCH + Duration::from_secs(epoch_secs))
    }

    #[test]
    fn the_epoch_renders_as_the_epoch() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_time_of_day_renders_in_every_field() {
        assert_eq!(at(86_400 + 3 * 3600 + 4 * 60 + 5), "1970-01-02T03:04:05Z");
    }

    /// Leap day, the 2100 century-non-leap boundary, and a year-end
    /// rollover, cross-checked against `date -u -d @…`.
    #[test]
    fn calendar_edges_land_on_the_right_civil_date() {
        assert_eq!(at(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(at(4_107_456_000), "2100-02-28T00:00:00Z");
        assert_eq!(at(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(at(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(at(1_767_225_600), "2026-01-01T00:00:00Z");
    }

    /// A pre-epoch stamp (the wire admits one) renders as its own civil
    /// date, not as the epoch, and a sub-second remainder floors the same
    /// way a post-epoch one does. It stops at 1900 because Windows'
    /// FILETIME-backed `SystemTime` panics on anything before 1601.
    #[test]
    fn a_pre_epoch_stamp_renders_its_own_civil_date() {
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH - Duration::from_secs(10)),
            "1969-12-31T23:59:50Z"
        );
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH - Duration::from_millis(500)),
            "1969-12-31T23:59:59Z"
        );
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH - Duration::from_secs(2_208_988_800)),
            "1900-01-01T00:00:00Z"
        );
    }

    /// The far past the wire admits but no platform clock can hold:
    /// year 0001 and the proleptic-Gregorian leap rules on the way to
    /// it.
    #[test]
    fn civil_from_days_reaches_the_wires_year_one_floor() {
        assert_eq!(civil_from_days(-719_162), (1, 1, 1));
        assert_eq!(civil_from_days(-536_906), (500, 1, 1));
        assert_eq!(civil_from_days(-25_567), (1900, 1, 1));
    }

    /// Hinnant's `days_from_civil`, the inverse of the production
    /// direction. It lives here rather than in `src/` because nothing the
    /// CLI renders needs it: it exists to say what `civil_from_days`
    /// means.
    fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
        let year = year - i64::from(month <= 2);
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let year_of_era = (year - era * 400).cast_unsigned();
        let shifted_month = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era.cast_signed() - 719_468
    }

    /// 0001-01-01 through 9999-12-31: the span
    /// `google.protobuf.Timestamp` admits.
    const WIRE_DAYS: std::ops::RangeInclusive<i64> = -719_162..=2_932_896;

    proptest::proptest! {
        #[test]
        fn every_day_number_round_trips_through_its_civil_date(z in WIRE_DAYS) {
            let (year, month, day) = civil_from_days(z);
            proptest::prop_assert!((1..=12).contains(&month), "month {month} for {z}");
            proptest::prop_assert!((1..=31).contains(&day), "day {day} for {z}");
            proptest::prop_assert_eq!(days_from_civil(year, month, day), z);
        }

        /// The successor of a civil date is the next day of the same
        /// month, the first of the next month, or the first of the next
        /// year, which pins where each month ends, leap years included.
        #[test]
        fn the_next_day_number_is_the_next_civil_date(z in WIRE_DAYS) {
            let (year, month, day) = civil_from_days(z);
            let next = civil_from_days(z + 1);
            proptest::prop_assert!(
                next == (year, month, day + 1)
                    || next == (year, month + 1, 1)
                    || next == (year + 1, 1, 1),
                "{:?} does not follow {:?}",
                next,
                (year, month, day),
            );
        }
    }
}
