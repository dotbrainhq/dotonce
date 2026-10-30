//! Facts: markdown files in the configured stores, kept in sync with one
//! search index. Each fact is indexed as a one-turn session for user `facts`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::config::{Config, expand, home_dir};
use crate::fact::{Fact, has_broken_front, parse, slug, to_markdown};
use crate::index::Index;
use crate::model::{SessionDoc, TurnIn};
use crate::query;
use crate::retrieve::{Options, estimate_tokens, rank};
use crate::timeq::parse_ts;

pub const USER: &str = "facts";
const SKIP_FILES: [&str; 3] = ["INDEX.md", "README.md", "MEMORY.md"];
const BODY_CHARS: usize = 300;
const INDEX_BODY_CHARS: usize = 600;
const MAX_FACTS: usize = 6;
const STALE_DAYS: i64 = 90;
/// Tokens kept back from the budget for the block's title and save hint.
const BLOCK_RESERVE: usize = 120;
/// Words that carry no topic. A prompt made only of these and stop words gets
/// no facts, since any match would be on the filler alone.
const FILLER: &[&str] = &[
    "ok", "okay", "yeah", "yep", "sure", "thanks", "thank", "please", "hi", "hello", "hey", "fine",
    "good", "great", "looks", "look", "continue", "go", "ahead", "yes", "nice", "cool", "done",
    "perfect", "right", "correct", "agree", "lgtm",
];
/// Words that open a question. A prompt that starts with one, or has a
/// question mark, asks for something.
const QUESTION_STARTS: &[&str] = &[
    "how", "what", "why", "when", "where", "which", "who", "can", "should", "does", "do", "is",
    "are", "could", "would",
];

const META: &str = "CREATE TABLE IF NOT EXISTS fact_meta(
    doc_id TEXT PRIMARY KEY,
    store TEXT NOT NULL,
    path TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL,
    private INTEGER NOT NULL,
    pinned INTEGER NOT NULL,
    replaced INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    body TEXT NOT NULL,
    updated TEXT,
    aliases TEXT NOT NULL DEFAULT ''
);";

#[derive(Debug, Default, PartialEq)]
pub struct SyncReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FactHit {
    pub doc_id: String,
    pub name: String,
    pub path: String,
    pub updated: Option<String>,
    pub line: String,
}

/// How facts are turned into turns. Bump it when `index_fact` changes, so rows
/// indexed the old way count as changed on the next sync.
const FORMAT: i64 = 2;

pub fn open_at(path: &str) -> Result<Index> {
    let ix = Index::open(path)?;
    ix.conn.execute_batch(META)?;
    let has_aliases: bool = ix.conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('fact_meta') WHERE name = 'aliases'",
        [],
        |r| r.get(0),
    )?;
    if !has_aliases {
        ix.conn
            .execute_batch("ALTER TABLE fact_meta ADD COLUMN aliases TEXT NOT NULL DEFAULT ''")?;
    }
    let v: i64 = ix.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < FORMAT {
        ix.conn.execute_batch(&format!(
            "BEGIN; UPDATE fact_meta SET mtime = -1; PRAGMA user_version = {FORMAT}; COMMIT;"
        ))?;
    }
    Ok(ix)
}

pub fn open() -> Result<Index> {
    let home = home_dir()?;
    fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
    open_at(&home.join("index.sqlite").to_string_lossy())
}

pub fn clear(ix: &Index) -> Result<()> {
    ix.reset()?;
    ix.conn.execute_batch(META)?;
    ix.conn.execute("DELETE FROM fact_meta", [])?;
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with('.') || n.starts_with('_') {
            continue;
        }
        // file_type does not follow symlinks, so a linked folder or file,
        // which may point outside the store, is never read.
        let Ok(ft) = e.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        } else if ft.is_dir() {
            walk(&p, out);
        } else if n.ends_with(".md") && !SKIP_FILES.contains(&n.as_str()) {
            out.push(p);
        }
    }
}

/// Modified time in nanoseconds and size, so a quick same-size edit still counts.
fn stamp(p: &Path) -> Option<(i64, i64)> {
    let m = fs::metadata(p).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((t, m.len() as i64))
}

/// The first `max` characters of `s`, cut back to the last word boundary.
fn cap_at_word(s: &str, max: usize) -> &str {
    let Some((end, _)) = s.char_indices().nth(max) else {
        return s;
    };
    let head = &s[..end];
    if s[end..].starts_with(char::is_whitespace) {
        return head.trim_end();
    }
    match head.rfind(char::is_whitespace) {
        Some(i) => head[..i].trim_end(),
        None => head,
    }
}

/// Title and body go in separate turns, and only the start of the body is
/// indexed, so a long body that repeats the query words can't outrank a short
/// fact whose title matches. Speakers stay empty: `rank` takes a query word
/// that names a speaker as a speaker filter, not as a search term.
fn index_fact(ix: &mut Index, doc_id: &str, name: &str, f: &Fact) -> Result<()> {
    let mut title = format!("{}: {}", name.replace('-', " "), f.description);
    if !f.aliases.is_empty() {
        title.push_str(&format!(" ({})", f.aliases.join(", ")));
    }
    let mut turns = vec![TurnIn {
        speaker: String::new(),
        text: title,
    }];
    if !f.body.is_empty() {
        turns.push(TurnIn {
            speaker: String::new(),
            text: cap_at_word(&f.body, INDEX_BODY_CHARS).to_string(),
        });
    }
    ix.add_sessions(&[SessionDoc {
        id: doc_id.to_string(),
        user_id: Some(USER.to_string()),
        timestamp: f.updated.clone().or_else(|| f.learned.clone()),
        turns,
    }])?;
    Ok(())
}

/// Whether a file was added or updated in the index.
enum Change {
    Added,
    Updated,
}

/// Brings one file's entry up to date. `None` when nothing changed or the file
/// is skipped; a skipped file leaves `seen`, so its old entry is removed.
fn sync_file(
    ix: &mut Index,
    store: &str,
    root: &Path,
    p: &Path,
    seen: &mut HashSet<String>,
) -> Result<Option<Change>> {
    let Some((mtime, size)) = stamp(p) else {
        return Ok(None);
    };
    let rel = p
        .strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string();
    let doc_id = format!("{store}:{rel}");
    seen.insert(doc_id.clone());
    let known: Option<(i64, i64)> = ix
        .conn
        .query_row(
            "SELECT mtime, size FROM fact_meta WHERE doc_id = ?1",
            params![doc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if known == Some((mtime, size)) {
        return Ok(None);
    }
    let Ok(src) = fs::read_to_string(p) else {
        seen.remove(&doc_id);
        return Ok(None);
    };
    if has_broken_front(&src) {
        seen.remove(&doc_id);
        return Ok(None);
    }
    let f = parse(&src);
    let name = if f.name.is_empty() {
        p.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        f.name.clone()
    };
    let updated = f.updated.clone().or_else(|| f.learned.clone());
    ix.atomic(|ix| {
        if known.is_some() {
            ix.remove_session(USER, &doc_id)?;
        }
        index_fact(ix, &doc_id, &name, &f)?;
        ix.conn.execute(
            "INSERT OR REPLACE INTO fact_meta(doc_id, store, path, mtime, size, private, pinned, replaced, name, description, body, updated, aliases)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                doc_id,
                store,
                p.to_string_lossy().to_string(),
                mtime,
                size,
                f.private,
                f.pinned,
                f.replaced_by.is_some(),
                name,
                f.description,
                f.body,
                updated,
                f.aliases.join("\n"),
            ],
        )?;
        Ok(())
    })?;
    Ok(Some(if known.is_some() {
        Change::Updated
    } else {
        Change::Added
    }))
}

fn remove_fact(ix: &mut Index, doc_id: &str) -> Result<()> {
    ix.atomic(|ix| {
        ix.remove_session(USER, doc_id)?;
        ix.conn
            .execute("DELETE FROM fact_meta WHERE doc_id = ?1", params![doc_id])?;
        Ok(())
    })
}

/// Syncs every store. A file that fails keeps its old entry and the sync goes
/// on with the rest, removals included; the first error is returned at the end.
pub fn sync(ix: &mut Index, cfg: &Config) -> Result<SyncReport> {
    let mut rep = SyncReport::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut first_err: Option<anyhow::Error> = None;
    for st in &cfg.stores {
        let root = expand(&st.path);
        let mut files = Vec::new();
        walk(&root, &mut files);
        for p in files {
            match sync_file(ix, &st.name, &root, &p, &mut seen) {
                Ok(Some(Change::Added)) => rep.added += 1,
                Ok(Some(Change::Updated)) => rep.updated += 1,
                Ok(None) => {}
                Err(e) => {
                    first_err.get_or_insert(e.context(format!("sync {}", p.display())));
                }
            }
        }
    }
    let known: rusqlite::Result<Vec<String>> = ix
        .conn
        .prepare("SELECT doc_id FROM fact_meta")
        .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect());
    match known {
        Ok(known) => {
            for d in known.iter().filter(|d| !seen.contains(*d)) {
                match remove_fact(ix, d) {
                    Ok(()) => rep.removed += 1,
                    Err(e) => {
                        first_err.get_or_insert(e.context(format!("remove {d}")));
                    }
                }
            }
        }
        Err(e) => {
            first_err.get_or_insert(e.into());
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(rep),
    }
}

fn line(name: &str, desc: &str, body: &str, updated: Option<&str>, path: &str, now: i64) -> String {
    let date = updated.unwrap_or("undated");
    let stale = updated
        .and_then(parse_ts)
        .is_some_and(|t| (now - t) / 86_400 > STALE_DAYS);
    let flag = if stale { ", verify before use" } else { "" };
    let mut s = format!("- **{name}** ({date}{flag}) `{path}`: {desc}");
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if !flat.is_empty() {
        s.push_str(" | ");
        s.extend(flat.chars().take(BODY_CHARS));
        if flat.chars().count() > BODY_CHARS {
            s.push_str(" ...");
        }
    }
    s
}

/// The rendered hit for a fact, and whether it is private. Replaced facts give None.
fn hit_for(ix: &Index, doc_id: &str, now: i64) -> Result<Option<(FactHit, bool)>> {
    let row = ix
        .conn
        .query_row(
            "SELECT name, description, body, updated, path, private, replaced FROM fact_meta WHERE doc_id = ?1",
            params![doc_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((name, desc, body, updated, path, private, replaced)) = row else {
        return Ok(None);
    };
    if replaced {
        return Ok(None);
    }
    let hit = FactHit {
        doc_id: doc_id.to_string(),
        line: line(&name, &desc, &body, updated.as_deref(), &path, now),
        name,
        path,
        updated,
    };
    Ok(Some((hit, private)))
}

fn take_within(
    ix: &Index,
    ids: impl IntoIterator<Item = String>,
    private_ok: bool,
    now: i64,
    budget: usize,
) -> Result<Vec<FactHit>> {
    let mut out = Vec::new();
    let mut used = 0;
    for d in ids {
        if out.len() >= MAX_FACTS {
            break;
        }
        let Some((h, private)) = hit_for(ix, &d, now)? else {
            continue;
        };
        if private && !private_ok {
            continue;
        }
        let cost = estimate_tokens(&h.line);
        if used + cost > budget {
            continue;
        }
        used += cost;
        out.push(h);
    }
    Ok(out)
}

pub fn search(
    ix: &Index,
    cfg: &Config,
    query: &str,
    cwd: &Path,
    now: i64,
    budget: usize,
) -> Result<Vec<FactHit>> {
    let private_ok = cfg.private_allowed(cwd);
    let (typed, material) = split_prompt(query);
    let titles = Titles::load(ix, private_ok)?;
    let mut said = Vec::new();
    if !typed.trim().is_empty() && !is_status(&typed) {
        let opt = Options {
            fill: false,
            names: false,
            ..Options::default()
        };
        let mut order: HashMap<String, usize> = HashMap::new();
        for (sid, _, _) in rank(ix, &typed, USER, Some(now), &opt)? {
            let doc_id = ix.session(sid)?.doc_id;
            let next = order.len();
            order.entry(doc_id).or_insert(next);
        }
        said = titles.typed_matches(&typed, &order);
    }
    let pasted = if material.is_empty() {
        Vec::new()
    } else {
        titles.material_matches(&material, &typed)
    };
    // The best match for what was typed and the best for what was pasted
    // both get a place before the second best of either.
    let mut ids: Vec<String> = Vec::new();
    for i in 0..said.len().max(pasted.len()) {
        for id in [said.get(i), pasted.get(i)].into_iter().flatten() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    ids.truncate(MAX_PROMPT_FACTS);
    let budget = budget.saturating_sub(BLOCK_RESERVE);
    take_within(ix, ids, private_ok, now, budget)
}

/// At most this many facts go with one prompt: past the first few, a fact is
/// more often a near miss than a second answer.
pub const MAX_PROMPT_FACTS: usize = 3;
/// The best fact must cover this share of what was typed, or nothing is sent.
const BEST_COVER: f64 = 0.5;
/// Facts after the best must cover at least this share of what the best
/// covers, unless the text search also ranks them near the top.
const NEAR_BEST: f64 = 0.7;
/// What any fact must cover of what was typed, to be sent at all.
const MIN_COVER: f64 = 0.3;
/// A word in at most this share of fact titles (and never fewer than two) is
/// rare. Pasted material only counts rare words: common ones turn up in any
/// long paste.
const RARE_SHARE: f64 = 0.03;
/// A fact matches pasted material when the words it shares with it carry this
/// share of its title's weight.
const MATERIAL_COVER: f64 = 0.3;
/// A typed word no fact title has counts toward the prompt's weight at the
/// rarest word's weight times n / (n + STORE_SCALE) for a store of n facts. A
/// small store doesn't know most words yet, so a new word says little there;
/// in a large store it suggests the prompt is about something else.
const STORE_SCALE: f64 = 200.0;
/// A block of at most this many words and two lines, before or after pasted
/// material, is what was typed.
const TYPED_WORDS: usize = 25;
/// A prompt of at most two lines and this many words is all typed.
const SHORT_PROMPT_WORDS: usize = 60;

/// Words that say how the work goes rather than what it is about. A clause
/// that starts or ends with one, like "deploy done" or "fixed the login
/// redirect", reports progress.
const PROGRESS: &[&str] = &[
    "done", "next", "finished", "fine", "good", "clean", "worked", "moving", "continue", "passing",
    "complete",
];
/// Common past forms that don't end in "ed".
const IRREGULAR_PAST: &[&str] = &[
    "ran", "sent", "went", "made", "built", "wrote", "did", "gave", "took", "saw", "began", "came",
    "fell", "held", "kept", "lost", "met", "paid", "said", "sold", "spent", "stood", "told", "won",
];
/// Words that report trouble. A line with one is a bug report, not progress,
/// even when it ends in a past form, like "lint failed" or "found a 429".
/// "timed out" counts as well.
const TROUBLE: &[&str] = &[
    "fail",
    "fails",
    "failed",
    "failing",
    "failure",
    "failures",
    "crash",
    "crashes",
    "crashed",
    "crashing",
    "broke",
    "broken",
    "breaks",
    "breaking",
    "found",
    "got",
    "rejected",
    "rejects",
    "rejecting",
    "expired",
    "expires",
    "expiring",
    "stuck",
    "missing",
    "error",
    "errors",
    "errored",
    "erroring",
    "timeout",
    "timeouts",
];
/// Words that stand in for a topic without naming one.
const GENERIC: &[&str] = &[
    "one",
    "ones",
    "thing",
    "things",
    "stuff",
    "bit",
    "lot",
    "way",
    "something",
    "anything",
    "everything",
];

/// Characters that are common in code and logs and rare in typed text.
const CODE_CHARS: &str = "{}()[];=<>$#/\\|`*&@%_:\"'+~";

fn swapped_pair(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len() != b.len() {
        return false;
    }
    let d: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    d.len() == 2 && d[1] == d[0] + 1 && a[d[0]] == b[d[1]] && a[d[1]] == b[d[0]]
}

/// A word that reports progress: a progress word (or one with two letters
/// swapped, like "dnoe"), or a past form such as "merged" or "sent".
fn reports(w: &str) -> bool {
    let n = w.chars().count();
    PROGRESS.contains(&w)
        || IRREGULAR_PAST.contains(&w)
        || (n >= 5 && w.ends_with("ed") && !w.ends_with("eed"))
        || (n >= 4 && PROGRESS.iter().any(|p| swapped_pair(w, p)))
}

fn has_trouble(line: &str) -> bool {
    let ws: Vec<String> = line
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    ws.iter().any(|w| TROUBLE.contains(&w.as_str()))
        || ws.windows(2).any(|p| p[0] == "timed" && p[1] == "out")
}

/// Whether typed text is a status line: it asks nothing, reports no trouble,
/// and one of its clauses starts or ends with a word that reports progress.
/// Lines that look like code don't count, so a pasted `<pasted_content>` tag
/// is no report.
fn is_status(text: &str) -> bool {
    if asks(text) {
        return false;
    }
    let prose: Vec<&str> = text.lines().filter(|l| !looks_like_code(l)).collect();
    if prose.iter().any(|l| has_trouble(l)) {
        return false;
    }
    prose
        .into_iter()
        .flat_map(|l| l.split([',', ';', '.', '!']))
        .any(|clause| {
            let ws: Vec<String> = query::terms(clause)
                .into_iter()
                .filter(|w| !FILLER.contains(&w.as_str()) || PROGRESS.contains(&w.as_str()))
                .collect();
            ws.first().is_some_and(|w| reports(w)) || ws.last().is_some_and(|w| reports(w))
        })
}

fn looks_like_code(line: &str) -> bool {
    let s = line.trim();
    if s.is_empty() {
        return false;
    }
    if line.starts_with([' ', '\t']) || s.starts_with(['>', '$', '%', '#', '@', '[']) {
        return true;
    }
    let marks = s.chars().filter(|c| CODE_CHARS.contains(*c)).count();
    marks * 10 > s.chars().count()
}

/// Splits a prompt into what was typed and what was pasted. A short prompt is
/// all typed. In a longer one, a short block after (or else before) the rest
/// is what was typed, and so are one or two plain lines right before or after
/// lines that look like code. Without either, the prompt is pasted when most
/// of its lines look like code or a log, and typed otherwise.
fn split_prompt(q: &str) -> (String, String) {
    let lines: Vec<&str> = q.trim().lines().collect();
    if lines.len() <= 2 && q.split_whitespace().count() <= SHORT_PROMPT_WORDS {
        return (q.to_string(), String::new());
    }
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for l in &lines {
        if l.trim().is_empty() {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(l);
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    let short = |b: &Vec<&str>| {
        b.len() <= 2
            && b.iter()
                .map(|l| l.split_whitespace().count())
                .sum::<usize>()
                <= TYPED_WORDS
    };
    let join = |bs: &[Vec<&str>]| {
        bs.iter()
            .map(|b| b.join("\n"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if blocks.len() > 1 {
        if short(&blocks[blocks.len() - 1]) {
            let (rest, last) = blocks.split_at(blocks.len() - 1);
            return (join(last), join(rest));
        }
        if short(&blocks[0]) {
            let (first, rest) = blocks.split_at(1);
            return (join(first), join(rest));
        }
    }
    // No blank line sets the typed text apart: look for typed lines right
    // before or after lines that look like code or a log.
    let code: Vec<bool> = lines.iter().map(|l| looks_like_code(l)).collect();
    let lead = code.iter().take_while(|c| !**c).count();
    let tail = code.iter().rev().take_while(|c| !**c).count();
    let words = |ls: &[&str]| {
        ls.iter()
            .map(|l| l.split_whitespace().count())
            .sum::<usize>()
    };
    if (1..=2).contains(&lead) && lead < lines.len() && words(&lines[..lead]) <= TYPED_WORDS {
        return (lines[..lead].join("\n"), lines[lead..].join("\n"));
    }
    let cut = lines.len() - tail;
    if (1..=2).contains(&tail) && tail < lines.len() && words(&lines[cut..]) <= TYPED_WORDS {
        return (lines[cut..].join("\n"), lines[..cut].join("\n"));
    }
    if code.iter().filter(|c| **c).count() * 2 >= lines.len() {
        return (String::new(), q.to_string());
    }
    (q.to_string(), String::new())
}

/// The words of each fact title the caller may see: its name and aliases,
/// and those plus its description.
struct Titles {
    ids: Vec<String>,
    names: Vec<Vec<String>>,
    words: Vec<Vec<String>>,
    /// The name and description words alone, without the aliases.
    own: Vec<Vec<String>>,
    /// The name words alone.
    own_names: Vec<Vec<String>>,
    /// Each distinct title word and the facts that have it, by the word's
    /// first four characters: `word_match` only pairs words that share them
    /// (or are equal).
    vocab: HashMap<String, Vec<(String, Vec<usize>)>>,
}

/// The bucket `word_match` partners of `w` are in.
fn bucket(w: &str) -> String {
    w.chars().take(4).collect()
}

impl Titles {
    fn load(ix: &Index, private_ok: bool) -> Result<Self> {
        let mut st = ix.conn.prepare(
            "SELECT doc_id, name, description, aliases FROM fact_meta
             WHERE replaced = 0 AND (?1 OR private = 0) ORDER BY doc_id",
        )?;
        let rows = st
            .query_map(params![private_ok], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut t = Titles {
            ids: Vec::new(),
            names: Vec::new(),
            words: Vec::new(),
            own: Vec::new(),
            own_names: Vec::new(),
            vocab: HashMap::new(),
        };
        let mut by_word: HashMap<String, Vec<usize>> = HashMap::new();
        for (id, name, desc, aliases) in rows {
            t.own.push(distinct(tokens(&format!("{name} {desc}"))));
            t.own_names.push(distinct(query::terms(&name)));
            let name = format!("{name} {aliases}");
            let mut words = distinct(tokens(&format!("{name} {desc}")));
            words.retain(|w| !query::terms(w).is_empty());
            let k = t.ids.len();
            for w in &words {
                by_word.entry(w.clone()).or_default().push(k);
            }
            t.ids.push(id);
            t.names.push(distinct(query::terms(&name)));
            t.words.push(words);
        }
        for (w, facts) in by_word {
            t.vocab.entry(bucket(&w)).or_default().push((w, facts));
        }
        Ok(t)
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    /// How many titles have a word that matches `term`.
    fn df(&self, term: &str) -> usize {
        let mut seen: HashSet<usize> = HashSet::new();
        for (w, facts) in self.vocab.get(&bucket(term)).into_iter().flatten() {
            if word_match(term, w) {
                seen.extend(facts);
            }
        }
        seen.len()
    }

    fn idf(&self, df: usize) -> f64 {
        if df == 0 {
            0.0
        } else {
            (1.0 + self.len() as f64 / df as f64).ln()
        }
    }

    fn rare(&self, df: usize) -> bool {
        df > 0 && df as f64 <= (self.len() as f64 * RARE_SHARE).max(2.0)
    }

    /// Facts whose titles cover what was typed, in `order` (the text search's
    /// ranking), then by cover.
    fn typed_matches(&self, typed: &str, order: &HashMap<String, usize>) -> Vec<String> {
        let terms: Vec<String> = query::terms(typed)
            .into_iter()
            .filter(|t| !FILLER.contains(&t.as_str()) && !GENERIC.contains(&t.as_str()))
            .collect();
        let idf: Vec<f64> = terms.iter().map(|t| self.idf(self.df(t))).collect();
        let useful = idf.iter().filter(|w| **w > 0.0).count();
        if useful == 0 {
            return Vec::new();
        }
        let n = self.len() as f64;
        let unseen = idf.iter().filter(|w| **w == 0.0).count() as f64;
        let total = idf.iter().sum::<f64>() + unseen * (1.0 + n).ln() * n / (n + STORE_SCALE);
        // (fact, cover, words matched, words matched in its name or aliases)
        let mut found: Vec<(usize, f64, usize, usize)> = Vec::new();
        for k in 0..self.len() {
            let (mut got, mut hits, mut named) = (0.0, 0, 0);
            for (t, w) in terms.iter().zip(&idf) {
                if *w > 0.0 && self.words[k].iter().any(|x| word_match(t, x)) {
                    got += w;
                    hits += 1;
                    if self.names[k].iter().any(|x| word_match(t, x)) {
                        named += 1;
                    }
                }
            }
            if hits > 0 {
                found.push((k, got / total, hits, named));
            }
        }
        let best = found.iter().map(|f| f.1).fold(0.0, f64::max);
        if best < BEST_COVER {
            return Vec::new();
        }
        if useful == 1 {
            // One word only says enough when it names a few facts.
            found.retain(|f| f.3 > 0);
            if found.len() > MAX_PROMPT_FACTS {
                return Vec::new();
            }
        }
        let pos = |k: usize| order.get(&self.ids[k]).copied().unwrap_or(usize::MAX);
        if useful > 1 {
            // Near the best title match, or near the top of the text search
            // too, which also reads the body.
            found.retain(|f| {
                f.2 >= 2
                    && (f.1 >= best * NEAR_BEST
                        || (f.1 >= MIN_COVER && pos(f.0) < MAX_PROMPT_FACTS))
            });
        }
        found.sort_by(|a, b| {
            pos(a.0)
                .cmp(&pos(b.0))
                .then(b.1.total_cmp(&a.1))
                .then(a.0.cmp(&b.0))
        });
        found.into_iter().map(|f| self.ids[f.0].clone()).collect()
    }

    /// Facts that pasted material is about: the title shares at least two
    /// rare words with it, one of them in the name itself (an alias can hold
    /// a plain word like "make"), and those
    /// shared words carry `MATERIAL_COVER` of the weight of the name and
    /// description. The typed
    /// words count as part of the material.
    fn material_matches(&self, material: &str, typed: &str) -> Vec<String> {
        let words = PastedWords::new(tokens(material).into_iter().chain(query::terms(typed)));
        let mut dfs: HashMap<&str, usize> = HashMap::new();
        let mut found: Vec<(usize, f64)> = Vec::new();
        for k in 0..self.len() {
            let mut title: Vec<&String> = Vec::new();
            for w in &self.words[k] {
                if !title.iter().any(|x| word_match(w, x)) {
                    title.push(w);
                }
            }
            let (mut all, mut got, mut rare, mut rare_named) = (0.0, 0.0, 0, false);
            for w in title {
                let df = *dfs.entry(w.as_str()).or_insert_with(|| self.df(w));
                let weight = self.idf(df);
                // An alias is another way to say the title, not more of it.
                if self.own[k].iter().any(|x| word_match(w, x)) {
                    all += weight;
                }
                if words.has(w) {
                    got += weight;
                    if self.rare(df) {
                        rare += 1;
                        rare_named |= self.own_names[k].iter().any(|x| word_match(w, x));
                    }
                }
            }
            let cover = if all > 0.0 { (got / all).min(1.0) } else { 0.0 };
            if rare >= 2 && rare_named && cover >= MATERIAL_COVER {
                found.push((k, cover));
            }
        }
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        found.into_iter().map(|f| self.ids[f.0].clone()).collect()
    }
}

fn distinct(ws: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in ws {
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// The words of pasted text, kept so `word_match` against all of them is a
/// few set lookups.
struct PastedWords {
    exact: HashSet<String>,
    /// Every prefix of four or more characters of each word.
    prefixes: HashSet<String>,
    /// The first five characters of each word of five or more.
    fives: HashSet<String>,
}

impl PastedWords {
    fn new(ws: impl IntoIterator<Item = String>) -> Self {
        let mut p = PastedWords {
            exact: HashSet::new(),
            prefixes: HashSet::new(),
            fives: HashSet::new(),
        };
        for w in ws {
            let cs: Vec<char> = w.chars().collect();
            for n in 4..=cs.len() {
                p.prefixes.insert(cs[..n].iter().collect());
            }
            if cs.len() >= 5 {
                p.fives.insert(cs[..5].iter().collect());
            }
            p.exact.insert(w);
        }
        p
    }

    /// Whether `word_match(w, x)` holds for some pasted word `x`.
    fn has(&self, w: &str) -> bool {
        let cs: Vec<char> = w.chars().collect();
        self.exact.contains(w)
            || (cs.len() >= 4 && self.prefixes.contains(w))
            || (4..cs.len()).any(|n| self.exact.contains(&cs[..n].iter().collect::<String>()))
            || (cs.len() >= 5 && self.fives.contains(&cs[..5].iter().collect::<String>()))
    }
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1)
        .map(str::to_lowercase)
        .collect()
}

/// Crude stemming: "repros" and "reproductions", "dash" and "dashes" match.
pub fn word_match(term: &str, word: &str) -> bool {
    if term == word {
        return true;
    }
    let (t, w) = (term.chars().count(), word.chars().count());
    if t >= 4 && word.starts_with(term) || w >= 4 && term.starts_with(word) {
        return true;
    }
    t >= 5 && w >= 5 && term.chars().take(5).eq(word.chars().take(5))
}

/// Whether the prompt asks something: a question mark, or a question word
/// first, after any filler such as "ok".
fn asks(query: &str) -> bool {
    query.contains('?')
        || query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .find(|w| !FILLER.contains(&w.as_str()))
            .is_some_and(|w| QUESTION_STARTS.contains(&w.as_str()))
}

pub fn pinned(
    ix: &Index,
    cfg: &Config,
    cwd: &Path,
    now: i64,
    budget: usize,
) -> Result<Vec<FactHit>> {
    let ids: Vec<String> = {
        let mut st = ix
            .conn
            .prepare("SELECT doc_id FROM fact_meta WHERE pinned = 1 ORDER BY updated DESC")?;
        st.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let budget = budget.saturating_sub(BLOCK_RESERVE);
    take_within(ix, ids, cfg.private_allowed(cwd), now, budget)
}

/// How to save a fact, with `{bin}` for the command that runs dotonce.
/// Aliases are other names for the fact's topic; the prompt filter matches
/// them like the fact's title, so a plain word there pulls the fact into any
/// prompt that uses it.
const SAVE_HINT: &str = "To keep something for next time, run: {bin} save \"<one line: the rule or fact>\" --source \"<where it came from>\" --aliases \"<at most 3 short other names for the same topic, comma separated, like: sev1, incident, on-call; never general words like server, test or deploy>\"";

/// The block ends with how to save a fact, run as `bin`: the command that
/// starts this dotonce, since it may not be on PATH.
pub fn render_block(title: &str, hits: &[FactHit], bin: &str) -> String {
    let mut s = format!("<dotonce>\n{title}\n");
    for h in hits {
        s.push_str(&h.line);
        s.push('\n');
    }
    s.push_str(&SAVE_HINT.replace("{bin}", bin));
    s.push_str("\n</dotonce>");
    s
}

pub fn save(cfg: &Config, store: Option<&str>, f: &Fact) -> Result<PathBuf> {
    let name = store.unwrap_or(&cfg.save_store);
    let st = cfg
        .store(name)
        .with_context(|| format!("no store named {name} in the config"))?;
    let dir = expand(&st.path);
    fs::create_dir_all(&dir)?;
    let base = slug(&f.name);
    let mut path = dir.join(format!("{base}.md"));
    let mut n = 2;
    let mut file = loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break file,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                path = dir.join(format!("{base}-{n}.md"));
                n += 1;
            }
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    };
    file.write_all(to_markdown(f).as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StoreCfg;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dotonce-facts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg_for(dir: &Path, private_paths: &[&str]) -> Config {
        Config {
            stores: vec![StoreCfg {
                name: "kb".into(),
                path: dir.to_string_lossy().into_owned(),
            }],
            private_paths: private_paths.iter().map(|s| s.to_string()).collect(),
            ignore_paths: Vec::new(),
            save_store: "kb".into(),
            prompt_budget: 1_500,
            session_budget: 1_500,
        }
    }

    fn write(dir: &Path, rel: &str, src: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, src).unwrap();
    }

    fn entry(name: &str, desc: &str, extra: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {desc}\nupdated: 2026-10-20\n{extra}---\nBody of {name}.\n"
        )
    }

    const NOW: i64 = 1_792_800_000; // 2026-10-24

    #[test]
    fn sync_adds_updates_and_removes_files() {
        let d = tmp("sync");
        write(
            &d,
            "work/a.md",
            &entry("staging-site", "The staging site is acme.example.com", ""),
        );
        write(
            &d,
            "work/b.md",
            &entry("deploy-day", "Deploys happen on Tuesdays", ""),
        );
        write(&d, "work/INDEX.md", "index file, skipped");
        write(&d, "_trash/old.md", &entry("old", "Old stuff", ""));
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(
            sync(&mut ix, &cfg).unwrap(),
            SyncReport {
                added: 2,
                updated: 0,
                removed: 0
            }
        );
        assert_eq!(sync(&mut ix, &cfg).unwrap(), SyncReport::default());
        write(
            &d,
            "work/a.md",
            &entry(
                "staging-site",
                "The staging site is now globex.example.com",
                "",
            ),
        );
        fs::remove_file(d.join("work/b.md")).unwrap();
        let rep = sync(&mut ix, &cfg).unwrap();
        assert_eq!((rep.updated, rep.removed), (1, 1));
        let hits = search(
            &ix,
            &cfg,
            "which staging site",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].line.contains("globex.example.com"),
            "{}",
            hits[0].line
        );
        assert!(
            search(&ix, &cfg, "deploys tuesday", Path::new("/tmp"), NOW, 1_500)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn search_finds_the_matching_fact_first() {
        let d = tmp("recall");
        let facts = [
            (
                "demo-site",
                "Client demos run on acme.example.com, not globex.example.com",
            ),
            (
                "no-exclamations",
                "Never use exclamation marks in any writing",
            ),
            (
                "commit-style",
                "Commit subjects start with Add, Fix, Update or Remove",
            ),
            ("monday-off", "Sam does not work on Mondays"),
            ("bug-reports", "File plugin bug reports in Jira, not GitHub"),
            (
                "gemini-judge",
                "The benchmark judge is gemini-3.5-flash-lite",
            ),
        ];
        for (n, desc) in facts {
            write(&d, &format!("{n}.md"), &entry(n, desc, ""));
        }
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let cases = [
            (
                "where should I demonstrate this client feature?",
                "demo-site",
            ),
            ("how should I write the commit message?", "commit-style"),
            ("can we schedule it for Monday", "monday-off"),
            ("where do I file this plugin bug", "bug-reports"),
            ("which judge model does the benchmark use", "gemini-judge"),
        ];
        for (q, want) in cases {
            let hits = search(&ix, &cfg, q, Path::new("/tmp"), NOW, 1_500).unwrap();
            assert_eq!(
                hits.first().map(|h| h.name.as_str()),
                Some(want),
                "query: {q}"
            );
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn short_fact_with_matching_title_beats_a_long_body() {
        let d = tmp("long-body");
        let filler = "Talked about where client demos run and which site each demo should run on. ";
        let mut body = String::new();
        while body.len() < 20_000 {
            body.push_str(filler);
        }
        write(
            &d,
            "notes.md",
            &format!(
                "---\nname: weekly-notes\ndescription: Notes from the support shifts\nupdated: 2026-10-20\n---\n{body}\n"
            ),
        );
        write(
            &d,
            "demo.md",
            &entry(
                "client-demo-site",
                "Use acme.example.com for every client demo",
                "",
            ),
        );
        write(
            &d,
            "marks.md",
            &entry(
                "no-exclamations",
                "Never use exclamation marks in any writing",
                "",
            ),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let hits = search(
            &ix,
            &cfg,
            "where do client demos run?",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert_eq!(
            hits.first().map(|h| h.name.as_str()),
            Some("client-demo-site"),
            "{:?}",
            hits.iter().map(|h| &h.name).collect::<Vec<_>>()
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn capitalized_words_do_not_outrank_a_text_match() {
        let d = tmp("names");
        write(
            &d,
            "rule.md",
            &entry(
                "jira-comments",
                "post a jira comment only on an explicit command",
                "",
            ),
        );
        write(
            &d,
            "notes.md",
            &entry(
                "watch-notes",
                "Sam said the Jira board and the Jira watch need review",
                "",
            ),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let hits = search(
            &ix,
            &cfg,
            "should I post the Jira comment?",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert_eq!(hits.first().map(|h| h.name.as_str()), Some("jira-comments"));
        fs::remove_dir_all(&d).unwrap();
    }

    /// Fifty strong zebra matches, then one public fact that mentions "codes"
    /// once, far down the ranking.
    fn floor_store(name: &str, zebra_desc_tail: &str, zebra_extra: &str) -> PathBuf {
        let d = tmp(name);
        for i in 0..50 {
            write(
                &d,
                &format!("p{i}.md"),
                &format!(
                    "---\nname: zebra-codes-{i}\ndescription: zebra crossing codes, part {i}{zebra_desc_tail}\n{zebra_extra}---\nMore zebra crossing codes.\n"
                ),
            );
        }
        let filler = "Notes about the release, the build and the weekly plan. ".repeat(10);
        write(
            &d,
            "public.md",
            &format!(
                "---\nname: weekly-notes\ndescription: Notes from the week\n---\n{filler} One line mentions codes.\n"
            ),
        );
        d
    }

    #[test]
    fn weak_matches_far_below_the_top_are_dropped() {
        // Every zebra line is over the budget, so only the floor keeps the
        // weak public match out.
        let tail = format!(". {}", "padding ".repeat(1_000));
        let d = floor_store("floor", tail.trim_end(), "");
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let hits = search(
            &ix,
            &cfg,
            "zebra crossing codes",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert!(
            hits.is_empty(),
            "{:?}",
            hits.iter().map(|h| &h.name).collect::<Vec<_>>()
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn floor_counts_only_facts_the_caller_may_see() {
        let d = floor_store("floor-private", "", "privacy: private\n");
        let root = d.join("app");
        let cfg = cfg_for(&d, &[root.to_str().unwrap()]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        // A question: as a bare status line, the 50 zebra facts that tie
        // for the best match would give nothing.
        let names = |cwd: &Path| -> Vec<String> {
            search(
                &ix,
                &cfg,
                "what are the zebra crossing codes?",
                cwd,
                NOW,
                1_500,
            )
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect()
        };
        // From a public cwd, the private facts must not change the result:
        // it matches a store that holds only the public fact.
        let alone = tmp("floor-public-alone");
        fs::copy(d.join("public.md"), alone.join("public.md")).unwrap();
        let alone_cfg = cfg_for(&alone, &[]);
        let mut alone_ix = open_at(":memory:").unwrap();
        sync(&mut alone_ix, &alone_cfg).unwrap();
        let expected: Vec<String> = search(
            &alone_ix,
            &alone_cfg,
            "what are the zebra crossing codes?",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
        assert_eq!(names(Path::new("/tmp")), expected);
        fs::remove_dir_all(&alone).unwrap();
        let inside = names(&root.join("src"));
        assert!(!inside.is_empty());
        assert!(!inside.contains(&"weekly-notes".to_string()), "{inside:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    fn gate_store(name: &str) -> (PathBuf, Config, Index) {
        let d = tmp(name);
        write(
            &d,
            "a.md",
            &entry(
                "lint-before-commit",
                "Run the linter before every commit",
                "",
            ),
        );
        write(
            &d,
            "b.md",
            &entry(
                "deploy-day",
                "Deploys happen on Tuesdays after the review",
                "",
            ),
        );
        write(
            &d,
            "c.md",
            &entry(
                "staging-site",
                "The staging site is staging.example.com",
                "",
            ),
        );
        for i in 0..40 {
            write(
                &d,
                &format!("n{i}.md"),
                &entry(
                    &format!("team-note-{i}"),
                    &format!("A team note about the weekly plan, part {i}"),
                    "",
                ),
            );
        }
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        (d, cfg, ix)
    }

    fn gate_names(ix: &Index, cfg: &Config, q: &str) -> Vec<String> {
        search(ix, cfg, q, Path::new("/tmp"), NOW, 1_500)
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect()
    }

    #[test]
    fn gate_keeps_a_fact_the_prompt_is_about() {
        let (d, cfg, ix) = gate_store("gate-keep");
        assert_eq!(
            gate_names(&ix, &cfg, "should I run the linter before this commit?")[0],
            "lint-before-commit"
        );
        assert_eq!(
            gate_names(&ix, &cfg, "which day do deploys happen?")[0],
            "deploy-day"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn gate_sends_nothing_for_a_pasted_log() {
        let (d, cfg, ix) = gate_store("gate-log");
        let log = "user@host % ./build.sh --release\nCompiling widgets v0.3.1\nFinished release in 41s\nwrote /var/tmp/out/widgets.tar.gz (2.1 MB)\nnote: the team plan cache was refreshed";
        let got = gate_names(&ix, &cfg, log);
        assert!(got.is_empty(), "{got:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn gate_needs_a_rare_word_when_the_prompt_has_one() {
        let (d, cfg, ix) = gate_store("gate-one-word");
        let got = gate_names(&ix, &cfg, "team");
        assert!(got.is_empty(), "{got:?}");
        assert_eq!(gate_names(&ix, &cfg, "linter"), vec!["lint-before-commit"]);
        let got = gate_names(&ix, &cfg, "thanks, looks good");
        assert!(got.is_empty(), "{got:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn gate_sends_nothing_for_a_status_line() {
        let (d, cfg, ix) = gate_store("gate-status");
        for (n, desc) in [
            ("next-batch-window", "The next batch export runs at noon"),
            (
                "plugin-update-review",
                "Each plugin update gets a second review",
            ),
        ] {
            write(&d, &format!("{n}.md"), &entry(n, desc, ""));
        }
        let mut ix = ix;
        sync(&mut ix, &cfg).unwrap();
        for q in [
            "done plugin update, next batch",
            "okay done plugin udapte nad next",
            // A topic word among the status words.
            "done plugin update, next batch of reviews",
            // Words every team note shares.
            "weekly plan done",
        ] {
            let got = gate_names(&ix, &cfg, q);
            assert!(got.is_empty(), "{q:?}: {got:?}");
        }
        for (q, want) in [
            ("when does the next batch export run?", "next-batch-window"),
            (
                "how do I check the next plugin update",
                "plugin-update-review",
            ),
            ("run the linter before this commit", "lint-before-commit"),
            (
                "should I run the linter before this commit?",
                "lint-before-commit",
            ),
        ] {
            let got = gate_names(&ix, &cfg, q);
            assert_eq!(
                got.first().map(String::as_str),
                Some(want),
                "{q:?}: {got:?}"
            );
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn gate_works_on_a_store_with_one_fact() {
        let d = tmp("gate-tiny");
        write(
            &d,
            "a.md",
            &entry(
                "staging-site",
                "The staging site is staging.example.com",
                "",
            ),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        assert_eq!(
            gate_names(&ix, &cfg, "where is the staging site?"),
            vec!["staging-site"]
        );
        // A short request whose other words no fact has yet.
        assert_eq!(
            gate_names(&ix, &cfg, "draft the staging site reply"),
            vec!["staging-site"]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn speaker_words_stay_search_terms() {
        let d = tmp("speaker-words");
        write(
            &d,
            "a.md",
            &entry("pr-titles", "PR titles use sentence case", ""),
        );
        write(
            &d,
            "b.md",
            &entry(
                "fix-commits",
                "Fix commits name the PR fix in the subject",
                "",
            ),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let names = |q: &str| -> Vec<String> {
            search(&ix, &cfg, q, Path::new("/tmp"), NOW, 1_500)
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(
            names("how do I fix the title of this PR?")
                .first()
                .map(String::as_str),
            Some("pr-titles")
        );
        assert!(
            names("is this a fact?").is_empty(),
            "{:?}",
            names("is this a fact?")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn prompt_without_search_terms_gets_no_facts() {
        let d = tmp("no-terms");
        write(
            &d,
            "a.md",
            &entry("ok-checks", "Run the ok checks before release", ""),
        );
        write(
            &d,
            "b.md",
            &entry("deploy-day", "Deploys happen on Tuesdays", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        for q in ["is this ok?", "", "what is this about?"] {
            let hits = search(&ix, &cfg, q, Path::new("/tmp"), NOW, 1_500).unwrap();
            assert!(
                hits.is_empty(),
                "{q:?}: {:?}",
                hits.iter().map(|h| &h.name).collect::<Vec<_>>()
            );
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rows_from_an_older_format_are_reindexed_once() {
        let d = tmp("format");
        write(
            &d,
            "a.md",
            &entry("pr-titles", "PR titles use sentence case", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let db = d.join("ix.sqlite");
        let db = db.to_str().unwrap();
        {
            let mut ix = open_at(db).unwrap();
            assert_eq!(sync(&mut ix, &cfg).unwrap().added, 1);
            ix.conn.execute_batch("PRAGMA user_version = 0").unwrap();
        }
        let mut ix = open_at(db).unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap().updated, 1);
        assert_eq!(sync(&mut ix, &cfg).unwrap(), SyncReport::default());
        drop(ix);
        let mut ix = open_at(db).unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap(), SyncReport::default());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_index_from_before_aliases_gets_the_column_and_a_reindex() {
        let d = tmp("old-table");
        write(
            &d,
            "a.md",
            &entry(
                "pr-titles",
                "PR titles use sentence case",
                "aliases: [pull request names]\n",
            ),
        );
        let cfg = cfg_for(&d, &[]);
        let db = d.join("ix.sqlite");
        let db = db.to_str().unwrap();
        {
            let ix = Index::open(db).unwrap();
            ix.conn
                .execute_batch(&format!(
                    "{}; PRAGMA user_version = 1;",
                    META.replace(",\n    aliases TEXT NOT NULL DEFAULT ''", "")
                ))
                .unwrap();
        }
        let mut ix = open_at(db).unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap().added, 1);
        assert_eq!(
            gate_names(&ix, &cfg, "how should pull request names look?"),
            vec!["pr-titles"]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn body_cap_cuts_at_a_word_boundary() {
        assert_eq!(cap_at_word("alpha beta gamma", 8), "alpha");
        assert_eq!(cap_at_word("alpha beta gamma", 10), "alpha beta");
        assert_eq!(cap_at_word("alpha beta", 100), "alpha beta");
        assert_eq!(cap_at_word("abcdefgh", 4), "abcd");
        assert_eq!(cap_at_word("héllo wörld", 7), "héllo");
    }

    #[test]
    fn private_and_replaced_facts_are_filtered() {
        let d = tmp("filter");
        write(
            &d,
            "p.md",
            &entry(
                "secret-plan",
                "The secret plan is launch day",
                "privacy: private\n",
            ),
        );
        write(
            &d,
            "r.md",
            &entry(
                "old-plan",
                "The old plan was launch day",
                "replaced_by: secret-plan\n",
            ),
        );
        let private_root = d.join("app");
        let cfg = cfg_for(&d, &[private_root.to_str().unwrap()]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        assert!(
            search(&ix, &cfg, "launch day plan", Path::new("/tmp"), NOW, 1_500)
                .unwrap()
                .is_empty()
        );
        let inside = search(
            &ix,
            &cfg,
            "launch day plan",
            &private_root.join("src"),
            NOW,
            1_500,
        )
        .unwrap();
        assert_eq!(
            inside.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            vec!["secret-plan"]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pinned_facts_and_stale_tags() {
        let d = tmp("pinned");
        write(
            &d,
            "a.md",
            "---\nname: always\ndescription: Always say this\nupdated: 2026-01-01\npinned: true\n---\n",
        );
        write(&d, "b.md", &entry("sometimes", "Only when asked", ""));
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let hits = pinned(&ix, &cfg, Path::new("/tmp"), NOW, 1_500).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].line.contains("verify before use"),
            "{}",
            hits[0].line
        );
        let block = render_block("Pinned:", &hits, "dotonce");
        assert!(
            block.starts_with("<dotonce>\nPinned:\n- **always**"),
            "{block}"
        );
        assert!(block.contains("dotonce save"));
        assert!(block.ends_with("</dotonce>"));
        fs::remove_dir_all(&d).unwrap();
    }

    fn pinned_entry(name: &str, desc: &str, updated: &str, extra: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {desc}\nupdated: {updated}\npinned: true\n{extra}---\n"
        )
    }

    #[test]
    fn pinned_facts_respect_privacy() {
        let d = tmp("pinned-private");
        write(
            &d,
            "a.md",
            &pinned_entry(
                "secret",
                "The secret plan",
                "2026-10-20",
                "privacy: private\n",
            ),
        );
        write(
            &d,
            "b.md",
            &pinned_entry("open", "The open plan", "2026-10-19", ""),
        );
        let root = d.join("app");
        let cfg = cfg_for(&d, &[root.to_str().unwrap()]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let names = |cwd: &Path| -> Vec<String> {
            pinned(&ix, &cfg, cwd, NOW, 1_500)
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(names(Path::new("/tmp")), vec!["open"]);
        assert_eq!(names(&root.join("src")), vec!["secret", "open"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn at_most_six_facts_come_back() {
        let d = tmp("max-facts");
        for i in 0..8 {
            write(
                &d,
                &format!("f{i}.md"),
                &pinned_entry(&format!("fact-{i}"), "Short rule", "2026-10-20", ""),
            );
        }
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        assert_eq!(
            pinned(&ix, &cfg, Path::new("/tmp"), NOW, 100_000)
                .unwrap()
                .len(),
            MAX_FACTS
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn budget_skips_an_oversized_fact_and_counts_the_block_text() {
        let d = tmp("budget");
        let long = "word ".repeat(400);
        write(
            &d,
            "big.md",
            &pinned_entry("big", long.trim(), "2026-10-22", ""),
        );
        write(
            &d,
            "small.md",
            &pinned_entry("small", "Short rule", "2026-10-21", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let names = |budget: usize| -> Vec<String> {
            pinned(&ix, &cfg, Path::new("/tmp"), NOW, budget)
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(names(300), vec!["small"]);
        let hits = pinned(&ix, &cfg, Path::new("/tmp"), NOW, 300).unwrap();
        assert!(estimate_tokens(&render_block("Pinned facts:", &hits, "dotonce")) <= 300);
        let small_line = estimate_tokens(&hits[0].line);
        assert!(names(small_line + 10).is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    /// A small store like a new user's: a few facts on different topics.
    fn small_store(name: &str) -> (PathBuf, Config, Index) {
        let d = tmp(name);
        for (n, desc, extra) in [
            (
                "session-keys",
                "Garden service session keys expire after one hour; renew them at /keys/renew",
                "",
            ),
            (
                "schema-changes",
                "Schema changes live in db/changes; never edit one that already ran",
                "",
            ),
            (
                "request-quota",
                "The garden service allows 100 calls a minute per key and answers 429 above that",
                "",
            ),
            (
                "release-tags",
                "Releases are tagged vX.Y.Z on the trunk branch",
                "",
            ),
            (
                "branch-names",
                "Branch names look like fix/seed-picker or feat/rain-gauge",
                "",
            ),
            (
                "paging",
                "Page the duty gardener in #garden-duty for a broken pump",
                "aliases: [on duty, pager]\n",
            ),
        ] {
            write(&d, &format!("{n}.md"), &entry(n, desc, extra));
        }
        for i in 0..6 {
            write(
                &d,
                &format!("tour-{i}.md"),
                &entry(
                    &format!("garden-tour-{i}"),
                    &format!("Garden tour stop {i} has a bench and a map"),
                    "",
                ),
            );
        }
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        (d, cfg, ix)
    }

    #[test]
    fn short_request_on_a_small_store_finds_its_fact() {
        let (d, cfg, ix) = small_store("small-request");
        assert_eq!(
            gate_names(&ix, &cfg, "add a schema change for the seed table"),
            vec!["schema-changes"]
        );
        assert_eq!(gate_names(&ix, &cfg, "tag a release"), vec!["release-tags"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn status_lines_get_nothing_even_when_a_fact_matches_them() {
        let (d, cfg, ix) = small_store("small-status");
        for q in [
            "fixed the seed picker, moving on",
            "schema change ran fine, next one",
            "release tagged",
            "release tgaged dnoe",
            "keys renewed, what's next",
        ] {
            let got = gate_names(&ix, &cfg, q);
            assert!(got.is_empty(), "{q:?}: {got:?}");
        }
        assert_eq!(
            gate_names(&ix, &cfg, "fix the seed picker branch")
                .first()
                .map(String::as_str),
            Some("branch-names")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn trouble_reports_are_not_status_lines() {
        let (d, cfg, ix) = small_store("small-trouble");
        for (q, want) in [
            ("found a bug in the request quota", "request-quota"),
            ("found a 429 above the request quota", "request-quota"),
            ("the schema change crashed", "schema-changes"),
            ("release tagging failed", "release-tags"),
            ("the session keys expired again", "session-keys"),
            ("renewing session keys timed out", "session-keys"),
            ("our session keys got rejected", "session-keys"),
            ("schema change broke after the update", "schema-changes"),
        ] {
            let got = gate_names(&ix, &cfg, q);
            assert_eq!(
                got.first().map(String::as_str),
                Some(want),
                "{q:?}: {got:?}"
            );
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_lone_word_needs_to_pick_out_a_few_facts_by_name() {
        let (d, cfg, ix) = small_store("small-lone");
        // Six facts carry "garden" in their name, so it says too little.
        let got = gate_names(&ix, &cfg, "garden?");
        assert!(got.is_empty(), "{got:?}");
        assert_eq!(gate_names(&ix, &cfg, "paging"), vec!["paging"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn aliases_match_like_the_title() {
        let (d, cfg, ix) = small_store("small-alias");
        assert_eq!(
            gate_names(&ix, &cfg, "who is on duty tonight?"),
            vec!["paging"]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pasted_material_finds_a_fact_by_its_rare_words() {
        let (d, cfg, ix) = small_store("small-paste");
        let log = "HTTP/1.1 429 Too Many Requests\nRetry-After: 37\n\n{\"error\":\"quota\",\"message\":\"over the request quota for this key\"}\n\nwhy is it rejecting us?";
        assert_eq!(gate_names(&ix, &cfg, log), vec!["request-quota"]);
        let code = "-- db/changes/20260611_add_beds.sql (already applied)\nCREATE TABLE beds (\n  id serial PRIMARY KEY\n);\n\nadd a width column to this";
        assert_eq!(gate_names(&ix, &cfg, code), vec!["schema-changes"]);
        let unrelated = "fn main() {\n    let xs = vec![3, 1, 2];\n    println!(\"{:?}\", xs.iter().max());\n}\n\nwhy does this print Some?";
        let got = gate_names(&ix, &cfg, unrelated);
        assert!(got.is_empty(), "{got:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn aliases_do_not_water_down_a_pasted_match() {
        let d = tmp("alias-paste");
        write(
            &d,
            "a.md",
            &entry(
                "schema-changes",
                "Schema changes live in db/changes; never edit one that already ran",
                "aliases: [migrations, upgrade scripts, version steps, layout history, evolution]\n",
            ),
        );
        for i in 0..6 {
            write(
                &d,
                &format!("tour-{i}.md"),
                &entry(
                    &format!("garden-tour-{i}"),
                    &format!("Garden tour stop {i} has a bench and a map"),
                    "",
                ),
            );
        }
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let code = "-- db/changes/20260611_add_beds.sql (already applied)\nCREATE TABLE beds (\n  id serial PRIMARY KEY\n);\n\nadd a width column to this";
        assert_eq!(gate_names(&ix, &cfg, code), vec!["schema-changes"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn prose_paste_needs_a_name_word_to_match() {
        let (d, cfg, ix) = small_store("small-prose");
        // Shares "renew", "service" and "branch" with titles, but no fact name.
        let essay = "Tree care is mostly patience. Renew the mulch each spring, keep the service paths clear,\nand cut a dead branch back to healthy wood before the sap rises. Most trees need\nlittle else, and the rest is watching what the weather does over a few seasons.\n\ntl;dr?";
        let got = gate_names(&ix, &cfg, essay);
        assert!(got.is_empty(), "{got:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn at_most_three_facts_come_back_for_a_prompt() {
        let (d, cfg, ix) = small_store("small-cap");
        let got = gate_names(&ix, &cfg, "which garden tour stop has a bench and a map?");
        assert!(!got.is_empty() && got.len() <= MAX_PROMPT_FACTS, "{got:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_weaker_title_match_stays_when_the_text_search_ranks_it_high() {
        let d = tmp("both-rankers");
        write(
            &d,
            "a.md",
            &entry(
                "reply-layout",
                "Format each reply with the code block last",
                "",
            ),
        );
        write(
            &d,
            "b.md",
            "---\nname: block-labels\ndescription: Never label a code block with a language\nupdated: 2026-10-20\n---\nThis holds for the code block in every reply, whatever the format.\n",
        );
        for (n, desc) in [
            ("reply-tone", "Keep each reply short and plain"),
            ("format-dates", "Format dates as 2026-10-20"),
            ("code-owners", "Code owners approve each pull request"),
        ] {
            write(&d, &format!("{n}.md"), &entry(n, desc, ""));
        }
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        let got = gate_names(&ix, &cfg, "how should I format code blocks in a reply?");
        assert!(got.contains(&"reply-layout".to_string()), "{got:?}");
        assert!(got.contains(&"block-labels".to_string()), "{got:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn save_writes_a_new_file_and_never_overwrites() {
        let d = tmp("save");
        let cfg = cfg_for(&d, &[]);
        let f = Fact {
            name: "note".into(),
            description: "A note".into(),
            ..Fact::default()
        };
        let a = save(&cfg, None, &f).unwrap();
        let b = save(&cfg, None, &f).unwrap();
        assert_ne!(a, b);
        assert!(b.to_string_lossy().ends_with("note-2.md"));
        assert_eq!(
            parse(&fs::read_to_string(&a).unwrap()).description,
            "A note"
        );
        assert!(save(&cfg, Some("missing"), &f).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn save_keeps_the_file_inside_the_store() {
        let root = tmp("save-escape");
        let d = root.join("store");
        let cfg = cfg_for(&d, &[]);
        for name in ["../escaped", "/abs/escaped", "a/../../escaped"] {
            let f = Fact {
                name: name.into(),
                description: "Should stay inside".into(),
                ..Fact::default()
            };
            let p = save(&cfg, None, &f).unwrap();
            assert_eq!(p.parent(), Some(d.as_path()), "{name}: {}", p.display());
        }
        assert!(!root.join("escaped.md").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_with_an_empty_name_is_indexed() {
        let d = tmp("save-empty");
        let cfg = cfg_for(&d, &[]);
        let f = Fact {
            description: "zebra crossing codes".into(),
            ..Fact::default()
        };
        let p = save(&cfg, None, &f).unwrap();
        assert_eq!(p.parent(), Some(d.as_path()));
        assert!(p.file_name().is_some_and(|n| n != ".md"), "{}", p.display());
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap().added, 1);
        let hits = search(
            &ix,
            &cfg,
            "zebra crossing codes",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn same_second_same_size_edit_is_picked_up() {
        let d = tmp("mtime");
        let p = d.join("a.md");
        let base = UNIX_EPOCH + std::time::Duration::from_secs(1_792_800_000);
        let set = |src: &str, t: std::time::SystemTime| {
            fs::write(&p, src).unwrap();
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        set(&entry("site", "The site is aaaa.test", ""), base);
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap().added, 1);
        set(
            &entry("site", "The site is bbbb.test", ""),
            base + std::time::Duration::from_millis(5),
        );
        assert_eq!(sync(&mut ix, &cfg).unwrap().updated, 1);
        let hits = search(&ix, &cfg, "which site", Path::new("/tmp"), NOW, 1_500).unwrap();
        assert!(hits[0].line.contains("bbbb.test"), "{}", hits[0].line);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn failed_update_keeps_the_old_entry() {
        let d = tmp("atomic");
        write(
            &d,
            "a.md",
            &entry("staging-site", "The staging site is acme.example.com", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        ix.conn
            .execute_batch(
                "CREATE TRIGGER fail_meta BEFORE INSERT ON fact_meta WHEN NEW.description LIKE '%globex%'
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();
        write(
            &d,
            "a.md",
            &entry(
                "staging-site",
                "The staging site is now globex.example.com",
                "",
            ),
        );
        assert!(sync(&mut ix, &cfg).is_err());
        let hits = search(
            &ix,
            &cfg,
            "which staging site",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].line.contains("acme.example.com"),
            "{}",
            hits[0].line
        );
        let sessions: i64 = ix
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn one_failed_update_does_not_stop_the_sync() {
        let d = tmp("keep-going");
        write(
            &d,
            "a.md",
            &entry("staging-site", "The staging site is acme.example.com", ""),
        );
        write(
            &d,
            "c.md",
            &entry("deploy-day", "Deploys happen on Tuesdays", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        sync(&mut ix, &cfg).unwrap();
        ix.conn
            .execute_batch(
                "CREATE TRIGGER fail_meta BEFORE INSERT ON fact_meta WHEN NEW.description LIKE '%globex%'
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();
        write(
            &d,
            "a.md",
            &entry(
                "staging-site",
                "The staging site is now globex.example.com",
                "",
            ),
        );
        for i in 0..5 {
            write(
                &d,
                &format!("b{i}.md"),
                &entry(&format!("zebra-{i}"), "zebra codes", ""),
            );
        }
        fs::remove_file(d.join("c.md")).unwrap();
        let err = sync(&mut ix, &cfg).unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");
        let names = |q: &str| -> Vec<String> {
            search(&ix, &cfg, q, Path::new("/tmp"), NOW, 1_500)
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect()
        };
        let zebras: i64 = ix
            .conn
            .query_row(
                "SELECT COUNT(*) FROM fact_meta WHERE name LIKE 'zebra-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(zebras, 5);
        assert_eq!(names("zebra codes").len(), MAX_PROMPT_FACTS);
        assert!(names("deploys tuesday").is_empty());
        let old = search(
            &ix,
            &cfg,
            "which staging site",
            Path::new("/tmp"),
            NOW,
            1_500,
        )
        .unwrap();
        assert!(old[0].line.contains("acme.example.com"), "{}", old[0].line);
        ix.conn.execute_batch("DROP TRIGGER fail_meta").unwrap();
        assert_eq!(
            sync(&mut ix, &cfg).unwrap(),
            SyncReport {
                added: 0,
                updated: 1,
                removed: 0
            }
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sync_waits_for_another_writer() {
        let d = tmp("busy");
        write(
            &d,
            "a.md",
            &entry("staging-site", "The staging site is acme.example.com", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let db = d.join("ix.sqlite").to_string_lossy().into_owned();
        let mut ix = open_at(&db).unwrap();
        let timeout: i64 = ix
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 2_000);
        sync(&mut ix, &cfg).unwrap();
        write(
            &d,
            "a.md",
            &entry(
                "staging-site",
                "The staging site is now globex.example.com",
                "",
            ),
        );
        let other = open_at(&db).unwrap();
        other
            .conn
            .execute_batch("BEGIN IMMEDIATE; UPDATE fact_meta SET pinned = 0;")
            .unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            other.conn.execute_batch("COMMIT").unwrap();
        });
        let started = std::time::Instant::now();
        let rep = sync(&mut ix, &cfg);
        holder.join().unwrap();
        assert_eq!(
            rep.unwrap(),
            SyncReport {
                added: 0,
                updated: 1,
                removed: 0
            }
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(250));
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_are_not_followed() {
        let d = tmp("symlink");
        write(
            &d,
            "work/a.md",
            &entry("staging-site", "The staging site", ""),
        );
        std::os::unix::fs::symlink(&d, d.join("work/loop")).unwrap();
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(
            sync(&mut ix, &cfg).unwrap(),
            SyncReport {
                added: 1,
                updated: 0,
                removed: 0
            }
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_are_not_read() {
        let root = tmp("symlink-file");
        let d = root.join("store");
        write(&d, "a.md", &entry("staging-site", "The staging site", ""));
        write(
            &root,
            "outside.md",
            &entry("outside", "zebra crossing codes", ""),
        );
        std::os::unix::fs::symlink(root.join("outside.md"), d.join("linked.md")).unwrap();
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap().added, 1);
        assert!(
            search(&ix, &cfg, "zebra codes", Path::new("/tmp"), NOW, 1_500)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_file_is_skipped_and_its_old_entry_removed() {
        let d = tmp("unreadable");
        write(
            &d,
            "good.md",
            &entry("staging-site", "The staging site is acme.example.com", ""),
        );
        write(
            &d,
            "bad.md",
            &entry("zebra-codes", "zebra crossing codes", ""),
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap().added, 2);
        fs::write(
            d.join("bad.md"),
            b"---\nname: zebra\n---\n\xff\xfe zebra crossing\n",
        )
        .unwrap();
        let rep = sync(&mut ix, &cfg).unwrap();
        assert_eq!(
            rep,
            SyncReport {
                added: 0,
                updated: 0,
                removed: 1
            }
        );
        let names: Vec<String> = search(&ix, &cfg, "zebra crossing", Path::new("/tmp"), NOW, 1_500)
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert!(!names.contains(&"zebra-codes".to_string()), "{names:?}");
        let hits = search(&ix, &cfg, "staging site", Path::new("/tmp"), NOW, 1_500).unwrap();
        assert_eq!(hits.len(), 1);

        let mut fresh = open_at(":memory:").unwrap();
        assert_eq!(sync(&mut fresh, &cfg).unwrap().added, 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn broken_header_files_are_not_indexed() {
        let d = tmp("broken");
        write(
            &d,
            "x.md",
            "---\nname: leaky\nprivacy: private\ndescription: zebra crossing codes\n\nzebra body\n",
        );
        let cfg = cfg_for(&d, &[]);
        let mut ix = open_at(":memory:").unwrap();
        assert_eq!(sync(&mut ix, &cfg).unwrap(), SyncReport::default());
        assert!(
            search(&ix, &cfg, "zebra crossing", Path::new("/tmp"), NOW, 1_500)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(&d).unwrap();
    }
}
