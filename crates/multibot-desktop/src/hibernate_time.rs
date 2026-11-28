use chrono::{DateTime, Datelike, Duration, Local, Months, NaiveDate, NaiveTime, TimeZone};

#[derive(Debug)]
pub(super) struct HibernateMatch {
    pub label: String,
    pub timestamp_ms: i64,
    pub preview: String,
}

pub(super) const HIBERNATE_PRESETS: [(&str, &str); 5] = [
    ("pal-reminder-0", "In 15 minutes"),
    ("pal-reminder-1", "In 1 hour"),
    ("pal-reminder-2", "In 3 hours"),
    ("pal-reminder-3", "Tomorrow"),
    ("pal-reminder-5", "Next week"),
];

fn clock(query: &str) -> Option<NaiveTime> {
    let text = query.replace(' ', "");
    let (digits, period) = if let Some(digits) = text.strip_suffix("am") {
        (digits, Some(false))
    } else if let Some(digits) = text.strip_suffix("pm") {
        (digits, Some(true))
    } else {
        (text.as_str(), None)
    };
    if period.is_none() && !digits.contains(':') {
        return None;
    }
    let (hour, minute) = digits.split_once(':').unwrap_or((digits, "0"));
    let mut hour = hour.parse::<u32>().ok()?;
    let minute = minute.parse().ok()?;
    if let Some(pm) = period {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn calendar_time(
    date: NaiveDate,
    time: NaiveTime,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    // Reject nonexistent DST times; for repeated times use the first future occurrence.
    let result = Local.from_local_datetime(&date.and_time(time));
    result
        .earliest()
        .filter(|at| *at > now)
        .or_else(|| result.latest().filter(|at| *at > now))
}

fn resolved(label: String, at: DateTime<Local>, now: DateTime<Local>) -> HibernateMatch {
    let days = at
        .date_naive()
        .signed_duration_since(now.date_naive())
        .num_days();
    let time = at.format("%-I:%M %p");
    let preview = if days == 0 {
        time.to_string()
    } else if days <= 6 {
        format!("{}, {time}", at.format("%a").to_string().to_uppercase())
    } else {
        format!(
            "{}, {time}",
            at.format("%a, %b %d").to_string().to_uppercase()
        )
    };
    HibernateMatch {
        label,
        timestamp_ms: at.timestamp_millis(),
        preview,
    }
}

pub(super) fn parse_hibernate_query(query: &str, now: DateTime<Local>) -> Option<HibernateMatch> {
    let query = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let query = query.strip_prefix("in ").unwrap_or(&query);
    if query.is_empty() {
        return None;
    }
    let today = now.date_naive();
    if let Some(time) = clock(query) {
        let at = calendar_time(today, time, now)
            .or_else(|| calendar_time(today.succ_opt()?, time, now))?;
        return Some(resolved(query.into(), at, now));
    }
    let mut base = query;
    let mut time = NaiveTime::from_hms_opt(8, 0, 0)?;
    let mut explicit_time = false;
    for (index, _) in query.match_indices(' ') {
        if let Some(parsed) = clock(&query[index + 1..]) {
            base = &query[..index];
            time = parsed;
            explicit_time = true;
            break;
        }
    }
    let digit_count = base.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count > 0 {
        let number = base[..digit_count].parse::<u32>().ok()?;
        let unit = base[digit_count..].trim();
        let canonical = match unit {
            "m" | "min" | "mins" | "minute" | "minutes" => Some("minute"),
            "h" | "hr" | "hrs" | "hour" | "hours" => Some("hour"),
            "d" | "day" | "days" => Some("day"),
            "w" | "week" | "weeks" => Some("week"),
            "mo" | "month" | "months" => Some("month"),
            _ => None,
        };
        if let Some(unit) = canonical {
            if number == 0 {
                return None;
            }
            let label = format!("in {number} {unit}{}", if number == 1 { "" } else { "s" });
            let at = match unit {
                "minute" | "hour" if !explicit_time => {
                    let minutes = i64::from(number) * if unit == "hour" { 60 } else { 1 };
                    now.checked_add_signed(Duration::try_minutes(minutes)?)?
                }
                "day" | "week" => {
                    let days = i64::from(number) * if unit == "week" { 7 } else { 1 };
                    calendar_time(
                        today.checked_add_signed(Duration::try_days(days)?)?,
                        time,
                        now,
                    )?
                }
                "month" => {
                    calendar_time(today.checked_add_months(Months::new(number))?, time, now)?
                }
                _ => return None,
            };
            let label = if explicit_time {
                format!("{label} {}", time.format("%-I:%M %p"))
            } else {
                label
            };
            return Some(resolved(label, at, now));
        }
    }
    let (date, label) = if base.len() >= 3 && "tomorrow".starts_with(base) {
        (today.succ_opt()?, "Tomorrow".to_string())
    } else if matches!(base, "next w" | "next we" | "next wee" | "next week") {
        let days = 7 - i64::from(today.weekday().num_days_from_monday());
        (
            today.checked_add_signed(Duration::days(days))?,
            "Next week".into(),
        )
    } else if matches!(base, "next weekend" | "weekend") {
        let days = (5 - i64::from(today.weekday().num_days_from_monday())).rem_euclid(7);
        let days = if days == 0 { 7 } else { days };
        (
            today.checked_add_signed(Duration::days(days))?,
            "Next weekend".into(),
        )
    } else if matches!(base, "later today" | "evening" | "tonight") {
        if !explicit_time {
            time = NaiveTime::from_hms_opt(18, 0, 0)?;
        }
        let date = if calendar_time(today, time, now).is_some() {
            today
        } else {
            today.succ_opt()?
        };
        (date, "Later today".into())
    } else if let Some(day) = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ]
    .iter()
    .position(|day| {
        let name = base.strip_prefix("next ").unwrap_or(base);
        name.len() >= 3 && day.starts_with(name)
    }) {
        let days = (day as i64 - i64::from(today.weekday().num_days_from_monday())).rem_euclid(7);
        let days = if days == 0 { 7 } else { days };
        (today.checked_add_signed(Duration::days(days))?, base.into())
    } else {
        let date = if let Ok(date) = NaiveDate::parse_from_str(base, "%Y-%m-%d") {
            if calendar_time(date, time, now).is_some() {
                date
            } else {
                (today.year()..=today.year().checked_add(4)?)
                    .filter_map(|year| NaiveDate::from_ymd_opt(year, date.month(), date.day()))
                    .find(|date| calendar_time(*date, time, now).is_some())?
            }
        } else {
            let (first, second) = base.split_once(' ')?;
            let months = [
                "january",
                "february",
                "march",
                "april",
                "may",
                "june",
                "july",
                "august",
                "september",
                "october",
                "november",
                "december",
            ];
            let month = |name: &str| {
                months
                    .iter()
                    .position(|month| name.len() >= 3 && month.starts_with(name))
                    .map(|m| m as u32 + 1)
            };
            let (month, day) = if let Some(month) = month(first) {
                (month, second.parse::<u32>().ok()?)
            } else {
                (month(second)?, first.parse::<u32>().ok()?)
            };
            // Search also covers Feb 29 when this year is not a leap year.
            (today.year()..=today.year().checked_add(4)?)
                .filter_map(|year| NaiveDate::from_ymd_opt(year, month, day))
                .find(|date| calendar_time(*date, time, now).is_some())?
        };
        (date, base.into())
    };
    let at = calendar_time(date, time, now)?;
    let label = if explicit_time {
        format!("{label} {}", time.format("%-I:%M %p"))
    } else {
        label
    };
    Some(resolved(label, at, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn now() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 4, 14, 20, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn durations_and_previews() {
        let now = now();
        for query in ["2 days", "in 2 days", "2d", "IN 2 DAYS"] {
            let result = parse_hibernate_query(query, now).unwrap();
            assert_eq!(result.label, "in 2 days");
            assert_eq!(result.preview, "MON, 8:00 AM");
            assert_eq!(
                result.timestamp_ms,
                Local
                    .with_ymd_and_hms(2026, 7, 6, 8, 0, 0)
                    .single()
                    .unwrap()
                    .timestamp_millis()
            );
        }
        for query in ["15m", "15 min", "15 mins", "15 minutes", "in 15 minutes"] {
            let result = parse_hibernate_query(query, now).unwrap();
            assert_eq!(
                result.timestamp_ms,
                (now + Duration::minutes(15)).timestamp_millis()
            );
            assert_eq!(result.preview, "2:35 PM");
        }
        for query in ["3h", "3 hr", "3 hours", "in 3 hours"] {
            assert_eq!(
                parse_hibernate_query(query, now).unwrap().timestamp_ms,
                (now + Duration::hours(3)).timestamp_millis()
            );
        }
        for (query, month, day, hour, preview) in [
            ("1 week", 7, 11, 8, "SAT, JUL 11, 8:00 AM"),
            ("1w", 7, 11, 8, "SAT, JUL 11, 8:00 AM"),
            ("2 months", 9, 4, 8, "FRI, SEP 04, 8:00 AM"),
            ("2 days 3pm", 7, 6, 15, "MON, 3:00 PM"),
        ] {
            let result = parse_hibernate_query(query, now).unwrap();
            assert_eq!(
                result.timestamp_ms,
                Local
                    .with_ymd_and_hms(2026, month, day, hour, 0, 0)
                    .single()
                    .unwrap()
                    .timestamp_millis()
            );
            assert_eq!(result.preview, preview);
        }
    }

    #[test]
    fn named_dates_and_clocks() {
        let now = now();
        for (query, month, day, hour, minute) in [
            ("8 am", 7, 5, 8, 0),
            ("8am", 7, 5, 8, 0),
            ("8:30 am", 7, 5, 8, 30),
            ("14:00", 7, 5, 14, 0),
            ("5pm", 7, 4, 17, 0),
            ("5:30pm", 7, 4, 17, 30),
            ("tom", 7, 5, 8, 0),
            ("tomorrow", 7, 5, 8, 0),
            ("tomorrow 2pm", 7, 5, 14, 0),
            ("tomorrow 9am", 7, 5, 9, 0),
            ("next week", 7, 6, 8, 0),
            ("next w", 7, 6, 8, 0),
            ("next weekend", 7, 11, 8, 0),
            ("weekend", 7, 11, 8, 0),
            ("monday", 7, 6, 8, 0),
            ("mon", 7, 6, 8, 0),
            ("next fri 2pm", 7, 10, 14, 0),
            ("sat", 7, 11, 8, 0),
            ("later today", 7, 4, 18, 0),
            ("evening", 7, 4, 18, 0),
            ("tonight", 7, 4, 18, 0),
            ("aug 7", 8, 7, 8, 0),
            ("7 aug", 8, 7, 8, 0),
            ("dec 5", 12, 5, 8, 0),
            ("dec 5 2pm", 12, 5, 14, 0),
            ("2026-12-05", 12, 5, 8, 0),
        ] {
            let result = parse_hibernate_query(query, now).unwrap_or_else(|| panic!("{query}"));
            assert_eq!(
                result.timestamp_ms,
                Local
                    .with_ymd_and_hms(2026, month, day, hour, minute, 0)
                    .single()
                    .unwrap()
                    .timestamp_millis(),
                "{query}"
            );
        }
        assert_eq!(
            parse_hibernate_query("dec 5", now).unwrap().preview,
            "SAT, DEC 05, 8:00 AM"
        );
        assert_eq!(
            parse_hibernate_query("tomorrow", now).unwrap().preview,
            "SUN, 8:00 AM"
        );
        let late = now.with_hour(23).unwrap();
        assert_eq!(
            parse_hibernate_query("3 hours", late).unwrap().preview,
            "SUN, 2:20 AM"
        );
        assert_eq!(
            parse_hibernate_query("tonight", late).unwrap().preview,
            "SUN, 6:00 PM"
        );
    }

    #[test]
    fn calendar_days_preserve_local_time_across_dst_boundaries() {
        for (month, day) in [(3, 7), (3, 28), (10, 24), (10, 31)] {
            let now = Local
                .with_ymd_and_hms(2026, month, day, 14, 20, 0)
                .single()
                .unwrap();
            let date = now
                .date_naive()
                .checked_add_signed(Duration::days(2))
                .unwrap();
            let expected = Local
                .from_local_datetime(&date.and_hms_opt(8, 0, 0).unwrap())
                .single()
                .unwrap();
            assert_eq!(
                parse_hibernate_query("2 days", now).unwrap().timestamp_ms,
                expected.timestamp_millis()
            );
            assert_eq!(
                parse_hibernate_query("48 hours", now).unwrap().timestamp_ms,
                (now + Duration::hours(48)).timestamp_millis()
            );
            let tomorrow = now
                .date_naive()
                .succ_opt()
                .unwrap()
                .and_hms_opt(2, 30, 0)
                .unwrap();
            if Local.from_local_datetime(&tomorrow).earliest().is_none() {
                assert!(parse_hibernate_query("tomorrow 2:30am", now).is_none());
            }
        }
    }

    #[test]
    fn invalid_input_rollover_and_calendar_boundaries() {
        let now = now();
        for query in [
            "",
            "inbox",
            "reminder note",
            "0m",
            "-2 days",
            "25:00",
            "0am",
            "13pm",
            "8:99am",
            "feb 30",
            "2026-02-30",
            "999999999999 days",
            "4294967295 months",
            "2h 5pm",
            "tomorrow nonsense",
        ] {
            assert!(parse_hibernate_query(query, now).is_none(), "{query}");
        }
        let rolled = parse_hibernate_query("jan 1", now).unwrap();
        assert_eq!(
            rolled.timestamp_ms,
            Local
                .with_ymd_and_hms(2027, 1, 1, 8, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        );
        let end = Local
            .with_ymd_and_hms(2026, 1, 31, 12, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            parse_hibernate_query("1 month", end).unwrap().timestamp_ms,
            Local
                .with_ymd_and_hms(2026, 2, 28, 8, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            parse_hibernate_query("2026-01-01", now)
                .unwrap()
                .timestamp_ms,
            rolled.timestamp_ms
        );
    }
}
