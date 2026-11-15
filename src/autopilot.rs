//! The autopilot: rule sets run without waiting for anyone's OK. A new rule
//! file, or a new set Claude proposes, starts log-only (warn): its checks
//! note what they catch and change nothing. A set that keeps catching slips
//! moves to fix by itself, and one whose sent-back replies keep coming back
//! broken moves back to log-only. The user sees each change once, with a
//! code that undoes it (see chat.rs).
//!
//! Two things still wait for the user: a proposal that would change a set
//! at fix, which could weaken a check that is working, and one for a set
//! the user turned off. Rule files are the user's own, since Claude can't
//! write them, so an edited one keeps its level.
//!
//! It runs only where the user sees its notice (`chat::interactive`), so
//! nothing changes in a headless run or when Claude runs a hook itself.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};

use crate::approvals::{self, Approvals, State};
use crate::catches::{self, Catch, Outcome};
use crate::chat;
use crate::hook;
use crate::rules::{self, Level, RuleSet};

/// Days a set stays log-only before it can move to fix.
const TRIAL_DAYS: i64 = 7;
/// Days back that catches count toward moving a set to fix.
const CATCH_DAYS: i64 = 14;
const MIN_CATCHES: usize = 3;
const MIN_SESSIONS: usize = 2;
/// Replies still broken after a send-back, within this many days, that
/// move a set back to log-only.
const BROKEN_DAYS: i64 = 7;
const MAX_BROKEN: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// On for the first time: a new rule file, or a new set Claude proposed.
    Adopted,
    /// A proposal replaced a log-only set. The old file was moved here.
    Updated { old: PathBuf },
    /// Moved from log-only to fix.
    Promoted { catches: usize, sessions: usize },
    /// Moved from fix back to log-only.
    Demoted { broken: usize },
}

#[derive(Debug, Clone)]
pub struct Event {
    pub set: RuleSet,
    pub level: Level,
    pub change: Change,
    pub private: bool,
}

/// The level a set starts at: log-only, or reminders only when its file
/// asks for that.
pub fn start_level(set: &RuleSet) -> Level {
    match set.level {
        Level::Remind => Level::Remind,
        _ => Level::Warn,
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// How many of `set`'s catches with `outcome` fall after `since` and within
/// `days` of `now`, and in how many sessions.
fn count(
    log: &[Catch],
    set: &str,
    outcome: Outcome,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    days: i64,
) -> (usize, usize) {
    let from = since.max(now - Duration::days(days));
    let mut n = 0;
    let mut sessions = BTreeSet::new();
    for c in log.iter().filter(|c| c.set == set && c.outcome == outcome) {
        if parse_time(&c.time).is_some_and(|t| t >= from && t <= now) {
            n += 1;
            sessions.insert(c.session.as_str());
        }
    }
    (n, sessions.len())
}

/// Moves sets between log-only and fix on what the catch log shows, and
/// returns each move. Pinned sets and sets in `skip` stay as they are.
pub fn review(
    a: &mut Approvals,
    sets: &[RuleSet],
    log: &[Catch],
    now: DateTime<Utc>,
    skip: &BTreeSet<String>,
) -> Vec<(RuleSet, Change, Level)> {
    let stamp = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut out = Vec::new();
    for set in sets.iter().filter(|s| !skip.contains(&s.id)) {
        let State::On(level) = a.state(set) else {
            continue;
        };
        let Some(r) = a.sets.get_mut(&set.id) else {
            continue;
        };
        let Some(since) = parse_time(&r.changed).or_else(|| parse_time(&r.approved)) else {
            continue;
        };
        if r.pinned {
            continue;
        }
        match level {
            Level::Warn if now - since >= Duration::days(TRIAL_DAYS) => {
                let (catches, sessions) =
                    count(log, &set.id, Outcome::Caught, since, now, CATCH_DAYS);
                if catches >= MIN_CATCHES && sessions >= MIN_SESSIONS {
                    r.level = Level::Fix;
                    r.promoted.clone_from(&stamp);
                    r.changed.clone_from(&stamp);
                    out.push((
                        set.clone(),
                        Change::Promoted { catches, sessions },
                        Level::Fix,
                    ));
                }
            }
            Level::Fix if !r.promoted.is_empty() => {
                let (broken, _) =
                    count(log, &set.id, Outcome::StillBroken, since, now, BROKEN_DAYS);
                if broken >= MAX_BROKEN {
                    r.level = Level::Warn;
                    r.pinned = true;
                    r.promoted.clear();
                    r.changed.clone_from(&stamp);
                    out.push((set.clone(), Change::Demoted { broken }, Level::Warn));
                }
            }
            _ => {}
        }
    }
    out
}

/// Turns on new rule files and safe proposals, records edited rule files,
/// and moves sets on their catches. Saves what changed and returns the
/// changes the user should hear about.
pub fn run(now: DateTime<Utc>) -> Result<Vec<Event>> {
    if !chat::interactive() {
        return Ok(Vec::new());
    }
    let loaded = rules::load()?;
    let proposals = rules::load_dir(&chat::proposals_dir()?)?.sets;
    if loaded.sets.is_empty() && proposals.is_empty() {
        return Ok(Vec::new());
    }
    let mut a = approvals::load()?;
    let before = a.clone();
    let stamp = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let recorded = hook::recorded_private();
    let private = |set: &RuleSet| set.private || recorded.contains(&set.id);
    let mut events = Vec::new();
    let mut touched = BTreeSet::new();

    for set in &loaded.sets {
        match a.state(set) {
            // A starter nobody filled in yet would remind and check with
            // its examples.
            State::NotApproved if rules::is_starter(set) => {}
            State::NotApproved => {
                let level = start_level(set);
                a.adopt(set, level, &stamp);
                touched.insert(set.id.clone());
                events.push(Event {
                    private: private(set),
                    set: set.clone(),
                    level,
                    change: Change::Adopted,
                });
            }
            // The user edited their own file: it keeps its level.
            State::Changed => {
                a.readopt(set, &stamp);
                touched.insert(set.id.clone());
            }
            _ => {}
        }
    }

    for p in proposals {
        let old = loaded.sets.iter().find(|s| s.id == p.id);
        let safe = match old.map(|s| a.state(s)) {
            None => true,
            Some(State::On(level)) => level != Level::Fix,
            Some(_) => false,
        };
        if !safe {
            continue;
        }
        let was_private = old.is_some_and(private);
        let (set, replaced) = chat::install(&p, old.map(|s| s.file.as_path()))?;
        let (level, change) = match replaced {
            Some(path) if old.is_some() => {
                a.readopt(&set, &stamp);
                let level = a.sets.get(&set.id).map_or(Level::Warn, |r| r.level);
                (level, Change::Updated { old: path })
            }
            _ => {
                let level = start_level(&set);
                a.adopt(&set, level, &stamp);
                (level, Change::Adopted)
            }
        };
        touched.insert(set.id.clone());
        events.push(Event {
            private: private(&set) || was_private,
            set,
            level,
            change,
        });
    }

    let log = catches::read(true)?;
    for (set, change, level) in review(&mut a, &loaded.sets, &log, now, &touched) {
        events.push(Event {
            private: private(&set),
            set,
            level,
            change,
        });
    }

    if a != before {
        approvals::save(&a)?;
    }
    let notes: Vec<(String, Option<Level>, bool)> = events
        .iter()
        .map(|e| (e.set.id.clone(), Some(e.level), e.private))
        .collect();
    hook::note_live(&notes);
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn set(id: &str) -> RuleSet {
        let file = format!("/r/{id}.toml");
        rules::parse(&format!("id = \"{id}\"\n"), Path::new(&file)).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    fn catch(set: &str, session: &str, time: &str, outcome: Outcome) -> Catch {
        Catch {
            time: time.to_string(),
            session: session.to_string(),
            set: set.to_string(),
            check: "c".to_string(),
            level: Level::Warn,
            outcome,
        }
    }

    #[test]
    fn a_set_moves_to_fix_after_a_week_of_repeats_in_more_than_one_session() {
        let s = set("acme");
        let sets = vec![s.clone()];
        let mut a = Approvals::default();
        a.adopt(&s, Level::Warn, "2026-11-01T00:00:00Z");
        let log = vec![
            catch("acme", "s1", "2026-11-05T00:00:00Z", Outcome::Caught),
            catch("acme", "s1", "2026-11-06T00:00:00Z", Outcome::Caught),
            catch("acme", "s2", "2026-11-07T00:00:00Z", Outcome::Caught),
            catch("other", "s3", "2026-11-07T00:00:00Z", Outcome::Caught),
        ];
        let none = BTreeSet::new();
        // Six days in, it's still on trial.
        let early = review(
            &mut a.clone(),
            &sets,
            &log,
            at("2026-11-07T01:00:00Z"),
            &none,
        );
        assert!(early.is_empty());
        let moved = review(&mut a, &sets, &log, at("2026-11-08T00:00:00Z"), &none);
        assert_eq!(
            moved[0].1,
            Change::Promoted {
                catches: 3,
                sessions: 2
            }
        );
        assert_eq!(a.state(&s), State::On(Level::Fix));
        assert_eq!(a.sets["acme"].promoted, "2026-11-08T00:00:00Z");
    }

    #[test]
    fn one_session_old_catches_or_a_pin_keep_a_set_log_only() {
        let s = set("acme");
        let sets = vec![s.clone()];
        let none = BTreeSet::new();
        let now = at("2026-11-30T00:00:00Z");
        let mut a = Approvals::default();
        a.adopt(&s, Level::Warn, "2026-11-01T00:00:00Z");
        let one_session: Vec<Catch> = (1..=4)
            .map(|d| {
                catch(
                    "acme",
                    "s1",
                    &format!("2026-11-2{d}T00:00:00Z"),
                    Outcome::Caught,
                )
            })
            .collect();
        assert!(review(&mut a.clone(), &sets, &one_session, now, &none).is_empty());
        let old: Vec<Catch> = (1..=4)
            .map(|d| {
                catch(
                    "acme",
                    &format!("s{d}"),
                    &format!("2026-11-0{d}T00:00:00Z"),
                    Outcome::Caught,
                )
            })
            .collect();
        assert!(review(&mut a.clone(), &sets, &old, now, &none).is_empty());
        let recent: Vec<Catch> = (1..=4)
            .map(|d| {
                catch(
                    "acme",
                    &format!("s{d}"),
                    &format!("2026-11-2{d}T00:00:00Z"),
                    Outcome::Caught,
                )
            })
            .collect();
        a.sets.get_mut("acme").unwrap().pinned = true;
        assert!(review(&mut a.clone(), &sets, &recent, now, &none).is_empty());
        a.sets.get_mut("acme").unwrap().pinned = false;
        let skip = BTreeSet::from(["acme".to_string()]);
        assert!(review(&mut a.clone(), &sets, &recent, now, &skip).is_empty());
        assert_eq!(review(&mut a, &sets, &recent, now, &none).len(), 1);
    }

    #[test]
    fn a_promoted_set_goes_back_when_its_fixes_keep_failing() {
        let s = set("acme");
        let sets = vec![s.clone()];
        let none = BTreeSet::new();
        let mut a = Approvals::default();
        a.adopt(&s, Level::Warn, "2026-11-01T00:00:00Z");
        let r = a.sets.get_mut("acme").unwrap();
        r.level = Level::Fix;
        r.promoted = "2026-11-10T00:00:00Z".to_string();
        r.changed = "2026-11-10T00:00:00Z".to_string();
        let log: Vec<Catch> = (1..=3)
            .map(|d| {
                catch(
                    "acme",
                    "s1",
                    &format!("2026-11-1{d}T00:00:00Z"),
                    Outcome::StillBroken,
                )
            })
            .collect();
        // A set the user put at fix stays there.
        let mut user_set = a.clone();
        user_set.sets.get_mut("acme").unwrap().promoted.clear();
        assert!(
            review(
                &mut user_set,
                &sets,
                &log,
                at("2026-11-14T00:00:00Z"),
                &none
            )
            .is_empty()
        );
        let back = review(&mut a, &sets, &log, at("2026-11-14T00:00:00Z"), &none);
        assert_eq!(back[0].1, Change::Demoted { broken: 3 });
        assert_eq!(a.state(&s), State::On(Level::Warn));
        assert!(a.sets["acme"].pinned);
    }

    #[test]
    fn sets_start_log_only_unless_their_file_asks_for_reminders_only() {
        let fix = rules::parse("id = \"a\"\nlevel = \"fix\"\n", Path::new("/r/a.toml")).unwrap();
        let remind =
            rules::parse("id = \"b\"\nlevel = \"remind\"\n", Path::new("/r/b.toml")).unwrap();
        assert_eq!(start_level(&fix), Level::Warn);
        assert_eq!(start_level(&remind), Level::Remind);
        assert_eq!(start_level(&set("c")), Level::Warn);
    }
}
