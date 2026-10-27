//! Claude Code hooks: UserPromptSubmit adds facts that match the prompt,
//! SessionStart adds the pinned facts.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{config, facts};

const PROMPT_TITLE: &str = "Facts from your memory that match this prompt, with the date each was last checked and its file. Open the file for the full entry.";
const SESSION_TITLE: &str = "dotonce is on. Pinned facts for every session:";

/// Handles one Claude Code hook call. Returns the JSON to print, or None when
/// there is nothing to add.
pub fn handle(event: &str, input: &str, now: i64) -> Result<Option<String>> {
    let name = match event {
        "prompt" => "UserPromptSubmit",
        "session-start" => "SessionStart",
        other => bail!("unknown hook event: {other}"),
    };
    let v: Value = serde_json::from_str(input)?;
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    // Without a config, dotonce isn't set up yet: leave no index behind.
    let Some(cfg) = config::load_file()? else {
        return Ok(None);
    };
    let mut ix = facts::open()?;
    // A failed sync can leave an old entry in place, such as a fact that was
    // public before it was marked private, so nothing is shown until it syncs.
    facts::sync(&mut ix, &cfg)?;
    let (title, hits) = if name == "UserPromptSubmit" {
        let prompt = v.get("prompt").and_then(Value::as_str).unwrap_or_default();
        let hits = facts::search(&ix, &cfg, prompt, &cwd, now, cfg.prompt_budget)?;
        (PROMPT_TITLE, hits)
    } else {
        let hits = facts::pinned(&ix, &cfg, &cwd, now, cfg.session_budget)?;
        (SESSION_TITLE, hits)
    };
    if hits.is_empty() {
        return Ok(None);
    }
    let block = facts::render_block(title, &hits, &self_command());
    Ok(Some(
        json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": block}})
            .to_string(),
    ))
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
