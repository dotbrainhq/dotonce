//! Calls Claude Code's `claude` command for the judge: from an empty folder,
//! with no tools, settings, hooks, MCP servers or saved session, so the
//! model reads only the prompt it gets on stdin.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::config;
use crate::hook::CHILD_VAR;

const SYSTEM: &str = "You check drafts an AI assistant wrote for its user against the user's rules. A draft is text the user will send, post or publish to someone else, like a customer, a teammate or a forum.

You get:
- the rules.
- the session so far, oldest first. Each message the user typed starts with \"User:\", and the one that started this turn with \"User, this turn:\". Each tool call the assistant made shows the tool, the start of its input and, after \"->\", the start of what it returned. Long text is cut, and the oldest part of the session may be missing.
- the assistant's reply, with each draft swapped for a marker like [draft 1].
- the drafts.

How to read them:
- A fact is checked when a tool result or a user message in the session shows it. A cut or missing result is not proof the fact went unchecked: when the session has a tool call that could have checked it, the draft passes.
- Content counts as asked for when any user message in the session asks for it, gives it or approves it, not only the message that started this turn.
- The user's own questions to the assistant get answered in the reply around the drafts. A draft answers the person it is written to, so judge it against that person's message, often one the user pasted.

For each rule, decide whether a draft clearly breaks it. Report a break only when the text plainly breaks the rule and you could show the user why. When you are unsure, or the rule depends on facts you don't have, the draft passes. Most drafts break nothing.

For each break, give the rule's id, the words from the draft that break it, copied exactly (one sentence or less), and a one-sentence reason. Report at most 3 breaks, the clearest first. Return an empty list when no rule is clearly broken.";

const SCHEMA: &str = r#"{"type":"object","properties":{"breaks":{"type":"array","maxItems":3,"items":{"type":"object","properties":{"rule":{"type":"string"},"quote":{"type":"string"},"why":{"type":"string"}},"required":["rule","quote","why"],"additionalProperties":false}}},"required":["breaks"],"additionalProperties":false}"#;

/// Why a call gave no verdict. Each one lets the draft through.
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    NoClaude,
    Start,
    Timeout,
    Exit(Option<i32>),
    BadOutput,
    /// Claude Code ran and reported an error, such as not being logged in.
    Error(String),
}

impl Fail {
    /// The word judge.log and doctor use.
    pub fn kind(&self) -> String {
        match self {
            Fail::NoClaude => "no-claude".to_string(),
            Fail::Start => "start".to_string(),
            Fail::Timeout => "timeout".to_string(),
            Fail::Exit(Some(c)) => format!("exit-{c}"),
            Fail::Exit(None) => "exit-signal".to_string(),
            Fail::BadOutput => "bad-output".to_string(),
            Fail::Error(e) => {
                let e: String = e
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .take(40)
                    .collect();
                format!("error-{e}")
            }
        }
    }
}

fn is_program(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Claude Code's `claude` command: the first on PATH, else where its
/// installers put it.
pub fn find_claude() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    };
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let usual = [
        config::expand("~/.local/bin"),
        config::expand("~/.claude/local"),
    ];
    dirs.into_iter()
        .filter(|d| !d.as_os_str().is_empty())
        .chain(usual)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| is_program(p))
}

/// Runs `cmd` with `input` on stdin and returns its stdout. Stops it once
/// `limit` passes.
fn run_limited(mut cmd: Command, input: String, limit: Duration) -> Result<Vec<u8>, Fail> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Fail::Start)?;
    let stdin = child.stdin.take();
    let writer = thread::spawn(move || {
        if let Some(mut s) = stdin {
            let _ = s.write_all(input.as_bytes());
        }
    });
    let stdout = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < limit => thread::sleep(Duration::from_millis(20)),
            other => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if other.is_ok() {
                    Fail::Timeout
                } else {
                    Fail::Start
                });
            }
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        // Claude Code still prints its JSON when it fails, with the reason.
        return Err(match parse(&out) {
            Err(Fail::Error(e)) => Fail::Error(e),
            _ => Fail::Exit(status.code()),
        });
    }
    Ok(out)
}

/// One break as the model reported it, before its quote is checked.
#[derive(Debug, Deserialize)]
pub(super) struct RawBreak {
    pub(super) rule: String,
    pub(super) quote: String,
    #[serde(default)]
    pub(super) why: String,
}

/// What a call returned: the breaks and the tokens in and out.
pub(super) type Answer = (Vec<RawBreak>, u64, u64);

/// Reads Claude Code's JSON result.
fn parse(out: &[u8]) -> Result<Answer, Fail> {
    let v: Value = serde_json::from_slice(out).map_err(|_| Fail::BadOutput)?;
    if v.get("is_error").and_then(Value::as_bool) == Some(true) {
        let sub = v
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(Fail::Error(sub.to_string()));
    }
    let used = |k: &str| {
        v.pointer(&format!("/usage/{k}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let tokens_in = used("input_tokens")
        + used("cache_creation_input_tokens")
        + used("cache_read_input_tokens");
    let structured = v.get("structured_output").cloned().or_else(|| {
        v.get("result")
            .and_then(Value::as_str)
            .and_then(|r| serde_json::from_str::<Value>(r).ok())
    });
    let breaks = structured
        .and_then(|s| s.get("breaks").cloned())
        .ok_or(Fail::BadOutput)?;
    let breaks: Vec<RawBreak> = serde_json::from_value(breaks).map_err(|_| Fail::BadOutput)?;
    Ok((breaks, tokens_in, used("output_tokens")))
}

/// Runs the model on `input` from an empty folder.
pub(super) fn call(
    bin: &Path,
    model: &str,
    input: String,
    limit: Duration,
) -> Result<Answer, Fail> {
    let cwd = std::env::temp_dir().join("dotonce-judge");
    fs::create_dir_all(&cwd).map_err(|_| Fail::Start)?;
    let mut cmd = Command::new(bin);
    cmd.current_dir(&cwd).env(CHILD_VAR, "1").args([
        "-p",
        "--model",
        model,
        "--system-prompt",
        SYSTEM,
        "--tools",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--setting-sources",
        "",
        "--disable-slash-commands",
        "--output-format",
        "json",
        "--effort",
        "low",
        "--json-schema",
        SCHEMA,
    ]);
    parse(&run_limited(cmd, input, limit)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_codes_result_gives_the_breaks_and_tokens() {
        let out = json!({"type": "result", "is_error": false, "usage": {"input_tokens": 2, "cache_creation_input_tokens": 1500, "cache_read_input_tokens": 100, "output_tokens": 80}, "structured_output": {"breaks": [{"rule": "every-ask", "quote": "Hi", "why": "w"}]}});
        let (b, tin, tout) = parse(out.to_string().as_bytes()).unwrap();
        assert_eq!((b.len(), tin, tout), (1, 1602, 80));
        let in_result = json!({"is_error": false, "result": "{\"breaks\": []}"});
        assert!(
            parse(in_result.to_string().as_bytes())
                .unwrap()
                .0
                .is_empty()
        );
        let err = json!({"is_error": true, "subtype": "error_during_execution"});
        assert_eq!(
            parse(err.to_string().as_bytes()).unwrap_err().kind(),
            "error-error_during_execution"
        );
        assert_eq!(parse(b"not json").unwrap_err(), Fail::BadOutput);
        assert_eq!(
            parse(b"{\"result\": \"text\"}").unwrap_err(),
            Fail::BadOutput
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_command_is_stopped_at_the_limit() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5"]);
        let start = Instant::now();
        assert_eq!(
            run_limited(cmd, String::new(), Duration::from_millis(200)),
            Err(Fail::Timeout)
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "cat"]);
        assert_eq!(
            run_limited(cmd, "acme".to_string(), Duration::from_secs(5)).unwrap(),
            b"acme"
        );
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 3"]);
        assert_eq!(
            run_limited(cmd, String::new(), Duration::from_secs(5)),
            Err(Fail::Exit(Some(3)))
        );
        let cmd = Command::new("/no/such/dotonce-claude");
        assert_eq!(
            run_limited(cmd, String::new(), Duration::from_secs(5)),
            Err(Fail::Start)
        );
    }
}
