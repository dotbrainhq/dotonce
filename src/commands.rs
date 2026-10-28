//! The `dotonce rules`, `stats` and `doctor` commands.

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::checks::{self, Failure};
use crate::rules::{self, RuleSet};

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
