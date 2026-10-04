//! Times as the API writes them: RFC 3339, UTC, to the second.

/// RFC 3339 in UTC from unix seconds: `2026-10-03T12:00:00Z`.
pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant), as the fake's `iso`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The HTTP date of `Last-Modified`: `Sat, 03 Oct 2026 12:00:00 GMT`.
pub fn http_date(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let r = rfc3339(secs);
    let (y, m, d) = (&r[0..4], r[5..7].parse::<usize>().expect("a month"), &r[8..10]);
    let wd = DAYS[secs.div_euclid(86_400).rem_euclid(7) as usize];
    format!("{wd}, {d} {} {y} {} GMT", MONTHS[m - 1], &r[11..19])
}

/// Now, in unix seconds.
pub fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: times print as the service prints them. Method: the epoch,
    // a leap day, the webhook test vector's time, and dates before 1970.
    #[test]
    fn times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_790_000_000), "Mon, 21 Sep 2026 14:13:20 GMT");
    }
}
