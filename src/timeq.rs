use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime};

const DAY: i64 = 86_400;
const MONTHS: [&str; 12] = [
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

/// Unix seconds from the timestamp shapes the benchmark datasets use.
/// A weekday in parentheses ("2023/05/20 (Sat) 02:21") is dropped first.
pub fn parse_ts(raw: &str) -> Option<i64> {
    let stripped = match raw.find('(') {
        Some(i) => {
            let end = raw[i..].find(')').map_or(raw.len(), |j| i + j + 1);
            format!("{}{}", &raw[..i], &raw[end..])
        }
        None => raw.to_string(),
    };
    let s = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Ok(d) = DateTime::parse_from_rfc3339(&s) {
        return Some(d.timestamp());
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&s, f) {
            return Some(n.and_utc().timestamp());
        }
    }
    for f in ["%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(&s, f) {
            return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
        }
    }
    None
}

fn contains_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after = hay[i + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        before && after
    })
}

pub fn wants_latest(q: &str) -> bool {
    let l = q.to_lowercase();
    [
        "latest",
        "most recent",
        "currently",
        "current",
        "these days",
        "recently",
        "last time",
        "now",
    ]
    .iter()
    .any(|k| contains_word(&l, k))
}

fn number(w: &str) -> Option<i64> {
    if let Ok(n) = w.parse::<i64>() {
        return (n <= 10_000).then_some(n);
    }
    if w == "a" || w == "an" {
        return Some(1);
    }
    [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve",
    ]
    .iter()
    .position(|x| *x == w)
    .map(|p| p as i64)
}

fn unit_days(w: &str) -> Option<i64> {
    match w.trim_end_matches('s') {
        "day" => Some(1),
        "week" => Some(7),
        "month" => Some(30),
        "year" => Some(365),
        _ => None,
    }
}

fn day_start(d: NaiveDate) -> Option<i64> {
    Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

/// Time range a question points at, relative to when it was asked.
pub fn window(q: &str, anchor: Option<i64>) -> Option<Window> {
    let a = anchor?;
    let l = q.to_lowercase();
    if contains_word(&l, "yesterday") {
        return Some(Window {
            start: a - 2 * DAY,
            end: a,
        });
    }
    if contains_word(&l, "today") {
        return Some(Window {
            start: a - DAY,
            end: a + DAY,
        });
    }
    for (phrase, days) in [
        ("last week", 7),
        ("past week", 7),
        ("this week", 7),
        ("last month", 31),
        ("past month", 31),
        ("this month", 31),
        ("last year", 366),
        ("past year", 366),
    ] {
        if contains_word(&l, phrase) {
            return Some(Window {
                start: a - days * DAY,
                end: a,
            });
        }
    }
    let words: Vec<&str> = l
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for w in words.windows(3) {
        if w[2] == "ago"
            && let (Some(n), Some(u)) = (number(w[0]), unit_days(w[1]))
        {
            let half = (u / 2).max(1);
            return Some(Window {
                start: a - (n * u + half) * DAY,
                end: a - (n * u - half).max(0) * DAY,
            });
        }
    }
    let anchor_date = DateTime::from_timestamp(a, 0)?.date_naive();
    for (i, w) in words.iter().enumerate() {
        let Some(m) = MONTHS.iter().position(|x| x == w) else {
            continue;
        };
        let prev = if i > 0 { words[i - 1] } else { "" };
        let next = words.get(i + 1).copied().unwrap_or("");
        if *w == "may"
            && !matches!(prev, "in" | "of" | "early" | "late" | "mid")
            && next.parse::<u32>().is_err()
        {
            continue;
        }
        let month = m as u32 + 1;
        let year = words
            .iter()
            .filter_map(|x| x.parse::<i32>().ok())
            .find(|y| (1900..=2100).contains(y))
            .unwrap_or(if month > anchor_date.month() {
                anchor_date.year() - 1
            } else {
                anchor_date.year()
            });
        let start = NaiveDate::from_ymd_opt(year, month, 1)?;
        let end = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)?
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)?
        };
        return Some(Window {
            start: day_start(start)?,
            end: day_start(end)?,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dataset_timestamp_shapes() {
        assert_eq!(parse_ts("2023/05/20 (Sat) 02:21"), Some(1_684_549_260));
        assert_eq!(parse_ts("2023-05-20T02:21:00+00:00"), Some(1_684_549_260));
        assert_eq!(parse_ts("2023-05-20"), Some(1_684_540_800));
        assert_eq!(parse_ts("not a date"), None);
    }

    #[test]
    fn n_units_ago_gives_a_window_around_that_point() {
        let a = parse_ts("2023-06-01T00:00:00+00:00").unwrap();
        let w = window("What did I buy two weeks ago?", Some(a)).unwrap();
        assert_eq!(
            w,
            Window {
                start: a - 17 * DAY,
                end: a - 11 * DAY
            }
        );
    }

    #[test]
    fn month_name_gives_that_month() {
        let a = parse_ts("2023-08-01T00:00:00+00:00").unwrap();
        let w = window("What did I do in May?", Some(a)).unwrap();
        assert_eq!(w.start, parse_ts("2023-05-01").unwrap());
        assert_eq!(w.end, parse_ts("2023-06-01").unwrap());
    }

    #[test]
    fn may_as_a_verb_is_not_a_month() {
        let a = parse_ts("2023-08-01T00:00:00+00:00").unwrap();
        assert_eq!(window("May I ask what my dog is called?", Some(a)), None);
    }

    #[test]
    fn huge_number_ago_is_ignored() {
        let a = parse_ts("2023-06-01T00:00:00+00:00").unwrap();
        assert_eq!(
            window("What happened 100000000000000000 days ago?", Some(a)),
            None
        );
    }

    #[test]
    fn month_after_anchor_month_means_previous_year() {
        let a = parse_ts("2023-03-01").unwrap();
        let w = window("What did I do in October?", Some(a)).unwrap();
        assert_eq!(w.start, parse_ts("2022-10-01").unwrap());
        assert_eq!(w.end, parse_ts("2022-11-01").unwrap());
    }

    #[test]
    fn december_rolls_into_next_january() {
        let a = parse_ts("2023-12-20").unwrap();
        let w = window("What did I buy in December?", Some(a)).unwrap();
        assert_eq!(w.start, parse_ts("2023-12-01").unwrap());
        assert_eq!(w.end, parse_ts("2024-01-01").unwrap());
    }

    #[test]
    fn explicit_year_wins() {
        let a = parse_ts("2023-08-01").unwrap();
        let w = window("What did I do in May 2021?", Some(a)).unwrap();
        assert_eq!(w.start, parse_ts("2021-05-01").unwrap());
        assert_eq!(w.end, parse_ts("2021-06-01").unwrap());
    }

    #[test]
    fn yesterday_and_last_week() {
        let a = parse_ts("2023-06-01").unwrap();
        assert_eq!(
            window("What did I buy yesterday?", Some(a)),
            Some(Window {
                start: a - 2 * DAY,
                end: a
            })
        );
        assert_eq!(
            window("What did I buy last week?", Some(a)),
            Some(Window {
                start: a - 7 * DAY,
                end: a
            })
        );
    }

    #[test]
    fn no_anchor_means_no_window() {
        assert_eq!(window("What did I buy yesterday?", None), None);
    }

    #[test]
    fn detects_latest_questions() {
        assert!(wants_latest("What is my current job?"));
        assert!(wants_latest("Where do I live now?"));
        assert!(!wants_latest("Do you know my name?"));
    }
}
