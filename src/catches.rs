//! The catch log: one JSON line per check result. Lines for private sets go
//! to their own file, since set and check ids can hold private words. No
//! reply text is ever stored.

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
}
