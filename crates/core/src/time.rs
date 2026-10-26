//! UTC date formatting for file names. Never used for ordering.

/// `YYYYMMDD-HHMMSS` (UTC) for `ns` nanoseconds since the Unix epoch.
pub fn format_utc_compact(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Howard Hinnant's days → (year, month, day) for the proleptic Gregorian
/// calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(format_utc_compact(0), "19700101-000000");
        // 2024-02-29T13:14:15Z
        assert_eq!(
            format_utc_compact(1_709_212_455 * 1_000_000_000),
            "20240229-131415"
        );
        // 2000-12-31T23:59:59Z
        assert_eq!(
            format_utc_compact(978_307_199 * 1_000_000_000 + 999),
            "20001231-235959"
        );
        assert_eq!(format_utc_compact(-1), "19691231-235959");
    }
}
