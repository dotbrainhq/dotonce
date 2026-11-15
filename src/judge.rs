//! Model checks: a second model judges a draft against rules written as
//! plain sentences, the kind no pattern can hold. dotonce runs Claude Code's
//! `claude` command on the user's own login, from an empty folder, with no
//! tools, settings, hooks, MCP servers or saved session, so the model reads
//! only what dotonce hands it. Anything that goes wrong lets the draft
//! through and leaves a line in judge.log.

mod claude;
mod context;
mod record;

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use crate::approvals::Live;
use crate::catches::{self, Catch, Outcome};
use crate::checks;
use crate::config;
use crate::hook::{log_error, prompt_id, session_id};
use crate::rules::{CheckKind, Level};
use crate::session;

pub use claude::{Fail, find_claude};
use claude::{RawBreak, call};
pub use context::{Context, context};
pub use record::{LOG, LogLine, PRIVATE_LOG, read_log};
use record::{judged_path, key, load_judged, note_judged, write_log};

/// How long the background hook lets the model run. Claude Code doesn't
/// time out a background hook, so the judge stops the model itself.
pub const WARN_LIMIT: Duration = Duration::from_secs(25);
/// How long Stop waits for the model when a fix set's draft may go back.
pub const FIX_LIMIT: Duration = Duration::from_secs(15);
/// Calls per session per day, so a loop can't run up the user's usage.
pub const DAILY_CAP: u32 = 40;
/// How the next turn's note on a warn set's catch ends.
pub const NOTE_END: &str = "A second model wrote this and can be wrong. If a catch holds, fix the draft. If not, ignore it.";
const MAX_BREAKS: usize = 3;

/// One model check in a call.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// The id the model sees: the check's id, or set/check when two sets
    /// share it.
    pub key: String,
    pub set: String,
    pub check: String,
    pub ask: String,
    pub why: String,
}

/// A draft, and the sets whose `[draft]` table found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub text: String,
    pub sets: Vec<String>,
}

/// A rule the model found broken, with the words it quoted.
#[derive(Debug, Clone, PartialEq)]
pub struct Break {
    pub set: String,
    pub check: String,
    pub quote: String,
    /// The check's own why.
    pub rule: String,
    /// The model's reason.
    pub reason: String,
}

impl Break {
    /// The check's why, then the model's reason.
    pub fn why(&self) -> String {
        let rule = self.rule.trim_end();
        if self.reason.is_empty() {
            rule.to_string()
        } else {
            format!("{rule} The checker's reason: {}", self.reason)
        }
    }
}

/// What a call found. No breaks means the drafts pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    pub breaks: Vec<Break>,
    /// The sets whose drafts the model read.
    pub sets: Vec<String>,
}

impl Verdict {
    /// Fixed or still broken for a check that went back, or unverified when
    /// the model didn't read a draft of its set.
    pub fn outcome(&self, set: &str, check: &str) -> Outcome {
        if !self.sets.iter().any(|s| s == set) {
            Outcome::Unverified
        } else if self.breaks.iter().any(|b| b.set == set && b.check == check) {
            Outcome::StillBroken
        } else {
            Outcome::Fixed
        }
    }
}

/// Whether the set has a model check.
pub fn has_model(l: &Live) -> bool {
    l.set.has_model_check()
}

/// The model checks of these sets.
pub fn asks(sets: &[&Live]) -> Vec<Ask> {
    let mut out: Vec<Ask> = Vec::new();
    for l in sets {
        for c in &l.set.checks {
            if let CheckKind::Model(ask) = &c.kind {
                out.push(Ask {
                    key: c.id.clone(),
                    set: l.set.id.clone(),
                    check: c.id.clone(),
                    ask: ask.clone(),
                    why: c.why.clone(),
                });
            }
        }
    }
    let ids: Vec<String> = out.iter().map(|a| a.check.clone()).collect();
    for a in &mut out {
        if ids.iter().filter(|i| **i == a.check).count() > 1 {
            a.key = format!("{}/{}", a.set, a.check);
        }
    }
    out
}

/// The prose drafts in `text` for each set with a model check, each text
/// once. A command or a log in a fence isn't a draft to judge.
pub fn drafts(sets: &[&Live], text: &str) -> Vec<Draft> {
    let mut out: Vec<Draft> = Vec::new();
    for l in sets.iter().filter(|l| has_model(l)) {
        for d in checks::drafts(&l.set, text) {
            if !checks::prose(&d) {
                continue;
            }
            match out.iter_mut().find(|x| x.text == d) {
                Some(x) if !x.sets.contains(&l.set.id) => x.sets.push(l.set.id.clone()),
                Some(_) => {}
                None => out.push(Draft {
                    text: d,
                    sets: vec![l.set.id.clone()],
                }),
            }
        }
    }
    out
}

/// The reply with each draft swapped for its number, cut to `REPLY_CHARS`.
pub fn around(text: &str, drafts: &[Draft]) -> String {
    let mut s = text.to_string();
    for (i, d) in drafts.iter().enumerate() {
        s = s.replacen(&d.text, &format!("[draft {}]", i + 1), 1);
    }
    context::trim_middle(s.trim(), context::REPLY_CHARS)
}

/// The message the model gets.
pub fn prompt(asks: &[Ask], ctx: &Context, drafts: &[Draft]) -> String {
    let mut s = String::from("<rules>\n");
    for a in asks {
        let _ = writeln!(s, "- {}: {}", a.key, a.ask);
    }
    s.push_str("</rules>\n\n<session>\n");
    if ctx.session.is_empty() {
        s.push_str("(not found)\n");
    }
    for e in &ctx.session {
        let _ = writeln!(s, "{e}\n");
    }
    s.push_str("</session>\n\n<reply>\n");
    s.push_str(if ctx.reply.is_empty() {
        "(not found)"
    } else {
        &ctx.reply
    });
    s.push_str("\n</reply>\n");
    for (i, d) in drafts.iter().enumerate() {
        let _ = write!(
            s,
            "\n<draft n=\"{}\">\n{}\n</draft>\n",
            i + 1,
            d.text.trim()
        );
    }
    s
}

/// Straight quotes, "..." for an ellipsis, one space between words.
fn norm(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            c => c,
        })
        .collect();
    s.replace('\u{2026}', "...")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The quote without the quote marks the model may wrap it in.
fn bare_quote(quote: &str) -> String {
    norm(quote)
        .trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
        .to_string()
}

/// Whether the quote is in the draft word for word. The model may leave
/// words out with "...".
fn quoted(quote: &str, draft: &str) -> bool {
    let q = bare_quote(quote);
    let d = norm(draft);
    let parts: Vec<&str> = q
        .split("...")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    parts.iter().any(|p| p.chars().count() >= 3) && parts.iter().all(|p| d.contains(p))
}

/// The breaks whose quote is in a draft of the check's own set, the first
/// for each check, and how many were dropped.
fn verify(raw: Vec<RawBreak>, asks: &[Ask], drafts: &[Draft]) -> (Vec<Break>, usize) {
    let mut out: Vec<Break> = Vec::new();
    let mut dropped = 0;
    for r in raw.into_iter().take(MAX_BREAKS) {
        let rule = r.rule.trim();
        let by_id: Vec<&Ask> = asks.iter().filter(|a| a.check == rule).collect();
        let found = asks
            .iter()
            .find(|a| a.key == rule)
            .or_else(|| (by_id.len() == 1).then(|| by_id[0]));
        let Some(a) = found else {
            dropped += 1;
            continue;
        };
        if out.iter().any(|b| b.set == a.set && b.check == a.check) {
            continue;
        }
        if !drafts
            .iter()
            .any(|d| d.sets.contains(&a.set) && quoted(&r.quote, &d.text))
        {
            dropped += 1;
            continue;
        }
        out.push(Break {
            set: a.set.clone(),
            check: a.check.clone(),
            quote: checks::cut(&bare_quote(&r.quote)),
            rule: a.why.clone(),
            reason: r.why.split_whitespace().collect::<Vec<_>>().join(" "),
        });
    }
    (out, dropped)
}

/// Where a judge call runs from and how long it may take.
pub struct Run<'a> {
    pub sid: &'a str,
    pub now: i64,
    pub transcript: Option<&'a Path>,
    pub limit: Duration,
}

/// Has the model judge the drafts in `text` that these sets' model checks
/// haven't judged yet. Logs the call and returns what it found, or None
/// when nothing was judged: no new draft, the daily cap, or a failure.
pub fn judge(run: &Run, sets: &[&Live], text: &str) -> Option<Verdict> {
    let path = session::dir()
        .and_then(|d| judged_path(&d, run.sid))
        .map_err(|e| log_error(&format!("judge: {e:#}")))
        .ok()?;
    let judged = load_judged(&path);
    let mut drafts = drafts(sets, text);
    for d in &mut drafts {
        d.sets.retain(|s| !judged.keys.contains(&key(s, &d.text)));
    }
    drafts.retain(|d| !d.sets.is_empty());
    let asks: Vec<Ask> = asks(sets)
        .into_iter()
        .filter(|a| drafts.iter().any(|d| d.sets.contains(&a.set)))
        .collect();
    if asks.is_empty() {
        return None;
    }
    let mut set_ids: Vec<String> = asks.iter().map(|a| a.set.clone()).collect();
    set_ids.dedup();
    let private = sets
        .iter()
        .any(|l| l.set.private && set_ids.contains(&l.set.id));
    let time = catches::timestamp(run.now);
    let day = time.get(..10).unwrap_or_default().to_string();
    let mut line = LogLine {
        time,
        session: run.sid.to_string(),
        sets: set_ids.clone(),
        checks: asks.len(),
        drafts: drafts.len(),
        result: String::new(),
        ms: 0,
        tokens_in: 0,
        tokens_out: 0,
        breaks: 0,
        unquoted: 0,
    };
    let calls = if judged.day == day { judged.calls } else { 0 };
    let mut called = false;
    let verdict = match find_claude() {
        _ if calls >= DAILY_CAP => {
            line.result = "cap".to_string();
            None
        }
        None => {
            line.result = Fail::NoClaude.kind();
            None
        }
        Some(bin) => {
            let model = config::load_file()
                .ok()
                .flatten()
                .unwrap_or_default()
                .judge_model;
            let mut ctx = run
                .transcript
                .and_then(crate::reply::read_tail)
                .map(|s| context(&s))
                .unwrap_or_default();
            ctx.reply = around(text, &drafts);
            let start = Instant::now();
            let got = call(&bin, &model, prompt(&asks, &ctx, &drafts), run.limit);
            line.ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            called = true;
            match got {
                Err(f) => {
                    line.result = f.kind();
                    None
                }
                Ok((raw, tokens_in, tokens_out)) => {
                    line.tokens_in = tokens_in;
                    line.tokens_out = tokens_out;
                    let (breaks, dropped) = verify(raw, &asks, &drafts);
                    line.breaks = breaks.len();
                    line.unquoted = dropped;
                    line.result = if !breaks.is_empty() {
                        "caught"
                    } else if dropped > 0 {
                        "unquoted"
                    } else {
                        "pass"
                    }
                    .to_string();
                    Some(Verdict {
                        breaks,
                        sets: set_ids,
                    })
                }
            }
        }
    };
    let keys: Vec<String> = if verdict.is_some() {
        drafts
            .iter()
            .flat_map(|d| d.sets.iter().map(|s| key(s, &d.text)))
            .collect()
    } else {
        Vec::new()
    };
    if let Err(e) = note_judged(&path, &day, called, &keys) {
        log_error(&format!("judge: {e:#}"));
    }
    write_log(&line, private);
    verdict
}

/// The note the next turn gets for a warn set's catches.
pub fn note(breaks: &[Break]) -> String {
    let mut s = String::new();
    for b in breaks {
        let _ = writeln!(
            s,
            "Your last draft may break {}: \"{}\". {}",
            b.check,
            b.quote,
            b.why()
        );
    }
    s.push_str(NOTE_END);
    s
}

/// The background Stop hook: the warn sets' model checks judge this turn's
/// new drafts, and a catch reaches Claude on the next turn. Fix sets are
/// judged inside Stop, where a draft can still go back (see reply.rs).
pub fn hook(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let sid = session_id(v)?;
    let dir = session::dir()?;
    let checks = session::checks_now(&session::load(&dir, sid)?, cwd, prompt_id(v))?;
    let warn: Vec<&Live> = session::checking(&checks.state, &checks.live)
        .into_iter()
        .filter(|l| l.level == Level::Warn && has_model(l))
        .collect();
    if warn.is_empty() {
        return Ok(None);
    }
    let transcript = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(Path::new);
    let turn = crate::reply::read_turn(
        transcript,
        v.get("last_assistant_message").and_then(Value::as_str),
    );
    let run = Run {
        sid,
        now,
        transcript,
        limit: WARN_LIMIT,
    };
    let Some(verdict) = judge(&run, &warn, &turn.parts.join("\n\n")) else {
        return Ok(None);
    };
    let time = catches::timestamp(now);
    for b in &verdict.breaks {
        let private = warn.iter().any(|l| l.set.id == b.set && l.set.private);
        let c = Catch {
            time: time.clone(),
            session: sid.to_string(),
            set: b.set.clone(),
            check: b.check.clone(),
            level: Level::Warn,
            outcome: Outcome::Caught,
        };
        catches::record(&c, private);
    }
    if verdict.breaks.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::hook::context("Stop", &note(&verdict.breaks))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse;

    fn live(id: &str, body: &str, private: bool) -> Live {
        let src = format!("id = \"{id}\"\nprivate = {private}\n[draft]\nfence = [\"\"]\n{body}");
        Live {
            set: parse(&src, Path::new(&format!("/rules/{id}.toml"))).unwrap(),
            level: Level::Warn,
        }
    }

    const ASKS: &str = "[[check]]\nid = \"every-ask\"\nkind = \"model\"\nask = \"Answers every question.\"\nwhy = \"Answer what was asked.\"\n[[check]]\nid = \"short\"\nmax_words = 300\nwhy = \"Short.\"\n";
    const DRAFT: &str = "Hi Sam,\n\nThanks for the details on the acme checkout page. I tested the same steps on a fresh store at example.com and the coupon applies there. Could you share the plugin list?";

    /// The reply with the draft in a bare fence, as the test sets want it.
    fn reply() -> String {
        format!("Here:\n\n```\n{DRAFT}\n```\n")
    }

    fn raw(rule: &str, quote: &str) -> RawBreak {
        RawBreak {
            rule: rule.to_string(),
            quote: quote.to_string(),
            why: "  It skips   the refund question. ".to_string(),
        }
    }

    #[test]
    fn asks_are_the_model_checks_and_shared_ids_get_the_set() {
        let a = live("acme-replies", ASKS, false);
        let b = live("acme-notes", ASKS, false);
        let only = asks(&[&a]);
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].key, "every-ask");
        assert_eq!(only[0].ask, "Answers every question.");
        let both = asks(&[&a, &b]);
        let keys: Vec<&str> = both.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["acme-replies/every-ask", "acme-notes/every-ask"]);
    }

    #[test]
    fn drafts_are_prose_blocks_each_text_once() {
        let a = live("acme-replies", ASKS, false);
        let b = live("acme-notes", ASKS, false);
        let text = format!("Here:\n\n```\n{DRAFT}\n```\n\nRun:\n\n```\ngit pull\nnpm test\n```");
        let d = drafts(&[&a, &b], &text);
        assert_eq!(d.len(), 1, "commands aren't a draft");
        assert_eq!(d[0].sets, ["acme-replies", "acme-notes"]);
        let none = live(
            "acme-plain",
            "[[check]]\nid = \"s\"\nmax_words = 3\nwhy = \"w\"\n",
            false,
        );
        assert!(
            drafts(&[&none], &text).is_empty(),
            "no model check, nothing to judge"
        );
    }

    #[test]
    fn the_prompt_lists_rules_session_reply_and_drafts() {
        let a = live("acme-replies", ASKS, false);
        let drafts = drafts(&[&a], &reply());
        let ctx = Context {
            session: vec![
                "User, this turn: Draft the reply".to_string(),
                "Bash: git status -> clean".to_string(),
            ],
            reply: around(&reply(), &drafts),
        };
        assert_eq!(ctx.reply, "Here:\n\n```\n[draft 1]\n```");
        let p = prompt(&asks(&[&a]), &ctx, &drafts);
        assert_eq!(
            p,
            format!(
                "<rules>\n- every-ask: Answers every question.\n</rules>\n\n<session>\nUser, this turn: Draft the reply\n\nBash: git status -> clean\n\n</session>\n\n<reply>\nHere:\n\n```\n[draft 1]\n```\n</reply>\n\n<draft n=\"1\">\n{DRAFT}\n</draft>\n"
            )
        );
        let empty = prompt(&asks(&[&a]), &Context::default(), &[]);
        assert!(empty.contains("<session>\n(not found)\n"), "{empty}");
        assert!(empty.contains("<reply>\n(not found)\n"), "{empty}");
    }

    #[test]
    fn a_break_counts_only_when_its_quote_is_in_the_draft() {
        let a = live("acme-replies", ASKS, false);
        let sets = [&a];
        let (asks, drafts) = (asks(&sets), drafts(&sets, &reply()));
        let (b, dropped) = verify(
            vec![
                raw(
                    "every-ask",
                    "\u{201C}I tested the same steps   on a fresh store\u{201D}",
                ),
                raw("every-ask", "Could you share the plugin list?"),
            ],
            &asks,
            &drafts,
        );
        assert_eq!(dropped, 0, "a second break of the same check is left out");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].quote, "I tested the same steps on a fresh store");
        assert_eq!(b[0].rule, "Answer what was asked.");
        assert_eq!(
            b[0].why(),
            "Answer what was asked. The checker's reason: It skips the refund question."
        );
        let (b, dropped) = verify(
            vec![
                raw("every-ask", "I never tested anything"),
                raw("no-such-check", "Hi Sam,"),
                raw("every-ask", "Hi"),
            ],
            &asks,
            &drafts,
        );
        assert!(b.is_empty());
        assert_eq!(dropped, 3);
        let (b, _) = verify(
            vec![raw(
                "every-ask",
                "Thanks for the details\u{2026}could you share",
            )],
            &asks,
            &drafts,
        );
        assert!(b.is_empty(), "case counts");
        let (b, _) = verify(
            vec![raw(
                "every-ask",
                "Thanks for the details ... Could you share",
            )],
            &asks,
            &drafts,
        );
        assert_eq!(b.len(), 1, "words left out with an ellipsis");
    }

    #[test]
    fn a_quote_must_come_from_a_draft_of_the_checks_own_set() {
        let a = live("acme-replies", ASKS, false);
        let other = live(
            "acme-notes",
            "[[check]]\nid = \"scope\"\nkind = \"model\"\nask = \"Adds nothing.\"\nwhy = \"Build only what was asked.\"\n",
            false,
        );
        let sets = [&a, &other];
        let mut d = drafts(&sets, &reply());
        d[0].sets = vec!["acme-replies".to_string()];
        let (b, dropped) = verify(vec![raw("scope", "Hi Sam,")], &asks(&sets), &d);
        assert!(b.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn the_note_names_each_catch_and_ends_with_the_doubt() {
        let b = Break {
            set: "acme-replies".to_string(),
            check: "every-ask".to_string(),
            quote: "Hi Sam,".to_string(),
            rule: "Answer what was asked.".to_string(),
            reason: String::new(),
        };
        assert_eq!(
            note(&[b]),
            format!(
                "Your last draft may break every-ask: \"Hi Sam,\". Answer what was asked.\n{NOTE_END}"
            )
        );
    }
}
