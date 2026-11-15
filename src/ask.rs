//! A rule set proposed from a correction, with `rules propose --ask`. It
//! waits for the user's yes in the chat instead of turning on by itself,
//! and a yes turns it on at fix, since the user read it first. With no
//! answer it's dropped after `DAYS`. Without `--ask` a proposal takes the
//! autopilot's path, so a forgotten flag breaks nothing.
//!
//! What the notice shows with it sits next to the proposal as
//! `<id>.ask.json`: Claude's note on any instruction line that says the
//! opposite, the backtest, the overlaps with sets already on, and the
//! level a yes gives.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::approvals::{Approvals, State};
use crate::chat;
use crate::config;
use crate::rules::{self, CheckKind, Level, RuleSet, When};
use crate::session::skill_match;

/// Days an unanswered proposal waits before it's dropped.
pub const DAYS: i64 = 7;
const MAX_NOTE: usize = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub created: String,
    /// The level a yes turns the set on at.
    pub level: Level,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub backtest: String,
    #[serde(default)]
    pub overlaps: Vec<String>,
    /// Sets on already where this one would be.
    #[serde(default)]
    pub also_on: Vec<String>,
}

impl Ask {
    pub fn new(level: Level, note: &str, now: DateTime<Utc>) -> Ask {
        Ask {
            created: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            level,
            note: one_line(note, MAX_NOTE),
            backtest: String::new(),
            overlaps: Vec::new(),
            also_on: Vec::new(),
        }
    }

    /// True once `DAYS` have passed, or when the time can't be read.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        DateTime::parse_from_rfc3339(&self.created).map_or(true, |t| {
            now - t.with_timezone(&Utc) >= Duration::days(DAYS)
        })
    }
}

/// The text up to `max` characters, printable and on one line.
fn one_line(s: &str, max: usize) -> String {
    let one = rules::printable(&s.split_whitespace().collect::<Vec<_>>().join(" "));
    if one.chars().count() <= max {
        return one;
    }
    let mut t: String = one.chars().take(max).collect();
    t.push_str("...");
    t
}

pub fn path(id: &str) -> Result<PathBuf> {
    Ok(chat::proposals_dir()?.join(format!("{id}.ask.json")))
}

/// The proposal's ask, if it has one. A file that can't be parsed reads as
/// an ask with no time, which counts as expired: the set is dropped, never
/// turned on by itself.
pub fn load(id: &str) -> Option<Ask> {
    let src = fs::read_to_string(path(id).ok()?).ok()?;
    Some(serde_json::from_str(&src).unwrap_or_else(|_| Ask {
        created: String::new(),
        ..Ask::new(Level::Fix, "", Utc::now())
    }))
}

pub fn save(id: &str, ask: &Ask) -> Result<()> {
    let p = path(id)?;
    let dir = p.parent().context("no proposals folder")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{id}.ask.json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(ask)? + "\n")?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

/// Moves the ask to proposals/done next to its proposal, as `name`, so the
/// record of what was asked and how it ended stays. No ask, nothing to do.
pub fn archive(id: &str, name: &str) -> Result<()> {
    let p = path(id)?;
    if !p.exists() {
        return Ok(());
    }
    let done = chat::proposals_dir()?.join("done");
    fs::create_dir_all(&done)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let target = done.join(format!("{name}-{stamp}.ask.json"));
    match fs::rename(&p, &target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("move {}", p.display())),
    }
}

/// Where the set turns on, in plain words.
pub fn scope_text(w: &When) -> String {
    let mut parts = Vec::new();
    if !w.folders.is_empty() {
        parts.push(rules::printable(&w.folders.join(", ")));
    }
    if !w.skills.is_empty() {
        let s = if w.skills.len() == 1 {
            "skill"
        } else {
            "skills"
        };
        parts.push(format!("{s} {}", rules::printable(&w.skills.join(", "))));
    }
    if w.check_on_prompt && !w.prompt.is_empty() {
        parts.push(format!(
            "prompts with {}",
            rules::printable(&w.prompt.join(", "))
        ));
    }
    if parts.is_empty() {
        "nowhere: the set names no folder, skill or prompt word".to_string()
    } else {
        parts.join("; ")
    }
}

fn nested(a: &str, b: &str) -> bool {
    let (x, y) = (config::expand(a.trim()), config::expand(b.trim()));
    x.is_absolute() && y.is_absolute() && (x.starts_with(&y) || y.starts_with(&x))
}

/// On in every folder under home, such as `~`.
fn everywhere(w: &When) -> bool {
    let home = config::expand("~");
    w.folders.iter().any(|f| {
        let p = config::expand(f.trim());
        p.is_absolute() && home.starts_with(&p)
    })
}

fn has_scope(w: &When) -> bool {
    !w.folders.is_empty() || !w.skills.is_empty() || !w.prompt.is_empty()
}

/// Whether two sets can be on in the same place: nested folders, a shared
/// skill or prompt word, or one on everywhere.
pub fn scopes_overlap(a: &When, b: &When) -> bool {
    if !has_scope(a) || !has_scope(b) {
        return false;
    }
    everywhere(a)
        || everywhere(b)
        || a.folders
            .iter()
            .any(|x| b.folders.iter().any(|y| nested(x, y)))
        || a.skills.iter().any(|x| skill_match(&b.skills, x).is_some())
        || a.prompt
            .iter()
            .any(|x| b.prompt.iter().any(|y| x.eq_ignore_ascii_case(y)))
}

/// The phrases or the pattern a check looks for, to compare checks by.
fn looks_for(k: &CheckKind) -> Vec<String> {
    match k {
        CheckKind::Forbid(l) | CheckKind::Require(l) => {
            l.iter().map(|p| p.to_lowercase()).collect()
        }
        CheckKind::ForbidRegex(p) | CheckKind::RepeatOf { pattern: p, .. } => {
            vec![p.source().to_string()]
        }
        _ => Vec::new(),
    }
}

/// True when `forbid` would fail text that holds `phrase`.
fn forbids(forbid: &CheckKind, phrase: &str) -> bool {
    match forbid {
        CheckKind::Forbid(l) => l.iter().any(|p| p.eq_ignore_ascii_case(phrase)),
        CheckKind::ForbidRegex(p) => p.regex().is_ok_and(|re| re.is_match(phrase)),
        _ => false,
    }
}

fn kind_matches(a: &CheckKind, b: &CheckKind) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

#[derive(Debug, Default, PartialEq)]
pub struct Overlaps {
    /// Same id, duplicate checks and opposite checks, one line each.
    pub notes: Vec<String>,
    /// Sets on already where the new one would be.
    pub sets: Vec<String>,
    /// The same as `set/check` pairs, for Claude.
    pub checks: Vec<String>,
}

/// What the new set has in common with the sets on now. `hidden` holds the
/// private sets this folder can't name.
pub fn overlaps(
    new: &RuleSet,
    live: &[RuleSet],
    a: &Approvals,
    hidden: &BTreeSet<String>,
) -> Overlaps {
    let mut out = Overlaps::default();
    for s in live {
        let level = match a.state(s) {
            State::On(l) => l,
            _ => continue,
        };
        if hidden.contains(&s.id) {
            continue;
        }
        let id = rules::printable(&s.id);
        if s.id == new.id {
            let n = s.checks.len();
            out.notes.push(format!(
                "{id} is on now at {}, with {n} check{}: a yes replaces it.",
                level.as_str(),
                if n == 1 { "" } else { "s" }
            ));
            continue;
        }
        if !scopes_overlap(&new.when, &s.when) {
            continue;
        }
        out.sets.push(id.clone());
        for sc in &s.checks {
            out.checks
                .push(format!("{id}/{}", rules::printable(&sc.id)));
        }
        for nc in &new.checks {
            for sc in s.checks.iter().filter(|sc| sc.on == nc.on) {
                let theirs = format!("{id}/{}", rules::printable(&sc.id));
                let same: Vec<String> = looks_for(&nc.kind)
                    .into_iter()
                    .filter(|p| kind_matches(&nc.kind, &sc.kind) && looks_for(&sc.kind).contains(p))
                    .collect();
                if let Some(p) = same.first() {
                    out.notes.push(format!(
                        "{theirs} already checks {}: widening that set's scope may fit better \
                         than a new set.",
                        one_line(p, 60)
                    ));
                }
                let opposite = |forbid: &CheckKind, require: &CheckKind| match require {
                    CheckKind::Require(l) => l.iter().find(|p| forbids(forbid, p)).cloned(),
                    _ => None,
                };
                if let Some(p) = opposite(&nc.kind, &sc.kind) {
                    out.notes.push(format!(
                        "{theirs} requires \"{}\", which this set forbids.",
                        one_line(&p, 60)
                    ));
                }
                if let Some(p) = opposite(&sc.kind, &nc.kind) {
                    out.notes.push(format!(
                        "{theirs} forbids \"{}\", which this set requires.",
                        one_line(&p, 60)
                    ));
                }
            }
        }
    }
    out
}

/// The level a yes gives: the file's own `level` when it names one, or fix.
pub fn level_for(src: &str, set: &RuleSet) -> Level {
    let named = toml::from_str::<toml::Table>(src).is_ok_and(|t| t.contains_key("level"));
    if named { set.level } else { Level::Fix }
}

/// The ask's lines in the notice, indented like the set's details.
pub fn notice_lines(set: &RuleSet, ask: &Ask) -> String {
    let mut s = format!("  Scope: {}.\n", scope_text(&set.when));
    if !ask.backtest.is_empty() {
        s.push_str(&format!("  {}\n", ask.backtest));
    }
    for o in &ask.overlaps {
        s.push_str(&format!("  Overlap: {o}\n"));
    }
    if !ask.also_on.is_empty() {
        s.push_str(&format!("  Also on here: {}.\n", ask.also_on.join(", ")));
    }
    if !ask.note.is_empty() {
        s.push_str(&format!("  Claude's note: {}\n", ask.note));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn set(id: &str, body: &str) -> RuleSet {
        let file = format!("/r/{id}.toml");
        rules::parse(&format!("id = \"{id}\"\n{body}"), Path::new(&file)).unwrap()
    }

    #[test]
    fn scopes_overlap_by_folder_skill_or_everywhere() {
        let a = set("a", "[when]\nfolders = [\"/acme/site\"]\n").when;
        let b = set("b", "[when]\nfolders = [\"/acme\"]\n").when;
        let c = set("c", "[when]\nfolders = [\"/other\"]\n").when;
        let d = set("d", "[when]\nskills = [\"acme-writer\"]\n").when;
        let e = set(
            "e",
            "[when]\nskills = [\"acme-writer\"]\nfolders = [\"/x\"]\n",
        )
        .when;
        let none = set("n", "").when;
        assert!(scopes_overlap(&a, &b));
        assert!(!scopes_overlap(&a, &c));
        assert!(!scopes_overlap(&a, &d));
        assert!(scopes_overlap(&d, &e));
        assert!(!scopes_overlap(&a, &none));
    }

    #[test]
    fn a_yes_gives_fix_unless_the_file_names_a_level() {
        let src = "id = \"a\"\n";
        assert_eq!(level_for(src, &set("a", "")), Level::Fix);
        let src = "id = \"a\"\nlevel = \"warn\"\n";
        let s = rules::parse(src, Path::new("/r/a.toml")).unwrap();
        assert_eq!(level_for(src, &s), Level::Warn);
    }

    #[test]
    fn an_ask_expires_after_a_week_or_when_its_time_is_unreadable() {
        let now = Utc::now();
        let a = Ask::new(Level::Fix, "a  note\nhere", now - Duration::days(6));
        assert_eq!(a.note, "a note here");
        assert!(!a.expired(now));
        assert!(Ask::new(Level::Fix, "", now - Duration::days(7)).expired(now));
        let mut b = a.clone();
        b.created = String::new();
        assert!(b.expired(now));
    }
}
