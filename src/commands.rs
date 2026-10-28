//! The `dotonce rules`, `stats` and `doctor` commands.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::SecondsFormat;
use serde::Serialize;
use serde_json::{Value, json};

use crate::approvals;
use crate::catches;
use crate::checks::{self, Failure};
use crate::config;
use crate::hook;
use crate::retrieve::estimate_tokens;
use crate::rules::{self, Level, RuleSet, printable};
use crate::session::{self, Event, State, Trigger};
use crate::tools;

/// A reminder this long leaves little room under the hooks' 2,000-token cap.
const LONG_REMINDER: usize = 1_800;

/// Writes a starter rule set. `whole` picks whole replies over fenced
/// drafts; when it is None the user is asked on `input`, and an empty answer
/// means drafts.
pub fn rules_new(id: &str, whole: Option<bool>, input: &mut dyn BufRead) -> Result<PathBuf> {
    if !rules::valid_id(id) {
        bail!("the id {id:?} can only use a-z, 0-9, - and _");
    }
    let dir = rules::rules_dir()?;
    let path = dir.join(format!("{id}.toml"));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let whole = match whole {
        Some(w) => w,
        None => {
            println!("Should {id} check fenced drafts in replies (d) or whole replies (w)? [d]");
            let mut line = String::new();
            input.read_line(&mut line)?;
            line.trim().eq_ignore_ascii_case("w")
        }
    };
    fs::create_dir_all(&dir)?;
    fs::write(&path, rules::starter(id, whole))?;
    if whole {
        println!("This set checks whole replies.");
    }
    Ok(path)
}

/// Prints one line per rule file and fails when any file has an error.
pub fn rules_check() -> Result<()> {
    let loaded = rules::load()?;
    if loaded.sets.is_empty() && loaded.errors.is_empty() {
        println!("no rule sets in {}", rules::rules_dir()?.display());
        return Ok(());
    }
    for s in &loaded.sets {
        println!("ok     {} ({} checks)", s.id, s.checks.len());
        if let Some(r) = &s.remind {
            // The cost the hooks count for it: `[id]`, the text, two newlines.
            let tokens = estimate_tokens(&format!("[{}]\n{}\n", s.id, r.text.trim()));
            if tokens > LONG_REMINDER {
                println!(
                    "warn   {}: its reminder is about {tokens} tokens, over 1,800, so it may not fit under the hooks' 2,000-token cap",
                    s.id
                );
            }
        }
    }
    for e in &loaded.errors {
        println!("error  {}: {}", e.file.display(), e.error);
    }
    if !loaded.errors.is_empty() {
        bail!(
            "{} rule file(s) have errors, and dotonce skips them until they're fixed",
            loaded.errors.len()
        );
    }
    Ok(())
}

/// The set with this id. A file with an error is named with its error, so
/// the user sees why the set is missing.
fn find_set(id: &str) -> Result<RuleSet> {
    let mut loaded = rules::load()?;
    if let Some(i) = loaded.sets.iter().position(|s| s.id == id) {
        return Ok(loaded.sets.swap_remove(i));
    }
    if let Some(e) = loaded
        .errors
        .iter()
        .find(|e| e.file.file_stem().is_some_and(|s| s == id))
    {
        bail!(
            "{} has an error, fix it first: {}",
            e.file.display(),
            e.error
        );
    }
    bail!("no rule set {id} in {}", rules::rules_dir()?.display())
}

#[derive(Serialize)]
struct TestReport<'a> {
    drafts: usize,
    failures: &'a [Failure],
}

fn print_failures(fails: &[Failure]) {
    if fails.is_empty() {
        println!("passes: no check fails");
    }
    for f in fails {
        println!("{}: \"{}\"\n  {}", f.check, f.line, f.why);
    }
}

/// Runs a set's checks on a text file, approved or not: the reply checks, or
/// with `tool` that tool's checks, reading the file as the tool's text.
pub fn rules_test(
    id: &str,
    file: &Path,
    json: bool,
    tool: Option<&str>,
    path: Option<&str>,
) -> Result<()> {
    let set = find_set(id)?;
    let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let (drafts, fails) = match tool {
        Some(t) => (1, tools::run_tool(&set, t, path, &text)),
        None => (
            checks::drafts(&set, &text).len(),
            checks::run_reply(&set, &text),
        ),
    };
    if json {
        let report = TestReport {
            drafts,
            failures: &fails,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if drafts == 0 {
        let fences = set
            .draft
            .as_ref()
            .map(|d| d.fence.join(", "))
            .unwrap_or_default();
        println!(
            "no drafts in {}: {id} checks fenced blocks marked {fences}",
            file.display()
        );
        return Ok(());
    }
    print_failures(&fails);
    Ok(())
}

/// The terminal check and the typed id, read from this process's stdin.
fn confirmed(id: &str, what: &str) -> Result<()> {
    approvals::confirm(id, what, approvals::on_terminal(), || {
        print!("To {what}, type the set's id ({id}) and press Return: ");
        io::stdout().flush()?;
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        Ok(s)
    })
}

fn now_text() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn rules_approve(id: &str) -> Result<()> {
    approvals::refuse_agent()?;
    let set = find_set(id)?;
    print!("{}", approvals::summary(&set, &approvals::load()?));
    confirmed(id, &format!("approve {id}"))?;
    // Read again: the prompt can wait a long time, and saving the copy from
    // before it would drop any change made meanwhile.
    let mut a = approvals::load()?;
    let level = a.approve(&set, &now_text());
    approvals::save(&a)?;
    println!("approved {id} at level {}", level.as_str());
    Ok(())
}

pub fn rules_level(id: &str, level: &str) -> Result<()> {
    approvals::refuse_agent()?;
    let level = Level::parse(level)?;
    let set = find_set(id)?;
    confirmed(id, &format!("set {id} to {}", level.as_str()))?;
    let mut a = approvals::load()?;
    a.set_level(&set, level, &now_text())?;
    approvals::save(&a)?;
    println!("{id} is now at level {}", level.as_str());
    Ok(())
}

pub fn rules_off(id: &str) -> Result<()> {
    approvals::refuse_agent()?;
    if !approvals::load()?.sets.contains_key(id) {
        bail!("{id} isn't approved, so it's already off");
    }
    confirmed(id, &format!("turn {id} off"))?;
    let mut a = approvals::load()?;
    a.turn_off(id, &now_text())?;
    approvals::save(&a)?;
    println!("{id} is off. Turn it on again with dotonce rules level {id} <level>");
    Ok(())
}

/// True when this folder is inside one of the config's private paths.
fn private_here() -> Result<bool> {
    let cfg = config::load()?;
    Ok(cfg.private_allowed(&std::env::current_dir().unwrap_or_default()))
}

pub fn rules_list() -> Result<()> {
    let private_ok = private_here()?;
    let loaded = rules::load()?;
    let a = approvals::load()?;
    if loaded.sets.is_empty() && loaded.errors.is_empty() {
        println!("no rule sets in {}", rules::rules_dir()?.display());
        return Ok(());
    }
    // The last time a check caught something: fixes and re-checks don't
    // count, and neither does a line whose time doesn't parse.
    let mut last: BTreeMap<String, String> = BTreeMap::new();
    for c in catches::read(private_ok)? {
        let counts = matches!(
            c.outcome,
            catches::Outcome::Caught | catches::Outcome::Denied
        );
        if counts && chrono::DateTime::parse_from_rfc3339(&c.time).is_ok() {
            last.insert(c.set, c.time);
        }
    }
    let mut hidden = 0;
    for s in &loaded.sets {
        if s.private && !private_ok {
            hidden += 1;
            continue;
        }
        let changed = a
            .sets
            .get(&s.id)
            .filter(|x| !x.changed.is_empty())
            .map(|x| format!("  changed {}", printable(&x.changed)))
            .unwrap_or_default();
        let caught = last
            .get(&s.id)
            .map(|t| printable(&t.chars().take(10).collect::<String>()))
            .unwrap_or_else(|| "none".to_string());
        println!(
            "{:<24} {:<24} checks: {}  last catch: {caught}{changed}",
            printable(&s.id),
            a.state(s).label(),
            s.checks.len()
        );
    }
    for e in &loaded.errors {
        let stem = e
            .file
            .file_stem()
            .map(|s| printable(&s.to_string_lossy()))
            .unwrap_or_default();
        println!("{stem:<24} {:<24} {}", "error", printable(&e.line()));
    }
    if hidden > 0 {
        println!("{hidden} private set(s) hidden outside private_paths");
    }
    Ok(())
}

pub fn rules_log(limit: usize) -> Result<()> {
    let all = catches::read(private_here()?)?;
    if all.is_empty() {
        println!("no catches yet");
        return Ok(());
    }
    if limit == 0 {
        println!("no catches shown (limit 0)");
        return Ok(());
    }
    let skip = all.len().saturating_sub(limit);
    for c in &all[skip..] {
        println!(
            "{}  {}/{}  {}  {}",
            printable(&c.time),
            printable(&c.set),
            printable(&c.check),
            c.level.as_str(),
            c.outcome.as_str()
        );
    }
    Ok(())
}

pub fn stats() -> Result<()> {
    let all = catches::read(private_here()?)?;
    let rows = catches::stats(&all, chrono::Utc::now().timestamp());
    if rows.is_empty() {
        println!("no catches in the last 30 days");
        return Ok(());
    }
    println!(
        "{:<40} {:>4} {:>5} {:>6} {:>13} {:>11} {:>7} {:>9}",
        "set/check", "7d", "30d", "fixed", "still broken", "unverified", "denied", "per week"
    );
    for r in rows {
        println!(
            "{:<40} {:>4} {:>5} {:>6} {:>13} {:>11} {:>7} {:>9.1}",
            printable(&format!("{}/{}", r.set, r.check)),
            r.week,
            r.month,
            r.fixed,
            r.still_broken,
            r.unverified,
            r.denied,
            r.per_week()
        );
    }
    Ok(())
}

/// The session named with `--session`, which must have a state file, or the
/// newest one.
fn pick_session(dir: &Path, session_id: Option<&str>) -> Result<Option<String>> {
    match session_id {
        Some(s) => session::existing(dir, s)
            .map(Some)
            .map_err(|e| anyhow::anyhow!(printable(&e.to_string()))),
        None => Ok(session::newest(dir)),
    }
}

/// For this folder and the newest session (or `--session`), the sets that
/// are on and what turned each one on.
pub fn rules_why(session_id: Option<&str>) -> Result<()> {
    let cfg = config::load()?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let private_ok = cfg.private_allowed(&cwd);
    let (live, _) = approvals::load_live()?;
    let dir = session::dir()?;
    let sid = pick_session(&dir, session_id)?;
    let mut state = match &sid {
        Some(s) => session::load(&dir, s)?,
        None => State::default(),
    };
    session::refresh(&mut state, &live, &cfg, &cwd, None, Event::Other);
    match &sid {
        Some(s) => println!("session {}", printable(s)),
        None => println!("no session yet"),
    }
    let mut shown = 0;
    for a in &state.active {
        let Some(l) = live.iter().find(|l| l.set.id == a.id) else {
            continue;
        };
        if l.set.private && !private_ok {
            continue;
        }
        let by = match a.by {
            Trigger::Skill => "skill",
            Trigger::Folder => "folder",
            Trigger::Prompt => "prompt",
        };
        let what = printable(a.what.as_deref().unwrap_or(""));
        let note = if a.by == Trigger::Prompt && !l.set.when.check_on_prompt {
            "  (reminder only)"
        } else {
            ""
        };
        println!(
            "{:<24} {:<7} {by} {what}{note}",
            printable(&a.id),
            l.level.as_str()
        );
        shown += 1;
    }
    if shown == 0 {
        println!("no rule sets are on here");
    }
    for p in &state.paused {
        // A paused id that no longer matches a live set could be a private
        // set's, so it shows only where private sets do.
        let private = live
            .iter()
            .find(|l| l.set.id == *p)
            .is_none_or(|l| l.set.private);
        if private && !private_ok {
            continue;
        }
        println!("{:<24} paused for this session", printable(p));
    }
    Ok(())
}

/// Turns a set off for one session. Guarded like `rules off`, so Claude
/// can't switch a set off for its own session through Bash.
pub fn rules_pause(id: &str, session_id: Option<&str>) -> Result<()> {
    approvals::refuse_agent()?;
    find_set(id)?;
    let dir = session::dir()?;
    let sid = pick_session(&dir, session_id)?.context("no session yet: start Claude Code first")?;
    confirmed(id, &format!("pause {id} for session {}", printable(&sid)))?;
    // Read after the prompt, so hook changes made while it waited are kept.
    let mut state = session::load(&dir, &sid)?;
    if !state.paused.iter().any(|p| p == id) {
        state.paused.push(id.to_string());
    }
    state.active.retain(|a| a.id != id);
    session::save(&dir, &sid, &state)?;
    println!("paused {id} for session {}", printable(&sid));
    Ok(())
}

/// True when some hook group for `event` runs `dotonce hook <arg>` and its
/// matcher covers every tool in `matcher`.
fn has_hook(v: &Value, event: &str, arg: &str, matcher: Option<&str>) -> bool {
    let want = format!("hook {arg}");
    let Some(groups) = v
        .pointer(&format!("/hooks/{event}"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    groups.iter().any(|g| {
        let covers = match matcher {
            None => true,
            Some(m) => {
                let gm = g.get("matcher").and_then(Value::as_str).unwrap_or("");
                gm.is_empty()
                    || gm == "*"
                    || m.split('|').all(|t| gm.split('|').any(|x| x.trim() == t))
            }
        };
        covers
            && g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                hs.iter().any(|h| {
                    h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|c| c.contains("dotonce") && c.contains(&want))
                })
            })
    })
}

/// A warning line for each settings file that sets DOTONCE_HOME in its
/// `env` block: Claude Code passes it to the hooks, so they would read
/// another folder's approvals.json.
fn home_overrides(files: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        let Some(v) = fs::read_to_string(f)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        else {
            continue;
        };
        if let Some(home) = v.pointer("/env/DOTONCE_HOME") {
            let value = match home.as_str() {
                Some(s) => s.to_string(),
                None => home.to_string(),
            };
            out.push(format!(
                "warning  {} sets DOTONCE_HOME to {}: the hooks then read rule approvals from there, not from your dotonce home. Remove it unless you set it yourself.",
                printable(&f.to_string_lossy()),
                printable(&value)
            ));
        }
    }
    out
}

/// Reads the settings files Claude Code merges hooks from (never writes
/// them) and lists the dotonce hooks that are there and missing.
pub fn doctor() -> Result<()> {
    let user = config::expand("~/.claude/settings.json");
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut files = vec![
        user.clone(),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ];
    let mut seen = Vec::new();
    files.retain(|f| {
        let real = fs::canonicalize(f).unwrap_or_else(|_| f.clone());
        let new = !seen.contains(&real);
        seen.push(real);
        new
    });
    let mut parsed: Vec<(&PathBuf, Value)> = Vec::new();
    for f in &files {
        let src = fs::read_to_string(f).unwrap_or_default();
        if src.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&src) {
            Ok(v) => parsed.push((f, v)),
            Err(e) => println!(
                "warning  {} isn't valid JSON, so doctor skipped it: {}",
                printable(&f.to_string_lossy()),
                printable(&e.to_string())
            ),
        }
    }
    let mut missing = 0;
    for &(event, arg, matcher) in hook::HOOKS {
        match parsed
            .iter()
            .find(|(_, v)| has_hook(v, event, arg, matcher))
        {
            Some((f, _)) => println!(
                "{:<8} {event} (dotonce hook {arg}) in {}",
                "ok",
                printable(&f.to_string_lossy())
            ),
            None => {
                missing += 1;
                println!("{:<8} {event} (dotonce hook {arg})", "missing");
            }
        }
    }
    if missing > 0 {
        println!(
            "{missing} hook(s) missing from every settings file doctor read. Run dotonce hook-config and add them to {}; dotonce never edits that file.",
            user.display()
        );
    }
    for line in home_overrides(&files) {
        println!("{line}");
    }
    let loaded = rules::load()?;
    let a = approvals::load()?;
    let waiting = loaded
        .sets
        .iter()
        .filter(|s| !matches!(a.state(s), approvals::State::On(_) | approvals::State::Off))
        .count();
    println!(
        "rule sets: {} loaded, {} with errors, {} waiting for approval",
        loaded.sets.len(),
        loaded.errors.len(),
        waiting
    );
    Ok(())
}

/// Prints the hooks block for Claude Code's settings.json, with this
/// binary's absolute path.
pub fn hook_config() -> Result<()> {
    let exe = std::env::current_exe()?;
    let bin = hook::shell_word(&exe.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for &(event, arg, matcher) in hook::HOOKS {
        let mut group = json!({"hooks": [{"type": "command", "command": format!("{bin} hook {arg}"), "timeout": 5}]});
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        hooks.insert(event.to_string(), json!([group]));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"hooks": hooks}))?
    );
    Ok(())
}
