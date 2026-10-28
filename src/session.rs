//! Which rule sets are on in a session and what turned each on, kept per
//! session in `~/.dotonce/sessions/<session_id>.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::approvals::Live;
use crate::checks::{fence_closes, fence_open};
use crate::config::{Config, home_dir, path_inside};
use crate::facts::word_match;
use crate::rules::Level;

/// The Skill tool's input field that names the skill (task 1, D1).
pub const SKILL_FIELD: &str = "skill";
const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SENT_BACK: usize = 50;
const PASTE_OPEN: &str = "<pasted_content";
const PASTE_CLOSE: &str = "</pasted_content";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Skill,
    Folder,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Active {
    pub id: String,
    pub by: Trigger,
    /// The skill, folder or prompt words that matched. Never kept for a private set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what: Option<String>,
    /// The turn a prompt-word set belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub reminded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub turn: String,
    /// Hash of the last assistant text at send-back: the corrected draft comes after it.
    pub after: String,
    /// (set id, check id) of each failure that was sent back.
    pub checks: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub active: Vec<Active>,
    #[serde(default)]
    pub paused: Vec<String>,
    #[serde(default)]
    pub sent_back: Vec<String>,
    #[serde(default)]
    pub pending: Vec<Pending>,
    #[serde(default)]
    pub prompt_id: Option<String>,
}

impl State {
    pub fn sent_back(&self, turn: &str) -> bool {
        self.sent_back.iter().any(|t| t == turn)
    }

    pub fn mark_sent_back(&mut self, p: Pending) {
        self.sent_back.retain(|t| t != &p.turn);
        self.sent_back.push(p.turn.clone());
        while self.sent_back.len() > MAX_SENT_BACK {
            self.sent_back.remove(0);
        }
        self.pending.retain(|x| x.turn != p.turn);
        self.pending.push(p);
        while self.pending.len() > MAX_SENT_BACK {
            self.pending.remove(0);
        }
    }

    pub fn take_pending(&mut self, turn: &str) -> Option<Pending> {
        let i = self.pending.iter().position(|p| p.turn == turn)?;
        Some(self.pending.remove(i))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    Prompt(&'a str),
    Skill(&'a str),
    Other,
}

pub fn dir() -> Result<PathBuf> {
    Ok(home_dir()?.join("sessions"))
}

/// The session id with anything but letters, digits, `-` and `_` taken out,
/// so it can never point outside the sessions folder.
fn safe_name(session_id: &str) -> Result<String> {
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    if safe.is_empty() {
        bail!("the hook input has no usable session_id");
    }
    Ok(safe)
}

/// The session's state. A missing or unreadable file is a fresh state, so a
/// broken file heals on the next save.
///
/// Load, change and save is not locked. Two hooks running at once can each
/// save over the other's change. The worst case is a reminder shown again or
/// a second send-back, and Claude Code's own continuation cap bounds that.
pub fn load(dir: &Path, session_id: &str) -> Result<State> {
    let path = dir.join(format!("{}.json", safe_name(session_id)?));
    Ok(fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}

pub fn save(dir: &Path, session_id: &str, state: &State) -> Result<()> {
    let name = safe_name(session_id)?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string(state)?)?;
    fs::rename(&tmp, dir.join(format!("{name}.json")))?;
    Ok(())
}

/// Where the MessageDisplay hook keeps the lines it holds back.
pub fn display_file(dir: &Path, session_id: &str) -> Result<PathBuf> {
    Ok(dir.join(format!("{}.display.json", safe_name(session_id)?)))
}

/// The id of the most recently written session file.
pub fn newest(dir: &Path) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".json")?.to_string();
            if stem.ends_with(".display") {
                return None;
            }
            let t = e.metadata().ok()?.modified().ok()?;
            Some((t, stem))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, s)| s)
}

/// Deletes dotonce's own session files (`.json` and `.tmp`) older than 7
/// days. Returns how many it deleted.
pub fn clean_old(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let p = e.path();
        let ours = p.extension().is_some_and(|x| x == "json" || x == "tmp");
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > KEEP);
        if ours && old && fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// Where a pasted block's closing tag starts and ends in `s`. Claude Code
/// repeats the opening tag's attributes in it (`</pasted_content id="1">`),
/// so anything from `</pasted_content` up to the next `>` counts, as long as
/// the tag name ends there (not `</pasted_contents>`).
fn find_paste_close(s: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = s[from..].find(PASTE_CLOSE) {
        let start = from + i;
        let after = start + PASTE_CLOSE.len();
        let rest = &s[after..];
        if rest.starts_with('>') || rest.starts_with(char::is_whitespace) {
            let end = rest.find('>')?;
            return Some((start, after + end + 1));
        }
        from = after;
    }
    None
}

/// Adds the part of `line` outside pasted blocks to `out`, and tracks
/// whether a pasted block is still open at the end of the line.
fn strip_pasted(line: &str, pasted: &mut bool, out: &mut String) {
    let mut rest = line;
    loop {
        if *pasted {
            match find_paste_close(rest) {
                Some((_, end)) => {
                    rest = &rest[end..];
                    *pasted = false;
                }
                None => return,
            }
        } else {
            match rest.find(PASTE_OPEN) {
                Some(i) => {
                    out.push_str(&rest[..i]);
                    rest = &rest[i..];
                    *pasted = true;
                }
                None => {
                    out.push_str(rest);
                    return;
                }
            }
        }
    }
}

/// The prompt without pasted material: fenced blocks and
/// `<pasted_content ...>` blocks.
pub fn typed_text(prompt: &str) -> String {
    let mut out = String::new();
    let mut fence: Option<(char, usize)> = None;
    let mut pasted = false;
    for line in prompt.lines() {
        if !pasted {
            if let Some((ch, len)) = fence {
                if fence_closes(line, ch, len) {
                    fence = None;
                }
                continue;
            }
            if let Some((ch, len, _)) = fence_open(line) {
                fence = Some((ch, len));
                continue;
            }
        }
        strip_pasted(line, &mut pasted, &mut out);
        out.push('\n');
    }
    out
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The first entry whose words all appear next to each other, in order, in
/// the typed part of the prompt, with the search's light stemming.
pub fn prompt_match(entries: &[String], prompt: &str) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let typed = words(&typed_text(prompt));
    entries
        .iter()
        .find(|e| {
            let want = words(e);
            !want.is_empty()
                && typed
                    .windows(want.len())
                    .any(|w| w.iter().zip(&want).all(|(t, x)| word_match(x, t)))
        })
        .cloned()
}

/// The first configured name the skill matches. A name without a plugin
/// prefix matches the skill with or without one; a name with a prefix needs
/// the same prefix.
pub fn skill_match(names: &[String], skill: &str) -> Option<String> {
    let skill = skill.trim().trim_start_matches('/');
    let base = skill.rsplit(':').next().unwrap_or(skill);
    names
        .iter()
        .find(|n| {
            let n = n.trim().trim_start_matches('/');
            if n.contains(':') {
                n.eq_ignore_ascii_case(skill)
            } else {
                n.eq_ignore_ascii_case(base)
            }
        })
        .cloned()
}

/// Brings the session's active sets up to date for one hook call: ends a
/// finished turn's prompt-word sets, drops sets that stopped applying, and
/// turns on the sets this call matches.
pub fn refresh(
    state: &mut State,
    live: &[Live],
    cfg: &Config,
    cwd: &Path,
    prompt_id: Option<&str>,
    event: Event,
) {
    let private_ok = cfg.private_allowed(cwd);
    let new_turn = match prompt_id {
        Some(p) => state.prompt_id.as_deref() != Some(p),
        None => matches!(event, Event::Prompt(_)),
    };
    if new_turn {
        state.active.retain(|a| a.by != Trigger::Prompt);
        state.prompt_id = prompt_id.map(String::from);
    }
    let allowed = |l: &Live| private_ok || !l.set.private;
    state.active.retain(|a| {
        let Some(l) = live.iter().find(|l| l.set.id == a.id) else {
            return false;
        };
        let in_folder = l.set.when.folders.iter().any(|f| path_inside(cwd, f));
        allowed(l) && !state.paused.contains(&a.id) && (a.by != Trigger::Folder || in_folder)
    });
    for l in live {
        let s = &l.set;
        if !allowed(l) || state.paused.contains(&s.id) {
            continue;
        }
        let keep = |w: &str| (!s.private).then(|| w.to_string());
        let found = state.active.iter().position(|a| a.id == s.id);
        if let Event::Skill(name) = event {
            if let Some(w) = skill_match(&s.when.skills, name) {
                match found {
                    Some(i) => {
                        let a = &mut state.active[i];
                        if a.by != Trigger::Skill {
                            a.by = Trigger::Skill;
                            a.what = keep(&w);
                            a.prompt_id = None;
                        }
                    }
                    None => state.active.push(Active {
                        id: s.id.clone(),
                        by: Trigger::Skill,
                        what: keep(&w),
                        prompt_id: None,
                        reminded: false,
                    }),
                }
                continue;
            }
        }
        if found.is_some() {
            continue;
        }
        if let Some(f) = s.when.folders.iter().find(|f| path_inside(cwd, f)) {
            state.active.push(Active {
                id: s.id.clone(),
                by: Trigger::Folder,
                what: keep(f),
                prompt_id: None,
                reminded: false,
            });
        } else if let Event::Prompt(p) = event {
            if let Some(w) = prompt_match(&s.when.prompt, p) {
                state.active.push(Active {
                    id: s.id.clone(),
                    by: Trigger::Prompt,
                    what: keep(&w),
                    prompt_id: prompt_id.map(String::from),
                    reminded: false,
                });
            }
        }
    }
}

fn reminder(live: &[Live], id: &str) -> Option<(String, String)> {
    let l = live.iter().find(|l| l.set.id == id)?;
    let text = l.set.remind.as_ref()?.text.trim().to_string();
    Some((id.to_string(), text))
}

/// The reminders of active sets that haven't had theirs yet. Mark the ones
/// actually given with `mark_reminded`, so one left out at the token cap
/// comes with the next hook that can add context.
pub fn pending_reminders(state: &State, live: &[Live]) -> Vec<(String, String)> {
    state
        .active
        .iter()
        .filter(|a| !a.reminded)
        .filter_map(|a| reminder(live, &a.id))
        .collect()
}

/// Records that these sets' reminders were given.
pub fn mark_reminded(state: &mut State, ids: &[String]) {
    for a in state.active.iter_mut().filter(|a| ids.contains(&a.id)) {
        a.reminded = true;
    }
}

/// The reminders to give again after the context was compacted: skill and
/// folder sets, and prompt-word sets from the turn `current`. With no
/// current turn, no prompt-word set comes back, since it may be an earlier
/// turn's.
pub fn compact_reminders(
    state: &State,
    live: &[Live],
    current: Option<&str>,
) -> Vec<(String, String)> {
    state
        .active
        .iter()
        .filter(|a| {
            a.by != Trigger::Prompt || (current.is_some() && a.prompt_id.as_deref() == current)
        })
        .filter_map(|a| reminder(live, &a.id))
        .collect()
}

/// The active sets whose checks run now: warn and fix sets, without the
/// prompt-word sets that don't ask for checks.
pub fn checking<'a>(state: &State, live: &'a [Live]) -> Vec<&'a Live> {
    live.iter()
        .filter(|l| l.level != Level::Remind)
        .filter(|l| {
            state.active.iter().any(|a| {
                a.id == l.set.id && (a.by != Trigger::Prompt || l.set.when.check_on_prompt)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse;
    use std::fs::File;

    fn live(id: &str, body: &str, level: Level) -> Live {
        let src = format!("id = \"{id}\"\n{body}");
        Live {
            set: parse(&src, Path::new(&format!("/rules/{id}.toml"))).unwrap(),
            level,
        }
    }

    fn cfg() -> Config {
        Config::parse("private_paths = [\"/work/private\"]\n").unwrap()
    }

    const REMIND: &str = "[remind]\ntext = \"Links go inline.\"\n";

    /// Gives every pending reminder and marks it given.
    fn take(st: &mut State, live: &[Live]) -> Vec<(String, String)> {
        let r = pending_reminders(st, live);
        let ids: Vec<String> = r.iter().map(|(id, _)| id.clone()).collect();
        mark_reminded(st, &ids);
        r
    }

    fn ids(st: &State) -> Vec<&str> {
        st.active.iter().map(|a| a.id.as_str()).collect()
    }

    #[test]
    fn typed_text_drops_pasted_material() {
        let p = "Draft it\n```\nplease reply soon\n```\n<pasted_content id=\"1\">reply here\nmore</pasted_content> thanks\nend <pasted_content id=\"2\">inline</pasted_content> done";
        let t = typed_text(p);
        assert!(
            t.contains("Draft it")
                && t.contains("thanks")
                && t.contains("end")
                && t.contains("done"),
            "{t}"
        );
        assert!(
            !t.contains("please")
                && !t.contains("reply")
                && !t.contains("more")
                && !t.contains("inline"),
            "{t}"
        );
    }

    #[test]
    fn a_paste_closed_with_the_same_id_ends_at_its_closing_tag() {
        let entries = vec!["draft".to_string(), "later".to_string()];
        let p = "<pasted_content id=\"1\">draft this\nmore</pasted_content id=\"1\"> later please";
        assert_eq!(prompt_match(&entries, p).as_deref(), Some("later"));
        let inside = "<pasted_content id=\"1\">draft this</pasted_content id=\"1\"> thanks";
        assert_eq!(prompt_match(&entries, inside), None);
        let multi = "<pasted_content id=\"1\">\ndraft\n</pasted_content id=\"1\">\nthen later";
        assert_eq!(prompt_match(&entries, multi).as_deref(), Some("later"));
    }

    #[test]
    fn the_bare_paste_closing_tag_still_works() {
        let entries = vec!["draft".to_string(), "later".to_string()];
        let p = "<pasted_content>draft</pasted_content> later";
        assert_eq!(prompt_match(&entries, p).as_deref(), Some("later"));
        let inside = "<pasted_content>draft</pasted_content> thanks";
        assert_eq!(prompt_match(&entries, inside), None);
    }

    #[test]
    fn two_pastes_keep_the_typed_text_between_them() {
        let entries = ["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        let p = "<pasted_content id=\"1\">alpha</pasted_content id=\"1\"> beta <pasted_content id=\"2\">gamma</pasted_content id=\"2\"> end";
        let t = typed_text(p);
        assert!(t.contains("beta") && t.contains("end"), "{t}");
        assert!(!t.contains("alpha") && !t.contains("gamma"), "{t}");
        assert_eq!(prompt_match(&entries[1..2], p).as_deref(), Some("beta"));
        assert_eq!(prompt_match(&entries[0..1], p), None);
        assert_eq!(prompt_match(&entries[2..3], p), None);
    }

    #[test]
    fn a_look_alike_tag_does_not_close_a_paste() {
        let entries = vec!["later".to_string()];
        let p = "<pasted_content id=\"1\">x</pasted_contents> later";
        assert_eq!(prompt_match(&entries, p), None);
    }

    #[test]
    fn prompt_words_match_whole_words_with_light_stemming() {
        let words = vec!["draft".to_string(), "send it".to_string()];
        assert_eq!(
            prompt_match(&words, "Drafting the answer now").as_deref(),
            Some("draft")
        );
        assert_eq!(
            prompt_match(&words, "please SEND it today").as_deref(),
            Some("send it")
        );
        assert_eq!(prompt_match(&words, "the drift is wrong"), None);
        assert_eq!(prompt_match(&words, "send them it"), None);
        assert_eq!(prompt_match(&words, "```\ndraft\n```"), None);
        assert_eq!(prompt_match(&[], "draft"), None);
    }

    #[test]
    fn skill_names_match_with_or_without_a_plugin_prefix() {
        let names = vec!["writing-support".to_string(), "acme:review".to_string()];
        assert_eq!(
            skill_match(&names, "writing-support").as_deref(),
            Some("writing-support")
        );
        assert!(skill_match(&names, "acme-tools:writing-support").is_some());
        assert!(skill_match(&names, "/writing-support").is_some());
        assert!(skill_match(&names, "Writing-Support").is_some());
        assert!(skill_match(&names, "acme:review").is_some());
        assert!(skill_match(&names, "review").is_none());
        assert!(skill_match(&names, "other:review").is_none());
        assert!(skill_match(&names, "writing").is_none());
    }

    #[test]
    fn each_trigger_turns_a_set_on_and_reminds_once() {
        let sets = vec![
            live(
                "by-skill",
                &format!("[when]\nskills = [\"writing-support\"]\n{REMIND}"),
                Level::Fix,
            ),
            live(
                "by-folder",
                &format!("[when]\nfolders = [\"/work/support\"]\n{REMIND}"),
                Level::Warn,
            ),
            live(
                "by-prompt",
                &format!("[when]\nprompt = [\"draft\"]\n{REMIND}"),
                Level::Fix,
            ),
        ];
        let mut st = State::default();
        let cwd = Path::new("/work/support/app");
        refresh(
            &mut st,
            &sets,
            &cfg(),
            cwd,
            Some("p1"),
            Event::Prompt("draft it"),
        );
        assert_eq!(ids(&st), ["by-folder", "by-prompt"]);
        assert_eq!(take(&mut st, &sets).len(), 2);
        assert!(take(&mut st, &sets).is_empty());
        let on: Vec<&str> = checking(&st, &sets)
            .iter()
            .map(|l| l.set.id.as_str())
            .collect();
        assert_eq!(on, ["by-folder"]);
        refresh(
            &mut st,
            &sets,
            &cfg(),
            cwd,
            Some("p1"),
            Event::Skill("acme:writing-support"),
        );
        let r = take(&mut st, &sets);
        assert_eq!(
            r,
            vec![("by-skill".to_string(), "Links go inline.".to_string())]
        );
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/elsewhere"),
            Some("p2"),
            Event::Prompt("what now"),
        );
        assert_eq!(ids(&st), ["by-skill"]);
        assert_eq!(st.active[0].by, Trigger::Skill);
        assert_eq!(st.active[0].what.as_deref(), Some("writing-support"));
        let on: Vec<&str> = checking(&st, &sets)
            .iter()
            .map(|l| l.set.id.as_str())
            .collect();
        assert_eq!(on, ["by-skill"]);
    }

    #[test]
    fn a_skill_upgrades_a_prompt_set_so_it_lasts_and_checks() {
        let sets = vec![live(
            "both",
            "[when]\nprompt = [\"draft\"]\nskills = [\"writing-support\"]\n",
            Level::Warn,
        )];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Prompt("draft"),
        );
        assert!(checking(&st, &sets).is_empty());
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Skill("writing-support"),
        );
        assert_eq!(st.active.len(), 1);
        assert_eq!(st.active[0].by, Trigger::Skill);
        assert_eq!(checking(&st, &sets).len(), 1);
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p2"),
            Event::Other,
        );
        assert_eq!(st.active.len(), 1);
    }

    #[test]
    fn check_on_prompt_lets_prompt_words_turn_on_checks() {
        let sets = vec![live(
            "chat",
            "[when]\nprompt = [\"draft\"]\ncheck_on_prompt = true\n",
            Level::Fix,
        )];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Prompt("draft"),
        );
        assert_eq!(checking(&st, &sets).len(), 1);
    }

    #[test]
    fn remind_sets_never_check() {
        let sets = vec![live("note", "[when]\nfolders = [\"/x\"]\n", Level::Remind)];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Other,
        );
        assert_eq!(st.active.len(), 1);
        assert!(checking(&st, &sets).is_empty());
    }

    #[test]
    fn a_failed_prompt_hook_leaves_nothing_for_the_next_turn() {
        let sets = vec![live(
            "by-prompt",
            "[when]\nprompt = [\"draft\"]\ncheck_on_prompt = true\n",
            Level::Fix,
        )];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Prompt("draft"),
        );
        assert_eq!(st.active.len(), 1);
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Other,
        );
        assert_eq!(st.active.len(), 1);
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p2"),
            Event::Other,
        );
        assert!(st.active.is_empty());
        assert_eq!(st.prompt_id.as_deref(), Some("p2"));
    }

    #[test]
    fn without_a_prompt_id_the_next_prompt_ends_the_turn() {
        let sets = vec![live(
            "by-prompt",
            "[when]\nprompt = [\"draft\"]\n",
            Level::Warn,
        )];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            None,
            Event::Prompt("draft"),
        );
        assert_eq!(st.active.len(), 1);
        refresh(&mut st, &sets, &cfg(), Path::new("/x"), None, Event::Other);
        assert_eq!(st.active.len(), 1);
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            None,
            Event::Prompt("hello"),
        );
        assert!(st.active.is_empty());
    }

    #[test]
    fn private_sets_stay_off_outside_private_paths_and_store_no_words() {
        let sets = vec![live(
            "secret",
            "private = true\n[when]\nfolders = [\"/work/private\"]\nprompt = [\"draft\"]\nskills = [\"writing-support\"]\n",
            Level::Fix,
        )];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/work/public"),
            Some("p1"),
            Event::Prompt("draft"),
        );
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/work/public"),
            Some("p1"),
            Event::Skill("writing-support"),
        );
        assert!(st.active.is_empty());
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/work/private/app"),
            Some("p1"),
            Event::Prompt("draft"),
        );
        assert_eq!(ids(&st), ["secret"]);
        assert_eq!(st.active[0].what, None);
        let json = serde_json::to_string(&st).unwrap();
        assert!(
            !json.contains("draft") && !json.contains("/work/private"),
            "{json}"
        );
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/work/public"),
            Some("p1"),
            Event::Other,
        );
        assert!(st.active.is_empty());
    }

    #[test]
    fn paused_and_removed_sets_drop_out() {
        let sets = vec![
            live("a", "[when]\nfolders = [\"/x\"]\n", Level::Warn),
            live("b", "[when]\nfolders = [\"/x\"]\n", Level::Warn),
        ];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Other,
        );
        assert_eq!(ids(&st), ["a", "b"]);
        st.paused.push("a".to_string());
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Other,
        );
        assert_eq!(ids(&st), ["b"]);
        refresh(
            &mut st,
            &sets[..1],
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Other,
        );
        assert!(st.active.is_empty());
    }

    #[test]
    fn compaction_gives_back_skill_folder_and_this_turns_prompt_sets() {
        let sets = vec![
            live(
                "folder",
                &format!("[when]\nfolders = [\"/x\"]\n{REMIND}"),
                Level::Warn,
            ),
            live(
                "words",
                &format!("[when]\nprompt = [\"draft\"]\n{REMIND}"),
                Level::Warn,
            ),
        ];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Prompt("draft it"),
        );
        assert_eq!(take(&mut st, &sets).len(), 2);
        let ids = |r: Vec<(String, String)>| r.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(
            ids(compact_reminders(&st, &sets, Some("p1"))),
            ["folder", "words"]
        );
        assert_eq!(ids(compact_reminders(&st, &sets, Some("p2"))), ["folder"]);
        assert_eq!(ids(compact_reminders(&st, &sets, None)), ["folder"]);
        assert!(take(&mut st, &sets).is_empty());
    }

    #[test]
    fn only_reminders_marked_given_stop_coming() {
        let sets = vec![
            live(
                "a",
                &format!("[when]\nfolders = [\"/x\"]\n{REMIND}"),
                Level::Warn,
            ),
            live(
                "b",
                &format!("[when]\nfolders = [\"/x\"]\n{REMIND}"),
                Level::Warn,
            ),
        ];
        let mut st = State::default();
        refresh(
            &mut st,
            &sets,
            &cfg(),
            Path::new("/x"),
            Some("p1"),
            Event::Other,
        );
        assert_eq!(pending_reminders(&st, &sets).len(), 2);
        mark_reminded(&mut st, &["a".to_string()]);
        let left = pending_reminders(&st, &sets);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, "b");
    }

    #[test]
    fn send_back_is_tracked_per_turn() {
        let mut st = State::default();
        assert!(!st.sent_back("p1"));
        st.mark_sent_back(Pending {
            turn: "p1".to_string(),
            after: "h".to_string(),
            checks: vec![("s".to_string(), "c".to_string())],
        });
        assert!(st.sent_back("p1") && !st.sent_back("p2"));
        assert_eq!(st.take_pending("p1").unwrap().checks.len(), 1);
        assert!(st.take_pending("p1").is_none());
        assert!(st.sent_back("p1"));
        for i in 0..60 {
            st.mark_sent_back(Pending {
                turn: format!("t{i}"),
                after: String::new(),
                checks: Vec::new(),
            });
        }
        assert_eq!(st.sent_back.len(), 50);
        assert!(st.sent_back("t59") && !st.sent_back("p1"));
    }

    #[test]
    fn pending_is_capped_like_sent_back() {
        let mut st = State::default();
        for i in 0..60 {
            st.mark_sent_back(Pending {
                turn: format!("t{i}"),
                after: String::new(),
                checks: Vec::new(),
            });
        }
        assert_eq!(st.pending.len(), 50);
        assert!(st.take_pending("t59").is_some() && st.take_pending("t0").is_none());
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dotonce-session-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn age(p: &Path, secs: u64) {
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn state_files_round_trip_with_safe_names() {
        let dir = temp("roundtrip");
        let st = State {
            paused: vec!["a".to_string()],
            ..State::default()
        };
        save(&dir, "s-1", &st).unwrap();
        assert_eq!(load(&dir, "s-1").unwrap(), st);
        assert_eq!(load(&dir, "missing").unwrap(), State::default());
        fs::write(dir.join("broken.json"), "{").unwrap();
        assert_eq!(load(&dir, "broken").unwrap(), State::default());
        save(&dir, "../x", &st).unwrap();
        assert!(dir.join("x.json").exists());
        assert!(load(&dir, "///").is_err());
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(left.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_old_removes_old_tmp_files_and_leaves_other_files() {
        let dir = temp("clean-tmp");
        for name in ["s.123.tmp", "fresh.456.tmp", "notes.log", "data.bak"] {
            fs::write(dir.join(name), "x").unwrap();
        }
        age(&dir.join("s.123.tmp"), 8 * 86_400);
        age(&dir.join("notes.log"), 8 * 86_400);
        age(&dir.join("data.bak"), 8 * 86_400);
        assert_eq!(clean_old(&dir, SystemTime::now()), 1);
        assert!(!dir.join("s.123.tmp").exists());
        assert!(dir.join("fresh.456.tmp").exists());
        assert!(dir.join("notes.log").exists() && dir.join("data.bak").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_session_files_are_cleaned_and_newest_skips_display_files() {
        let dir = temp("clean");
        for name in ["old.json", "a.json", "b.json", "b.display.json", "keep.txt"] {
            fs::write(dir.join(name), "{}").unwrap();
        }
        age(&dir.join("old.json"), 8 * 86_400);
        age(&dir.join("keep.txt"), 8 * 86_400);
        age(&dir.join("a.json"), 100);
        age(&dir.join("b.json"), 50);
        assert_eq!(clean_old(&dir, SystemTime::now()), 1);
        assert!(!dir.join("old.json").exists());
        assert!(dir.join("keep.txt").exists() && dir.join("a.json").exists());
        assert_eq!(newest(&dir).as_deref(), Some("b"));
        assert_eq!(clean_old(&dir.join("missing"), SystemTime::now()), 0);
        assert_eq!(newest(&dir.join("missing")), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
