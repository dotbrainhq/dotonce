//! The `dotonce rules`, `stats` and `doctor` commands.

use std::fs;
use std::io::BufRead;
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::rules;

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
