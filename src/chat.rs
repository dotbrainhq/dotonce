//! The user's say over rule sets, from the chat. The autopilot turns sets
//! on and moves them between levels by itself (see autopilot.rs). Each
//! change is shown once, in a hook message only the user sees, with a
//! four-digit code: `no <code>` undoes it. A proposal that would change a
//! set at fix, or one the user turned off, waits instead: `yes <code>` uses
//! it and `no <code>` drops it. The prompt hook acts on the reply and stops
//! the message before it reaches Claude.
//!
//! The code is what makes the reply the user's. Claude, a scheduled prompt
//! or another session can send text that looks typed, but can't see the
//! code. dotonce keeps only a salted hash of it, makes a new one each time
//! it shows a change, and ties it to the file as shown. Like the terminal
//! check, this stops the plain case, not a determined one: a four-digit hash
//! is easy to reverse, and a shown code also sits in the session transcript
//! on disk. That's why the code store is guarded like approvals.json, codes
//! are only shown or taken when Claude Code itself runs the hook and never
//! in headless runs, and every change still shows in `rules list`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::approvals::{self, Approvals, State};
use crate::autopilot::{Change, Event};
use crate::config::home_dir;
use crate::hook;
use crate::rules::{self, Level, RuleSet};

const MAX_CHECKS_SHOWN: usize = 5;
const MAX_REMIND: usize = 200;
const MAX_WHY: usize = 120;
/// Days a change can be undone from the chat.
const UNDO_DAYS: i64 = 7;
/// Stands in for the code in a block of text until the code is made. Rule
/// text goes through `printable`, which drops control characters, so it
/// never holds this.
const CODE: &str = "\u{0}";

/// Why a proposal waits for the user's OK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// It changes a set at fix.
    Fix,
    /// It changes a set the user turned off.
    Off,
}

#[derive(Debug, Clone)]
pub struct Waiting {
    pub set: RuleSet,
    pub wait: Wait,
    /// Private by its file, by the file it would replace, or by the last
    /// session's record, so an edit can't make a private set public.
    pub private: bool,
}

/// What `no <code>` does to a change the autopilot made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
enum Undo {
    Off,
    Restore { from: PathBuf },
    Level { to: Level },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Code {
    /// The hash of the file as shown.
    file: String,
    salt: String,
    /// sha256 of the salt and the code.
    code: String,
    /// The session it was last shown in.
    session: String,
    /// Set for a change that can be undone. None for a waiting proposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    undo: Option<Undo>,
    /// When the change was made, for an undo.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    since: String,
}

/// chat.json: the current code of each shown change, by set id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    codes: std::collections::BTreeMap<String, Code>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Yes(String),
    No(String),
    /// The prompt was only `dotonce`: show what can be undone and what waits.
    List,
}

pub fn store_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("chat.json"))
}

pub fn proposals_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join("proposals"))
}

pub fn load_store() -> Result<Store> {
    let p = store_path()?;
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

pub fn save_store(s: &Store) -> Result<()> {
    let p = store_path()?;
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(s)? + "\n")?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

/// The store without undo codes older than `UNDO_DAYS`.
fn load_current(now: DateTime<Utc>) -> Result<Store> {
    let mut store = load_store()?;
    store.codes.retain(|_, c| {
        c.undo.is_none()
            || DateTime::parse_from_rfc3339(&c.since)
                .is_ok_and(|t| now - t.with_timezone(&Utc) < Duration::days(UNDO_DAYS))
    });
    Ok(store)
}

/// Whether codes are shown and taken here: only when Claude Code itself
/// runs the hook, and not in a headless run, such as `claude -p` or the
/// Agent SDK, where no one reads the notice and a script writes the prompts.
///
/// Claude Code sets `AI_AGENT` to `claude-code_<version>_harness` for the
/// hooks it runs and to `claude-code_<version>_agent` for Claude's own
/// shell, so Claude running `dotonce hook stop` itself gets no code and its
/// `yes 1234` changes nothing. A Claude Code without that variable shows no
/// codes, and sets are approved from the terminal as before.
pub fn interactive() -> bool {
    let from_hook = std::env::var("AI_AGENT")
        .is_ok_and(|a| a.starts_with("claude-code") && a.ends_with("_harness"));
    let headless = std::env::var("CLAUDE_CODE_ENTRYPOINT").is_ok_and(|e| e.starts_with("sdk"));
    from_hook && !headless
}

/// The user's reply, when the whole typed prompt is one: `yes 4821`,
/// `no 4821` or `dotonce`, in any case, with or without `dotonce` first.
pub fn parse_reply(typed: &str) -> Option<Reply> {
    let lower = typed.trim().to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let (named, rest) = match words.as_slice() {
        ["dotonce", rest @ ..] => (true, rest),
        rest => (false, rest),
    };
    match rest {
        [] if named => Some(Reply::List),
        [word, code] => {
            let code = code.trim_end_matches(['.', '!']);
            if code.len() != 4 || !code.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            match *word {
                "yes" => Some(Reply::Yes(code.to_string())),
                "no" => Some(Reply::No(code.to_string())),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Checks a rule set file and keeps it in the proposals folder. Anyone can
/// propose, Claude included. The autopilot turns it on log-only at the end
/// of the turn, unless it changes a set at fix or one the user turned off.
pub fn propose(file: &Path) -> Result<RuleSet> {
    let src = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let table: toml::Table = toml::from_str(&src).context("parse the rule set")?;
    let id = table
        .get("id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| anyhow!("the rule set has no id"))?;
    if !rules::valid_id(id) {
        bail!("{} isn't a valid id", rules::printable(id));
    }
    let dir = proposals_dir()?;
    let target = dir.join(format!("{id}.toml"));
    let set = rules::parse(&src, &target)?;
    let loaded = rules::load()?;
    let a = approvals::load()?;
    if loaded
        .sets
        .iter()
        .any(|s| s.id == set.id && s.src == set.src && a.state(s) != State::NotApproved)
    {
        bail!(
            "{} is already on as written, so there's nothing to propose",
            set.id
        );
    }
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{id}.toml.{}.tmp", std::process::id()));
    fs::write(&tmp, &src)?;
    fs::rename(&tmp, &target)?;
    Ok(set)
}

/// Proposals that wait for the user's OK: those that change a set at fix
/// or one the user turned off. The autopilot takes the rest.
pub fn waiting(a: &Approvals) -> Result<Vec<Waiting>> {
    let loaded = rules::load()?;
    let recorded = hook::recorded_private();
    let mut out = Vec::new();
    for set in rules::load_dir(&proposals_dir()?)?.sets {
        let Some(old) = loaded.sets.iter().find(|s| s.id == set.id) else {
            continue;
        };
        let wait = match a.state(old) {
            State::On(Level::Fix) => Wait::Fix,
            State::Off => Wait::Off,
            _ => continue,
        };
        out.push(Waiting {
            private: set.private || old.private || recorded.contains(&set.id),
            wait,
            set,
        });
    }
    Ok(out)
}

/// Four random digits not in `taken`.
fn new_code(taken: &BTreeSet<String>) -> Result<String> {
    let mut f = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    loop {
        let mut b = [0u8; 8];
        f.read_exact(&mut b)?;
        let code = format!("{:04}", u64::from_le_bytes(b) % 10_000);
        if !taken.contains(&code) {
            return Ok(code);
        }
    }
}

fn salt() -> Result<String> {
    let mut f = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    let mut b = [0u8; 16];
    f.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn code_hash(salt: &str, code: &str) -> String {
    approvals::hash(&format!("{salt}:{code}"))
}

/// The text up to `max` characters, on one line.
fn cut(s: &str, max: usize) -> String {
    let one: String = rules::printable(&s.split_whitespace().collect::<Vec<_>>().join(" "));
    if one.chars().count() <= max {
        return one;
    }
    let mut t: String = one.chars().take(max).collect();
    t.push_str("...");
    t
}

fn checks_word(n: usize) -> String {
    format!("{n} check{}", if n == 1 { "" } else { "s" })
}

/// What a level does, in plain words.
fn level_words(level: Level) -> &'static str {
    match level {
        Level::Remind => "reminders only",
        Level::Warn => "log-only, so it notes slips and changes nothing",
        Level::Fix => "at fix",
    }
}

/// The set's reminder and first checks, one per line.
fn details(set: &RuleSet) -> String {
    let mut s = String::new();
    if let Some(r) = &set.remind {
        let _ = writeln!(s, "  Reminds: \"{}\"", cut(&r.text, MAX_REMIND));
    }
    for c in set.checks.iter().take(MAX_CHECKS_SHOWN) {
        let _ = writeln!(
            s,
            "  - {}: {}",
            rules::printable(&c.id),
            cut(&c.why, MAX_WHY)
        );
    }
    if set.checks.len() > MAX_CHECKS_SHOWN {
        let _ = writeln!(s, "  and {} more.", set.checks.len() - MAX_CHECKS_SHOWN);
    }
    s
}

/// One thing to show with a code.
struct Block {
    id: String,
    /// The hash of the file the code is tied to.
    file: String,
    undo: Option<Undo>,
    since: String,
    /// The text, with `CODE` where the code goes.
    text: String,
}

fn event_block(e: &Event, since: &str) -> Block {
    let id = rules::printable(&e.set.id);
    let n = checks_word(e.set.checks.len());
    let (text, undo) = match &e.change {
        Change::Adopted => (
            format!(
                "{id} is on, {}: {n}.\n{}  Send no {CODE} to turn it off.",
                level_words(e.level),
                details(&e.set)
            ),
            Undo::Off,
        ),
        Change::Updated { old } => (
            format!(
                "{id} changed, still {}: {n}.\n{}  Send no {CODE} to put the old version back.",
                level_words(e.level),
                details(&e.set)
            ),
            Undo::Restore { from: old.clone() },
        ),
        Change::Promoted { catches, sessions } => (
            format!(
                "{id} moved to fix: it caught {catches} slips in {sessions} sessions. Replies and \
                 commands that break it now go back to Claude to correct. Send no {CODE} to keep \
                 it log-only."
            ),
            Undo::Level { to: Level::Warn },
        ),
        Change::Demoted { broken } => (
            format!(
                "{id} went back to log-only: {broken} replies still broke it after Claude's \
                 correction. Send no {CODE} to put it back at fix."
            ),
            Undo::Level { to: Level::Fix },
        ),
    };
    Block {
        id: e.set.id.clone(),
        file: approvals::hash(&e.set.src),
        undo: Some(undo),
        since: since.to_string(),
        text,
    }
}

/// An undo code shown again, for `dotonce`.
fn undo_block(set: &RuleSet, c: &Code) -> Option<Block> {
    let id = rules::printable(&set.id);
    let text = match c.undo.as_ref()? {
        Undo::Off => format!("{id} turned on by itself. Send no {CODE} to turn it off."),
        Undo::Restore { .. } => {
            format!("{id} changed by itself. Send no {CODE} to put the old version back.")
        }
        Undo::Level { to: Level::Fix } => {
            format!("{id} went back to log-only by itself. Send no {CODE} to put it back at fix.")
        }
        Undo::Level { .. } => {
            format!("{id} moved to fix by itself. Send no {CODE} to keep it log-only.")
        }
    };
    Some(Block {
        id: set.id.clone(),
        file: c.file.clone(),
        undo: c.undo.clone(),
        since: c.since.clone(),
        text,
    })
}

fn waiting_block(w: &Waiting, a: &Approvals) -> Block {
    let id = rules::printable(&w.set.id);
    let level = a.sets.get(&w.set.id).map_or(w.set.level, |p| p.level);
    let what = match w.wait {
        Wait::Fix => "a change to a set at fix",
        Wait::Off => "a change to a set you turned off",
    };
    let text = format!(
        "{id} ({what}): runs at {}, {}.\n{}  Send yes {CODE} to use it, or no {CODE} to drop it.",
        level.as_str(),
        checks_word(w.set.checks.len()),
        details(&w.set)
    );
    Block {
        id: w.set.id.clone(),
        file: approvals::hash(&w.set.src),
        undo: None,
        since: String::new(),
        text,
    }
}

/// Gives each block a fresh code, saved in `store`, and returns the texts.
/// The caller saves the store before the notice goes out.
fn issue(
    store: &mut Store,
    blocks: &[Block],
    taken: &mut BTreeSet<String>,
    sid: &str,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for b in blocks {
        let code = new_code(taken)?;
        taken.insert(code.clone());
        let salt = salt()?;
        store.codes.insert(
            b.id.clone(),
            Code {
                file: b.file.clone(),
                code: code_hash(&salt, &code),
                salt,
                session: sid.to_string(),
                undo: b.undo.clone(),
                since: b.since.clone(),
            },
        );
        out.push(b.text.replace(CODE, &code));
    }
    Ok(out)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The notice: changes dotonce made, then proposals that wait, each with a
/// fresh code. `hidden` counts private ones this folder doesn't show. None
/// when there is nothing to say.
fn notice(
    store: &mut Store,
    changes: &[Block],
    waits: &[Block],
    hidden: usize,
    sid: &str,
) -> Result<Option<String>> {
    if changes.is_empty() && waits.is_empty() && hidden == 0 {
        return Ok(None);
    }
    let mut taken = BTreeSet::new();
    let mut parts = Vec::new();
    let private_note = " Only you see this message and its codes.";
    if !changes.is_empty() {
        let texts = issue(store, changes, &mut taken, sid)?;
        parts.push(format!(
            "dotonce changed {} by itself.{private_note}\n\n{}",
            plural(changes.len(), "rule set", "rule sets"),
            texts.join("\n\n")
        ));
    }
    if !waits.is_empty() {
        let texts = issue(store, waits, &mut taken, sid)?;
        let verb = if waits.len() == 1 { "waits" } else { "wait" };
        let note = if changes.is_empty() { private_note } else { "" };
        parts.push(format!(
            "dotonce: {} {verb} for your OK.{note}\n\n{}",
            plural(waits.len(), "rule set change", "rule set changes"),
            texts.join("\n\n")
        ));
    }
    if hidden > 0 {
        let what = if parts.is_empty() {
            format!(
                "dotonce: {} to private rule sets",
                plural(hidden, "change", "changes")
            )
        } else {
            format!("{} private", plural(hidden, "more is", "more are"))
        };
        parts.push(format!(
            "{what}: send dotonce from a session in one of your private folders to see it."
        ));
    }
    Ok(Some(parts.join("\n\n")))
}

fn now_text(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The change blocks for `events` this folder may show. Private ones are
/// counted, and still get an undo code, shown when `dotonce` is sent from a
/// private folder.
fn event_blocks(
    store: &mut Store,
    events: &[Event],
    private_ok: bool,
    sid: &str,
    now: DateTime<Utc>,
) -> Result<(Vec<Block>, usize)> {
    let since = now_text(now);
    let (shown, hidden): (Vec<&Event>, Vec<&Event>) =
        events.iter().partition(|e| private_ok || !e.private);
    let unseen: Vec<Block> = hidden.iter().map(|e| event_block(e, &since)).collect();
    issue(store, &unseen, &mut BTreeSet::new(), sid)?;
    Ok((
        shown.iter().map(|e| event_block(e, &since)).collect(),
        hidden.len(),
    ))
}

/// The session start notice: what the autopilot just changed and every
/// proposal that waits.
pub fn notice_all(
    sid: &str,
    private_ok: bool,
    events: &[Event],
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    if !interactive() {
        return Ok(None);
    }
    let mut store = load_current(now)?;
    let a = approvals::load()?;
    let (changes, hidden_changes) = event_blocks(&mut store, events, private_ok, sid, now)?;
    let all = waiting(&a)?;
    let waits: Vec<Block> = all
        .iter()
        .filter(|w| private_ok || !w.private)
        .map(|w| waiting_block(w, &a))
        .collect();
    let hidden = hidden_changes + all.len() - waits.len();
    let text = notice(&mut store, &changes, &waits, hidden, sid)?;
    save_store(&store)?;
    Ok(text)
}

/// The end of turn notice: what the autopilot just changed, such as a set
/// Claude proposed this turn, and proposals this session hasn't shown yet.
pub fn notice_new(
    sid: &str,
    private_ok: bool,
    events: &[Event],
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    if !interactive() {
        return Ok(None);
    }
    let mut store = load_current(now)?;
    let a = approvals::load()?;
    let (changes, hidden) = event_blocks(&mut store, events, private_ok, sid, now)?;
    let waits: Vec<Block> = waiting(&a)?
        .iter()
        .filter(|w| private_ok || !w.private)
        .filter(|w| {
            store
                .codes
                .get(&w.set.id)
                .is_none_or(|c| c.session != sid || c.file != approvals::hash(&w.set.src))
        })
        .map(|w| waiting_block(w, &a))
        .collect();
    let text = notice(&mut store, &changes, &waits, hidden, sid)?;
    if !events.is_empty() || text.is_some() {
        save_store(&store)?;
    }
    Ok(text)
}

/// Moves a file into proposals/done with the time in its name, as the
/// record of what was applied, replaced or dropped. Returns where it went.
fn archive(file: &Path, name: &str) -> Result<PathBuf> {
    let done = proposals_dir()?.join("done");
    fs::create_dir_all(&done)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let target = done.join(format!("{name}-{stamp}.toml"));
    fs::rename(file, &target)
        .with_context(|| format!("move {} to {}", file.display(), done.display()))?;
    Ok(target)
}

/// Writes `src` to `target`, moving the file there before to
/// proposals/done as `old_name`. Returns where the old file went.
fn put(target: &Path, src: &str, old_name: &str) -> Result<Option<PathBuf>> {
    let dir = target.parent().ok_or_else(|| anyhow!("no rules folder"))?;
    fs::create_dir_all(dir)?;
    let old = if fs::symlink_metadata(target).is_ok() {
        Some(archive(target, old_name)?)
    } else {
        None
    };
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, src)?;
    fs::rename(&tmp, target)?;
    Ok(old)
}

/// Puts a proposal in place as the rule file at `existing`, or a new one
/// named after its id. The file it replaces and the proposal itself go to
/// proposals/done. Returns the set as installed and where the replaced
/// file went.
pub fn install(p: &RuleSet, existing: Option<&Path>) -> Result<(RuleSet, Option<PathBuf>)> {
    rules::validate(p)?;
    let target = match existing {
        Some(f) => f.to_path_buf(),
        None => rules::rules_dir()?.join(format!("{}.toml", p.id)),
    };
    let old = put(&target, &p.src, &format!("{}-replaced", p.id))?;
    archive(&p.file, &format!("{}-applied", p.id))?;
    let mut set = p.clone();
    set.file = target;
    Ok((set, old))
}

/// What the user's `no` does to a change the autopilot made.
fn undo(set: &RuleSet, how: &Undo, private: bool, now: &str) -> Result<String> {
    let id = rules::printable(&set.id);
    let mut a = approvals::load()?;
    let (text, level) = match how {
        Undo::Off => {
            a.turn_off(&set.id, now)?;
            (format!("dotonce: {id} is off."), None)
        }
        Undo::Restore { from } => {
            let Ok(src) = fs::read_to_string(from) else {
                return Ok(format!(
                    "dotonce: the old version of {id} is gone, so nothing changed."
                ));
            };
            let old = rules::parse(&src, &set.file)?;
            rules::validate(&old)?;
            put(&set.file, &src, &format!("{}-undone", set.id))?;
            a.readopt(&old, now);
            let level = a.sets.get(&set.id).map(|r| r.level);
            (
                format!("dotonce: {id} is back to the version before the change."),
                level,
            )
        }
        Undo::Level { to } => {
            a.set_level(set, *to, now)?;
            let text = match to {
                Level::Fix => {
                    format!("dotonce: {id} is back at fix, and dotonce won't move it again.")
                }
                _ => format!("dotonce: {id} stays log-only, and dotonce won't move it again."),
            };
            (text, Some(*to))
        }
    };
    approvals::save(&a)?;
    hook::note_live(&[(set.id.clone(), level, private)]);
    Ok(text)
}

/// Uses a waiting proposal: it becomes the rule file and is approved as it
/// is now, at the level its set already has.
fn use_proposal(w: &Waiting, now: &str) -> Result<Level> {
    let old = rules::load()?
        .sets
        .into_iter()
        .find(|s| s.id == w.set.id)
        .map(|s| s.file);
    let (set, _) = install(&w.set, old.as_deref())?;
    let mut a = approvals::load()?;
    let level = a.approve(&set, now);
    approvals::save(&a)?;
    hook::note_live(&[(set.id.clone(), Some(level), w.private)]);
    Ok(level)
}

/// Everything `dotonce` lists: changes that can still be undone and
/// proposals that wait, split into those this folder shows and a count of
/// the private rest.
fn listed(
    store: &Store,
    a: &Approvals,
    waiting: &[Waiting],
    private_ok: bool,
) -> Result<(Vec<Block>, Vec<Block>, usize)> {
    let loaded = rules::load()?;
    let recorded = hook::recorded_private();
    let mut changes = Vec::new();
    let mut hidden = 0;
    for (id, c) in store.codes.iter().filter(|(_, c)| c.undo.is_some()) {
        let Some(set) = loaded.sets.iter().find(|s| &s.id == id) else {
            continue;
        };
        if approvals::hash(&set.src) != c.file {
            continue;
        }
        if !private_ok && (set.private || recorded.contains(id)) {
            hidden += 1;
        } else if let Some(b) = undo_block(set, c) {
            changes.push(b);
        }
    }
    let mut waits = Vec::new();
    for w in waiting {
        if private_ok || !w.private {
            waits.push(waiting_block(w, a));
        } else {
            hidden += 1;
        }
    }
    Ok((changes, waits, hidden))
}

/// What the prompt hook does with the user's reply: the text to show in
/// place of the message, or None to let the message through to Claude, as
/// for a `yes 1234` sent while nothing can take it.
pub fn answer(
    reply: &Reply,
    sid: &str,
    private_ok: bool,
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    if !interactive() {
        return Ok(None);
    }
    let mut store = load_current(now)?;
    let a = approvals::load()?;
    let waiting = waiting(&a)?;
    let (changes, waits, hidden) = listed(&store, &a, &waiting, private_ok)?;
    let code = match reply {
        Reply::List => {
            let text = notice(&mut store, &changes, &waits, hidden, sid)?;
            save_store(&store)?;
            return Ok(Some(text.unwrap_or_else(|| {
                "dotonce: nothing to undo, and nothing waits for your OK.".to_string()
            })));
        }
        Reply::Yes(c) | Reply::No(c) => c,
    };
    if changes.is_empty() && waits.is_empty() {
        return Ok(None);
    }
    let shown_again = |store: &mut Store| -> Result<String> {
        Ok(notice(store, &changes, &waits, hidden, sid)?.unwrap_or_default())
    };
    let matched = store
        .codes
        .iter()
        .find(|(_, c)| c.code == code_hash(&c.salt, code))
        .map(|(id, c)| (id.clone(), c.clone()));
    let Some((id, c)) = matched else {
        let text = shown_again(&mut store)?;
        save_store(&store)?;
        return Ok(Some(format!(
            "dotonce: that code isn't current, so nothing changed.\n\n{text}"
        )));
    };
    let shown_id = rules::printable(&id);
    let current = if c.undo.is_some() {
        changes.iter().any(|b| b.id == id)
    } else {
        waits.iter().any(|b| b.id == id)
    };
    if !current {
        return Ok(Some(
            "dotonce: that code isn't for anything this folder can change, so nothing changed."
                .to_string(),
        ));
    }
    store.codes.remove(&id);
    let stamp = now_text(now);
    let text = match (&c.undo, reply) {
        (Some(_), Reply::Yes(_)) => format!("dotonce: kept {shown_id} as it is."),
        (Some(how), _) => {
            let set = rules::load()?
                .sets
                .into_iter()
                .find(|s| s.id == id && approvals::hash(&s.src) == c.file);
            match set {
                Some(set) => {
                    let private = set.private || hook::recorded_private().contains(&id);
                    undo(&set, how, private, &stamp)?
                }
                None => format!("dotonce: {shown_id} changed since, so nothing changed."),
            }
        }
        (None, _) => {
            let Some(w) = waiting.iter().find(|w| w.set.id == id) else {
                save_store(&store)?;
                return Ok(Some(format!(
                    "dotonce: {shown_id} doesn't wait for an OK any more, so nothing changed."
                )));
            };
            if approvals::hash(&w.set.src) != c.file {
                let text = shown_again(&mut store)?;
                save_store(&store)?;
                return Ok(Some(format!(
                    "dotonce: {shown_id} changed after you saw it, so nothing changed. Here it is now.\n\n{text}"
                )));
            }
            if matches!(reply, Reply::Yes(_)) {
                let level = use_proposal(w, &stamp)?;
                format!("dotonce: {shown_id} is on at {}.", level.as_str())
            } else {
                archive(&w.set.file, &format!("{id}-dropped"))?;
                format!("dotonce: dropped the proposed {shown_id}. Nothing changed.")
            }
        }
    };
    save_store(&store)?;
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_the_whole_prompt() {
        for (p, r) in [
            ("yes 4821", Some(Reply::Yes("4821".into()))),
            ("  YES 4821. ", Some(Reply::Yes("4821".into()))),
            ("no 0042", Some(Reply::No("0042".into()))),
            ("dotonce yes 4821", Some(Reply::Yes("4821".into()))),
            ("dotonce", Some(Reply::List)),
            ("Dotonce ", Some(Reply::List)),
        ] {
            assert_eq!(parse_reply(p), r, "{p}");
        }
        for p in [
            "",
            "yes",
            "yes 482",
            "yes 48211",
            "yes 48a1",
            "yes 4821 please",
            "ok 4821",
            "yes, 4821",
            "I said yes 4821",
            "dotonce rules list",
            "dotoncey",
        ] {
            assert_eq!(parse_reply(p), None, "{p}");
        }
    }

    #[test]
    fn codes_are_four_digits_and_never_repeat_in_one_notice() {
        let mut taken = BTreeSet::new();
        for _ in 0..200 {
            let c = new_code(&taken).unwrap();
            assert_eq!(c.len(), 4);
            assert!(c.chars().all(|x| x.is_ascii_digit()));
            assert!(taken.insert(c));
        }
    }

    #[test]
    fn a_code_matches_only_its_own_hash() {
        let s = salt().unwrap();
        let h = code_hash(&s, "4821");
        assert_eq!(h, code_hash(&s, "4821"));
        assert_ne!(h, code_hash(&s, "4822"));
        assert_ne!(h, code_hash(&salt().unwrap(), "4821"));
        assert!(!h.contains("4821"));
    }

    #[test]
    fn long_text_is_cut_to_one_line() {
        assert_eq!(cut("a\n  b\tc", 10), "a b c");
        assert_eq!(cut(&"x".repeat(12), 10), format!("{}...", "x".repeat(10)));
        assert_eq!(cut("a\u{1b}[2Jb", 10), "a[2Jb");
        assert!(!cut("a\u{0}b", 10).contains(CODE));
    }

    #[test]
    fn undo_codes_round_trip_and_old_stores_still_parse() {
        let c = Code {
            undo: Some(Undo::Level { to: Level::Warn }),
            since: "2026-11-15T00:00:00Z".into(),
            ..Code::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""do":"level""#), "{json}");
        assert_eq!(serde_json::from_str::<Code>(&json).unwrap(), c);
        let plain = serde_json::to_string(&Code::default()).unwrap();
        assert!(!plain.contains("undo"), "{plain}");
        let old: Store = serde_json::from_str(r#"{"codes":{},"declined":{"acme":"h"}}"#).unwrap();
        assert!(old.codes.is_empty());
    }
}
