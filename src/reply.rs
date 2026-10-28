//! Stop: checks this turn's drafts against the active rule sets, logs every
//! result, and sends a failing draft of a fix set back once per turn.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::approvals::{self, Live};
use crate::catches::{self, Catch, Outcome};
use crate::checks::{self, Failure};
use crate::hook::{log_error, prompt_id, session_id};
use crate::session::{self, Pending};

pub const SEND_BACK_END: &str =
    "Write the draft again with these fixed. Add one line saying what you fixed.";
pub const SEND_BACK_START: &str =
    "dotonce checked the draft in your reply against the user's rule sets. It breaks these rules:";
const TAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PARTS: usize = 200;

pub fn send_back_text(fails: &[Failure]) -> String {
    let mut s = format!("{SEND_BACK_START}\n");
    for f in fails {
        s.push_str(&format!("- {}: \"{}\". {}\n", f.check, f.line, f.why));
    }
    s.push_str(SEND_BACK_END);
    s
}

/// Continues the turn with the send-back text (task 1, D4).
fn stop_output(text: &str) -> String {
    json!({"hookSpecificOutput": {"hookEventName": "Stop", "additionalContext": text}}).to_string()
}

/// The last `TAIL_BYTES` of the file, from its first whole line.
fn read_tail(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let skip = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(skip)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf).into_owned();
    if skip == 0 {
        return Some(s);
    }
    Some(
        s.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default(),
    )
}

/// A user entry that starts a turn: typed text, not a tool result or a
/// message Claude Code added itself.
fn starts_turn(e: &Value) -> bool {
    if e.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match e.pointer("/message/content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => !blocks
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")),
        _ => false,
    }
}

fn assistant_text(e: &Value) -> String {
    match e.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The record entry Claude Code writes when dotonce's send-back continues
/// the turn (task 1, D7). Other hooks' added context, such as dotonce's own
/// prompt hook, is stored the same way, so only content that starts with the
/// send-back text counts.
fn continues_turn(e: &Value) -> bool {
    if e.get("type").and_then(Value::as_str) != Some("attachment")
        || e.pointer("/attachment/type").and_then(Value::as_str) != Some("hook_additional_context")
    {
        return false;
    }
    let ours = |c: &Value| c.as_str().is_some_and(|t| t.starts_with(SEND_BACK_START));
    match e.pointer("/attachment/content") {
        Some(Value::Array(items)) => items.iter().any(ours),
        Some(c) => ours(c),
        None => false,
    }
}

/// The assistant text of the record's last turn, oldest first, and how many
/// of those parts came before the turn's last continuation.
fn scan(src: &str) -> (Vec<String>, Option<usize>) {
    let mut parts = Vec::new();
    let mut after_continue = None;
    for line in src.lines().rev() {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if e.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if after_continue.is_none() && continues_turn(&e) {
            after_continue = Some(parts.len());
        }
        match e.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let t = assistant_text(&e);
                if !t.trim().is_empty() {
                    parts.push(t);
                }
            }
            Some("user") if starts_turn(&e) => break,
            _ => {}
        }
        if parts.len() >= MAX_PARTS {
            break;
        }
    }
    parts.reverse();
    let continued_at = after_continue.map(|n| parts.len() - n);
    (parts, continued_at)
}

/// The assistant text of the record's last turn, oldest first.
pub fn from_transcript(src: &str) -> Vec<String> {
    scan(src).0
}

/// One turn's assistant text, and what is known about where a send-back
/// continued it.
struct Turn {
    parts: Vec<String>,
    /// How many parts came from the session record.
    recorded: usize,
    /// How many parts came before the record's last continuation entry.
    continued_at: Option<usize>,
}

fn read_turn(transcript: Option<&Path>, last: Option<&str>) -> Turn {
    let (mut parts, continued_at) = transcript
        .and_then(read_tail)
        .map(|s| scan(&s))
        .unwrap_or_default();
    let recorded = parts.len();
    let last =
        last.filter(|l| !l.trim().is_empty() && parts.last().map(String::as_str) != Some(*l));
    if let Some(l) = last {
        parts.push(l.to_string());
    }
    Turn {
        parts,
        recorded,
        continued_at,
    }
}

/// This turn's assistant text: the turn's messages from the session record,
/// then `last` unless the record already ends with it.
pub fn turn_text(transcript: Option<&Path>, last: Option<&str>) -> Vec<String> {
    read_turn(transcript, last).parts
}

/// Where the text written after the send-back starts: after the part it
/// was sent back at, else after the record's continuation entry. With no
/// record, the only part is the last message, which came after it. None
/// when the cut can't be found, since checking the whole turn would count
/// the old broken draft against the new one.
fn fresh_start(p: &Pending, turn: &Turn) -> Option<usize> {
    turn.parts
        .iter()
        .rposition(|t| approvals::hash(t) == p.after)
        .map(|i| i + 1)
        .or(turn.continued_at)
        .or((turn.recorded == 0).then_some(0))
}

/// After a send-back, checks only the text that came after it and logs
/// whether each sent-back check is now fixed.
fn recheck(p: &Pending, live: &[Live], turn: &Turn, log: &dyn Fn(&Live, &str, Outcome)) {
    let fresh = fresh_start(p, turn).map(|start| turn.parts[start..].join("\n\n"));
    for (set_id, check_id) in &p.checks {
        let Some(l) = live.iter().find(|l| &l.set.id == set_id) else {
            continue;
        };
        let Some(c) = l.set.checks.iter().find(|c| &c.id == check_id) else {
            continue;
        };
        let drafts = fresh
            .as_deref()
            .map(|f| checks::drafts(&l.set, f))
            .unwrap_or_default();
        let outcome = if drafts.is_empty() {
            Outcome::Unverified
        } else if drafts.iter().any(|d| checks::failing_line(c, d).is_some()) {
            Outcome::StillBroken
        } else {
            Outcome::Fixed
        };
        log(l, check_id, outcome);
    }
}

/// Sends a failing draft back, or not. Whatever stops the send-back (an
/// error, a failed save, a set no longer at fix) sends nothing.
pub fn stop(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let sid = session_id(v)?;
    let dir = session::dir()?;
    let sent = send_back(v, cwd, now, sid, &dir).unwrap_or_else(|e| {
        log_error(&format!("hook stop: {e:#}"));
        None
    });
    Ok(sent.map(|text| stop_output(&text)))
}

/// The send-back text when this turn's drafts go back. An error, including
/// a failed save of the state that records the send-back, means nothing
/// goes back.
fn send_back(v: &Value, cwd: &Path, now: i64, sid: &str, dir: &Path) -> Result<Option<String>> {
    let checks = session::checks_now(&session::load(dir, sid)?, cwd, prompt_id(v))?;
    if checks.live.is_empty() {
        return Ok(None);
    }
    let fix: Vec<String> = checks.fix().iter().map(|l| l.set.id.clone()).collect();
    let session::Now {
        live,
        mut state,
        turn,
    } = checks;
    let checking = session::checking(&state, &live);
    let again = state.sent_back(&turn);
    let turn_read = if checking.is_empty() && !again {
        Turn {
            parts: Vec::new(),
            recorded: 0,
            continued_at: None,
        }
    } else {
        read_turn(
            v.get("transcript_path")
                .and_then(Value::as_str)
                .map(Path::new),
            v.get("last_assistant_message").and_then(Value::as_str),
        )
    };
    let parts = &turn_read.parts;
    let time = catches::timestamp(now);
    let log = |l: &Live, check: &str, outcome: Outcome| {
        let c = Catch {
            time: time.clone(),
            session: sid.to_string(),
            set: l.set.id.clone(),
            check: check.to_string(),
            level: l.level,
            outcome,
        };
        catches::record(&c, l.set.private);
    };
    let out = if again {
        if let Some(p) = state.take_pending(&turn) {
            recheck(&p, &live, &turn_read, &log);
        }
        None
    } else {
        let text = parts.join("\n\n");
        let mut send = Vec::new();
        for &l in &checking {
            for f in checks::run_reply(&l.set, &text) {
                if state.first_log(&turn, &l.set.id, &f.check) {
                    log(l, &f.check, Outcome::Caught);
                }
                if fix.contains(&l.set.id) {
                    send.push(f);
                }
            }
        }
        if send.is_empty() {
            None
        } else {
            let after = parts.last().map(|t| approvals::hash(t)).unwrap_or_default();
            state.mark_sent_back(Pending {
                turn: turn.clone(),
                after,
                checks: send
                    .iter()
                    .map(|f| (f.set.clone(), f.check.clone()))
                    .collect(),
            });
            Some(send_back_text(&send))
        }
    };
    session::save(dir, sid, &state)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: serde_json::Value) -> String {
        v.to_string()
    }

    fn record() -> String {
        [
            entry(json!({"type": "user", "message": {"role": "user", "content": "Draft the old one"}, "promptId": "p0"})),
            entry(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "old draft"}]}})),
            entry(json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "Draft the reply"}]}, "promptId": "p1"})),
            "not json".to_string(),
            entry(json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "skill text"}, "promptId": "p1"})),
            entry(json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "side"}]}})),
            entry(json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "hmm"}]}})),
            entry(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "first part"}]}})),
            entry(json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Write", "input": {}}]}})),
            entry(json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}, "promptId": "p1"})),
            entry(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Saved."}]}})),
        ]
        .join("\n")
    }

    #[test]
    fn the_turn_starts_at_the_last_real_prompt() {
        assert_eq!(from_transcript(&record()), vec!["first part", "Saved."]);
        assert!(from_transcript("").is_empty());
        assert!(from_transcript("{\"type\":\"assistant\"}\nnot json").is_empty());
    }

    #[test]
    fn the_last_message_is_added_once() {
        let dir = std::env::temp_dir().join(format!("dotonce-reply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("record.jsonl");
        std::fs::write(&t, record()).unwrap();
        assert_eq!(
            turn_text(Some(&t), Some("Saved.")),
            vec!["first part", "Saved."]
        );
        assert_eq!(
            turn_text(Some(&t), Some("Done")),
            vec!["first part", "Saved.", "Done"]
        );
        assert_eq!(turn_text(Some(&dir), Some("Done")), vec!["Done"]);
        assert_eq!(turn_text(None, Some("  ")), Vec::<String>::new());
        assert_eq!(
            turn_text(Some(&dir.join("missing")), None),
            Vec::<String>::new()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_send_back_names_each_check_line_and_why() {
        let f = vec![Failure {
            set: "acme-replies".to_string(),
            check: "inline-links".to_string(),
            line: "See https://example.com".to_string(),
            why: "Links go inline as anchor text.".to_string(),
        }];
        assert_eq!(
            send_back_text(&f),
            "dotonce checked the draft in your reply against the user's rule sets. It breaks these rules:\n- inline-links: \"See https://example.com\". Links go inline as anchor text.\nWrite the draft again with these fixed. Add one line saying what you fixed."
        );
    }
}
