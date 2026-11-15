//! Runs a proposed rule set's checks over the last two weeks of Claude Code
//! session records, as if it had been on then, so the user sees what it
//! would have caught before saying yes. Hits in a folder where the behavior
//! is wanted are a conflict found by behavior.
//!
//! It reads only: nothing is written and nothing leaves the machine. Hits in
//! private folders are counted, but their folders and lines are shown only
//! when the command runs inside a private folder.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::checks::{self, Failure};
use crate::config::{self, Config};
use crate::rules::{self, On, RuleSet};
use crate::session::{prompt_match, skill_match};
use crate::skills::{self, Seen};
use crate::tools;

/// Days back the backtest reads.
pub const DAYS: u64 = 14;
/// How long it may read before it stops and says how far it got.
pub const BUDGET: Duration = Duration::from_secs(20);
const EXAMPLES: usize = 3;
/// Lines read between looks at the clock.
const CLOCK_EVERY: usize = 5_000;
const PRIVATE: &str = "a private folder";

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The session record's name, cut to 8 characters.
    pub session: String,
    pub day: String,
    pub folder: String,
    pub check: String,
    pub line: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub hits: usize,
    pub sessions: usize,
    /// Folders with hits and how many, most first.
    pub folders: Vec<(String, usize)>,
    /// Up to three hits, from different sessions when there are enough.
    pub examples: Vec<Hit>,
    /// Session records read, out of those in the window.
    pub read: usize,
    pub total: usize,
    pub stopped: bool,
}

impl Report {
    /// One line for the notice.
    pub fn line(&self) -> String {
        let mut s = format!(
            "Last {DAYS} days: {} in {}",
            plural(self.hits, "hit", "hits"),
            plural(self.sessions, "session", "sessions")
        );
        if !self.folders.is_empty() {
            let top: Vec<String> = self
                .folders
                .iter()
                .take(3)
                .map(|(f, n)| format!("{f} ({n})"))
                .collect();
            s.push_str(&format!(", in {}", top.join(", ")));
            if self.folders.len() > 3 {
                s.push_str(&format!(" and {} more", self.folders.len() - 3));
            }
        }
        s.push('.');
        if self.stopped {
            s.push_str(&format!(
                " Stopped after {} s, with {} of {} sessions read.",
                BUDGET.as_secs(),
                self.read,
                self.total
            ));
        }
        s
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Where Claude Code keeps session records.
pub fn projects_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".claude"),
    };
    Some(base.join("projects"))
}

/// The path with the home folder written as `~`.
fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && (p == h || p.starts_with(&format!("{h}/"))) => {
            format!("~{}", &p[h.len()..])
        }
        _ => p.to_string(),
    }
}

/// Claude Code names a project's folder after its path with every other
/// character made a dash, so both sides are compared that way.
fn dashed(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Session records changed in the last `DAYS`, newest first. For a set on
/// only in folders, only the projects that start in one of them.
fn records(dir: &Path, set: &RuleSet, now: SystemTime) -> Vec<PathBuf> {
    let since = now - Duration::from_secs(DAYS * 86_400);
    let folders_only =
        set.when.skills.is_empty() && !(set.when.check_on_prompt && !set.when.prompt.is_empty());
    let prefixes: Vec<String> = set
        .when
        .folders
        .iter()
        .flat_map(|f| {
            let e = config::expand(f.trim());
            [
                dashed(&e.to_string_lossy()),
                dashed(&config::resolve(&e).to_string_lossy()),
            ]
        })
        .collect();
    let mut out: Vec<(SystemTime, PathBuf)> = Vec::new();
    for project in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = dashed(&project.file_name().to_string_lossy());
        if folders_only && !prefixes.iter().any(|p| name.starts_with(p.as_str())) {
            continue;
        }
        for f in fs::read_dir(project.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(m) = f.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if m >= since {
                out.push((m, p));
            }
        }
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.0));
    out.into_iter().map(|(_, p)| p).collect()
}

/// What one session record does to the set as it is read.
struct Scan<'a> {
    set: &'a RuleSet,
    cfg: &'a Config,
    private_ok: bool,
    reply: bool,
    /// `"name":"<tool>"` for each tool the set checks.
    tool_needles: Vec<String>,
    /// Whether a folder is in scope and whether it is private, by path.
    folders: HashMap<String, (bool, bool)>,
    hits: usize,
    sessions: BTreeSet<String>,
    by_folder: BTreeMap<String, usize>,
    examples: Vec<Hit>,
}

/// The session's state while its record is read.
#[derive(Default)]
struct Session {
    id: String,
    skills: Vec<String>,
    prompted: bool,
    /// The turn's reply text so far, and where and when it was written.
    text: String,
    cwd: String,
    day: String,
}

impl Scan<'_> {
    fn folder(&mut self, cwd: &str) -> (bool, bool) {
        if let Some(v) = self.folders.get(cwd) {
            return *v;
        }
        let p = Path::new(cwd);
        let v = (
            self.set
                .when
                .folders
                .iter()
                .any(|f| config::path_inside(p, f)),
            self.cfg.private_allowed(p),
        );
        self.folders.insert(cwd.to_string(), v);
        v
    }

    fn on(&mut self, s: &Session, cwd: &str) -> bool {
        self.folder(cwd).0
            || s.skills
                .iter()
                .any(|k| skill_match(&self.set.when.skills, k).is_some())
            || (self.set.when.check_on_prompt && s.prompted)
    }

    fn add(&mut self, s: &Session, cwd: &str, day: &str, fails: Vec<Failure>) {
        if fails.is_empty() {
            return;
        }
        let private = self.folder(cwd).1 && !self.private_ok;
        let folder = if private {
            PRIVATE.to_string()
        } else {
            tilde(cwd)
        };
        for f in fails {
            self.hits += 1;
            self.sessions.insert(s.id.clone());
            *self.by_folder.entry(folder.clone()).or_default() += 1;
            let other_session = !self.examples.iter().any(|e| e.session == s.id);
            if !private && self.examples.len() < EXAMPLES && other_session {
                self.examples.push(Hit {
                    session: s.id.clone(),
                    day: day.to_string(),
                    folder: folder.clone(),
                    check: f.check,
                    line: rules::printable(&f.line),
                });
            }
        }
    }

    /// Runs the reply checks on the turn so far, once, as the Stop hook does.
    fn end_turn(&mut self, s: &mut Session) {
        if s.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut s.text);
        let cwd = s.cwd.clone();
        let seen = Seen::new(&s.skills, None, Path::new(&cwd));
        let fails = checks::run_reply(self.set, &text, &seen);
        let day = s.day.clone();
        self.add(s, &cwd, &day, fails);
    }

    fn assistant(&mut self, s: &mut Session, v: &Value) {
        for k in skills::loads_in(v) {
            if !s.skills.contains(&k) {
                s.skills.push(k);
            }
        }
        let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or_default();
        if cwd.is_empty() || !self.on(s, cwd) {
            return;
        }
        let day = v
            .get("timestamp")
            .and_then(Value::as_str)
            .map(|t| t.chars().take(10).collect())
            .unwrap_or_default();
        let blocks = v.pointer("/message/content").and_then(Value::as_array);
        for b in blocks.into_iter().flatten() {
            match b.get("type").and_then(Value::as_str) {
                Some("text") if self.reply => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        s.text.push_str(t);
                        s.text.push('\n');
                        s.cwd = cwd.to_string();
                        s.day.clone_from(&day);
                    }
                }
                Some("tool_use") => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    if let Some((path, body)) = tools::tool_text(name, &input) {
                        let fails = tools::run_tool(self.set, name, path.as_deref(), &body);
                        self.add(s, cwd, &day, fails);
                    }
                }
                _ => {}
            }
        }
    }

    fn user(&mut self, s: &mut Session, v: &Value) {
        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            for k in skills::loads_in(v) {
                if !s.skills.contains(&k) {
                    s.skills.push(k);
                }
            }
            return;
        }
        self.end_turn(s);
        if self.set.when.check_on_prompt && !s.prompted {
            let typed = match v.pointer("/message/content") {
                Some(Value::String(t)) => t.clone(),
                _ => String::new(),
            };
            s.prompted = prompt_match(&self.set.when.prompt, &typed).is_some();
        }
    }

    /// Reads one session record. False once the time is up.
    fn file(&mut self, path: &Path, started: Instant) -> bool {
        let Ok(f) = File::open(path) else {
            return true;
        };
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let mut s = Session {
            id: stem.chars().take(8).collect(),
            ..Session::default()
        };
        let mut r = BufReader::new(f);
        let mut buf = Vec::new();
        let mut n = 0;
        let skills = !self.set.when.skills.is_empty();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(k) if k > 0 => {}
                _ => break,
            }
            n += 1;
            if n % CLOCK_EVERY == 0 && started.elapsed() > BUDGET {
                return false;
            }
            let line = String::from_utf8_lossy(&buf);
            let assistant = line.contains("\"type\":\"assistant\"");
            // Tool results are most of the record's bytes and never start a
            // turn, so they're skipped unread.
            let user = !assistant
                && line.contains("\"type\":\"user\"")
                && !line.contains("\"tool_result\"");
            let wanted = user
                || (assistant
                    && ((self.reply && line.contains("\"type\":\"text\""))
                        || self.tool_needles.iter().any(|t| line.contains(t.as_str()))
                        || (skills && (line.contains("\"Skill\"") || line.contains("SKILL.md")))));
            if !wanted {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            match v.get("type").and_then(Value::as_str) {
                Some("assistant") => self.assistant(&mut s, &v),
                Some("user") if !tool_result(&v) => self.user(&mut s, &v),
                _ => {}
            }
        }
        self.end_turn(&mut s);
        true
    }
}

fn tool_result(v: &Value) -> bool {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// Runs `set` over the session records in `dir` from the last `DAYS`.
/// `private_ok` says whether private folders may be named in the report.
pub fn run(set: &RuleSet, dir: &Path, cfg: &Config, private_ok: bool, now: SystemTime) -> Report {
    let files = records(dir, set, now);
    let mut tool_needles: Vec<String> = set
        .checks
        .iter()
        .filter(|c| c.on == On::Tool)
        .filter_map(|c| c.tool.as_deref())
        .map(|t| format!("\"name\":\"{t}\""))
        .collect();
    tool_needles.dedup();
    let mut scan = Scan {
        set,
        cfg,
        private_ok,
        reply: set.checks.iter().any(|c| c.on == On::Reply),
        tool_needles,
        folders: HashMap::new(),
        hits: 0,
        sessions: BTreeSet::new(),
        by_folder: BTreeMap::new(),
        examples: Vec::new(),
    };
    let started = Instant::now();
    let mut read = 0;
    let mut stopped = false;
    for f in &files {
        if started.elapsed() > BUDGET || !scan.file(f, started) {
            stopped = true;
            break;
        }
        read += 1;
    }
    let mut folders: Vec<(String, usize)> = scan.by_folder.into_iter().collect();
    folders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Report {
        hits: scan.hits,
        sessions: scan.sessions.len(),
        folders,
        examples: scan.examples,
        read,
        total: files.len(),
        stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_compare_with_dashes() {
        assert_eq!(dashed("/Users/acme/my.site"), "-Users-acme-my-site");
    }

    #[test]
    fn the_line_names_the_top_folders() {
        let r = Report {
            hits: 5,
            sessions: 2,
            folders: vec![("~/acme".into(), 4), (PRIVATE.into(), 1)],
            ..Report::default()
        };
        assert_eq!(
            r.line(),
            "Last 14 days: 5 hits in 2 sessions, in ~/acme (4), a private folder (1)."
        );
    }
}
