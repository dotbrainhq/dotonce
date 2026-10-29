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
/// A fact scoring below this share of the best match is noise, not a weaker
/// hit. The best match is the best fact the caller may see, so a private
/// match can't push public facts under the floor.
const FLOOR: f64 = 0.35;
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
/// Words about the flow of work, not its topic: a status line like "done
/// plugin update, next batch" is made of them. They still count, but a
/// prompt whose known words are all of these needs a fact that covers
/// `WORKFLOW_COVER` of it, since any weaker match is on the status words alone.
const WORKFLOW: &[&str] = &[
    "update", "updated", "updates", "updating", "next", "done", "batch", "batches", "plugin",
    "plugins", "fix", "fixed", "fixes", "fixing", "check", "checked", "checks", "checking", "run",
    "ran", "running", "runs", "step", "steps", "task", "tasks", "start", "started", "finish",
    "finished", "now", "again", "later", "first", "last", "item", "items", "part", "work",
    "working", "change", "changes", "changed", "move", "moving", "keep", "try", "test", "tests",
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
    updated TEXT
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
const FORMAT: i64 = 1;

pub fn open_at(path: &str) -> Result<Index> {
    let ix = Index::open(path)?;
    ix.conn.execute_batch(META)?;
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
    let mut turns = vec![TurnIn {
        speaker: String::new(),
        text: format!("{}: {}", name.replace('-', " "), f.description),
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
            "INSERT OR REPLACE INTO fact_meta(doc_id, store, path, mtime, size, private, pinned, replaced, name, description, body, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
    if query::terms(query)
        .iter()
        .all(|t| FILLER.contains(&t.as_str()))
    {
        return Ok(Vec::new());
    }
    let opt = Options {
        fill: false,
        names: false,
        ..Options::default()
    };
    let private_ok = cfg.private_allowed(cwd);
    let mut top = None;
    let mut ids = Vec::new();
    for (sid, score, _) in rank(ix, query, USER, Some(now), &opt)? {
        let doc_id = ix.session(sid)?.doc_id;
        match hit_for(ix, &doc_id, now)? {
            Some((_, private)) if private_ok || !private => {}
            _ => continue,
        }
        if score < *top.get_or_insert(score) * FLOOR {
            break;
        }
        ids.push(doc_id);
    }
    let ids = gate(ix, query, ids, private_ok)?;
    let budget = budget.saturating_sub(BLOCK_RESERVE);
    take_within(ix, ids, private_ok, now, budget)
}

/// Share of the prompt's weight a fact's title must cover to count as a match.
const COVER: f64 = 0.3;
/// The best fact must cover this much of the prompt, or nothing is sent: a
/// prompt no fact is clearly about (a pasted log, a status line) gets no block.
const BEST_COVER: f64 = 0.5;
/// The best cover a prompt of workflow words alone needs (see `WORKFLOW`).
const WORKFLOW_COVER: f64 = 0.75;
/// Facts after the best must cover at least this share of what the best covers.
const NEAR_BEST: f64 = 0.4;
/// A prompt with one meaningful word only gets facts when that word is in at
/// most this share of fact titles (or in one title), so "looks" alone
/// matches nothing.
const RARE_SHARE: f64 = 0.03;
/// A prompt word no fact title has still counts toward the prompt's weight, at
/// this share of the rarest word's weight. A pasted log is mostly such words,
/// so a fact that matches its few known ones covers little of it.
const UNSEEN: f64 = 0.5;

/// How one prompt word weighs across fact titles.
struct Weight {
    idf: f64,
    rare: bool,
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

/// Keeps only facts whose name and description are about the prompt, not
/// facts that share a few common words with it. Each prompt word is weighted
/// by how rare it is across fact titles.
fn gate(ix: &Index, query: &str, ids: Vec<String>, private_ok: bool) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(ids);
    }
    let titles: HashMap<String, Vec<String>> = {
        let mut st = ix
            .conn
            .prepare("SELECT doc_id, name, description, private FROM fact_meta")?;
        st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter(|(_, _, _, private)| private_ok || !private)
        .map(|(id, name, desc, _)| (id, tokens(&format!("{name} {desc}"))))
        .collect()
    };
    let n = titles.len().max(1) as f64;
    let terms: Vec<String> = query::terms(query)
        .into_iter()
        .filter(|t| !FILLER.contains(&t.as_str()))
        .collect();
    let weights: Vec<Weight> = terms
        .iter()
        .map(|t| {
            let df = titles
                .values()
                .filter(|ws| ws.iter().any(|w| word_match(t, w)))
                .count() as f64;
            // Smoothed so a word every title has still counts on a small store.
            let idf = if df == 0.0 { 0.0 } else { (1.0 + n / df).ln() };
            Weight {
                idf,
                rare: df > 0.0 && df <= (n * RARE_SHARE).max(1.0),
            }
        })
        .collect();
    let seen: f64 = weights.iter().map(|w| w.idf).sum();
    if seen <= 0.0 {
        return Ok(Vec::new());
    }
    let unseen = weights.iter().filter(|w| w.idf == 0.0).count() as f64;
    let total = seen + unseen * UNSEEN * (1.0 + n).ln();
    let useful = weights.iter().filter(|w| w.idf > 0.0).count();
    if useful == 1 && !weights.iter().any(|w| w.rare) {
        return Ok(Vec::new());
    }
    let scored: Vec<(String, f64, usize)> = ids
        .into_iter()
        .filter_map(|id| {
            let ws = titles.get(&id)?;
            let (mut got, mut hits) = (0.0, 0);
            for (t, wt) in terms.iter().zip(&weights) {
                if wt.idf > 0.0 && ws.iter().any(|w| word_match(t, w)) {
                    got += wt.idf;
                    hits += 1;
                }
            }
            Some((id, got / total, hits))
        })
        .collect();
    let best = scored.iter().map(|s| s.1).fold(0.0, f64::max);
    let status_only = terms
        .iter()
        .zip(&weights)
        .all(|(t, w)| w.idf == 0.0 || WORKFLOW.contains(&t.as_str()));
    let need = if status_only {
        WORKFLOW_COVER
    } else {
        BEST_COVER
    };
    if best < need {
        return Ok(Vec::new());
    }
    let min_cover = COVER.max(best * NEAR_BEST);
    Ok(scored
        .into_iter()
        .filter(|(_, cover, hits)| *cover >= min_cover && *hits >= useful.min(2))
        .map(|(id, ..)| id)
        .collect())
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

/// The block ends with how to save a fact, run as `bin`: the command that
/// starts this dotonce, since it may not be on PATH.
pub fn render_block(title: &str, hits: &[FactHit], bin: &str) -> String {
    let mut s = format!("<dotonce>\n{title}\n");
    for h in hits {
        s.push_str(&h.line);
        s.push('\n');
    }
    s.push_str(&format!(
        "To keep something for next time, run: {bin} save \"<one line: the rule or fact>\" --source \"<where it came from>\""
    ));
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
            ("write the commit message", "commit-style"),
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
        let names = |cwd: &Path| -> Vec<String> {
            search(&ix, &cfg, "zebra crossing codes", cwd, NOW, 1_500)
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
            "zebra crossing codes",
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
    fn gate_sends_nothing_for_a_status_line_of_workflow_words() {
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
        ] {
            let got = gate_names(&ix, &cfg, q);
            assert!(got.is_empty(), "{q:?}: {got:?}");
        }
        assert_eq!(
            gate_names(&ix, &cfg, "when does the next batch export run?").first(),
            Some(&"next-batch-window".to_string())
        );
        assert_eq!(
            gate_names(&ix, &cfg, "should I run the linter before this commit?")[0],
            "lint-before-commit"
        );
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
            names("fix the title of this PR")
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
        let hits = search(&ix, &cfg, "acme", Path::new("/tmp"), NOW, 1_500).unwrap();
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
        assert_eq!(names("zebra codes").len(), 5);
        assert!(names("deploys tuesday").is_empty());
        let old = search(&ix, &cfg, "acme", Path::new("/tmp"), NOW, 1_500).unwrap();
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
