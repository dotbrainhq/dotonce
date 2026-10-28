//! The `dotonce rules`, `stats` and `doctor` commands.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::SecondsFormat;
use serde::Serialize;

use crate::approvals;
use crate::checks::{self, Failure};
use crate::config;
use crate::rules::{self, Level, RuleSet};

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

/// Runs a set's reply checks on a text file, approved or not.
pub fn rules_test(id: &str, file: &Path, json: bool) -> Result<()> {
    let set = find_set(id)?;
    let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let drafts = checks::drafts(&set, &text).len();
    let fails = checks::run_reply(&set, &text);
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
    a.set_level(&set, level)?;
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
    a.turn_off(id)?;
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
    let mut hidden = 0;
    for s in &loaded.sets {
        if s.private && !private_ok {
            hidden += 1;
            continue;
        }
        println!(
            "{:<24} {:<24} checks: {}",
            s.id,
            a.state(s).label(),
            s.checks.len()
        );
    }
    for e in &loaded.errors {
        let stem = e
            .file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!("{stem:<24} {:<24} {}", "error", e.line());
    }
    if hidden > 0 {
        println!("{hidden} private set(s) hidden outside private_paths");
    }
    Ok(())
}
