use time::{Date, Month, OffsetDateTime};

pub const FIVE_HOURS_MS: i64 = 5 * 3_600_000;
pub const WEEK_MS: i64 = 7 * 86_400_000;

/// A window a key can be capped on. Ordered tightest first so the 429 names the
/// limit the caller will hit again soonest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Window {
    FiveHour,
    Week,
    Month,
}

impl Window {
    pub const ALL: [Window; 3] = [Window::FiveHour, Window::Week, Window::Month];

    /// Short label used in error bodies and admin output.
    pub fn name(self) -> &'static str {
        match self {
            Window::FiveHour => "5h",
            Window::Week => "weekly",
            Window::Month => "monthly",
        }
    }

    /// Wording for the 429 message.
    pub fn description(self) -> &'static str {
        match self {
            Window::FiveHour => "5-hour",
            Window::Week => "weekly",
            Window::Month => "monthly",
        }
    }

    /// First timestamp (ms) that still counts against the window.
    pub fn since(self, now: OffsetDateTime) -> i64 {
        match self {
            Window::FiveHour => to_ms(now) - FIVE_HOURS_MS,
            Window::Week => to_ms(now) - WEEK_MS,
            Window::Month => month_start_ms(now),
        }
    }

    /// Window length for rolling windows, `None` for the calendar month.
    fn span_ms(self) -> Option<i64> {
        match self {
            Window::FiveHour => Some(FIVE_HOURS_MS),
            Window::Week => Some(WEEK_MS),
            Window::Month => None,
        }
    }
}

pub fn month_start_ms(now: OffsetDateTime) -> i64 {
    let date = Date::from_calendar_date(now.year(), now.month(), 1).unwrap_or_else(|_| now.date());
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

/// Seconds until the window frees enough room to accept a request again.
///
/// The month waits for the calendar boundary. Rolling windows wait for the
/// oldest request still inside the window to fall out — that is exactly when
/// the sum drops below the limit, provided a single request cannot exceed the
/// whole limit on its own.
pub fn retry_after_seconds(window: Window, now: OffsetDateTime, oldest: Option<i64>) -> i64 {
    match window.span_ms() {
        None => ((next_month_start_ms(now) - to_ms(now)) / 1000).max(1),
        Some(span) => oldest
            .map(|start| (start + span - to_ms(now)) / 1000)
            .unwrap_or(span / 1000)
            .max(1),
    }
}

fn to_ms(value: OffsetDateTime) -> i64 {
    (value.unix_timestamp_nanos() / 1_000_000) as i64
}
