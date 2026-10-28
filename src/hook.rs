//! Claude Code hooks. UserPromptSubmit adds rule reminders and the facts that
//! match the prompt; SessionStart adds pinned facts, re-adds reminders after
//! compaction and reports broken rule files; PostToolUse (Skill) and
//! UserPromptExpansion turn on the rule sets for a skill.
//! Stop runs the reply checks (see reply.rs); MessageDisplay labels a broken
//! draft on screen (see display.rs).
//! PreToolUse runs tool checks and the rules guard (see tools.rs).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::approvals::{self, Approvals, State};
use crate::config::{self, Config, MAX_BUDGET};
use crate::facts;
use crate::retrieve::estimate_tokens;
use crate::rules::{self, Level, Loaded};
use crate::session::{self, Event};

const PROMPT_TITLE: &str = "Facts from your memory that match this prompt, with the date each was last checked and its file. Open the file for the full entry.";
const SESSION_TITLE: &str = "dotonce is on. Pinned facts for every session:";
const RULES_TITLE: &str =
    "Rules from your rule sets for what you're doing now. Follow them in this reply:";

const EVENTS: &[&str] = &[
    "prompt",
    "session-start",
    "skill",
    "expansion",
    "stop",
    "display",
    "pre-tool",
];

/// Every hook dotonce needs: the Claude Code event, the `dotonce hook`
/// argument, and the matcher.
pub const HOOKS: &[(&str, &str, Option<&str>)] = &[
    ("UserPromptSubmit", "prompt", None),
    ("SessionStart", "session-start", None),
    ("Stop", "stop", None),
    ("PostToolUse", "skill", Some("Skill")),
    ("UserPromptExpansion", "expansion", None),
    (
        "PreToolUse",
        "pre-tool",
        Some("Bash|Write|Edit|MultiEdit|NotebookEdit"),
    ),
    ("MessageDisplay", "display", None),
];

/// Handles one Claude Code hook call. Returns the JSON to print, or None when
/// there is nothing to add.
pub fn handle(event: &str, input: &str, now: i64) -> Result<Option<String>> {
    if !EVENTS.contains(&event) {
        bail!("unknown hook event: {event}");
    }
    let v: Value = serde_json::from_str(input)?;
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    match event {
        "prompt" => prompt(&v, &cwd, now),
        "session-start" => session_start(&v, &cwd, now),
        "skill" => skill(&v, &cwd),
        "stop" => crate::reply::stop(&v, &cwd, now),
        "display" => crate::display::display(&v, &cwd),
        "pre-tool" => crate::tools::pre_tool(&v, &cwd, now),
        _ => expansion(&v, &cwd),
    }
}

pub fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub fn session_id(v: &Value) -> Result<&str> {
    let s = str_field(v, "session_id");
    if s.trim().is_empty() {
        bail!("the hook input has no session_id");
    }
    Ok(s)
}

pub fn prompt_id(v: &Value) -> Option<&str> {
    v.get("prompt_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub fn context(event: &str, block: &str) -> String {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": block}}).to_string()
}

/// The rules section of a `<dotonce>` block: whole reminders only, as many
/// as fit in `budget` tokens. Returns the text, the tokens it uses and the
/// ids of the sets whose reminder is in it.
pub fn reminder_section(rem: &[(String, String)], budget: usize) -> (String, usize, Vec<String>) {
    let mut body = String::new();
    let mut given = Vec::new();
    let mut used = estimate_tokens(RULES_TITLE) + 1;
    for (id, text) in rem {
        let item = format!("[{id}]\n{text}\n");
        let cost = estimate_tokens(&item);
        if used + cost > budget {
            log_error(&format!(
                "reminder for {id} skipped: over the {budget}-token cap"
            ));
            continue;
        }
        used += cost;
        body.push_str(&item);
        given.push(id.clone());
    }
    if body.is_empty() {
        return (String::new(), 0, given);
    }
    (format!("{RULES_TITLE}\n{body}"), used, given)
}

/// One `<dotonce>` block: the rules section first, then the facts block's
/// lines when there is one.
fn compose(rules: &str, facts: Option<String>) -> Option<String> {
    match (rules.is_empty(), facts) {
        (true, None) => None,
        (true, Some(f)) => Some(f),
        (false, None) => Some(format!("<dotonce>\n{rules}</dotonce>")),
        (false, Some(f)) => Some(f.replacen("<dotonce>\n", &format!("<dotonce>\n{rules}"), 1)),
    }
}

/// Turns on the sets this hook call matches and saves the session state.
/// With a budget, returns the rules section of the reminders not given yet
/// that fit in it, and its tokens, and marks only those reminders given.
fn turn_on(
    v: &Value,
    cfg: &Config,
    cwd: &Path,
    event: Event,
    budget: Option<usize>,
) -> Result<(String, usize)> {
    let (live, _) = approvals::load_live()?;
    if live.is_empty() {
        return Ok((String::new(), 0));
    }
    let sid = session_id(v)?;
    let dir = session::dir()?;
    let mut state = session::load(&dir, sid)?;
    session::refresh(&mut state, &live, cfg, cwd, prompt_id(v), event);
    let mut out = (String::new(), 0);
    if let Some(budget) = budget {
        let (text, used, given) =
            reminder_section(&session::pending_reminders(&state, &live), budget);
        session::mark_reminded(&mut state, &given);
        out = (text, used);
    }
    session::save(&dir, sid, &state)?;
    Ok(out)
}

/// Like `turn_on`, but a failure is logged and gives no reminders, so the
/// facts still arrive.
fn reminders_for(v: &Value, cfg: &Config, cwd: &Path, event: Event) -> (String, usize) {
    turn_on(v, cfg, cwd, event, Some(MAX_BUDGET)).unwrap_or_else(|e| {
        log_error(&format!("rules: {e:#}"));
        (String::new(), 0)
    })
}

fn prompt(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let text = str_field(v, "prompt");
    let file_cfg = config::load_file()?;
    let cfg = file_cfg.clone().unwrap_or_default();
    let (rules_text, used) = reminders_for(v, &cfg, cwd, Event::Prompt(text));
    // Without a config, dotonce isn't set up yet: leave no index behind.
    let facts_block = file_cfg.is_some().then(|| {
        facts_or_log(|| {
            let mut ix = facts::open()?;
            // A failed sync can leave an old entry in place, such as a fact
            // that was public before it was marked private, so nothing is
            // shown until it syncs.
            facts::sync(&mut ix, &cfg)?;
            let budget = cfg.prompt_budget.min(MAX_BUDGET.saturating_sub(used));
            let hits = facts::search(&ix, &cfg, text, cwd, now, budget)?;
            Ok((!hits.is_empty())
                .then(|| facts::render_block(PROMPT_TITLE, &hits, &self_command())))
        })
    });
    Ok(compose(&rules_text, facts_block.flatten()).map(|b| context("UserPromptSubmit", &b)))
}

/// The facts block from `f`. A failure, such as a locked index, is logged
/// and gives no facts, so the reminders, already marked given, still arrive.
fn facts_or_log(f: impl FnOnce() -> Result<Option<String>>) -> Option<String> {
    f().unwrap_or_else(|e| {
        log_error(&format!("facts: {e:#}"));
        None
    })
}

/// The last session's live sets, kept so the next session can say what
/// changed, and which set ids are private, so a set whose file is gone or
/// broken is still hidden outside the private paths.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct LastLive {
    #[serde(default)]
    live: BTreeMap<String, Level>,
    #[serde(default)]
    private: BTreeMap<String, bool>,
}

fn last_live_path() -> Result<PathBuf> {
    Ok(config::home_dir()?.join("last-live.json"))
}

fn save_last_live(record: &LastLive) -> Result<()> {
    let p = last_live_path()?;
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(record)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// Whether a rule file's text has a `private` key set to anything but
/// `false`. It works on lines, so it also reads a file that doesn't parse.
fn says_private(src: &str) -> bool {
    src.lines().any(|l| {
        l.trim_start()
            .strip_prefix("private")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .is_some_and(|value| value.trim() != "false")
    })
}

/// "1 private rule set file has errors", or the plural.
fn private_files(n: usize) -> String {
    if n == 1 {
        "1 private rule set file has errors.".to_string()
    } else {
        format!("{n} private rule set files have errors.")
    }
}

/// The SessionStart notice, one line per reason, and the record for the next
/// session. `raw` holds the text of each broken rule file that could be
/// read. `approved` is None when approvals.json can't be read; the record
/// then stays as it was.
///
/// Outside the private paths a set is named only when every source says it
/// is public. A loaded file and the record both have a say; a broken file
/// needs a readable file with no `private` key other than `false`, and the
/// record not saying private; a missing file has only the record.
fn compare(
    prev: Option<&LastLive>,
    loaded: &Loaded,
    raw: &BTreeMap<PathBuf, String>,
    approved: Option<&Approvals>,
    private_ok: bool,
    paused: &[String],
) -> (Vec<String>, LastLive) {
    let empty = Approvals::default();
    let a = approved.unwrap_or(&empty);
    let recorded = |id: &str| prev.and_then(|p| p.private.get(id)).copied();
    let mut next = LastLive {
        live: BTreeMap::new(),
        private: prev.map(|p| p.private.clone()).unwrap_or_default(),
    };
    for set in &loaded.sets {
        match a.state(set) {
            // The approved file says what the set is.
            State::On(_) | State::Off => {
                next.private.insert(set.id.clone(), set.private);
            }
            // A file nobody approved can make a set private, never public.
            State::Changed if set.private => {
                next.private.insert(set.id.clone(), true);
            }
            _ => {}
        }
    }

    let mut files = Vec::new();
    let mut private_broken = 0;
    let mut broken_ids = Vec::new();
    for e in &loaded.errors {
        let id = e
            .file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let raw_private = raw.get(&e.file).map(|s| says_private(s));
        if raw_private == Some(true) {
            next.private.insert(id.clone(), true);
        }
        let public = recorded(&id) != Some(true) && raw_private == Some(false);
        if private_ok || public {
            files.push(format!("{} ({})", e.name(), e.reason()));
        } else {
            private_broken += 1;
        }
        broken_ids.push(id);
    }

    let mut levels = Vec::new();
    let mut on_now = Vec::new();
    let mut private_levels = 0;
    let (mut off, mut changed, mut missing, mut held) = (vec![], vec![], vec![], vec![]);
    let mut private_stopped = 0;
    for id in a.sets.keys() {
        let set = loaded.sets.iter().find(|s| &s.id == id);
        let public = private_ok
            || match set {
                Some(s) => !s.private && recorded(id) != Some(true),
                None => recorded(id) == Some(false),
            };
        let stopped = match set.map(|s| a.state(s)) {
            Some(State::On(level)) => {
                next.live.insert(id.clone(), level);
                if paused.contains(id) {
                    Some(&mut held)
                } else {
                    match prev.map(|p| p.live.get(id)) {
                        Some(Some(old)) if *old != level && public => levels.push(format!(
                            "Rule set {id}: level changed from {} to {} since the last session.",
                            old.as_str(),
                            level.as_str()
                        )),
                        Some(None) if public => on_now.push(format!("{id} at {}", level.as_str())),
                        Some(Some(old)) if *old != level => private_levels += 1,
                        Some(None) => private_levels += 1,
                        _ => {}
                    }
                    None
                }
            }
            Some(State::Changed) => Some(&mut changed),
            Some(State::Off) => Some(&mut off),
            Some(State::NotApproved) => None,
            // Reported with the file errors, once.
            None if broken_ids.contains(id) => None,
            None => Some(&mut missing),
        };
        match stopped {
            Some(list) if public => list.push(id.clone()),
            Some(_) => private_stopped += 1,
            None => {}
        }
    }
    next.private.retain(|id, _| a.sets.contains_key(id));

    let mut lines = Vec::new();
    if approved.is_none() {
        lines.push("dotonce can't read approvals.json, so every rule set is off.".to_string());
    }
    lines.extend(levels);
    if !on_now.is_empty() {
        lines.push(format!(
            "On now, and not on last session: {}.",
            on_now.join(", ")
        ));
    }
    if private_levels > 0 {
        lines.push(format!(
            "Private rule sets with a new level since the last session: {private_levels}."
        ));
    }
    for (label, ids) in [
        ("Off", off),
        (
            "Changed since approval, run `dotonce rules approve <id>`",
            changed,
        ),
        ("Missing", missing),
        ("Paused in this session", held),
    ] {
        if !ids.is_empty() {
            lines.push(format!("{label}: {}.", ids.join(", ")));
        }
    }
    if private_stopped > 0 {
        lines.push(format!("Not running and private: {private_stopped}."));
    }
    if !files.is_empty() || private_broken > 0 {
        let mut line = String::new();
        if !files.is_empty() {
            line.push_str(&format!("File errors: {}.", files.join("; ")));
        }
        if private_broken > 0 {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&private_files(private_broken));
        }
        line.push_str(" Run `dotonce rules check` for details.");
        lines.push(line);
    }
    if approved.is_none() {
        next = prev.cloned().unwrap_or_default();
    }
    (lines, next)
}

/// The SessionStart notice for rule sets: approved sets that aren't running,
/// level changes since the last session, and rule files with errors. Each
/// set is listed every session it isn't running, not only after a change,
/// since the record of the last session is a file an agent could edit too.
/// Saves this session's record for the next one.
fn approval_lines(
    loaded: &Loaded,
    approved: Option<&Approvals>,
    cfg: &Config,
    cwd: &Path,
    paused: &[String],
) -> Vec<String> {
    let prev: Option<LastLive> = last_live_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    // A link could point outside the rules folder, so it counts as unreadable.
    let raw = loaded
        .errors
        .iter()
        .filter(|e| std::fs::symlink_metadata(&e.file).is_ok_and(|m| m.is_file()))
        .filter_map(|e| Some((e.file.clone(), std::fs::read_to_string(&e.file).ok()?)))
        .collect();
    let (lines, next) = compare(
        prev.as_ref(),
        loaded,
        &raw,
        approved,
        cfg.private_allowed(cwd),
        paused,
    );
    let has_sets = approved.is_some_and(|a| !a.sets.is_empty());
    if approved.is_some() && (has_sets || prev.is_some()) {
        if let Err(e) = save_last_live(&next) {
            log_error(&format!("rules: save last-live.json: {e:#}"));
        }
    }
    lines
}

/// Cleans old session files, reports rule sets that stopped running and rule
/// files with errors, and after compaction gives back the active sets'
/// reminders.
fn session_rules(v: &Value, cfg: &Config, cwd: &Path) -> Result<(String, usize, Option<String>)> {
    let dir = session::dir()?;
    session::clean_old(&dir, SystemTime::now());
    let loaded = rules::load()?;
    let approved = approvals::load()
        .map_err(|e| log_error(&format!("rules: {e:#}")))
        .ok();
    // A session that can't be loaded still gets the notice, with no paused sets.
    let state = session_id(v)
        .ok()
        .and_then(|sid| match session::load(&dir, sid) {
            Ok(s) => Some((sid, s)),
            Err(e) => {
                log_error(&format!("rules: {e:#}"));
                None
            }
        });
    let paused = state
        .as_ref()
        .map(|(_, s)| s.paused.clone())
        .unwrap_or_default();
    let lines = approval_lines(&loaded, approved.as_ref(), cfg, cwd, &paused);
    let system = (!lines.is_empty()).then(|| lines.join("\n"));
    if loaded.sets.is_empty() || str_field(v, "source") != "compact" {
        return Ok((String::new(), 0, system));
    }
    let live = approvals::live(loaded.sets, &approved.unwrap_or_default());
    let (text, used) = compact(v, cfg, cwd, &dir, &live, state).unwrap_or_else(|e| {
        log_error(&format!("rules: {e:#}"));
        (String::new(), 0)
    });
    Ok((text, used, system))
}

/// After compaction, the reminders the context lost: skill and folder sets,
/// and this turn's prompt-word sets.
fn compact(
    v: &Value,
    cfg: &Config,
    cwd: &Path,
    dir: &Path,
    live: &[approvals::Live],
    state: Option<(&str, session::State)>,
) -> Result<(String, usize)> {
    if live.is_empty() {
        return Ok((String::new(), 0));
    }
    let Some((sid, mut state)) = state else {
        bail!("the hook input has no usable session_id");
    };
    session::refresh(&mut state, live, cfg, cwd, prompt_id(v), Event::Other);
    let rem = session::compact_reminders(&state, live, prompt_id(v));
    let (text, used, given) = reminder_section(&rem, MAX_BUDGET);
    session::mark_reminded(&mut state, &given);
    session::save(dir, sid, &state)?;
    Ok((text, used))
}

fn session_start(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let file_cfg = config::load_file()?;
    let cfg = file_cfg.clone().unwrap_or_default();
    let (rules_text, used, system) = session_rules(v, &cfg, cwd).unwrap_or_else(|e| {
        log_error(&format!("rules: {e:#}"));
        (String::new(), 0, None)
    });
    let facts_block = file_cfg.is_some().then(|| {
        facts_or_log(|| {
            let mut ix = facts::open()?;
            facts::sync(&mut ix, &cfg)?;
            let budget = cfg.session_budget.min(MAX_BUDGET.saturating_sub(used));
            let hits = facts::pinned(&ix, &cfg, cwd, now, budget)?;
            Ok((!hits.is_empty())
                .then(|| facts::render_block(SESSION_TITLE, &hits, &self_command())))
        })
    });
    let facts_block = facts_block.flatten();
    let mut out = Map::new();
    if let Some(msg) = system {
        out.insert("systemMessage".to_string(), Value::String(msg));
    }
    if let Some(block) = compose(&rules_text, facts_block) {
        out.insert(
            "hookSpecificOutput".to_string(),
            json!({"hookEventName": "SessionStart", "additionalContext": block}),
        );
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(Value::Object(out).to_string()))
}

fn skill_named(v: &Value, cwd: &Path, name: &str, event_name: &str) -> Result<Option<String>> {
    if name.trim().is_empty() {
        return Ok(None);
    }
    let cfg = config::load_file()?.unwrap_or_default();
    let (text, _) = turn_on(v, &cfg, cwd, Event::Skill(name), Some(MAX_BUDGET))?;
    Ok(compose(&text, None).map(|b| context(event_name, &b)))
}

fn skill(v: &Value, cwd: &Path) -> Result<Option<String>> {
    if str_field(v, "tool_name") != "Skill" {
        return Ok(None);
    }
    let name = v
        .get("tool_input")
        .and_then(|t| t.get(session::SKILL_FIELD))
        .and_then(Value::as_str)
        .unwrap_or_default();
    skill_named(v, cwd, name, "PostToolUse")
}

fn expansion(v: &Value, cwd: &Path) -> Result<Option<String>> {
    skill_named(v, cwd, str_field(v, "command_name"), "UserPromptExpansion")
}

/// Quotes a word for the shell Claude Code runs commands in.
pub fn shell_word(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The running binary's path, quoted for the shell, or `dotonce` when the
/// path can't be found.
pub fn self_command() -> String {
    std::env::current_exe()
        .map(|p| shell_word(&p.to_string_lossy()))
        .unwrap_or_else(|_| "dotonce".to_string())
}

/// Appends one line to `errors.log` in the dotonce home. Never fails.
pub fn log_error(msg: &str) {
    let Ok(home) = config::home_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&home);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("errors.log"))
    {
        let line = msg.replace('\n', " | ");
        let _ = writeln!(f, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{FileError, parse};

    const BODY: &str = "[remind]\ntext = \"Keep it short.\"\n";

    fn set(id: &str, private: bool) -> rules::RuleSet {
        let src = format!("id = \"{id}\"\nprivate = {private}\n{BODY}");
        parse(&src, Path::new(&format!("/rules/{id}.toml"))).unwrap()
    }

    fn broken(id: &str) -> FileError {
        FileError {
            file: PathBuf::from(format!("/rules/{id}.toml")),
            error: "check 1 has no id".to_string(),
        }
    }

    /// Approvals for these sets as they are, at warn.
    fn approve_all(sets: &[rules::RuleSet]) -> Approvals {
        let mut a = Approvals::default();
        for s in sets {
            a.approve(s, "2026-10-28T00:00:00Z");
            a.sets.get_mut(&s.id).unwrap().level = Level::Warn;
        }
        a
    }

    /// The record a session with these sets on at warn would leave.
    fn record(ids: &[(&str, bool)]) -> LastLive {
        LastLive {
            live: ids
                .iter()
                .map(|(id, _)| (id.to_string(), Level::Warn))
                .collect(),
            private: ids.iter().map(|(id, p)| (id.to_string(), *p)).collect(),
        }
    }

    fn no_raw() -> BTreeMap<PathBuf, String> {
        BTreeMap::new()
    }

    #[test]
    fn says_private_reads_lines_of_any_file() {
        assert!(says_private("id = \"a\"\nprivate = true\n[[check]]"));
        assert!(says_private("  private=\"yes\""));
        assert!(says_private("private = false # not really"));
        assert!(!says_private("private = false\n"));
        assert!(!says_private("privately = true\nid = \"a\""));
        assert!(!says_private("[[check]]\n"));
    }

    #[test]
    fn each_reason_gets_its_own_line() {
        let sets = vec![set("on", false), set("off", false), set("held", false)];
        let mut a = approve_all(&sets);
        a.sets.get_mut("off").unwrap().off = true;
        let edited = parse(
            &format!("id = \"edited\"\n{BODY}"),
            Path::new("/rules/edited.toml"),
        )
        .unwrap();
        a.approve(&edited, "2026-10-28T00:00:00Z");
        let edited = parse(
            &format!("id = \"edited\"\n{BODY}# edited\n"),
            Path::new("/rules/edited.toml"),
        )
        .unwrap();
        a.approve(&set("gone", false), "2026-10-28T00:00:00Z");
        a.approve(&set("cracked", false), "2026-10-28T00:00:00Z");
        let mut all = sets;
        all.push(edited);
        let loaded = Loaded {
            sets: all,
            errors: vec![broken("cracked")],
        };
        let prev = record(&[
            ("on", false),
            ("off", false),
            ("held", false),
            ("edited", false),
            ("gone", false),
            ("cracked", false),
        ]);
        let raw = BTreeMap::from([(
            PathBuf::from("/rules/cracked.toml"),
            "id = \"cracked\"\n[[check]]\n".to_string(),
        )]);
        let (lines, next) = compare(
            Some(&prev),
            &loaded,
            &raw,
            Some(&a),
            false,
            &["held".to_string()],
        );
        assert_eq!(
            lines,
            [
                "Off: off.",
                "Changed since approval, run `dotonce rules approve <id>`: edited.",
                "Missing: gone.",
                "Paused in this session: held.",
                "File errors: cracked.toml (check 1 has no id). Run `dotonce rules check` for details.",
            ]
        );
        let live: Vec<&str> = next.live.keys().map(String::as_str).collect();
        assert_eq!(live, ["held", "on"], "paused sets stay in the record");
    }

    #[test]
    fn sets_with_the_same_reason_share_a_line() {
        let sets: Vec<_> = ["a", "b", "c"].iter().map(|id| set(id, false)).collect();
        let mut a = approve_all(&sets);
        for s in a.sets.values_mut() {
            s.off = true;
        }
        let loaded = Loaded {
            sets,
            errors: vec![],
        };
        let (lines, _) = compare(None, &loaded, &no_raw(), Some(&a), false, &[]);
        assert_eq!(lines, ["Off: a, b, c."]);
    }

    #[test]
    fn private_sets_are_counted_outside_private_paths_and_named_inside() {
        let sets = vec![set("open", false), set("quiet", true)];
        let mut a = approve_all(&sets);
        a.approve(&set("hidden-gone", true), "2026-10-28T00:00:00Z");
        for s in a.sets.values_mut() {
            s.off = true;
        }
        let loaded = Loaded {
            sets,
            errors: vec![],
        };
        let prev = record(&[("open", false), ("quiet", true), ("hidden-gone", true)]);
        let (lines, _) = compare(Some(&prev), &loaded, &no_raw(), Some(&a), false, &[]);
        assert_eq!(lines, ["Off: open.", "Not running and private: 2."]);
        let (lines, _) = compare(Some(&prev), &loaded, &no_raw(), Some(&a), true, &[]);
        assert_eq!(lines, ["Off: open, quiet.", "Missing: hidden-gone."]);
    }

    #[test]
    fn a_missing_set_with_no_record_is_hidden() {
        let mut a = Approvals::default();
        a.approve(&set("gone", false), "2026-10-28T00:00:00Z");
        let (lines, _) = compare(None, &Loaded::default(), &no_raw(), Some(&a), false, &[]);
        assert_eq!(lines, ["Not running and private: 1."]);
    }

    #[test]
    fn a_changed_file_can_hide_a_set_but_never_show_one() {
        let approved = set("s", true);
        let a = approve_all(std::slice::from_ref(&approved));
        let now_public = parse(
            &format!("id = \"s\"\nprivate = false\n{BODY}# edited\n"),
            Path::new("/rules/s.toml"),
        )
        .unwrap();
        let loaded = Loaded {
            sets: vec![now_public],
            errors: vec![],
        };
        let prev = record(&[("s", true)]);
        let (lines, next) = compare(Some(&prev), &loaded, &no_raw(), Some(&a), false, &[]);
        assert_eq!(lines, ["Not running and private: 1."]);
        assert_eq!(next.private.get("s"), Some(&true), "{next:?}");

        let a = approve_all(&[set("t", false)]);
        let now_private = parse(
            &format!("id = \"t\"\nprivate = true\n{BODY}"),
            Path::new("/rules/t.toml"),
        )
        .unwrap();
        let loaded = Loaded {
            sets: vec![now_private],
            errors: vec![],
        };
        let prev = record(&[("t", false)]);
        let (lines, next) = compare(Some(&prev), &loaded, &no_raw(), Some(&a), false, &[]);
        assert_eq!(lines, ["Not running and private: 1."]);
        assert_eq!(next.private.get("t"), Some(&true), "{next:?}");
    }

    #[test]
    fn a_broken_file_is_named_only_when_every_source_says_public() {
        let mut a = Approvals::default();
        a.approve(&set("x", false), "2026-10-28T00:00:00Z");
        let loaded = Loaded {
            sets: vec![],
            errors: vec![broken("x")],
        };
        let file = PathBuf::from("/rules/x.toml");
        let public = BTreeMap::from([(file.clone(), "id = \"x\"\n[[check]]\n".to_string())]);
        let private =
            BTreeMap::from([(file, "id = \"x\"\nprivate = true\n[[check]]\n".to_string())]);
        let named =
            "File errors: x.toml (check 1 has no id). Run `dotonce rules check` for details.";
        let counted = "1 private rule set file has errors. Run `dotonce rules check` for details.";
        let says_public = record(&[("x", false)]);
        let says_private = record(&[("x", true)]);
        let run = |prev: Option<&LastLive>, raw: &BTreeMap<PathBuf, String>, ok: bool| {
            compare(prev, &loaded, raw, Some(&a), ok, &[]).0
        };
        assert_eq!(run(Some(&says_public), &public, false), [named]);
        assert_eq!(run(None, &public, false), [named]);
        assert_eq!(run(Some(&says_public), &private, false), [counted]);
        assert_eq!(run(Some(&says_private), &public, false), [counted]);
        assert_eq!(
            run(Some(&says_public), &no_raw(), false),
            [counted],
            "unreadable: never on the record's word alone"
        );
        assert_eq!(run(Some(&says_private), &private, true), [named]);
        let (_, next) = compare(Some(&says_public), &loaded, &private, Some(&a), false, &[]);
        assert_eq!(next.private.get("x"), Some(&true));
    }

    #[test]
    fn a_broken_approved_set_is_reported_once() {
        let mut a = Approvals::default();
        a.approve(&set("x", false), "2026-10-28T00:00:00Z");
        let loaded = Loaded {
            sets: vec![],
            errors: vec![broken("x"), broken("y")],
        };
        let (lines, _) = compare(None, &loaded, &no_raw(), Some(&a), true, &[]);
        assert_eq!(
            lines,
            [
                "File errors: x.toml (check 1 has no id); y.toml (check 1 has no id). Run `dotonce rules check` for details."
            ]
        );
    }

    #[test]
    fn level_changes_get_a_line_per_set() {
        let sets = vec![
            set("up", false),
            set("down", false),
            set("new", false),
            set("quiet", true),
            set("same", false),
        ];
        let mut a = approve_all(&sets);
        a.sets.get_mut("up").unwrap().level = Level::Fix;
        a.sets.get_mut("down").unwrap().level = Level::Remind;
        a.sets.get_mut("quiet").unwrap().level = Level::Fix;
        let loaded = Loaded {
            sets,
            errors: vec![],
        };
        let prev = record(&[
            ("up", false),
            ("down", false),
            ("quiet", true),
            ("same", false),
        ]);
        let (lines, next) = compare(Some(&prev), &loaded, &no_raw(), Some(&a), false, &[]);
        assert_eq!(
            lines,
            [
                "Rule set down: level changed from warn to remind since the last session.",
                "Rule set up: level changed from warn to fix since the last session.",
                "On now, and not on last session: new at warn.",
                "Private rule sets with a new level since the last session: 1.",
            ]
        );
        assert_eq!(next.live.get("up"), Some(&Level::Fix));
        let (lines, _) = compare(None, &loaded, &no_raw(), Some(&a), false, &[]);
        assert!(lines.is_empty(), "no record, nothing to compare: {lines:?}");
    }

    #[test]
    fn a_paused_set_is_paused_not_a_level_change() {
        let sets = vec![set("p", false)];
        let mut a = approve_all(&sets);
        a.sets.get_mut("p").unwrap().level = Level::Fix;
        let loaded = Loaded {
            sets,
            errors: vec![],
        };
        let prev = record(&[("p", false)]);
        let paused = ["p".to_string()];
        let (lines, _) = compare(Some(&prev), &loaded, &no_raw(), Some(&a), false, &paused);
        assert_eq!(lines, ["Paused in this session: p."]);
    }

    #[test]
    fn unreadable_approvals_say_so_and_keep_the_record() {
        let loaded = Loaded {
            sets: vec![set("a", false)],
            errors: vec![broken("b")],
        };
        let prev = record(&[("a", false), ("b", false)]);
        let raw = BTreeMap::from([(PathBuf::from("/rules/b.toml"), "id = \"b\"\n".to_string())]);
        let (lines, next) = compare(Some(&prev), &loaded, &raw, None, false, &[]);
        assert_eq!(
            lines,
            [
                "dotonce can't read approvals.json, so every rule set is off.",
                "File errors: b.toml (check 1 has no id). Run `dotonce rules check` for details.",
            ]
        );
        assert_eq!(next, prev);
    }
}
