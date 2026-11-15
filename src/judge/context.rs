//! What the judge reads besides the rules and the drafts: the session so
//! far, from the tail of its record. That's the user's messages, and the
//! tool calls with the start of what each one returned, oldest first.

use std::collections::HashMap;

use serde_json::Value;

/// The turn's first message is cut to this many characters, start and end kept.
const PROMPT_CHARS: usize = 12_000;
/// Each earlier message is cut to this many.
const MESSAGE_CHARS: usize = 3_000;
/// Older entries stop being added once the session log is this long.
const SESSION_CHARS: usize = 40_000;
const TOOL_CALLS: usize = 60;
const TOOL_INPUT_CHARS: usize = 150;
const TOOL_RESULT_CHARS: usize = 300;
/// The reply around the drafts is cut to this many characters.
pub const REPLY_CHARS: usize = 8_000;

/// What the model reads besides the rules and the drafts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    /// The session so far, oldest first, one entry per message or tool call.
    pub session: Vec<String>,
    /// The reply the drafts sit in, each draft swapped for its number.
    pub reply: String,
}

fn blocks_text(content: Option<&Value>) -> String {
    match content {
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

/// What the user typed, without the reminders Claude Code adds to it.
fn typed(e: &Value) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let text = blocks_text(e.pointer("/message/content"));
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        rest = rest[i..]
            .find(CLOSE)
            .map_or("", |j| &rest[i + j + CLOSE.len()..]);
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The text cut to `max` characters, keeping its start and end.
pub(super) fn trim_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(n - half).collect();
    format!(
        "{head}\n[... {} characters left out ...]\n{tail}",
        n - 2 * half
    )
}

/// The first `max` characters on one line.
fn squash(s: &str, max: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// One line for a tool call: its name and the start of its input.
fn tool_line(name: &str, input: &Value) -> String {
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "notebook_path",
        "path",
        "url",
        "pattern",
        "query",
        "skill",
        "description",
    ];
    let detail = KEYS
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| match input {
            Value::Null => String::new(),
            Value::Object(o) if o.is_empty() => String::new(),
            v => v.to_string(),
        });
    let detail = squash(&detail, TOOL_INPUT_CHARS);
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {detail}")
    }
}

/// The session so far from the tail of its record. The turn's first message
/// is always in it; older entries come in until the log reaches
/// `SESSION_CHARS`, with at most `TOOL_CALLS` tool calls.
pub fn context(src: &str) -> Context {
    let mut session = Vec::new();
    let mut results: HashMap<String, String> = HashMap::new();
    let (mut used, mut tools, mut turn_found) = (0, 0, false);
    for line in src.lines().rev() {
        if turn_found && used >= SESSION_CHARS {
            break;
        }
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if e.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match e.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(Value::Array(blocks)) = e.pointer("/message/content") else {
                    continue;
                };
                for b in blocks.iter().rev() {
                    if tools >= TOOL_CALLS
                        || b.get("type").and_then(Value::as_str) != Some("tool_use")
                    {
                        continue;
                    }
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let mut entry = tool_line(name, b.get("input").unwrap_or(&Value::Null));
                    let result = b
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| results.remove(id))
                        .filter(|r| !r.is_empty());
                    if let Some(r) = result {
                        entry = format!("{entry} -> {r}");
                    }
                    used += entry.chars().count();
                    session.push(entry);
                    tools += 1;
                }
            }
            Some("user") => {
                if let Some(Value::Array(blocks)) = e.pointer("/message/content") {
                    for b in blocks {
                        if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        if let Some(id) = b.get("tool_use_id").and_then(Value::as_str) {
                            let text = blocks_text(b.get("content"));
                            results.insert(id.to_string(), squash(&text, TOOL_RESULT_CHARS));
                        }
                    }
                }
                if !crate::reply::starts_turn(&e)
                    || e.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                let text = typed(&e);
                if text.is_empty() {
                    continue;
                }
                let entry = if turn_found {
                    format!("User: {}", trim_middle(&text, MESSAGE_CHARS))
                } else {
                    format!("User, this turn: {}", trim_middle(&text, PROMPT_CHARS))
                };
                used += entry.chars().count();
                session.push(entry);
                turn_found = true;
            }
            _ => {}
        }
    }
    session.reverse();
    Context {
        session,
        reply: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn src(lines: &[Value]) -> String {
        lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_session_has_messages_and_tool_calls_with_their_results() {
        let lines = vec![
            json!({"type": "user", "message": {"content": "Sam at acme wrote in. <system-reminder>noise</system-reminder>"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "curl -sI   https://example.com"}}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "HTTP/2 200\nserver: nginx"}]}]}}),
            json!({"type": "user", "isCompactSummary": true, "message": {"content": "a summary"}}),
            json!({"type": "user", "message": {"content": [{"type": "text", "text": "Draft the reply to Sam"}]}}),
            json!({"type": "user", "isMeta": true, "message": {"content": "skill text"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/acme/a.txt"}}]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "tool_use", "name": "Grep", "input": {}}]}}),
        ];
        let c = context(&src(&lines));
        assert_eq!(
            c.session,
            [
                "User: Sam at acme wrote in.",
                "Bash: curl -sI https://example.com -> HTTP/2 200 server: nginx",
                "User, this turn: Draft the reply to Sam",
                "Read: /acme/a.txt",
            ]
        );
    }

    #[test]
    fn the_session_keeps_the_last_tool_calls_and_cuts_long_text() {
        let mut lines = vec![json!({"type": "user", "message": {"content": "Draft the reply"}})];
        for i in 0..70 {
            lines.push(json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": format!("t{i}"), "name": "Bash", "input": {"command": format!("echo   {i}\nls"), "description": "d"}}]}}));
            lines.push(json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": format!("t{i}"), "content": "x".repeat(400)}]}}));
        }
        let c = context(&src(&lines));
        assert_eq!(c.session.len(), TOOL_CALLS + 1);
        assert_eq!(c.session[0], "User, this turn: Draft the reply");
        let first = format!("Bash: echo 10 ls -> {}", "x".repeat(TOOL_RESULT_CHARS));
        assert_eq!(c.session[1], first);
        assert!(c.session[60].starts_with("Bash: echo 69 ls -> "));
        assert_eq!(
            tool_line("Agent", &json!({"prompt": "x"})),
            "Agent: {\"prompt\":\"x\"}"
        );
        assert_eq!(tool_line("TodoWrite", &json!({})), "TodoWrite");
        let long = "a".repeat(400);
        assert_eq!(
            tool_line("Bash", &json!({"command": long})).len(),
            6 + TOOL_INPUT_CHARS
        );
    }

    #[test]
    fn older_messages_stop_once_the_log_is_full() {
        let mut lines = Vec::new();
        for i in 0..30 {
            let text = format!("{i} {}", "m".repeat(5_000));
            lines.push(json!({"type": "user", "message": {"content": text}}));
        }
        let c = context(&src(&lines));
        let used: usize = c.session.iter().map(|s| s.chars().count()).sum();
        assert!(used < SESSION_CHARS + PROMPT_CHARS, "{used}");
        assert!(
            c.session
                .last()
                .unwrap()
                .starts_with("User, this turn: 29 ")
        );
        assert!(c.session[0].starts_with("User: "));
        assert!(c.session[0].contains("characters left out"));
        assert!(c.session.len() < 30);
    }

    #[test]
    fn a_long_message_keeps_its_start_and_end() {
        let s = format!(
            "{}{}{}",
            "a".repeat(7_000),
            "b".repeat(1_000),
            "c".repeat(7_000)
        );
        let t = trim_middle(&s, PROMPT_CHARS);
        assert!(t.starts_with(&"a".repeat(6_000)), "{}", &t[..20]);
        assert!(t.ends_with(&"c".repeat(6_000)));
        assert!(t.contains("[... 3000 characters left out ...]"));
        assert_eq!(trim_middle("short", PROMPT_CHARS), "short");
    }
}
