use time::{Date, Month, OffsetDateTime};

pub fn month_start_ms(now: OffsetDateTime) -> i64 {
    let date = Date::from_calendar_date(now.year(), now.month(), 1)
        .unwrap_or_else(|_| now.date());
    to_ms(date.midnight().assume_utc())
}

pub fn next_month_start_ms(now: OffsetDateTime) -> i64 {
    let (year, month) = match now.month() {
        Month::December => (now.year() + 1, Month::January),
        month => (now.year(), month.next()),
    };
    let date = Date::from_calendar_date(year, month, 1).unwrap_or_else(|_| now.date());
    to_ms(date.midnight().assume_utc())
}

pub fn retry_after_seconds(now: OffsetDateTime) -> i64 {
    let remaining_ms = next_month_start_ms(now) - to_ms(now);
    (remaining_ms / 1000).max(1)
}

fn to_ms(value: OffsetDateTime) -> i64 {
    (value.unix_timestamp_nanos() / 1_000_000) as i64
}
