//! Approvals: a rule set only reminds or checks after the user approved its
//! exact file from a terminal. The record lives outside the rule file, since
//! anything that can edit the file could approve it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::home_dir;
use crate::rules::{self, FileError, Level, RuleSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub hash: String,
    pub level: Level,
    #[serde(default)]
    pub off: bool,
    pub approved: String,
    /// When approve, level or off last wrote this record (RFC 3339).
    #[serde(default)]
    pub changed: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Approvals {
    #[serde(default)]
    pub sets: BTreeMap<String, Approval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    NotApproved,
    Changed,
    Off,
    On(Level),
}

impl State {
    pub fn label(self) -> String {
        match self {
            State::NotApproved => "not approved".to_string(),
            State::Changed => "changed since approval".to_string(),
            State::Off => "off".to_string(),
            State::On(l) => l.as_str().to_string(),
        }
    }
}

/// A rule set that is approved, unchanged since, and on, with its level.
#[derive(Debug, Clone)]
pub struct Live {
    pub set: RuleSet,
    pub level: Level,
}

pub fn hash(src: &str) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(src.as_bytes()).iter() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Approvals {
    pub fn state(&self, set: &RuleSet) -> State {
        match self.sets.get(&set.id) {
            None => State::NotApproved,
            Some(a) if a.hash != hash(&set.src) => State::Changed,
            Some(a) if a.off => State::Off,
            Some(a) => State::On(a.level),
        }
    }

    /// Records the file as it is now. The first approval takes the file's
    /// level; a later one keeps the level the user chose. Returns the level.
    pub fn approve(&mut self, set: &RuleSet, now: &str) -> Level {
        let level = self.sets.get(&set.id).map_or(set.level, |a| a.level);
        self.sets.insert(
            set.id.clone(),
            Approval {
                hash: hash(&set.src),
                level,
                off: false,
                approved: now.to_string(),
                changed: now.to_string(),
            },
        );
        level
    }

    pub fn set_level(&mut self, set: &RuleSet, level: Level, now: &str) -> Result<()> {
        match self.state(set) {
            State::NotApproved => {
                bail!(
                    "{} isn't approved yet: run dotonce rules approve {}",
                    set.id,
                    set.id
                )
            }
            State::Changed => bail!(
                "{} changed since you approved it: run dotonce rules approve {} first",
                set.id,
                set.id
            ),
            _ => {}
        }
        if let Some(a) = self.sets.get_mut(&set.id) {
            a.level = level;
            a.off = false;
            a.changed = now.to_string();
        }
        Ok(())
    }

    pub fn turn_off(&mut self, id: &str, now: &str) -> Result<()> {
        match self.sets.get_mut(id) {
            Some(a) => {
                a.off = true;
                a.changed = now.to_string();
                Ok(())
            }
            None => bail!("{id} isn't approved, so it's already off"),
        }
    }
}

/// What `rules approve` shows before it asks: the level the approval gives,
/// each check, and whether the file changed or the set is off.
pub fn summary(set: &RuleSet, a: &Approvals) -> String {
    let prev = a.sets.get(&set.id);
    let level = prev.map_or(set.level, |p| p.level);
    let mut s = format!(
        "{}: level {}, {} checks, file {}\n",
        set.id,
        level.as_str(),
        set.checks.len(),
        set.file.display()
    );
    for c in &set.checks {
        let _ = writeln!(s, "  {} ({}): {}", c.id, c.kind.name(), c.why);
    }
    if let Some(p) = prev {
        if p.hash != hash(&set.src) {
            s.push_str("The file changed since the last approval.\n");
        }
        if p.off {
            s.push_str("The set is off now, and approving it turns it back on.\n");
        }
    }
    s
}

pub fn path() -> Result<PathBuf> {
    Ok(home_dir()?.join("approvals.json"))
}

pub fn load() -> Result<Approvals> {
    let p = path()?;
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Approvals::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// Writes a temporary file and renames it, so a reader never sees half a file.
pub fn save(a: &Approvals) -> Result<()> {
    let p = path()?;
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(a)? + "\n")?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn live(sets: Vec<RuleSet>, a: &Approvals) -> Vec<Live> {
    sets.into_iter()
        .filter_map(|set| match a.state(&set) {
            State::On(level) => Some(Live { set, level }),
            _ => None,
        })
        .collect()
}

/// The live sets and the rule files with errors. With no rule sets the
/// approvals file isn't read at all.
pub fn load_live() -> Result<(Vec<Live>, Vec<FileError>)> {
    let loaded = rules::load()?;
    if loaded.sets.is_empty() {
        return Ok((Vec::new(), loaded.errors));
    }
    let a = load()?;
    Ok((live(loaded.sets, &a), loaded.errors))
}

/// The variables Claude Code sets for the commands its Bash tool runs.
const AGENT_VARS: [&str; 2] = ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"];

/// Refuses approve, level and off when they run under Claude Code. An agent
/// can unset these variables, so like the terminal check this stops the
/// plain case, not a determined one.
pub fn refuse_agent() -> Result<()> {
    if AGENT_VARS.iter().any(|v| std::env::var_os(v).is_some()) {
        bail!("Run this in your own terminal, not through an AI agent.");
    }
    Ok(())
}

/// True when stdin and stdout are both a terminal. This stops accidental and
/// unattended use, such as a script, a hook or a piped command. A determined
/// shell agent can fake a terminal (with `script` or a pty), which is why
/// every change is recorded in approvals.json and shown by `rules list`.
pub fn on_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Asks for the set's id to be typed. The caller passes whether this is a
/// terminal and how to read the line, so tests can drive it.
pub fn confirm(
    id: &str,
    what: &str,
    terminal: bool,
    read_line: impl FnOnce() -> io::Result<String>,
) -> Result<()> {
    if !terminal {
        bail!("{what} only works in a terminal you type in: run it yourself, not through an agent");
    }
    let typed = read_line()?;
    if typed.trim() != id {
        bail!("that wasn't {id}, so nothing changed");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn set(src: &str) -> RuleSet {
        rules::parse(src, Path::new("/r/acme.toml")).unwrap()
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash("a"),
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
        );
        assert_ne!(hash("a"), hash("a\n"));
    }

    #[test]
    fn approval_follows_the_file_and_keeps_the_chosen_level() {
        let s = set("id = \"acme\"\nlevel = \"warn\"\n");
        let mut a = Approvals::default();
        assert_eq!(a.state(&s), State::NotApproved);
        assert!(a.set_level(&s, Level::Fix, "t").is_err());
        assert_eq!(a.approve(&s, "2026-10-28T00:00:00Z"), Level::Warn);
        assert_eq!(a.state(&s), State::On(Level::Warn));
        a.set_level(&s, Level::Fix, "t").unwrap();
        assert_eq!(a.state(&s), State::On(Level::Fix));
        let edited = set("id = \"acme\"\nlevel = \"remind\"\n");
        assert_eq!(a.state(&edited), State::Changed);
        let e = a.set_level(&edited, Level::Warn, "t").unwrap_err();
        assert!(format!("{e:#}").contains("changed"), "{e:#}");
        assert_eq!(a.approve(&edited, "2026-10-29T00:00:00Z"), Level::Fix);
        a.turn_off("acme", "t").unwrap();
        assert_eq!(a.state(&edited), State::Off);
        a.set_level(&edited, Level::Warn, "t").unwrap();
        assert_eq!(a.state(&edited), State::On(Level::Warn));
        assert!(a.turn_off("other", "t").is_err());
    }

    #[test]
    fn live_keeps_only_approved_unchanged_sets_that_are_on() {
        let on = set("id = \"acme\"\n");
        let other = rules::parse("id = \"beta\"\n", Path::new("/r/beta.toml")).unwrap();
        let mut a = Approvals::default();
        a.approve(&on, "t");
        let l = live(vec![on.clone(), other], &a);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].set.id, "acme");
        assert_eq!(l[0].level, Level::Warn);
        a.turn_off("acme", "t").unwrap();
        assert!(live(vec![on], &a).is_empty());
    }

    #[test]
    fn approvals_round_trip_as_json() {
        let mut a = Approvals::default();
        a.approve(
            &set("id = \"acme\"\nlevel = \"fix\"\n"),
            "2026-10-28T00:00:00Z",
        );
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"level\":\"fix\""), "{json}");
        assert_eq!(serde_json::from_str::<Approvals>(&json).unwrap(), a);
        assert_eq!(
            serde_json::from_str::<Approvals>("{}").unwrap(),
            Approvals::default()
        );
    }

    #[test]
    fn confirm_needs_a_terminal_and_the_typed_id() {
        let e = confirm("acme", "approve acme", false, || {
            panic!("read without a terminal")
        })
        .unwrap_err();
        assert!(format!("{e:#}").contains("terminal"), "{e:#}");
        assert!(confirm("acme", "approve acme", true, || Ok("acme\n".to_string())).is_ok());
        let e = confirm("acme", "approve acme", true, || Ok("acm\n".to_string())).unwrap_err();
        assert!(format!("{e:#}").contains("nothing changed"), "{e:#}");
        assert!(
            confirm("acme", "approve acme", true, || Err(io::Error::other(
                "closed"
            )))
            .is_err()
        );
    }

    #[test]
    fn approve_summary_shows_checks_and_what_changes() {
        let s = set(
            "id = \"acme\"\nlevel = \"fix\"\n[[check]]\nid = \"no-x\"\nwhy = \"x reads badly\"\nforbid = [\"x\"]\n",
        );
        let mut a = Approvals::default();
        let first = summary(&s, &a);
        assert!(
            first.starts_with("acme: level fix, 1 checks, file /r/acme.toml\n"),
            "{first}"
        );
        assert!(
            first.contains("  no-x (forbid): x reads badly\n"),
            "{first}"
        );
        assert!(!first.contains("since the last approval"), "{first}");
        assert!(!first.contains("off now"), "{first}");
        a.approve(&s, "t");
        a.set_level(&s, Level::Remind, "t").unwrap();
        assert!(!summary(&s, &a).contains("since the last approval"));
        let edited = set(&format!("{}# edited\n", s.src));
        let again = summary(&edited, &a);
        assert!(again.starts_with("acme: level remind,"), "{again}");
        assert!(again.contains("changed since the last approval"), "{again}");
        a.turn_off("acme", "t").unwrap();
        let off = summary(&s, &a);
        assert!(
            off.contains("off now, and approving it turns it back on"),
            "{off}"
        );
    }

    #[test]
    fn every_write_records_when_it_changed() {
        let s = set("id = \"acme\"\n");
        let mut a = Approvals::default();
        a.approve(&s, "2026-10-28T00:00:00Z");
        assert_eq!(a.sets["acme"].changed, "2026-10-28T00:00:00Z");
        a.set_level(&s, Level::Fix, "2026-10-28T01:00:00Z").unwrap();
        assert_eq!(a.sets["acme"].changed, "2026-10-28T01:00:00Z");
        assert_eq!(a.sets["acme"].approved, "2026-10-28T00:00:00Z");
        a.turn_off("acme", "2026-10-28T02:00:00Z").unwrap();
        assert_eq!(a.sets["acme"].changed, "2026-10-28T02:00:00Z");
        let old: Approvals =
            serde_json::from_str(r#"{"sets":{"acme":{"hash":"h","level":"warn","approved":"t"}}}"#)
                .unwrap();
        assert_eq!(old.sets["acme"].changed, "");
    }

    #[test]
    fn state_labels_read_plainly() {
        assert_eq!(State::NotApproved.label(), "not approved");
        assert_eq!(State::Changed.label(), "changed since approval");
        assert_eq!(State::Off.label(), "off");
        assert_eq!(State::On(Level::Fix).label(), "fix");
    }
}
