//! What the judge keeps: judge.log, one line per call with no draft text,
//! and a file per session with today's calls and the drafts already judged.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::approvals;
use crate::config;
use crate::hook::log_error;
use crate::session;

pub const LOG: &str = "judge.log";
pub const PRIVATE_LOG: &str = "judge-private.log";
/// Drafts remembered as judged, per session.
const MAX_JUDGED: usize = 200;

/// Per session: the drafts already judged and today's calls.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Judged {
    #[serde(default)]
    pub(super) day: String,
    #[serde(default)]
    pub(super) calls: u32,
    #[serde(default)]
    pub(super) keys: Vec<String>,
}

pub(super) fn judged_path(dir: &Path, sid: &str) -> Result<PathBuf> {
    Ok(dir.join(format!("{}.judge.json", session::safe_name(sid)?)))
}

pub(super) fn load_judged(path: &Path) -> Judged {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Adds a call and the drafts it judged to the session's file. Read again
/// right before the write, since Stop and the background hook share it.
pub(super) fn note_judged(path: &Path, day: &str, called: bool, keys: &[String]) -> Result<()> {
    let mut j = load_judged(path);
    if j.day != day {
        j.day = day.to_string();
        j.calls = 0;
    }
    if called {
        j.calls += 1;
    }
    for k in keys {
        if !j.keys.contains(k) {
            j.keys.push(k.clone());
        }
    }
    let over = j.keys.len().saturating_sub(MAX_JUDGED);
    j.keys.drain(..over);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string(&j)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The key a draft is remembered by for one set.
pub(super) fn key(set: &str, text: &str) -> String {
    approvals::hash(&format!("{set}\n{text}"))[..16].to_string()
}

/// One judge.log line. No draft text: set ids, counts and times only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub time: String,
    pub session: String,
    pub sets: Vec<String>,
    pub checks: usize,
    pub drafts: usize,
    /// pass, caught, unquoted, cap, or a failure's kind.
    pub result: String,
    pub ms: u64,
    #[serde(rename = "in")]
    pub tokens_in: u64,
    #[serde(rename = "out")]
    pub tokens_out: u64,
    #[serde(default)]
    pub breaks: usize,
    #[serde(default)]
    pub unquoted: usize,
}

impl LogLine {
    /// Whether the call let the draft through without a verdict.
    pub fn failed(&self) -> bool {
        !matches!(self.result.as_str(), "pass" | "caught" | "unquoted")
    }
}

pub(super) fn write_log(line: &LogLine, private: bool) {
    let res = (|| -> Result<()> {
        let home = config::home_dir()?;
        fs::create_dir_all(&home)?;
        let name = if private { PRIVATE_LOG } else { LOG };
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join(name))?;
        writeln!(f, "{}", serde_json::to_string(line)?)?;
        Ok(())
    })();
    if let Err(e) = res {
        log_error(&format!("judge log: {e:#}"));
    }
}

/// Every judge.log line, oldest first, with the private log's when
/// `private_ok`. Lines that don't parse are skipped.
pub fn read_log(private_ok: bool) -> Vec<LogLine> {
    let Ok(home) = config::home_dir() else {
        return Vec::new();
    };
    let names: &[&str] = if private_ok {
        &[LOG, PRIVATE_LOG]
    } else {
        &[LOG]
    };
    let mut out: Vec<LogLine> = names
        .iter()
        .flat_map(|n| {
            fs::read_to_string(home.join(n))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect::<Vec<LogLine>>()
        })
        .collect();
    out.sort_by(|a, b| a.time.cmp(&b.time));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_judged_file_keeps_the_day_count_and_the_last_keys() {
        let dir = std::env::temp_dir().join(format!("dotonce-judged-{}", std::process::id()));
        let path = dir.join("s1.judge.json");
        note_judged(&path, "2026-11-15", true, &["a".to_string()]).unwrap();
        note_judged(
            &path,
            "2026-11-15",
            true,
            &["a".to_string(), "b".to_string()],
        )
        .unwrap();
        let j = load_judged(&path);
        assert_eq!((j.day.as_str(), j.calls), ("2026-11-15", 2));
        assert_eq!(j.keys, ["a", "b"]);
        note_judged(&path, "2026-11-16", false, &[]).unwrap();
        assert_eq!(load_judged(&path).calls, 0, "a new day starts at zero");
        let many: Vec<String> = (0..MAX_JUDGED + 5).map(|i| i.to_string()).collect();
        note_judged(&path, "2026-11-16", false, &many).unwrap();
        let j = load_judged(&path);
        assert_eq!(j.keys.len(), MAX_JUDGED);
        assert_eq!(j.keys.last().map(String::as_str), Some("204"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
