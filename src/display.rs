//! MessageDisplay: holds a draft back on screen until its checks pass, so a
//! broken draft never shows. Display only: what Claude sees doesn't change.

use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approvals::{self, Live};
use crate::checks::{self, fence_closes, fence_open};
use crate::hook::{log_error, prompt_id, session_id, str_field};
use crate::rules::Level;
use crate::session;

pub const HELD_NOTE: &str =
    "(dotonce held back a draft that breaks your rules. The corrected draft follows.)";
pub const INTERRUPTED_NOTE: &str =
    "(dotonce: an earlier draft, not checked because the reply was interrupted)";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Held {
    pub message_id: String,
    /// The open fence: its character, its length, and whether its lines are held.
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

/// The held block, or the note when a set that holds its fence fails it. A
/// whole-reply set checks the full reply at Stop, never one block of it.
fn release(held: &mut Held, sets: &[&Live]) -> String {
    let text = held.lines.concat();
    held.lines.clear();
    let info = text
        .lines()
        .next()
        .and_then(fence_open)
        .map(|(_, _, info)| info)
        .unwrap_or_default();
    let broken = sets
        .iter()
        .filter(|l| marks_drafts(l, &info))
        .any(|l| !checks::run_reply(&l.set, &text).is_empty());
    if broken {
        format!("{HELD_NOTE}\n")
    } else {
        text
    }
}

/// What to show in place of one batch of lines. `sets` are the checking fix
/// sets; `last` is the message's final batch.
pub fn step(held: &mut Held, delta: &str, last: bool, sets: &[&Live]) -> String {
    let mut shown = String::new();
    for line in delta.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        match held.fence {
            Some((ch, len, hold)) => {
                if hold {
                    held.lines.push(line.to_string());
                } else {
                    shown.push_str(line);
                }
                if fence_closes(bare, ch, len) {
                    held.fence = None;
                    if hold {
                        shown.push_str(&release(held, sets));
                    }
                }
            }
            None => {
                if let Some((ch, len, info)) = fence_open(bare) {
                    let hold = sets.iter().any(|l| marks_drafts(l, &info));
                    held.fence = Some((ch, len, hold));
                    if hold {
                        held.lines.push(line.to_string());
                        continue;
                    }
                }
                shown.push_str(line);
            }
        }
    }
    if last {
        shown.push_str(&held.lines.concat());
        held.lines.clear();
        held.fence = None;
    }
    shown
}

fn load_held(path: &Path) -> Held {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The session's checking fix sets. None once the turn was sent back: Stop
/// sends a turn back once, so a draft held after that would never be
/// followed by a corrected one.
fn fix_sets(
    live: Result<Vec<Live>>,
    dir: &Path,
    sid: &str,
    turn: Option<&str>,
) -> Result<Vec<Live>> {
    let live = live?;
    if live.is_empty() {
        return Ok(live);
    }
    let state = session::load(dir, sid)?;
    let turn = turn.or(state.prompt_id.as_deref()).unwrap_or("unknown");
    if state.sent_back(turn) {
        return Ok(Vec::new());
    }
    Ok(session::checking(&state, &live)
        .into_iter()
        .filter(|l| l.level == Level::Fix)
        .cloned()
        .collect())
}

/// Everything held, then the batch as it is.
fn release_all(held: &mut Held, delta: &str) -> String {
    let mut shown = held.lines.concat();
    shown.push_str(delta);
    held.lines.clear();
    held.fence = None;
    shown
}

/// Hidden lines only reach the screen through a later batch's output, so
/// anything that goes wrong after a hold shows the held lines at once and
/// lets the rest of the message through as it is.
pub fn display(v: &Value) -> Result<Option<String>> {
    let live = approvals::load_live().map(|(l, _)| l);
    let no_sets = live.as_ref().is_ok_and(Vec::is_empty);
    let sid = match session_id(v) {
        Err(_) if no_sets => return Ok(None),
        sid => sid?,
    };
    let dir = session::dir()?;
    let path = session::display_file(&dir, sid)?;
    let message = str_field(v, "message_id");
    let mut held = load_held(&path);
    let same = held.message_id == message;
    // Lines still held from another message: that message stopped before its
    // final batch (Esc), so they show now, ahead of this one.
    let mut stale = String::new();
    if !same {
        if !held.lines.is_empty() {
            stale = format!("{INTERRUPTED_NOTE}\n{}", held.lines.concat());
            if !stale.ends_with('\n') {
                stale.push('\n');
            }
        }
        held = Held {
            message_id: message.to_string(),
            ..Held::default()
        };
    }
    let delta = str_field(v, "delta");
    // Without a final flag, nothing is held past this batch.
    let last = v.get("final").and_then(Value::as_bool).unwrap_or(true);
    let mut shown = match fix_sets(live, &dir, sid, prompt_id(v)) {
        Ok(fix) if fix.is_empty() && held.lines.is_empty() && stale.is_empty() => {
            return Ok(None);
        }
        Ok(fix) => {
            if let Some(i) = v.get("index").and_then(Value::as_u64) {
                held.pass |= i != if same { held.next } else { 0 };
                held.next = i + 1;
            }
            if held.pass {
                release_all(&mut held, delta)
            } else {
                step(&mut held, delta, last, &fix.iter().collect::<Vec<_>>())
            }
        }
        Err(e) => {
            log_error(&format!("display: {e:#}"));
            held.pass = true;
            release_all(&mut held, delta)
        }
    };
    if last {
        let _ = fs::remove_file(&path);
    } else if let Err(e) = save_held(&dir, &path, &held) {
        log_error(&format!("display: {e:#}"));
        shown.push_str(&held.lines.concat());
        let _ = fs::remove_file(&path);
    }
    shown.insert_str(0, &stale);
    if shown == delta {
        return Ok(None);
    }
    Ok(Some(
        json!({"hookSpecificOutput": {"hookEventName": "MessageDisplay", "displayContent": shown}})
            .to_string(),
    ))
}

fn save_held(dir: &Path, path: &Path, held: &Held) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(path, serde_json::to_string(held)?)?;
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
    fn a_broken_draft_is_held_and_swapped_for_a_note() {
        let l = live();
        let sets = vec![&l];
        let mut held = Held::default();
        assert_eq!(
            step(
                &mut held,
                "Here it is:\n```markdown\nHi Sam,\n",
                false,
                &sets
            ),
            "Here it is:\n"
        );
        assert_eq!(held.lines.len(), 2);
        assert_eq!(
            step(&mut held, "Sorry for the wait.\n```\nDone.\n", false, &sets),
            format!("{HELD_NOTE}\nDone.\n")
        );
        assert!(held.lines.is_empty() && held.fence.is_none());
    }

    #[test]
    fn a_passing_draft_shows_in_full_at_its_closing_fence() {
        let l = live();
        let sets = vec![&l];
        let mut held = Held::default();
        assert_eq!(step(&mut held, "```markdown\nHi Sam,\n", false, &sets), "");
        assert_eq!(
            step(&mut held, "Thanks for the details.\n```\n", false, &sets),
            "```markdown\nHi Sam,\nThanks for the details.\n```\n"
        );
    }

    #[test]
    fn a_whole_reply_set_never_hides_a_passing_draft() {
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
        let mut held = Held::default();
        assert_eq!(
            step(
                &mut held,
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
        let mut held = Held::default();
        assert_eq!(
            step(
                &mut held,
                "```bash\nsorry for the wait\n```\n",
                false,
                &sets
            ),
            "```bash\nsorry for the wait\n```\n"
        );
        assert_eq!(
            step(&mut held, "```markdown\nSorry for the wait.", true, &sets),
            "```markdown\nSorry for the wait."
        );
        assert!(held.lines.is_empty());
        let mut held = Held::default();
        assert_eq!(
            step(
                &mut held,
                "```markdown\nSorry for the wait.\n```\n",
                false,
                &[]
            ),
            "```markdown\nSorry for the wait.\n```\n"
        );
    }
}
