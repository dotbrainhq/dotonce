//! MessageDisplay: labels a draft that breaks a fix set's checks, so the user
//! knows a corrected one follows. Never hides or drops text. Display only:
//! what Claude sees doesn't change.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approvals::{self, Live};
use crate::checks::{self, fence_closes, fence_open};
use crate::hook::{log_error, prompt_id, session_id, str_field};
use crate::session;

/// The line shown above a draft that breaks the checks with these ids.
pub fn label(ids: &[String]) -> String {
    format!(
        "dotonce: this draft breaks your {} rule. A corrected one follows below.",
        ids.join(", ")
    )
}

/// A message's lines still buffered while a draft streams in, so it can be
/// checked whole.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Buffer {
    pub message_id: String,
    /// The open fence: its character, its length, and whether its lines are
    /// buffered.
    pub fence: Option<(char, usize, bool)>,
    pub lines: Vec<String>,
    /// The batch index expected next.
    #[serde(default)]
    pub next: u64,
    /// A batch came out of order or a check failed: the rest of the message
    /// shows as it is.
    #[serde(default)]
    pub pass: bool,
}

/// A set marks fenced blocks with this info string as its drafts.
fn marks_drafts(l: &Live, info: &str) -> bool {
    l.set
        .draft
        .as_ref()
        .is_some_and(|d| d.fence.iter().any(|f| f == info))
}

/// The buffered block as written, under the label when a set that marks its
/// fence fails it. A whole-reply set checks the full reply at Stop, never
/// one block of it.
fn release(buf: &mut Buffer, sets: &[&Live]) -> String {
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
    if ids.is_empty() {
        text
    } else {
        format!("{}\n{text}", label(&ids))
    }
}

/// What to show in place of one batch of lines. `sets` are the fix sets
/// whose drafts get labeled; `last` is the message's final batch.
pub fn step(buf: &mut Buffer, delta: &str, last: bool, sets: &[&Live]) -> String {
    let mut shown = String::new();
    for line in delta.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        match buf.fence {
            Some((ch, len, buffered)) => {
                if buffered {
                    buf.lines.push(line.to_string());
                } else {
                    shown.push_str(line);
                }
                if fence_closes(bare, ch, len) {
                    buf.fence = None;
                    if buffered {
                        shown.push_str(&release(buf, sets));
                    }
                }
            }
            None => {
                if let Some((ch, len, info)) = fence_open(bare) {
                    let buffered = sets.iter().any(|l| marks_drafts(l, &info));
                    buf.fence = Some((ch, len, buffered));
                    if buffered {
                        buf.lines.push(line.to_string());
                        continue;
                    }
                }
                shown.push_str(line);
            }
        }
    }
    if last {
        shown.push_str(&buf.lines.concat());
        buf.lines.clear();
        buf.fence = None;
    }
    shown
}

fn load_buffer(path: &Path) -> Buffer {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Everything buffered, then the batch as it is.
fn release_all(buf: &mut Buffer, delta: &str) -> String {
    let mut shown = buf.lines.concat();
    shown.push_str(delta);
    buf.lines.clear();
    buf.fence = None;
    shown
}

/// Buffered lines only reach the screen through a later batch's output, so
/// anything that goes wrong while lines are buffered shows them at once and
/// lets the rest of the message through as it is.
pub fn display(v: &Value, cwd: &Path) -> Result<Option<String>> {
    let sid = match session_id(v) {
        Ok(sid) => sid,
        Err(_) if approvals::load_live().is_ok_and(|(l, _)| l.is_empty()) => return Ok(None),
        Err(e) => return Err(e),
    };
    let dir = session::dir()?;
    let path = session::display_file(&dir, sid)?;
    let message = str_field(v, "message_id");
    let state = session::load(&dir, sid).unwrap_or_default();
    let mut buf = load_buffer(&path);
    let mut early = String::new();
    let same = buf.message_id == message;
    // Lines still buffered from another message: that message stopped before
    // its final batch (Esc), so they show now, ahead of this one.
    if !same {
        if !buf.lines.is_empty() {
            early.push_str(&buf.lines.concat());
            if !early.ends_with('\n') {
                early.push('\n');
            }
            // The draft stopped inside its fence: close it, so this
            // message doesn't show inside the old block.
            if let Some((ch, len, _)) = buf.fence {
                early.push_str(&ch.to_string().repeat(len));
                early.push('\n');
            }
        }
        buf = Buffer {
            message_id: message.to_string(),
            ..Buffer::default()
        };
    }
    let delta = str_field(v, "delta");
    // Without a final flag, nothing is buffered past this batch.
    let last = v.get("final").and_then(Value::as_bool).unwrap_or(true);
    // Labeled only when Stop will send the draft back, so a corrected one
    // follows: a fix set, a turn not sent back yet, and dotonce run at Stop.
    let fix = session::checks_now(&state, cwd, prompt_id(v))
        .map(|now| now.fix().into_iter().cloned().collect::<Vec<_>>())
        .map(|fix| {
            if fix.is_empty() || stop_registered(&dir, sid, &state, cwd) {
                fix
            } else {
                Vec::new()
            }
        });
    let mut shown = match fix {
        Ok(fix) if fix.is_empty() && buf.lines.is_empty() && early.is_empty() => {
            return Ok(None);
        }
        Ok(fix) => {
            if let Some(i) = v.get("index").and_then(Value::as_u64) {
                buf.pass |= i != if same { buf.next } else { 0 };
                buf.next = i + 1;
            }
            if buf.pass {
                release_all(&mut buf, delta)
            } else {
                step(&mut buf, delta, last, &fix.iter().collect::<Vec<_>>())
            }
        }
        Err(e) => {
            log_error(&format!("display: {e:#}"));
            buf.pass = true;
            release_all(&mut buf, delta)
        }
    };
    let saved = if last {
        let _ = fs::remove_file(&path);
        Ok(())
    } else {
        save_buffer(&dir, &path, &buf)
    };
    if let Err(e) = saved {
        // What isn't saved can't show later, so it shows now.
        log_error(&format!("display: {e:#}"));
        shown.push_str(&buf.lines.concat());
        let _ = fs::remove_file(&path);
    }
    shown.insert_str(0, &early);
    if shown == delta {
        return Ok(None);
    }
    Ok(Some(
        json!({"hookSpecificOutput": {"hookEventName": "MessageDisplay", "displayContent": shown}})
            .to_string(),
    ))
}

/// Whether Claude Code runs `dotonce hook stop` at Stop. Without it no
/// corrected draft follows, so display never labels. Read once per session
/// and kept in the session state.
fn stop_registered(dir: &Path, sid: &str, state: &session::State, cwd: &Path) -> bool {
    if let Some(found) = state.stop_hook {
        return found;
    }
    let found = settings_run_stop(cwd);
    let saved = session::load(dir, sid).and_then(|mut fresh| {
        fresh.stop_hook = Some(found);
        session::save(dir, sid, &fresh)
    });
    if let Err(e) = saved {
        log_error(&format!("display: {e:#}"));
    }
    found
}

/// Reads Claude Code's user settings and the project's settings under
/// `cwd`, without changing them. A file that exists but can't be read or
/// parsed counts as no Stop hook.
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
fn save_buffer(dir: &Path, path: &Path, buf: &Buffer) -> Result<()> {
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

    #[test]
    fn a_broken_draft_shows_in_full_under_a_label() {
        let l = live();
        let sets = vec![&l];
        let mut buf = Buffer::default();
        assert_eq!(
            step(
                &mut buf,
                "Here it is:\n```markdown\nHi Sam,\n",
                false,
                &sets
            ),
            "Here it is:\n"
        );
        assert_eq!(buf.lines.len(), 2);
        assert_eq!(
            step(&mut buf, "Sorry for the wait.\n```\nDone.\n", false, &sets),
            "dotonce: this draft breaks your stock rule. A corrected one follows below.\n```markdown\nHi Sam,\nSorry for the wait.\n```\nDone.\n"
        );
        assert!(buf.lines.is_empty() && buf.fence.is_none());
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
            step(&mut Buffer::default(), draft, true, &[&l]),
            format!(
                "dotonce: this draft breaks your stock, links rule. A corrected one follows below.\n{draft}"
            )
        );
    }

    #[test]
    fn a_passing_draft_shows_in_full_at_its_closing_fence() {
        let l = live();
        let sets = vec![&l];
        let mut buf = Buffer::default();
        assert_eq!(step(&mut buf, "```markdown\nHi Sam,\n", false, &sets), "");
        assert_eq!(
            step(&mut buf, "Thanks for the details.\n```\n", false, &sets),
            "```markdown\nHi Sam,\nThanks for the details.\n```\n"
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
        let mut buf = Buffer::default();
        assert_eq!(
            step(
                &mut buf,
                "Here:\n```markdown\nHi Sam,\nThanks for the details.\n```\nCheers\n",
                true,
                &sets
            ),
            "Here:\n```markdown\nHi Sam,\nThanks for the details.\n```\nCheers\n"
        );
    }

    #[test]
    fn other_fences_and_unclosed_drafts_show_as_they_are() {
        let l = live();
        let sets = vec![&l];
        let mut buf = Buffer::default();
        assert_eq!(
            step(&mut buf, "```bash\nsorry for the wait\n```\n", false, &sets),
            "```bash\nsorry for the wait\n```\n"
        );
        assert_eq!(
            step(&mut buf, "```markdown\nSorry for the wait.", true, &sets),
            "```markdown\nSorry for the wait."
        );
        assert!(buf.lines.is_empty());
        let mut buf = Buffer::default();
        assert_eq!(
            step(
                &mut buf,
                "```markdown\nSorry for the wait.\n```\n",
                false,
                &[]
            ),
            "```markdown\nSorry for the wait.\n```\n"
        );
    }
}
