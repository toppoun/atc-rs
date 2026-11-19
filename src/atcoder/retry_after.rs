use reqwest::header::{DATE, HeaderMap, RETRY_AFTER};
use std::time::Duration;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::parsing::Parsed;
use time::{OffsetDateTime, PrimitiveDateTime};

const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(2);
pub(super) const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

// The two formats with a four-digit year have a fixed length and no year sign.
const HTTP_DATE_FORMATS: [(Option<usize>, &[BorrowedFormatItem<'static>]); 3] = [
    (
        Some(29),
        format_description!(
            "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
        ),
    ),
    (
        None,
        format_description!(
            "[weekday repr:long], [day]-[month repr:short]-[year repr:last_two] [hour]:[minute]:[second] GMT"
        ),
    ),
    (
        Some(24),
        format_description!(
            "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
        ),
    ),
];

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RetryAfterExceedsLimit;

pub(super) fn retry_wait(headers: &HeaderMap) -> Result<Duration, RetryAfterExceedsLimit> {
    retry_wait_at(headers, OffsetDateTime::now_utc())
}

fn retry_wait_at(
    headers: &HeaderMap,
    now: OffsetDateTime,
) -> Result<Duration, RetryAfterExceedsLimit> {
    let wait = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let value = value.trim_matches([' ', '\t']);
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                // An overflowing decimal still represents a valid wait above our limit.
                return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
            }
            // Measure against the response's clock when available. Start waiting on receipt
            // without subtracting transit time, so clock skew cannot shorten the delay.
            let reference = headers
                .get(DATE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_http_date(value, now))
                .unwrap_or(now);
            parse_http_date(value, reference)
                .map(|date| (date - reference).try_into().unwrap_or_default())
        })
        .unwrap_or(DEFAULT_RETRY_WAIT);
    if wait > MAX_RETRY_WAIT {
        return Err(RetryAfterExceedsLimit);
    }
    Ok(wait)
}

fn parse_http_date(value: &str, reference: OffsetDateTime) -> Option<OffsetDateTime> {
    let value = value.trim_matches([' ', '\t']).as_bytes();
    let mut parsed = HTTP_DATE_FORMATS
        .iter()
        .filter(|(length, _)| length.is_none_or(|length| length == value.len()))
        .find_map(|(_, format)| {
            let mut parsed = Parsed::new();
            parsed
                .parse_items(value, format)
                .ok()?
                .is_empty()
                .then_some(parsed)
        })?;
    let time_of_year =
        |month: time::Month, day, hour, minute, second| (month as u8, day, hour, minute, second);
    if let Some(two_digits) = parsed.year_last_two() {
        // RFC 9110 resolves an obsolete two-digit year as the most recent matching
        // year no more than 50 years in the future.
        let latest = reference.year() + 50;
        let mut year = latest - (latest - i32::from(two_digits)).rem_euclid(100);
        if year == latest
            && time_of_year(
                parsed.month()?,
                parsed.day()?.get(),
                parsed.hour_24()?,
                parsed.minute()?,
                parsed.second()?,
            ) > time_of_year(
                reference.month(),
                reference.day(),
                reference.hour(),
                reference.minute(),
                reference.second(),
            )
        {
            year -= 100;
        }
        parsed.set_year(year)?;
    }
    let leap_second = parsed.second()? == 60;
    if leap_second {
        parsed.set_second(59)?;
    }
    let date = PrimitiveDateTime::try_from(parsed).ok()?.assume_utc();
    if date.weekday() != parsed.weekday()? {
        return None;
    }
    date.checked_add(time::Duration::seconds(i64::from(leap_second)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(1994-11-06 08:49:35 UTC);

    #[test]
    fn numeric_retry_after_keeps_the_delay_or_stops_without_truncation() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_wait_at(&headers, NOW), Ok(DEFAULT_RETRY_WAIT));
        for (value, expected) in [
            ("0", Ok(Duration::ZERO)),
            ("7", Ok(Duration::from_secs(7))),
            ("60", Ok(MAX_RETRY_WAIT)),
            ("61", Err(RetryAfterExceedsLimit)),
            ("18446744073709551615000", Err(RetryAfterExceedsLimit)),
            ("-1", Ok(DEFAULT_RETRY_WAIT)),
            ("bad delay", Ok(DEFAULT_RETRY_WAIT)),
        ] {
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            assert_eq!(retry_wait_at(&headers, NOW), expected, "{value}");
        }
    }

    #[test]
    fn retry_after_accepts_the_three_http_date_formats() {
        for value in [
            "Sun, 06 Nov 1994 08:49:39 GMT",
            "Sunday, 06-Nov-94 08:49:39 GMT",
            "Sun Nov  6 08:49:39 1994",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            assert_eq!(
                retry_wait_at(&headers, NOW),
                Ok(Duration::from_secs(4)),
                "{value}"
            );
        }
        assert_eq!(
            parse_http_date(
                "Wednesday, 01-Jan-70 00:00:00 GMT",
                datetime!(2026-11-19 00:00 UTC)
            ),
            Some(datetime!(2070-01-01 00:00 UTC)),
        );
        assert_eq!(
            parse_http_date(
                "Thursday, 01-Jan-70 00:00:00 GMT",
                datetime!(2019-12-31 23:59:59 UTC)
            ),
            Some(datetime!(1970-01-01 00:00 UTC)),
        );
    }

    #[test]
    fn retry_after_dates_use_server_time_and_handle_expiry_and_invalid_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            DATE,
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:32 GMT"),
        );
        for (value, expected) in [
            ("Sun, 06 Nov 1994 08:49:34 GMT", Ok(Duration::from_secs(2))),
            ("Sun, 06 Nov 1994 08:49:36 GMT", Ok(Duration::from_secs(4))),
            ("Sun, 06 Nov 1994 08:49:31 GMT", Ok(Duration::ZERO)),
            ("Sun, 06 Nov 1994 08:50:33 GMT", Err(RetryAfterExceedsLimit)),
            ("Sun, 31 Feb 1994 08:49:37 GMT", Ok(DEFAULT_RETRY_WAIT)),
            ("Mon, 06 Nov 1994 08:49:37 GMT", Ok(DEFAULT_RETRY_WAIT)),
            ("Sun, 06 Nov +1994 08:49:37 GMT", Ok(DEFAULT_RETRY_WAIT)),
        ] {
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            assert_eq!(retry_wait_at(&headers, NOW), expected, "{value}");
        }
        headers.insert(DATE, HeaderValue::from_static("invalid"));
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:34 GMT"),
        );
        assert_eq!(retry_wait_at(&headers, NOW), Ok(Duration::ZERO));
    }
}
