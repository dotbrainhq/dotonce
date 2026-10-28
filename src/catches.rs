//! The catch log: one JSON line per check result. Lines for private sets go
//! to their own file, since set and check ids can hold private words. No
//! reply text is ever stored.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

use anyhow::Result;
use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};

use crate::config::home_dir;
use crate::hook::log_error;
use crate::rules::Level;

pub const PUBLIC_LOG: &str = "catches.jsonl";
pub const PRIVATE_LOG: &str = "catches-private.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Caught,
    Fixed,
    StillBroken,
    Unverified,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catch {
    pub time: String,
    pub session: String,
    pub set: String,
    pub check: String,
    pub level: Level,
    pub outcome: Outcome,
}

pub fn timestamp(now: i64) -> String {
    chrono::DateTime::from_timestamp(now, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Appends one line. The line is written in one call, so two hooks writing
/// at once don't interleave.
pub fn append(c: &Catch, private: bool) -> Result<()> {
    let home = home_dir()?;
    fs::create_dir_all(&home)?;
    let mut line = serde_json::to_string(c)?;
    line.push('\n');
    let name = if private { PRIVATE_LOG } else { PUBLIC_LOG };
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join(name))?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Appends, and logs a failure to errors.log instead of returning it, so a
/// full disk never stops a send-back.
pub fn record(c: &Catch, private: bool) {
    if let Err(e) = append(c, private) {
        log_error(&format!("catch log: {e:#}"));
    }
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Caught => "caught",
            Outcome::Fixed => "fixed",
            Outcome::StillBroken => "still_broken",
            Outcome::Unverified => "unverified",
            Outcome::Denied => "denied",
        }
    }
}

/// The catches in one log file's text, skipping empty and garbled lines.
fn parse_lines(src: &str) -> impl Iterator<Item = Catch> + '_ {
    src.lines()
        .filter_map(|l| serde_json::from_str::<Catch>(l).ok())
}

/// Every logged catch, oldest first. The private log is only read when
/// `private_ok`. Lines that don't parse are skipped.
pub fn read(private_ok: bool) -> Result<Vec<Catch>> {
    let home = home_dir()?;
    let files: &[&str] = if private_ok {
        &[PUBLIC_LOG, PRIVATE_LOG]
    } else {
        &[PUBLIC_LOG]
    };
    let mut out = Vec::new();
    for name in files {
        let Ok(src) = fs::read_to_string(home.join(name)) else {
            continue;
        };
        out.extend(parse_lines(&src));
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stat {
    pub set: String,
    pub check: String,
    pub week: usize,
    pub month: usize,
    pub fixed: usize,
    pub still_broken: usize,
    pub unverified: usize,
    pub denied: usize,
}

impl Stat {
    /// Catches per week over the last 30 days: the repeat-mistake rate.
    pub fn per_week(&self) -> f64 {
        self.month as f64 * 7.0 / 30.0
    }
}

const DAY: i64 = 86_400;

pub fn stats(catches: &[Catch], now: i64) -> Vec<Stat> {
    let mut map: BTreeMap<(String, String), Stat> = BTreeMap::new();
    for c in catches {
        let Ok(t) = chrono::DateTime::parse_from_rfc3339(&c.time) else {
            continue;
        };
        let age = now - t.timestamp();
        if !(0..=30 * DAY).contains(&age) {
            continue;
        }
        let s = map
            .entry((c.set.clone(), c.check.clone()))
            .or_insert_with(|| Stat {
                set: c.set.clone(),
                check: c.check.clone(),
                ..Stat::default()
            });
        match c.outcome {
            Outcome::Caught | Outcome::Denied => {
                s.month += 1;
                if age <= 7 * DAY {
                    s.week += 1;
                }
                if c.outcome == Outcome::Denied {
                    s.denied += 1;
                }
            }
            Outcome::Fixed => s.fixed += 1,
            Outcome::StillBroken => s.still_broken += 1,
            Outcome::Unverified => s.unverified += 1,
        }
    }
    map.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_are_one_json_line_with_snake_case_outcomes() {
        let c = Catch {
            time: timestamp(1_761_609_600),
            session: "s1".to_string(),
            set: "acme-replies".to_string(),
            check: "inline-links".to_string(),
            level: Level::Fix,
            outcome: Outcome::StillBroken,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"time":"2025-10-28T00:00:00Z","session":"s1","set":"acme-replies","check":"inline-links","level":"fix","outcome":"still_broken"}"#
        );
        assert_eq!(serde_json::from_str::<Catch>(&json).unwrap(), c);
    }

    #[test]
    fn reading_skips_garbage_and_empty_lines() {
        let good = r#"{"time":"2025-10-28T00:00:00Z","session":"s1","set":"acme","check":"c","level":"fix","outcome":"caught"}"#;
        let src = format!("{good}\nnot json at all\n\n{{\"time\":1}}\n{good}\n");
        let all: Vec<Catch> = parse_lines(&src).collect();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|c| c.set == "acme"));
        assert_eq!(parse_lines("").count(), 0);
        assert_eq!(parse_lines("\n\n").count(), 0);
    }

    #[test]
    fn stats_count_catches_per_check_over_7_and_30_days() {
        let now = 1_761_609_600;
        let c = |days: i64, check: &str, outcome: Outcome| Catch {
            time: timestamp(now - days * 86_400),
            session: "s".to_string(),
            set: "acme".to_string(),
            check: check.to_string(),
            level: Level::Fix,
            outcome,
        };
        let mut all = vec![
            c(1, "links", Outcome::Caught),
            c(2, "links", Outcome::Denied),
            c(10, "links", Outcome::Caught),
            c(10, "links", Outcome::Fixed),
            c(12, "links", Outcome::StillBroken),
            c(40, "links", Outcome::Caught),
            c(3, "opener", Outcome::Unverified),
        ];
        let mut bad = c(1, "links", Outcome::Caught);
        bad.time = "yesterday".to_string();
        all.push(bad);
        let s = stats(&all, now);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].set.as_str(), s[0].check.as_str()), ("acme", "links"));
        assert_eq!(
            (
                s[0].week,
                s[0].month,
                s[0].fixed,
                s[0].still_broken,
                s[0].unverified,
                s[0].denied
            ),
            (2, 3, 1, 1, 0, 1)
        );
        assert!((s[0].per_week() - 0.7).abs() < 1e-9);
        assert_eq!((s[1].month, s[1].unverified), (0, 1));
        assert_eq!(Outcome::StillBroken.as_str(), "still_broken");
    }
}
