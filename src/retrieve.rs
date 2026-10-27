use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Result;

use crate::fuse::rrf;
use crate::index::{Fts, Index, SessionRow};
use crate::model::SessionHit;
use crate::{query, timeq};

#[derive(Debug, Clone)]
pub struct Options {
    pub budget_tokens: usize,
    pub max_sessions: usize,
    pub rrf_k: f64,
    pub arm_limit: usize,
    /// Fill budget left after the matched sessions with other sessions, newest first.
    pub fill: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            budget_tokens: 40_000,
            max_sessions: 60,
            rrf_k: 60.0,
            arm_limit: 200,
            fill: true,
        }
    }
}

pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Windows, narrow to wide: the top `turns` turns by score, each with `radius`
/// neighbours on either side.
const STEPS: [(usize, i64); 2] = [(1, 1), (3, 2)];
const MIN_LEFT: usize = 200;
/// In a window, a neighbour of a matched turn shows only its first characters:
/// it is there for context, and long replies would crowd out other sessions.
const NEIGHBOUR_CHARS: usize = 200;

/// Share of the budget, in percent, that whole sessions leave for windows.
const RESERVE_PCT: usize = 20;

/// A window also shows up to this many of the next matched turns, whole, when
/// they are short. A short matched turn is cheap and often states the fact,
/// while the best-scored turn can be a long reply that only talks around it.
const EXTRA_MATCHED: usize = 3;
const SHORT_TURN_CHARS: usize = 500;

fn window(s: &SessionRow, by_score: &[i64], (turns, radius): (usize, i64), note: &str) -> String {
    let mut focus: BTreeSet<i64> = by_score.iter().take(turns).copied().collect();
    let mut shown: BTreeSet<i64> = focus.iter().flat_map(|h| h - radius..=h + radius).collect();
    let short: HashSet<i64> = s
        .turns
        .iter()
        .filter(|(_, _, t)| t.chars().count() <= SHORT_TURN_CHARS)
        .map(|(no, _, _)| *no)
        .collect();
    for n in by_score.iter().skip(turns).take(EXTRA_MATCHED) {
        if short.contains(n) {
            focus.insert(*n);
            shown.insert(*n);
        }
    }
    render(s, Some((&focus, &shown)), note)
}

/// Weekday and how long before the question a session happened, so the reader
/// doesn't have to do date arithmetic. Empty when either date is unknown.
fn date_note(ts: Option<i64>, anchor: Option<i64>) -> String {
    let (Some(ts), Some(anchor)) = (ts, anchor) else {
        return String::new();
    };
    let Some(day) = chrono::DateTime::from_timestamp(ts, 0) else {
        return String::new();
    };
    let weekday = day.format("%A");
    let days = (anchor - ts).div_euclid(86_400);
    match days {
        d if d < 0 => format!(" ({weekday}, after the question date)"),
        0 => format!(" ({weekday}, the same day as the question)"),
        1 => format!(" ({weekday}, 1 day before the question)"),
        d => format!(" ({weekday}, {d} days before the question)"),
    }
}

/// Renders a session, or with `focus` only the `shown` turns of it: focus turns
/// whole, the others shortened.
fn render(s: &SessionRow, focus: Option<(&BTreeSet<i64>, &BTreeSet<i64>)>, note: &str) -> String {
    let date = s.ts_text.as_deref().unwrap_or("unknown");
    let mut out = format!("### Session | date: {date}{note}\n");
    let mut last_kept: Option<i64> = None;
    let mut cut_after_last = false;
    for (no, speaker, text) in &s.turns {
        if let Some((_, shown)) = focus {
            if !shown.contains(no) {
                cut_after_last = true;
                continue;
            }
            cut_after_last = false;
            if last_kept.is_some_and(|l| no - l > 1) || (last_kept.is_none() && *no > 0) {
                out.push_str("...\n");
            }
        }
        out.push_str(speaker);
        out.push_str(": ");
        match focus {
            Some((f, _)) if !f.contains(no) && text.chars().count() > NEIGHBOUR_CHARS => {
                out.extend(text.chars().take(NEIGHBOUR_CHARS));
                out.push_str(" ...");
            }
            _ => out.push_str(text),
        }
        out.push('\n');
        last_kept = Some(*no);
    }
    if cut_after_last {
        out.push_str("...\n");
    }
    out
}

/// Terms the user's turns never hold, likely typos. Only these go to the trigram
/// search: for known terms it adds little and ranks nearly every turn.
pub fn unknown_terms(ix: &Index, terms: &[String], user: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for t in terms {
        if !ix.has_term(t, user)? {
            out.push(t.clone());
        }
    }
    Ok(out)
}

pub fn retrieve(
    ix: &Index,
    q: &str,
    user: &str,
    anchor: Option<i64>,
    opt: &Options,
) -> Result<Vec<SessionHit>> {
    let all = query::terms(q);
    let speakers = ix.speaker_terms(&all, user)?;
    let terms: Vec<String> = all.into_iter().filter(|t| !speakers.contains(t)).collect();
    let text_or = query::fts_or(&terms);
    let speaker_or = query::fts_or(&speakers);
    let stem_match = text_or.as_deref().map(|m| query::in_column("text", m));
    let mut lists: Vec<Vec<i64>> = Vec::new();

    let bm25 = match &stem_match {
        Some(m) => ix.match_turns(Fts::Stem, m, user, None, opt.arm_limit)?,
        None => Vec::new(),
    };
    if let Some(m) = query::trigram_or(&unknown_terms(ix, &terms, user)?) {
        lists.push(ix.match_turns(Fts::Trigram, &m, user, None, opt.arm_limit)?);
    }
    lists.push(ix.name_turns(&terms, user, opt.arm_limit)?);
    if let Some(w) = timeq::window(q, anchor) {
        lists.push(match &stem_match {
            Some(m) => ix.match_turns(Fts::Stem, m, user, Some(w), opt.arm_limit)?,
            None => ix.window_turns(user, w, opt.arm_limit)?,
        });
    }
    if timeq::wants_latest(q) {
        lists.push(ix.newest_first(&bm25)?);
    }
    if let (Some(sp), Some(m)) = (&speaker_or, &stem_match) {
        let by_speaker = format!("{} AND {m}", query::in_column("speaker", sp));
        lists.push(ix.match_turns(Fts::Stem, &by_speaker, user, None, opt.arm_limit)?);
    }
    lists.push(bm25);
    if let Some(sp) = &speaker_or
        && lists.iter().all(Vec::is_empty)
    {
        lists.push(ix.speaker_turns(&query::in_column("speaker", sp), user, opt.arm_limit)?);
    }

    let scores = rrf(&lists, opt.rrf_k);
    let ids: Vec<i64> = scores.keys().copied().collect();
    let where_turn = ix.turn_sessions(&ids)?;

    let mut per_session: HashMap<i64, Vec<(f64, i64)>> = HashMap::new();
    for (tid, score) in &scores {
        if let Some((sid, no)) = where_turn.get(tid) {
            per_session.entry(*sid).or_default().push((*score, *no));
        }
    }
    let mut ranked: Vec<(i64, f64, Vec<i64>)> = per_session
        .into_iter()
        .map(|(sid, mut v)| {
            v.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            let score: f64 = v
                .iter()
                .zip([1.0, 0.5, 0.25])
                .map(|((s, _), w)| s * w)
                .sum();
            (sid, score, v.iter().map(|(_, no)| *no).collect())
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    Ok(pack(ix, ranked, user, anchor, opt)?
        .into_iter()
        .map(|(_, h)| h)
        .collect())
}

/// Packs ranked sessions into the budget. Best first, a session goes in whole
/// while that leaves the reserve free, otherwise as a narrow window around its
/// best turn, so lower-ranked sessions still fit. Leftover budget then widens
/// the windows, best first.
fn pack(
    ix: &Index,
    ranked: Vec<(i64, f64, Vec<i64>)>,
    user: &str,
    anchor: Option<i64>,
    opt: &Options,
) -> Result<Vec<(Option<i64>, SessionHit)>> {
    let reserve = opt.budget_tokens * RESERVE_PCT / 100;
    let mut picked: Vec<(SessionRow, f64, Vec<i64>, String)> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut left = opt.budget_tokens;
    for (sid, score, by_score) in ranked {
        if picked.len() >= opt.max_sessions || (!picked.is_empty() && left < MIN_LEFT) {
            break;
        }
        let s = ix.session(sid)?;
        let note = date_note(s.ts, anchor);
        let full = render(&s, None, &note);
        let text = if estimate_tokens(&full) + reserve <= left {
            full
        } else {
            let narrow = window(&s, &by_score, STEPS[0], &note);
            if estimate_tokens(&narrow) > left {
                continue;
            }
            narrow
        };
        left -= estimate_tokens(&text);
        seen.insert(sid);
        picked.push((s, score, by_score, text));
    }
    for (s, _, by_score, text) in picked.iter_mut() {
        let now = estimate_tokens(text);
        let note = date_note(s.ts, anchor);
        let wider = std::iter::once(render(s, None, &note)).chain(
            STEPS[1..]
                .iter()
                .rev()
                .map(|st| window(s, by_score, *st, &note)),
        );
        for t in wider {
            let need = estimate_tokens(&t);
            if need > now && need - now <= left {
                left -= need - now;
                *text = t;
                break;
            }
        }
    }
    if opt.fill && !picked.is_empty() && left >= MIN_LEFT {
        for sid in ix.sessions_newest_first(user)? {
            if picked.len() >= opt.max_sessions || left < MIN_LEFT {
                break;
            }
            if seen.contains(&sid) {
                continue;
            }
            let s = ix.session(sid)?;
            let text = render(&s, None, &date_note(s.ts, anchor));
            if estimate_tokens(&text) <= left {
                left -= estimate_tokens(&text);
                picked.push((s, 0.0, Vec::new(), text));
            }
        }
    }
    let mut out: Vec<(Option<i64>, SessionHit)> = picked
        .into_iter()
        .map(|(s, score, _, text)| {
            (
                s.ts,
                SessionHit {
                    id: s.doc_id,
                    timestamp: s.ts_text,
                    text,
                    score,
                },
            )
        })
        .collect();
    out.sort_by_key(|(ts, _)| ts.unwrap_or(i64::MAX));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SessionDoc, TurnIn};
    use crate::timeq::parse_ts;

    fn doc(id: &str, user: &str, ts: &str, texts: &[&str]) -> SessionDoc {
        SessionDoc {
            id: id.to_string(),
            user_id: Some(user.to_string()),
            timestamp: Some(ts.to_string()),
            turns: texts
                .iter()
                .map(|t| TurnIn {
                    speaker: "user".to_string(),
                    text: t.to_string(),
                })
                .collect(),
        }
    }

    fn ids(hits: &[SessionHit]) -> Vec<&str> {
        hits.iter().map(|h| h.id.as_str()).collect()
    }

    /// Hits that matched the query, leaving out sessions added to fill the budget.
    fn matched(hits: &[SessionHit]) -> Vec<&str> {
        hits.iter()
            .filter(|h| h.score > 0.0)
            .map(|h| h.id.as_str())
            .collect()
    }

    #[test]
    fn finds_the_right_session_for_the_right_user() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "s1",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["I adopted a dog named Rex"],
            ),
            doc(
                "s2",
                "u1",
                "2023-02-01T10:00:00+00:00",
                &["We talked about taxes"],
            ),
            doc(
                "s3",
                "u2",
                "2023-01-01T10:00:00+00:00",
                &["My dog is called Max"],
            ),
        ])
        .unwrap();
        let hits = retrieve(
            &ix,
            "What is my dog's name?",
            "u1",
            None,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(matched(&hits), vec!["s1"]);
        assert!(!ids(&hits).contains(&"s3"));
        assert!(hits[0].text.contains("Rex"));
        assert!(hits[0].text.contains("2023-01-01"));
    }

    #[test]
    fn rendered_text_never_holds_the_doc_id() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[doc(
            "q7_answer_3f2a_abs",
            "u1",
            "2023-01-01T10:00:00+00:00",
            &["I adopted a dog named Rex"],
        )])
        .unwrap();
        let hits = retrieve(&ix, "dog", "u1", None, &Options::default()).unwrap();
        assert_eq!(ids(&hits), vec!["q7_answer_3f2a_abs"]);
        assert!(
            !hits[0].text.contains("q7_answer_3f2a_abs"),
            "{}",
            hits[0].text
        );
        assert!(
            hits[0].text.starts_with("### Session | date: 2023-01-01"),
            "{}",
            hits[0].text
        );
    }

    fn talk(id: &str, day: usize, turns: &[(&str, &str)]) -> SessionDoc {
        SessionDoc {
            id: id.to_string(),
            user_id: Some("u1".to_string()),
            timestamp: Some(format!("2023-05-{day:02}T10:00:00+00:00")),
            turns: turns
                .iter()
                .map(|(s, t)| TurnIn {
                    speaker: s.to_string(),
                    text: t.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn speaker_in_question_does_not_bury_the_topic() {
        let mut ix = Index::open(":memory:").unwrap();
        let mut docs: Vec<SessionDoc> = (1..=24)
            .map(|i| {
                talk(
                    &format!("chat{i}"),
                    i,
                    &[
                        ("Melanie", "Hey Caroline, good morning!"),
                        ("Caroline", "Morning Melanie, the weather is lovely"),
                        ("Melanie", "So sunny today, Caroline"),
                        ("Caroline", "I love sunny days"),
                    ],
                )
            })
            .collect();
        docs.insert(
            11,
            talk(
                "adoption",
                25,
                &[
                    ("Melanie", "Hi there, how was your week?"),
                    ("Caroline", "I have been researching adoption agencies"),
                    ("Melanie", "That is big news"),
                ],
            ),
        );
        ix.add_sessions(&docs).unwrap();
        let opt = Options {
            max_sessions: 1,
            ..Options::default()
        };
        let hits = retrieve(&ix, "What did Caroline research?", "u1", None, &opt).unwrap();
        assert_eq!(ids(&hits), vec!["adoption"]);
    }

    #[test]
    fn speaker_name_finds_their_turns() {
        let turn = |speaker: &str, text: &str| TurnIn {
            speaker: speaker.to_string(),
            text: text.to_string(),
        };
        let session = |id: &str, ts: &str, turns: Vec<TurnIn>| SessionDoc {
            id: id.to_string(),
            user_id: Some("u1".to_string()),
            timestamp: Some(ts.to_string()),
            turns,
        };
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            session(
                "c1",
                "2023-05-01T10:00:00+00:00",
                vec![
                    turn("Caroline", "I went to a support group"),
                    turn("Melanie", "That sounds lovely"),
                ],
            ),
            session(
                "c2",
                "2023-05-02T10:00:00+00:00",
                vec![turn("Melanie", "I painted a lake")],
            ),
        ])
        .unwrap();
        let hits = retrieve(
            &ix,
            "What did Caroline go to?",
            "u1",
            None,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(hits.first().map(|h| h.id.as_str()), Some("c1"));
    }

    #[test]
    fn stemmed_word_is_not_a_speaker() {
        let mut ix = Index::open(":memory:").unwrap();
        let mut docs: Vec<SessionDoc> = (1..=12)
            .map(|i| {
                talk(
                    &format!("noise{i}"),
                    i + 1,
                    &[
                        ("user", "Can you help me plan my week?"),
                        (
                            "assistant",
                            "Sure, I have many ideas, here is a plan for each day.",
                        ),
                    ],
                )
            })
            .collect();
        docs.insert(
            5,
            talk(
                "season",
                1,
                &[
                    ("user", "I had 3 goals and 2 assists this season"),
                    ("assistant", "That is a strong season."),
                ],
            ),
        );
        ix.add_sessions(&docs).unwrap();
        let opt = Options {
            max_sessions: 1,
            ..Options::default()
        };
        let hits = retrieve(&ix, "How many assists do I have?", "u1", None, &opt).unwrap();
        assert_eq!(ids(&hits), vec!["season"]);
    }

    #[test]
    fn typo_still_matches() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "s1",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["We compared Hindsight with other memory tools"],
            ),
            doc(
                "s2",
                "u1",
                "2023-01-02T10:00:00+00:00",
                &["Lunch was pasta"],
            ),
        ])
        .unwrap();
        let hits = retrieve(
            &ix,
            "what did we say about hindisght",
            "u1",
            None,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(matched(&hits), vec!["s1"]);
    }

    #[test]
    fn known_terms_skip_the_trigram_search() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "s1",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["We compared Hindsight with other memory tools"],
            ),
            doc(
                "s2",
                "u1",
                "2023-01-02T10:00:00+00:00",
                &["The horse hurt its hindquarters"],
            ),
        ])
        .unwrap();
        let known = retrieve(&ix, "hindsight", "u1", None, &Options::default()).unwrap();
        assert_eq!(matched(&known), vec!["s1"]);
        let typo = retrieve(&ix, "hindquaters", "u1", None, &Options::default()).unwrap();
        assert!(ids(&typo).contains(&"s2"));
    }

    #[test]
    fn unknown_terms_are_checked_per_user() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "s1",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["Hindsight helps"],
            ),
            doc(
                "s2",
                "u2",
                "2023-01-01T10:00:00+00:00",
                &["Lunch was pasta"],
            ),
        ])
        .unwrap();
        let t = query::terms("compare hindsight hindisght pasta");
        assert_eq!(
            unknown_terms(&ix, &t, "u1").unwrap(),
            vec!["compare", "hindisght", "pasta"]
        );
        assert_eq!(
            unknown_terms(&ix, &t, "u2").unwrap(),
            vec!["compare", "hindsight", "hindisght"]
        );
    }

    #[test]
    fn hits_come_back_in_date_order() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "new",
                "u1",
                "2023-06-01T10:00:00+00:00",
                &["I now work at Globex as a designer"],
            ),
            doc(
                "old",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["I work at Acme as a designer"],
            ),
        ])
        .unwrap();
        let hits = retrieve(
            &ix,
            "Where do I currently work as a designer?",
            "u1",
            None,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(ids(&hits), vec!["old", "new"]);
    }

    #[test]
    fn time_window_picks_the_session_in_range() {
        let mut ix = Index::open(":memory:").unwrap();
        let anchor = parse_ts("2023-06-15T00:00:00+00:00");
        ix.add_sessions(&[
            doc(
                "older",
                "u1",
                "2023-03-01T10:00:00+00:00",
                &["I bought running shoes"],
            ),
            doc(
                "recent",
                "u1",
                "2023-06-01T10:00:00+00:00",
                &["I bought running shoes"],
            ),
        ])
        .unwrap();
        let opt = Options {
            max_sessions: 1,
            ..Options::default()
        };
        let hits = retrieve(
            &ix,
            "What shoes did I buy two weeks ago?",
            "u1",
            anchor,
            &opt,
        )
        .unwrap();
        assert_eq!(ids(&hits), vec!["recent"]);
    }

    #[test]
    fn output_stays_inside_the_budget() {
        let mut ix = Index::open(":memory:").unwrap();
        let long = "garden tomatoes ".repeat(200);
        let docs: Vec<SessionDoc> = (0..20)
            .map(|i| {
                doc(
                    &format!("s{i}"),
                    "u1",
                    &format!("2023-01-{:02}T10:00:00+00:00", i + 1),
                    &[long.as_str()],
                )
            })
            .collect();
        ix.add_sessions(&docs).unwrap();
        let opt = Options {
            budget_tokens: 2_000,
            ..Options::default()
        };
        let hits = retrieve(&ix, "tomatoes in the garden", "u1", None, &opt).unwrap();
        assert!(!hits.is_empty());
        let used: usize = hits.iter().map(|h| estimate_tokens(&h.text)).sum();
        assert!(used <= 2_000, "used {used}");
    }

    #[test]
    fn nothing_matches_gives_nothing() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[doc("s1", "u1", "2023-01-01", &["hello"])])
            .unwrap();
        assert!(
            retrieve(
                &ix,
                "quantum chromodynamics",
                "u1",
                None,
                &Options::default()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn long_session_is_cut_to_the_matching_turns() {
        let mut ix = Index::open(":memory:").unwrap();
        let filler = "lorem ipsum dolor sit amet ".repeat(30);
        let mut texts: Vec<&str> = vec![filler.as_str(); 12];
        texts[5] = "my bicycle is red";
        ix.add_sessions(&[doc("long", "u1", "2023-01-01T10:00:00+00:00", &texts)])
            .unwrap();
        let full_tokens: usize = texts.iter().map(|t| estimate_tokens(t)).sum();
        assert!(full_tokens > 2_000, "session too small: {full_tokens}");
        let opt = Options {
            budget_tokens: 1_500,
            ..Options::default()
        };
        let hits = retrieve(&ix, "what color is my bicycle", "u1", None, &opt).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("my bicycle is red"));
        assert!(hits[0].text.contains("..."));
        let used = estimate_tokens(&hits[0].text);
        assert!(used <= 1_500, "used {used}");
        assert!(used < full_tokens, "not shorter: {used} vs {full_tokens}");
    }

    #[test]
    fn many_short_sessions_all_fit_when_the_budget_allows() {
        let mut ix = Index::open(":memory:").unwrap();
        let docs: Vec<SessionDoc> = (1..=30)
            .map(|i| {
                doc(
                    &format!("s{i}"),
                    "u1",
                    &format!("2023-01-{i:02}T10:00:00+00:00"),
                    &["we went hiking in the hills"],
                )
            })
            .collect();
        ix.add_sessions(&docs).unwrap();
        let hits = retrieve(&ix, "hiking", "u1", None, &Options::default()).unwrap();
        assert_eq!(hits.len(), 30);
    }

    #[test]
    fn whole_sessions_leave_room_for_windows_of_the_next_ones() {
        let mut ix = Index::open(":memory:").unwrap();
        let filler = "lorem ipsum dolor sit amet ".repeat(30);
        let session = |id: &str, day: u32, hit: &str| {
            let mut texts: Vec<&str> = vec![filler.as_str(); 10];
            texts[0] = hit;
            doc(
                id,
                "u1",
                &format!("2023-01-{day:02}T10:00:00+00:00"),
                &texts,
            )
        };
        ix.add_sessions(&[
            session("best", 1, "my red bicycle has a red bell, the red bicycle"),
            session("next", 2, "the bicycle shop was closed"),
            session("third", 3, "a bicycle"),
        ])
        .unwrap();
        let whole = estimate_tokens(&filler) * 10;
        let opt = Options {
            budget_tokens: whole * 2,
            ..Options::default()
        };
        let hits = retrieve(&ix, "red bicycle", "u1", None, &opt).unwrap();
        assert_eq!(ids(&hits), vec!["best", "next", "third"]);
        assert!(!hits[0].text.contains("..."), "{}", hits[0].text);
        let windows = hits.iter().filter(|h| h.text.contains("...")).count();
        assert_eq!(windows, 1);
        let used: usize = hits.iter().map(|h| estimate_tokens(&h.text)).sum();
        assert!(used <= opt.budget_tokens, "used {used}");
    }

    #[test]
    fn window_shortens_neighbours_but_not_the_matched_turn() {
        let long_reply = format!("Sure. {}", "Here is a long answer. ".repeat(40));
        let s = SessionRow {
            doc_id: "s1".to_string(),
            ts: None,
            ts_text: None,
            turns: [
                ("assistant", long_reply.as_str()),
                ("user", "my bicycle is red"),
                ("assistant", long_reply.as_str()),
            ]
            .iter()
            .enumerate()
            .map(|(i, (sp, t))| (i as i64, sp.to_string(), t.to_string()))
            .collect(),
        };
        let text = window(&s, &[1], STEPS[0], "");
        assert!(text.contains("user: my bicycle is red\n"), "{text}");
        assert!(!text.contains(&long_reply), "{text}");
        assert!(
            estimate_tokens(&text) < estimate_tokens(&long_reply),
            "{text}"
        );
        assert!(render(&s, None, "").contains(&long_reply));
    }

    #[test]
    fn window_also_shows_short_matched_turns_away_from_the_best_one() {
        let long_reply = format!("Rings are lovely. {}", "Here is a long answer. ".repeat(40));
        let mut turns: Vec<(&str, &str)> = vec![("user", "my aunt gave me a ring today")];
        turns.extend([("assistant", "Nice."), ("user", "Thanks.")].repeat(3));
        turns.push(("assistant", long_reply.as_str()));
        turns.push(("user", long_reply.as_str()));
        let s = SessionRow {
            doc_id: "s1".to_string(),
            ts: None,
            ts_text: None,
            turns: turns
                .iter()
                .enumerate()
                .map(|(i, (sp, t))| (i as i64, sp.to_string(), t.to_string()))
                .collect(),
        };
        let text = window(&s, &[7, 8, 0], STEPS[0], "");
        assert!(
            text.contains("user: my aunt gave me a ring today\n"),
            "{text}"
        );
        assert!(!text.contains("assistant: Nice."), "{text}");
        let without_short = window(&s, &[7, 8], STEPS[0], "");
        assert!(!without_short.contains("aunt"), "{without_short}");
        assert!(
            !without_short.contains(&format!("user: {long_reply}")),
            "{without_short}"
        );
    }

    #[test]
    fn headers_say_how_long_before_the_question() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[doc(
            "bake",
            "u1",
            "2022-03-21T15:54:00+00:00",
            &["I took a baking class at the culinary school"],
        )])
        .unwrap();
        let anchor = parse_ts("2022-04-15T18:46:00+00:00");
        let hits = retrieve(&ix, "baking class", "u1", anchor, &Options::default()).unwrap();
        assert!(
            hits[0].text.starts_with("### Session | date: 2022-03-21T15:54:00+00:00 (Monday, 25 days before the question)\n"),
            "{}",
            hits[0].text
        );
        let none = retrieve(&ix, "baking class", "u1", None, &Options::default()).unwrap();
        assert!(
            none[0]
                .text
                .starts_with("### Session | date: 2022-03-21T15:54:00+00:00\n")
        );
    }

    #[test]
    fn fill_off_returns_only_matched_sessions() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "dog",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["I adopted a dog named Rex"],
            ),
            doc(
                "lunch",
                "u1",
                "2023-01-02T10:00:00+00:00",
                &["Lunch was pasta"],
            ),
        ])
        .unwrap();
        let opt = Options {
            fill: false,
            ..Options::default()
        };
        let hits = retrieve(&ix, "dog", "u1", None, &opt).unwrap();
        assert_eq!(ids(&hits), vec!["dog"]);
    }

    #[test]
    fn leftover_budget_fills_with_other_sessions_newest_first() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "dog",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["I adopted a dog named Rex"],
            ),
            doc(
                "old",
                "u1",
                "2023-01-02T10:00:00+00:00",
                &["Lunch was pasta"],
            ),
            doc(
                "new",
                "u1",
                "2023-01-03T10:00:00+00:00",
                &["Dinner was soup"],
            ),
            doc(
                "other",
                "u2",
                "2023-01-04T10:00:00+00:00",
                &["Breakfast was eggs"],
            ),
        ])
        .unwrap();
        let hits = retrieve(&ix, "dog", "u1", None, &Options::default()).unwrap();
        assert_eq!(ids(&hits), vec!["dog", "old", "new"]);
        assert_eq!(matched(&hits), vec!["dog"]);
        let size = |sid| estimate_tokens(&render(&ix.session(sid).unwrap(), None, ""));
        let tight = Options {
            budget_tokens: size(1) + size(3) + MIN_LEFT - 1,
            ..Options::default()
        };
        let hits = retrieve(&ix, "dog", "u1", None, &tight).unwrap();
        assert_eq!(ids(&hits), vec!["dog", "new"]);
    }

    #[test]
    fn tiny_budget_still_returns_the_best_match() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[doc(
            "s1",
            "u1",
            "2023-01-01T10:00:00+00:00",
            &["I adopted a dog named Rex"],
        )])
        .unwrap();
        let opt = Options {
            budget_tokens: 100,
            ..Options::default()
        };
        let hits = retrieve(&ix, "dog", "u1", None, &opt).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn tied_scores_pick_the_same_turns_every_time() {
        let mut ix = Index::open(":memory:").unwrap();
        let filler = "lorem ipsum dolor sit amet ".repeat(30);
        let mut texts: Vec<&str> = vec![filler.as_str(); 40];
        texts[0] = "cat dog";
        texts[10] = "cat cat dog";
        texts[20] = "cat cat cat dog";
        texts[30] = "cat cat cat cat dog";
        ix.add_sessions(&[doc("tie", "u1", "2023-01-01T10:00:00+00:00", &texts)])
            .unwrap();
        let opt = Options {
            budget_tokens: 4_000,
            ..Options::default()
        };
        let first: Vec<String> = retrieve(&ix, "cat now", "u1", None, &opt)
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        assert_eq!(first.len(), 1);
        for _ in 0..20 {
            let again: Vec<String> = retrieve(&ix, "cat now", "u1", None, &opt)
                .unwrap()
                .into_iter()
                .map(|h| h.text)
                .collect();
            assert!(again == first, "hit text changed between runs");
        }
    }

    #[test]
    fn focused_render_marks_cut_turns_at_the_end() {
        let mut ix = Index::open(":memory:").unwrap();
        let filler = "lorem ipsum dolor sit amet ".repeat(30);
        let mut texts: Vec<&str> = vec![filler.as_str(); 12];
        texts[0] = "my bicycle is red";
        ix.add_sessions(&[doc("tail", "u1", "2023-01-01T10:00:00+00:00", &texts)])
            .unwrap();
        let full_tokens: usize = texts.iter().map(|t| estimate_tokens(t)).sum();
        assert!(full_tokens > 1_500, "session too small: {full_tokens}");
        let opt = Options {
            budget_tokens: 1_500,
            ..Options::default()
        };
        let hits = retrieve(&ix, "what color is my bicycle", "u1", None, &opt).unwrap();
        assert_eq!(hits.len(), 1);
        let text = &hits[0].text;
        assert!(text.ends_with("...\n"), "{text}");
        let mut lines = text.lines();
        lines.next();
        assert_ne!(lines.next(), Some("..."));
    }
}
