//! MessageDisplay: labels a draft that breaks a fix set's checks, so the user
//! knows a corrected one follows. Every batch shows as it streamed, with the
//! label added after the draft's closing fence. Display only: what Claude
//! sees doesn't change.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approvals::Live;
use crate::checks::{self, fence_closes, fence_open};
use crate::hook::{log_error, prompt_id, session_id, str_field};
use crate::session;

/// The line shown below a draft that breaks the checks with these ids.
pub fn label(ids: &[String]) -> String {
    format!(
        "dotonce: the draft above breaks your {} rule. A corrected one follows.",
        ids.join(", ")
    )
}

/// What display keeps of a message between batches to check a draft once
/// it closes. Never used to show text: every batch shows as it streamed.
///
/// Saved in `sessions/<sid>.display.json`. After an interrupted message
/// (Esc, an API error) that never sends its last batch, the draft's lines
/// and the unfinished last line stay in that file until the next message
/// replaces them or the 7-day sweep of old session files removes it. It is
/// never logged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftCopy {
    pub message_id: String,
    /// The open fence: its character, its length, and whether it opens a
    /// draft whose lines are copied.
    pub fence: Option<(char, usize, bool)>,
    /// The open draft's lines so far, its opening fence first.
    pub lines: Vec<String>,
    /// The batch's unfinished last line, read with the next batch. Kept to
    /// find fences only, never shown again.
    pub partial: String,
    /// The batch index expected next.
    pub next: u64,
    /// A batch came out of order or a check failed: the copy may miss
    /// lines, so the rest of the message isn't checked.
    pub pass: bool,
}

/// A set marks fenced blocks with this info string as its drafts.
fn marks_drafts(l: &Live, info: &str) -> bool {
    l.set
        .draft
        .as_ref()
        .is_some_and(|d| d.fence.iter().any(|f| f == info))
}

/// The ids of the checks the copied draft breaks, for the sets that mark
/// its fence. A whole-reply set checks the full reply at Stop, never one
/// block of it.
fn broken(buf: &mut DraftCopy, sets: &[&Live]) -> Vec<String> {
    let text = buf.lines.concat();
    buf.lines.clear();
    let info = text
        .lines()
        .next()
        .and_then(fence_open)
        .map(|(_, _, info)| info)
        .unwrap_or_default();
    let mut ids: Vec<String> = Vec::new();
    for l in sets.iter().filter(|l| marks_drafts(l, &info)) {
        for f in checks::run_reply(&l.set, &text) {
            if !ids.contains(&f.check) {
                ids.push(f.check);
            }
        }
    }
    ids
}

/// Reads one whole line of the message and returns the ids to label when
/// it closes a draft that breaks a set in `sets`.
fn read_line(buf: &mut DraftCopy, line: &str, sets: &[&Live]) -> Vec<String> {
    let bare = line.trim_end_matches(['\n', '\r']);
    match buf.fence {
        Some((ch, len, draft)) => {
            if draft {
                buf.lines.push(line.to_string());
            }
            if fence_closes(bare, ch, len) {
                buf.fence = None;
                if draft {
                    return broken(buf, sets);
                }
            }
        }
        None => {
            if let Some((ch, len, info)) = fence_open(bare) {
                let draft = sets.iter().any(|l| marks_drafts(l, &info));
                buf.fence = Some((ch, len, draft));
                if draft {
                    buf.lines.push(line.to_string());
                }
            }
        }
    }
    Vec::new()
}

/// Forgets the open fence, the draft's lines and the unfinished line.
fn forget(buf: &mut DraftCopy) {
    buf.fence = None;
    buf.lines.clear();
    buf.partial.clear();
}

/// One batch as it streamed, with the label after the closing fence of each
/// draft that breaks a set in `sets`, the fix sets whose drafts get labeled.
/// A blank line comes before and after the label, so it never joins the
/// next line or turns a following `---` into a heading. `last` is the
/// message's final batch.
///
/// Only whole lines are read: a batch's unfinished last line waits in
/// `partial` for the rest of it, except at the last batch. The batch's own
/// text is never changed; the one newline added is at the last batch, to
/// end a closing fence line before its label.
pub fn step(buf: &mut DraftCopy, delta: &str, last: bool, sets: &[&Live]) -> String {
    let mut text = std::mem::take(&mut buf.partial);
    let carried = text.len();
    text.push_str(delta);
    let mut shown = String::new();
    let mut shown_to = 0;
    let mut read_to = 0;
    for line in text.split_inclusive('\n') {
        read_to += line.len();
        let whole = line.ends_with('\n');
        if !whole && !last {
            buf.partial = line.to_string();
            break;
        }
        let ids = read_line(buf, line, sets);
        if ids.is_empty() {
            continue;
        }
        // A whole line ends in a newline from this batch, so its end lies
        // inside `delta`.
        let at = read_to - carried;
        shown.push_str(&delta[shown_to..at]);
        shown_to = at;
        if !whole {
            shown.push('\n');
        }
        shown.push_str(&format!("\n{}\n\n", label(&ids)));
    }
    shown.push_str(&delta[shown_to..]);
    if last {
        forget(buf);
    }
    shown
}

fn load_copy(path: &Path) -> DraftCopy {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Every batch shows exactly as it streamed; at most a label is added after
/// a draft's closing fence. Anything that goes wrong skips the label, never
/// the text.
pub fn display(v: &Value, cwd: &Path) -> Result<Option<String>> {
    let Ok(sid) = session_id(v) else {
        return Ok(None);
    };
    let dir = session::dir()?;
    let path = session::display_file(&dir, sid)?;
    let message = str_field(v, "message_id");
    let state = session::load(&dir, sid)?;
    let mut buf = load_copy(&path);
    let same = buf.message_id == message;
    if !same {
        buf = DraftCopy {
            message_id: message.to_string(),
            ..DraftCopy::default()
        };
    }
    let delta = str_field(v, "delta");
    // Without a final flag, nothing is kept past this batch.
    let last = v.get("final").and_then(Value::as_bool).unwrap_or(true);
    // Labeled only when Stop will send the draft back, so a corrected one
    // follows: a fix set, a turn not sent back yet, and dotonce run at Stop.
    let fix = session::checks_now(&state, cwd, prompt_id(v))
        .map(|now| now.fix().into_iter().cloned().collect::<Vec<_>>())
        .map(|fix| {
            if fix.is_empty() || settings_run_stop(cwd) {
                fix
            } else {
                Vec::new()
            }
        });
    let shown = match fix {
        Ok(fix) if fix.is_empty() => return Ok(None),
        Ok(fix) => {
            if let Some(i) = v.get("index").and_then(Value::as_u64) {
                buf.pass |= i != if same { buf.next } else { 0 };
                buf.next = i + 1;
            }
            if buf.pass {
                forget(&mut buf);
                delta.to_string()
            } else {
                step(&mut buf, delta, last, &fix.iter().collect::<Vec<_>>())
            }
        }
        Err(e) => {
            log_error(&format!("display: {e:#}"));
            buf.pass = true;
            forget(&mut buf);
            delta.to_string()
        }
    };
    let saved = if last {
        let _ = fs::remove_file(&path);
        Ok(())
    } else {
        save_copy(&dir, &path, &buf)
    };
    if let Err(e) = saved {
        // A copy that isn't saved can't be checked later: the next batch
        // finds none and skips the label.
        log_error(&format!("display: {e:#}"));
        let _ = fs::remove_file(&path);
    }
    if shown == delta {
        return Ok(None);
    }
    Ok(Some(
        json!({"hookSpecificOutput": {"hookEventName": "MessageDisplay", "displayContent": shown}})
            .to_string(),
    ))
}

/// Whether Claude Code runs `dotonce hook stop` at Stop. Without it no
/// corrected draft follows, so display never labels. Read at every call
/// that has a fix set, so display never writes the session state Stop
/// keeps. Reads Claude Code's user settings and the project's settings
/// under `cwd`, without changing them. A file that exists but can't be read
/// or parsed, or one that sets `"disableAllHooks": true`, counts as no Stop
/// hook.
fn settings_run_stop(cwd: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return false;
    };
    let files = [
        PathBuf::from(home).join(".claude/settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ];
    let mut found = false;
    for f in &files {
        let src = match fs::read_to_string(f) {
            Ok(src) => src,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        };
        let Ok(settings) = serde_json::from_str::<Value>(&src) else {
            return false;
        };
        if settings.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
            return false;
        }
        found |= runs_stop(&settings);
    }
    found
}

/// A Stop hook command that runs `dotonce hook stop`.
fn runs_stop(settings: &Value) -> bool {
    settings
        .pointer("/hooks/Stop")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .any(|c| c.contains("dotonce") && c.contains("hook stop"))
}

/// Written to a temp file and renamed, like `session::save`, so a hook that
/// stops halfway never leaves a broken file.
fn save_copy(dir: &Path, path: &Path, buf: &DraftCopy) -> Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string(buf)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Level, parse};
    use std::path::Path;

    fn live() -> Live {
        let src = "id = \"t\"\n[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"stock\"\nforbid = [\"sorry for the wait\"]\nwhy = \"w\"\n";
        Live {
            set: parse(src, Path::new("/r/t.toml")).unwrap(),
            level: Level::Fix,
        }
    }

    #[test]
    fn only_a_stop_hook_that_runs_dotonce_counts() {
        let entry =
            |c: &str| json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": c}]}]}});
        assert!(runs_stop(&entry("'/opt/acme/dotonce' hook stop")));
        assert!(!runs_stop(&entry("dotonce hook prompt")));
        assert!(!runs_stop(&entry("/opt/acme/notify hook stop")));
        assert!(!runs_stop(
            &json!({"hooks": {"UserPromptSubmit": [{"hooks": [{"command": "dotonce hook stop"}]}]}})
        ));
        assert!(!runs_stop(&json!({"hooks": {"Stop": "dotonce hook stop"}})));
        assert!(!runs_stop(&json!([])));
    }

    /// What the screen shows after a closing fence for a draft that breaks
    /// the checks with these ids.
    fn after(ids: &[&str]) -> String {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        format!("\n{}\n\n", label(&ids))
    }

    #[test]
    fn the_label_says_the_corrected_draft_follows() {
        assert_eq!(
            label(&["stock".to_string(), "links".to_string()]),
            "dotonce: the draft above breaks your stock, links rule. A corrected one follows."
        );
    }

    #[test]
    fn a_broken_draft_gets_the_label_after_its_closing_fence() {
        let l = live();
        let sets = vec![&l];
        let mut buf = DraftCopy::default();
        assert_eq!(
            step(
                &mut buf,
                "Here it is:\n```markdown\nHi Sam,\n",
                false,
                &sets
            ),
            "Here it is:\n```markdown\nHi Sam,\n"
        );
        assert_eq!(buf.lines.len(), 2);
        assert_eq!(
            step(&mut buf, "Sorry for the wait.\n```\nDone.\n", false, &sets),
            format!("Sorry for the wait.\n```\n{}Done.\n", after(&["stock"]))
        );
        assert!(buf.lines.is_empty() && buf.fence.is_none());
    }

    #[test]
    fn a_closing_fence_without_a_newline_gets_its_own_line_before_the_label() {
        let l = live();
        assert_eq!(
            step(
                &mut DraftCopy::default(),
                "```markdown\nSorry for the wait.\n```",
                true,
                &[&l]
            ),
            format!(
                "```markdown\nSorry for the wait.\n```\n{}",
                after(&["stock"])
            )
        );
    }

    #[test]
    fn the_label_names_only_the_failing_checks() {
        let src = "id = \"t\"\n[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"stock\"\nforbid = [\"sorry for the wait\"]\nwhy = \"w\"\n[[check]]\nid = \"links\"\nkind = \"bare-url\"\nwhy = \"w\"\n[[check]]\nid = \"tone\"\nforbid = [\"asap\"]\nwhy = \"w\"\n";
        let l = Live {
            set: parse(src, Path::new("/r/t.toml")).unwrap(),
            level: Level::Fix,
        };
        let draft = "```markdown\nSorry for the wait.\nSee https://example.com\n```\n";
        assert_eq!(
            step(&mut DraftCopy::default(), draft, true, &[&l]),
            format!("{draft}{}", after(&["stock", "links"]))
        );
    }

    #[test]
    fn a_line_split_across_batches_is_read_whole() {
        let l = live();
        let sets = vec![&l];
        let mut buf = DraftCopy::default();
        assert_eq!(
            step(
                &mut buf,
                "```markdown\nSorry for the wait.\n```",
                false,
                &sets
            ),
            "```markdown\nSorry for the wait.\n```",
            "an unfinished line isn't a closing fence yet"
        );
        assert_eq!(
            step(&mut buf, "bash\nmore\n```\nDone.\n", false, &sets),
            format!("bash\nmore\n```\n{}Done.\n", after(&["stock"])),
            "```bash is a line of the draft, not its close"
        );
        let mut buf = DraftCopy::default();
        assert_eq!(
            step(
                &mut buf,
                "```markdown\nSorry for the wait.\n``",
                false,
                &sets
            ),
            "```markdown\nSorry for the wait.\n``"
        );
        assert_eq!(
            step(&mut buf, "`\nDone.\n", false, &sets),
            format!("`\n{}Done.\n", after(&["stock"])),
            "a closing fence split in two is still found"
        );
        let mut buf = DraftCopy::default();
        assert_eq!(
            step(&mut buf, "``", false, &sets),
            "``",
            "an opening fence split in two"
        );
        assert_eq!(
            step(
                &mut buf,
                "`markdown\nSorry for the wait.\n```\n",
                true,
                &sets
            ),
            format!("`markdown\nSorry for the wait.\n```\n{}", after(&["stock"]))
        );
        assert!(buf.partial.is_empty() && buf.lines.is_empty());
    }

    #[test]
    fn a_passing_draft_gets_no_label() {
        let l = live();
        let sets = vec![&l];
        let mut buf = DraftCopy::default();
        assert_eq!(
            step(&mut buf, "```markdown\nHi Sam,\n", false, &sets),
            "```markdown\nHi Sam,\n"
        );
        assert_eq!(
            step(&mut buf, "Thanks for the details.\n```\n", false, &sets),
            "Thanks for the details.\n```\n"
        );
    }

    #[test]
    fn a_whole_reply_set_never_labels_a_passing_draft() {
        let l = live();
        let whole = Live {
            set: parse(
                "id = \"chat\"\n[[check]]\nid = \"signoff\"\nrequire = [\"cheers\"]\nwhy = \"w\"\n",
                Path::new("/r/chat.toml"),
            )
            .unwrap(),
            level: Level::Fix,
        };
        let sets = vec![&l, &whole];
        let text = "Here:\n```markdown\nHi Sam,\nThanks for the details.\n```\nCheers\n";
        assert_eq!(step(&mut DraftCopy::default(), text, true, &sets), text);
    }

    #[test]
    fn other_fences_and_unclosed_drafts_show_as_they_are() {
        let l = live();
        let sets = vec![&l];
        let mut buf = DraftCopy::default();
        assert_eq!(
            step(&mut buf, "```bash\nsorry for the wait\n```\n", false, &sets),
            "```bash\nsorry for the wait\n```\n"
        );
        assert!(buf.lines.is_empty(), "only draft lines are copied");
        assert_eq!(
            step(&mut buf, "```markdown\nSorry for the wait.", true, &sets),
            "```markdown\nSorry for the wait."
        );
        assert!(buf.lines.is_empty() && buf.fence.is_none());
        assert_eq!(
            step(
                &mut DraftCopy::default(),
                "```markdown\nSorry for the wait.\n```\n",
                false,
                &[]
            ),
            "```markdown\nSorry for the wait.\n```\n"
        );
    }
}
