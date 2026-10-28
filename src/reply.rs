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
use crate::config;
use crate::hook::{prompt_id, session_id};
use crate::rules::Level;
use crate::session::{self, Event, Pending};

pub const SEND_BACK_END: &str =
    "Write the draft again with these fixed. Add one line saying what you fixed.";
const SEND_BACK_START: &str =
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

/// The assistant text of the record's last turn, oldest first.
pub fn from_transcript(src: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for line in src.lines().rev() {
        if !line.contains("\"type\"") {
            continue;
        }
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if e.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
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
    parts
}

/// This turn's assistant text: the turn's messages from the session record,
/// then `last` unless the record already ends with it.
pub fn turn_text(transcript: Option<&Path>, last: Option<&str>) -> Vec<String> {
    let mut parts = transcript
        .and_then(read_tail)
        .map(|s| from_transcript(&s))
        .unwrap_or_default();
    let last =
        last.filter(|l| !l.trim().is_empty() && parts.last().map(String::as_str) != Some(*l));
    if let Some(l) = last {
        parts.push(l.to_string());
    }
    parts
}

/// After a send-back, checks only the text that came after it and logs
/// whether each sent-back check is now fixed.
fn recheck(p: &Pending, live: &[Live], parts: &[String], log: &dyn Fn(&Live, &str, Outcome)) {
    let start = parts
        .iter()
        .rposition(|t| approvals::hash(t) == p.after)
        .map_or(0, |i| i + 1);
    let fresh = parts[start..].join("\n\n");
    for (set_id, check_id) in &p.checks {
        let Some(l) = live.iter().find(|l| &l.set.id == set_id) else {
            continue;
        };
        let Some(c) = l.set.checks.iter().find(|c| &c.id == check_id) else {
            continue;
        };
        let drafts = checks::drafts(&l.set, &fresh);
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

pub fn stop(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let (live, _) = approvals::load_live()?;
    if live.is_empty() {
        return Ok(None);
    }
    let cfg = config::load_file()?.unwrap_or_default();
    let sid = session_id(v)?;
    let dir = session::dir()?;
    let mut state = session::load(&dir, sid)?;
    session::refresh(&mut state, &live, &cfg, cwd, prompt_id(v), Event::Other);
    let turn = state
        .prompt_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let checking = session::checking(&state, &live);
    let again = state.sent_back(&turn);
    let parts = if checking.is_empty() && !again {
        Vec::new()
    } else {
        turn_text(
            v.get("transcript_path")
                .and_then(Value::as_str)
                .map(Path::new),
            v.get("last_assistant_message").and_then(Value::as_str),
        )
    };
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
            recheck(&p, &live, &parts, &log);
        }
        None
    } else {
        let text = parts.join("\n\n");
        let mut send = Vec::new();
        for &l in &checking {
            for f in checks::run_reply(&l.set, &text) {
                log(l, &f.check, Outcome::Caught);
                if l.level == Level::Fix {
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
            Some(stop_output(&send_back_text(&send)))
        }
    };
    session::save(&dir, sid, &state)?;
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
