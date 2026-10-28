//! MessageDisplay: holds a draft back on screen until its checks pass, so a
//! broken draft never shows. Display only: what Claude sees doesn't change.

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

pub const HELD_NOTE: &str =
    "(dotonce held back a draft that breaks one of your rules. Claude is writing a corrected one.)";
pub const INTERRUPTED_NOTE: &str =
    "(dotonce: an earlier draft, not checked because the reply was interrupted)";
pub const UNFIXED_NOTE: &str = "dotonce held back a draft that wasn't fixed; here it is:";
/// A withheld draft this old shows at the next hook of any kind.
const MAX_WITHHELD_AGE: i64 = 60 * 60;

/// A draft that was swapped for the note. Stop clears it when it sends the
/// turn back, since the corrected draft follows; any other way, it shows.
/// It holds work text, so it lives only in the session's display file
/// (swept by `session::clean_old`) and is never logged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Withheld {
    pub turn: String,
    pub message_id: String,
    pub text: String,
    /// When it was held back, in Unix seconds.
    pub time: i64,
}

impl Withheld {
    pub fn is_old(&self, now: i64) -> bool {
        now - self.time > MAX_WITHHELD_AGE
    }
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<Withheld>,
    /// Drafts swapped for the note in this call, not yet recorded.
    #[serde(skip)]
    pub swapped: Vec<String>,
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
        held.swapped.push(text);
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

/// Everything held, then the batch as it is.
fn release_all(held: &mut Held, delta: &str) -> String {
    let mut shown = held.lines.concat();
    shown.push_str(delta);
    held.lines.clear();
    held.fence = None;
    shown
}

/// Each withheld draft under the note that says it wasn't fixed, one after
/// another, each ending in a newline.
fn unfixed_text(records: &[Withheld]) -> String {
    let mut s = String::new();
    for r in records {
        s.push_str(UNFIXED_NOTE);
        s.push('\n');
        s.push_str(&r.text);
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

/// The withheld drafts as a hook's systemMessage, or None when there are none.
pub fn unfixed_message(records: &[Withheld]) -> Option<String> {
    (!records.is_empty()).then(|| unfixed_text(records).trim_end().to_string())
}

/// Takes the withheld drafts `pick` selects out of the session's display
/// file and returns them, oldest first, for the caller to show. If the file
/// can't be saved they are still returned, and may show again later.
pub fn take_withheld(dir: &Path, sid: &str, pick: impl Fn(&Withheld) -> bool) -> Vec<Withheld> {
    let Ok(path) = session::display_file(dir, sid) else {
        return Vec::new();
    };
    let mut held = load_held(&path);
    let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut held.withheld)
        .into_iter()
        .partition(|r| pick(r));
    if taken.is_empty() {
        return taken;
    }
    held.withheld = kept;
    let saved = if held.lines.is_empty() && held.withheld.is_empty() {
        fs::remove_file(&path).map_err(Into::into)
    } else {
        save_held(dir, &path, &held)
    };
    if let Err(e) = saved {
        log_error(&format!("display: {e:#}"));
    }
    taken
}

/// Hidden lines only reach the screen through a later batch's output, so
/// anything that goes wrong after a hold shows the held lines at once and
/// lets the rest of the message through as it is. A draft swapped for the
/// note is kept as a withheld record until Stop sends its turn back; it
/// shows here once a call comes from another turn or message, or once it
/// is an hour old.
pub fn display(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let sid = match session_id(v) {
        Ok(sid) => sid,
        Err(_) if approvals::load_live().is_ok_and(|(l, _)| l.is_empty()) => return Ok(None),
        Err(e) => return Err(e),
    };
    let dir = session::dir()?;
    let path = session::display_file(&dir, sid)?;
    let message = str_field(v, "message_id");
    let state = session::load(&dir, sid).unwrap_or_default();
    let turn = session::turn_key(&state, prompt_id(v));
    let mut held = load_held(&path);
    let (unfixed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut held.withheld)
        .into_iter()
        .partition(|r| r.turn != turn || r.message_id != message || r.is_old(now));
    held.withheld = kept;
    let mut early = unfixed_text(&unfixed);
    let same = held.message_id == message;
    // Lines still held from another message: that message stopped before its
    // final batch (Esc), so they show now, ahead of this one.
    if !same {
        if !held.lines.is_empty() {
            early.push_str(INTERRUPTED_NOTE);
            early.push('\n');
            early.push_str(&held.lines.concat());
            if !early.ends_with('\n') {
                early.push('\n');
            }
        }
        held = Held {
            message_id: message.to_string(),
            withheld: std::mem::take(&mut held.withheld),
            ..Held::default()
        };
    }
    let delta = str_field(v, "delta");
    // Without a final flag, nothing is held past this batch.
    let last = v.get("final").and_then(Value::as_bool).unwrap_or(true);
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
        Ok(fix) if fix.is_empty() && held.lines.is_empty() && early.is_empty() => {
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
    for text in std::mem::take(&mut held.swapped) {
        held.withheld.push(Withheld {
            turn: turn.clone(),
            message_id: message.to_string(),
            text,
            time: now,
        });
    }
    let saved = if last && held.withheld.is_empty() {
        let _ = fs::remove_file(&path);
        Ok(())
    } else {
        save_held(&dir, &path, &held)
    };
    if let Err(e) = saved {
        // What isn't saved can't show later, so it shows now.
        log_error(&format!("display: {e:#}"));
        shown.push_str(&held.lines.concat());
        shown.push_str(&unfixed_text(&held.withheld));
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

/// Whether Claude Code runs `dotonce hook stop` at Stop. Without it nothing
/// follows a held draft, so display never holds. Read once per session and
/// kept in the session state.
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
fn save_held(dir: &Path, path: &Path, held: &Held) -> Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string(held)?)?;
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
