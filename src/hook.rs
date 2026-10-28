//! Claude Code hooks. UserPromptSubmit adds rule reminders and the facts that
//! match the prompt; SessionStart adds pinned facts, re-adds reminders after
//! compaction and reports broken rule files; PostToolUse (Skill) and
//! UserPromptExpansion turn on the rule sets for a skill.

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
use crate::rules::{self, FileError, Level, Loaded};
use crate::session::{self, Event};

const PROMPT_TITLE: &str = "Facts from your memory that match this prompt, with the date each was last checked and its file. Open the file for the full entry.";
const SESSION_TITLE: &str = "dotonce is on. Pinned facts for every session:";
const RULES_TITLE: &str =
    "Rules from your rule sets for what you're doing now. Follow them in this reply:";

const EVENTS: &[&str] = &["prompt", "session-start", "skill", "expansion"];

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
/// as fit in `budget` tokens. Returns the text and the tokens it uses.
pub fn reminder_section(rem: &[(String, String)], budget: usize) -> (String, usize) {
    let mut body = String::new();
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
    }
    if body.is_empty() {
        return (String::new(), 0);
    }
    (format!("{RULES_TITLE}\n{body}"), used)
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
/// With `remind`, returns the reminders not given yet and marks them given.
fn turn_on(
    v: &Value,
    cfg: &Config,
    cwd: &Path,
    event: Event,
    remind: bool,
) -> Result<Vec<(String, String)>> {
    let (live, _) = approvals::load_live()?;
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let sid = session_id(v)?;
    let dir = session::dir()?;
    let mut state = session::load(&dir, sid)?;
    session::refresh(&mut state, &live, cfg, cwd, prompt_id(v), event);
    let out = if remind {
        session::take_reminders(&mut state, &live)
    } else {
        Vec::new()
    };
    session::save(&dir, sid, &state)?;
    Ok(out)
}

/// Like `turn_on`, but a failure is logged and gives no reminders, so the
/// facts still arrive.
fn reminders_for(v: &Value, cfg: &Config, cwd: &Path, event: Event) -> Vec<(String, String)> {
    turn_on(v, cfg, cwd, event, true).unwrap_or_else(|e| {
        log_error(&format!("rules: {e:#}"));
        Vec::new()
    })
}

fn prompt(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let text = str_field(v, "prompt");
    let file_cfg = config::load_file()?;
    let cfg = file_cfg.clone().unwrap_or_default();
    let rem = reminders_for(v, &cfg, cwd, Event::Prompt(text));
    let (rules_text, used) = reminder_section(&rem, MAX_BUDGET);
    let mut facts_block = None;
    // Without a config, dotonce isn't set up yet: leave no index behind.
    if file_cfg.is_some() {
        let mut ix = facts::open()?;
        // A failed sync can leave an old entry in place, such as a fact that
        // was public before it was marked private, so nothing is shown until
        // it syncs.
        facts::sync(&mut ix, &cfg)?;
        let budget = cfg.prompt_budget.min(MAX_BUDGET.saturating_sub(used));
        let hits = facts::search(&ix, &cfg, text, cwd, now, budget)?;
        if !hits.is_empty() {
            facts_block = Some(facts::render_block(PROMPT_TITLE, &hits, &self_command()));
        }
    }
    Ok(compose(&rules_text, facts_block).map(|b| context("UserPromptSubmit", &b)))
}

fn file_errors_message(errors: &[FileError]) -> Option<String> {
    if errors.is_empty() {
        return None;
    }
    let list: Vec<String> = errors.iter().map(FileError::line).collect();
    Some(format!(
        "dotonce skipped {} rule file(s) with errors, so those sets are off: {}. Run dotonce rules check for details.",
        errors.len(),
        list.join("; ")
    ))
}

/// The last session's live sets, kept so the next session can say what
/// changed, and which set ids are private, so a set whose file is gone or
/// broken is still hidden outside the private paths.
#[derive(Debug, Default, Serialize, Deserialize)]
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

/// One line for each approved set that isn't running now, and for each live
/// set whose level changed since the last session, so a change the user
/// didn't make, such as an edited rule file, shows on their screen. Sets that
/// aren't running are listed every session, not only after a change, since
/// the record of the last session is a file an agent could edit too. Saves
/// this session's live sets for the next one.
fn approval_lines(
    loaded: &Loaded,
    approved: &Approvals,
    cfg: &Config,
    cwd: &Path,
    paused: &[String],
) -> Vec<String> {
    let prev: Option<LastLive> = last_live_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    if approved.sets.is_empty() && prev.is_none() {
        return Vec::new();
    }
    let mut next = LastLive {
        live: BTreeMap::new(),
        private: prev.as_ref().map(|p| p.private.clone()).unwrap_or_default(),
    };
    for set in &loaded.sets {
        next.private.insert(set.id.clone(), set.private);
    }
    next.private.retain(|id, _| approved.sets.contains_key(id));
    let private_ok = cfg.private_allowed(cwd);
    let mut lines = Vec::new();
    for id in approved.sets.keys() {
        let set = loaded.sets.iter().find(|s| &s.id == id);
        let note = match set.map(|s| approved.state(s)) {
            Some(State::On(level)) => {
                next.live.insert(id.clone(), level);
                let before = prev.as_ref().map(|p| p.live.get(id));
                if paused.contains(id) {
                    Some("not running, it's paused in this session".to_string())
                } else {
                    match before {
                        Some(Some(old)) if *old != level => Some(format!(
                            "level changed from {} to {} since the last session",
                            old.as_str(),
                            level.as_str()
                        )),
                        Some(None) => Some(format!(
                            "on at {} now, and it wasn't on last session",
                            level.as_str()
                        )),
                        _ => None,
                    }
                }
            }
            Some(State::Changed) => Some("not running, its file changed since approval".into()),
            Some(State::Off) => Some("not running, it's off".into()),
            Some(State::NotApproved) => None,
            None if loaded
                .errors
                .iter()
                .any(|e| e.file.file_stem().is_some_and(|s| s == id.as_str())) =>
            {
                Some("not running, its file has errors".into())
            }
            None => Some("not running, its file is missing".into()),
        };
        let Some(note) = note else { continue };
        // A set whose privacy is unknown, such as one never loaded, is hidden.
        if private_ok || next.private.get(id) == Some(&false) {
            lines.push(format!("Rule set {id}: {note}."));
        } else {
            lines.push(format!("A private rule set: {note}."));
        }
    }
    if let Err(e) = save_last_live(&next) {
        log_error(&format!("rules: save last-live.json: {e:#}"));
    }
    lines
}

/// Cleans old session files, reports rule sets that stopped running and rule
/// files with errors, and after compaction gives back every active set's
/// reminder.
fn session_rules(v: &Value, cfg: &Config, cwd: &Path) -> Result<(String, usize, Option<String>)> {
    let dir = session::dir()?;
    session::clean_old(&dir, SystemTime::now());
    let loaded = rules::load()?;
    let mut lines = Vec::new();
    let approved = match approvals::load() {
        Ok(a) => a,
        Err(e) => {
            log_error(&format!("rules: {e:#}"));
            lines.push("dotonce can't read approvals.json, so every rule set is off.".to_string());
            Approvals::default()
        }
    };
    let mut state = match session_id(v) {
        Ok(sid) => Some((sid, session::load(&dir, sid)?)),
        Err(_) => None,
    };
    let paused = state
        .as_ref()
        .map(|(_, s)| s.paused.clone())
        .unwrap_or_default();
    lines.extend(approval_lines(&loaded, &approved, cfg, cwd, &paused));
    lines.extend(file_errors_message(&loaded.errors));
    let system = (!lines.is_empty()).then(|| lines.join("\n"));
    if loaded.sets.is_empty() || str_field(v, "source") != "compact" {
        return Ok((String::new(), 0, system));
    }
    let live = approvals::live(loaded.sets, &approved);
    if live.is_empty() {
        return Ok((String::new(), 0, system));
    }
    let Some((sid, state)) = state.as_mut() else {
        bail!("the hook input has no session_id");
    };
    session::refresh(state, &live, cfg, cwd, prompt_id(v), Event::Other);
    let rem = session::all_reminders(state, &live);
    session::save(&dir, sid, state)?;
    let (text, used) = reminder_section(&rem, MAX_BUDGET);
    Ok((text, used, system))
}

fn session_start(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let file_cfg = config::load_file()?;
    let cfg = file_cfg.clone().unwrap_or_default();
    let (rules_text, used, system) = session_rules(v, &cfg, cwd).unwrap_or_else(|e| {
        log_error(&format!("rules: {e:#}"));
        (String::new(), 0, None)
    });
    let mut facts_block = None;
    if file_cfg.is_some() {
        let mut ix = facts::open()?;
        facts::sync(&mut ix, &cfg)?;
        let budget = cfg.session_budget.min(MAX_BUDGET.saturating_sub(used));
        let hits = facts::pinned(&ix, &cfg, cwd, now, budget)?;
        if !hits.is_empty() {
            facts_block = Some(facts::render_block(SESSION_TITLE, &hits, &self_command()));
        }
    }
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
    let rem = turn_on(v, &cfg, cwd, Event::Skill(name), true)?;
    let (text, _) = reminder_section(&rem, MAX_BUDGET);
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
