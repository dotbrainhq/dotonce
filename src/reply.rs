//! Stop: checks this turn's drafts against the active rule sets, logs every
//! result, and sends a failing draft of a fix set back once per turn. A fix
//! set's model checks are judged here too, so their catch can go back; a warn
//! set's run in the background judge hook (see judge.rs).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::approvals::{self, Live};
use crate::catches::{self, Catch, Outcome};
use crate::chat;
use crate::checks::{self, Failure};
use crate::config;
use crate::hook::{log_error, prompt_id, session_id};
use crate::judge::{self, Verdict};
use crate::rules::{CheckKind, Level};
use crate::session::{self, Nudge, Pending};
use crate::skills::Seen;

pub const SEND_BACK_END: &str =
    "Write the draft again with these fixed. Add one line saying what you fixed.";
pub const SEND_BACK_START: &str =
    "dotonce checked the draft in your reply against the user's rule sets. It breaks these rules:";
/// How the next prompt's line starts after a skill-loaded catch at warn.
pub const NUDGE_START: &str = "Your last reply had a draft and no writing skill was loaded.";
const TAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PARTS: usize = 200;

/// The send-back text. Two sets with the same check give the same line, so
/// each line is listed once.
pub fn send_back_text(fails: &[Failure]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for f in fails {
        let line = format!("- {}: \"{}\". {}\n", f.check, f.line, f.why);
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    let mut s = format!("{SEND_BACK_START}\n");
    s.extend(lines);
    s.push_str(SEND_BACK_END);
    s
}

/// Continues the turn with the send-back text (task 1, D4).
/// The Stop output: the send-back for Claude, and the notice of rule sets
/// that started waiting this turn for the user, who alone sees it.
fn stop_output(text: Option<&str>, notice: Option<&str>) -> Option<String> {
    let mut out = serde_json::Map::new();
    if let Some(n) = notice {
        out.insert("systemMessage".to_string(), Value::String(n.to_string()));
    }
    if let Some(t) = text {
        out.insert(
            "hookSpecificOutput".to_string(),
            json!({"hookEventName": "Stop", "additionalContext": t}),
        );
    }
    (!out.is_empty()).then(|| Value::Object(out).to_string())
}

/// The last `TAIL_BYTES` of the file, from its first whole line.
pub(crate) fn read_tail(path: &Path) -> Option<String> {
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
pub(crate) fn starts_turn(e: &Value) -> bool {
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

/// One turn's assistant text, and what is known about where a send-back
/// continued it.
pub(crate) struct Turn {
    pub(crate) parts: Vec<String>,
    /// How many parts came from the session record.
    recorded: usize,
    /// How many parts came before the record's last continuation entry.
    continued_at: Option<usize>,
}

/// This turn's assistant text: the turn's messages from the session record,
/// then `last` unless the record already ends with it.
pub(crate) fn read_turn(transcript: Option<&Path>, last: Option<&str>) -> Turn {
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

/// Where the text written after the send-back starts: after the record's
/// continuation entry, else after the last part matching the one it was
/// sent back at. The continuation comes first because two messages can
/// have the same text. With no record, the only part is the last message,
/// which came after it. None when the cut can't be found, since checking
/// the whole turn would count the old broken draft against the new one.
fn fresh_start(p: &Pending, turn: &Turn) -> Option<usize> {
    turn.continued_at
        .or_else(|| {
            turn.parts
                .iter()
                .rposition(|t| approvals::hash(t) == p.after)
                .map(|i| i + 1)
        })
        .or((turn.recorded == 0).then_some(0))
}

/// After a send-back, logs whether each sent-back check is fixed in
/// `fresh`, the text that came after it. None when the cut can't be found.
/// A model check's outcome comes from the judge's `verdict` on `fresh`, and
/// is unverified without one.
fn recheck(
    p: &Pending,
    live: &[Live],
    fresh: Option<&str>,
    seen: &Seen,
    verdict: Option<&Verdict>,
    log: &dyn Fn(&Live, &str, Outcome),
) {
    for (set_id, check_id) in &p.checks {
        let Some(l) = live.iter().find(|l| &l.set.id == set_id) else {
            continue;
        };
        let Some(c) = l.set.checks.iter().find(|c| &c.id == check_id) else {
            continue;
        };
        if let CheckKind::Model(_) = c.kind {
            let outcome = verdict.map_or(Outcome::Unverified, |v| v.outcome(set_id, check_id));
            log(l, check_id, outcome);
            continue;
        }
        let drafts = fresh.map(|f| checks::drafts(&l.set, f)).unwrap_or_default();
        let outcome = if drafts.is_empty() {
            Outcome::Unverified
        } else if drafts
            .iter()
            .any(|d| checks::failing(&l.set, c, d, seen).is_some())
        {
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
    let events = crate::hook::autopilot_or_log(now);
    let at = chrono::DateTime::from_timestamp(now, 0).unwrap_or_default();
    let notice = config::load_file()
        .map(Option::unwrap_or_default)
        .and_then(|cfg| chat::notice_new(sid, cfg.private_allowed(cwd), &events, at))
        .unwrap_or_else(|e| {
            log_error(&format!("chat: {e:#}"));
            None
        });
    Ok(stop_output(sent.as_deref(), notice.as_deref()))
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
    let record = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(|p| (Path::new(p), state.skills_read));
    let skills = state.skills.clone();
    let seen = Seen::new(&skills, record, cwd);
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
    // Runs every checking set on `text` and logs each failure not logged
    // yet this turn as caught.
    let caught = |state: &mut session::State, text: &str| {
        let mut fails = Vec::new();
        for &l in &checking {
            for f in checks::run_reply(&l.set, text, &seen) {
                if state.first_log(&turn, &l.set.id, &f.check) {
                    log(l, &f.check, Outcome::Caught);
                }
                fails.push((l, f));
            }
        }
        fails
    };
    // A fix set's model checks: the judge reads the drafts here, inside Stop,
    // so a catch can still go back. `fix` is empty once the turn went back,
    // and the rewrite still needs its verdict.
    let fix_model: Vec<&Live> = checking
        .iter()
        .copied()
        .filter(|l| l.level == Level::Fix && judge::has_model(l))
        .collect();
    let run = judge::Run {
        sid,
        now,
        transcript: v
            .get("transcript_path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(Path::new),
        limit: judge::FIX_LIMIT,
    };
    let judged = |state: &mut session::State, text: &str| -> Option<Verdict> {
        let verdict = judge::judge(&run, &fix_model, text)?;
        for b in &verdict.breaks {
            let set = fix_model.iter().find(|l| l.set.id == b.set);
            if let Some(l) = set
                && state.first_log(&turn, &b.set, &b.check)
            {
                log(l, &b.check, Outcome::Caught);
            }
        }
        Some(verdict)
    };
    let mut fails = Vec::new();
    let out = if again {
        if let Some(p) = state.take_pending(&turn) {
            let fresh = fresh_start(&p, &turn_read).map(|start| parts[start..].join("\n\n"));
            let verdict = match &fresh {
                Some(f) if !fix_model.is_empty() => judged(&mut state, f),
                _ => None,
            };
            recheck(&p, &live, fresh.as_deref(), &seen, verdict.as_ref(), &log);
            // The corrected draft can break a check the first one passed.
            if let Some(f) = &fresh {
                fails = caught(&mut state, f);
            }
        }
        None
    } else {
        let text = parts.join("\n\n");
        fails = caught(&mut state, &text);
        let mut send: Vec<Failure> = fails
            .iter()
            .filter(|(l, _)| fix.contains(&l.set.id))
            .map(|(_, f)| f.clone())
            .collect();
        // A draft a pattern already sends back waits: the judge reads the
        // rewrite instead.
        if send.is_empty() && !fix_model.is_empty() {
            let breaks = judged(&mut state, &text).map(|v| v.breaks);
            send.extend(breaks.unwrap_or_default().into_iter().map(|b| Failure {
                why: b.why(),
                set: b.set,
                check: b.check,
                line: b.quote,
            }));
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
    // A warn set's skill-loaded catch leaves a line for the next prompt:
    // drafts come in rounds, so it lands before the next one.
    if let Some((l, f)) = fails
        .iter()
        .find(|(l, f)| !fix.contains(&l.set.id) && skill_check(l, &f.check))
    {
        state.nudge = Some(Nudge {
            set: l.set.id.clone(),
            check: f.check.clone(),
            text: format!("{NUDGE_START} {}", f.why),
        });
    }
    if let Some((names, end)) = seen.read_from_record() {
        for n in names {
            state.saw_skill(n);
        }
        state.skills_read = *end;
    }
    session::save(dir, sid, &state)?;
    Ok(out)
}

/// Whether the set's check with this id is a skill-loaded check.
fn skill_check(l: &Live, check: &str) -> bool {
    l.set
        .checks
        .iter()
        .any(|c| c.id == check && matches!(c.kind, CheckKind::SkillLoaded(_)))
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
        assert_eq!(scan(&record()).0, vec!["first part", "Saved."]);
        assert!(scan("").0.is_empty());
        assert!(scan("{\"type\":\"assistant\"}\nnot json").0.is_empty());
    }

    #[test]
    fn the_last_message_is_added_once() {
        let dir = std::env::temp_dir().join(format!("dotonce-reply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("record.jsonl");
        std::fs::write(&t, record()).unwrap();
        assert_eq!(
            read_turn(Some(&t), Some("Saved.")).parts,
            vec!["first part", "Saved."]
        );
        assert_eq!(
            read_turn(Some(&t), Some("Done")).parts,
            vec!["first part", "Saved.", "Done"]
        );
        assert_eq!(read_turn(Some(&dir), Some("Done")).parts, vec!["Done"]);
        assert_eq!(read_turn(None, Some("  ")).parts, Vec::<String>::new());
        assert_eq!(
            read_turn(Some(&dir.join("missing")), None).parts,
            Vec::<String>::new()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_send_back_names_a_line_two_sets_share_once() {
        let fail = |set: &str, why: &str| Failure {
            set: set.to_string(),
            check: "inline-links".to_string(),
            line: "See https://example.com".to_string(),
            why: why.to_string(),
        };
        let same = "Links go inline as anchor text.";
        let text = send_back_text(&[
            fail("acme-replies", same),
            fail("acme-notes", same),
            fail("acme-docs", "Use anchor text."),
        ]);
        assert_eq!(text.matches("- inline-links: ").count(), 2, "{text}");
        assert_eq!(text.matches(same).count(), 1, "{text}");
        assert!(text.contains("Use anchor text."), "{text}");
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
