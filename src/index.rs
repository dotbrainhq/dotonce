use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params, params_from_iter, types::Value};

use crate::model::SessionDoc;
use crate::names::extract_names;
use crate::timeq::{Window, parse_ts};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions(
    sid INTEGER PRIMARY KEY,
    doc_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    ts INTEGER,
    ts_text TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS sessions_doc ON sessions(user_id, doc_id);
CREATE INDEX IF NOT EXISTS sessions_user_ts ON sessions(user_id, ts);
CREATE TABLE IF NOT EXISTS turns(
    id INTEGER PRIMARY KEY,
    sid INTEGER NOT NULL,
    turn_no INTEGER NOT NULL,
    speaker TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS turns_sid ON turns(sid, turn_no);
CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(speaker, text, content='turns', content_rowid='id', tokenize='porter unicode61');
CREATE VIRTUAL TABLE IF NOT EXISTS turns_tri USING fts5(text, content='turns', content_rowid='id', tokenize='trigram');
CREATE TABLE IF NOT EXISTS names(turn_id INTEGER NOT NULL, name TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS names_name ON names(name);
";

#[derive(Debug, Clone, Copy)]
pub enum Fts {
    Stem,
    Trigram,
}

impl Fts {
    fn table(self) -> &'static str {
        match self {
            Fts::Stem => "turns_fts",
            Fts::Trigram => "turns_tri",
        }
    }

    /// The speaker column gets no weight: a name in it says who talked, not what about.
    fn rank(self) -> &'static str {
        match self {
            Fts::Stem => "bm25(turns_fts, 0.0, 1.0)",
            Fts::Trigram => "bm25(turns_tri)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub doc_id: String,
    pub ts: Option<i64>,
    pub ts_text: Option<String>,
    pub turns: Vec<(i64, String, String)>,
}

/// Two Claude Code hooks may open and sync one index at once; the second
/// waits this long for the first to finish writing.
pub const BUSY_WAIT: Duration = Duration::from_millis(2_000);

pub struct Index {
    pub conn: Connection,
}

fn ids_vec(st: &mut rusqlite::Statement<'_>, vals: Vec<Value>) -> Result<Vec<i64>> {
    let rows = st.query_map(params_from_iter(vals), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

fn marks(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Switching a new file to WAL needs an exclusive lock, and SQLite returns
/// SQLITE_BUSY for it at once instead of calling the busy handler. So when
/// two processes open a new index together, the loser retries here.
fn set_wal(conn: &Connection) -> Result<()> {
    let start = Instant::now();
    loop {
        match conn.execute_batch("PRAGMA journal_mode=WAL;") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::DatabaseBusy && start.elapsed() < BUSY_WAIT =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            r => return Ok(r?),
        }
    }
}

impl Index {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open index at {path}"))?;
        conn.busy_timeout(BUSY_WAIT)?;
        set_wal(&conn)?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn reset(&self) -> Result<()> {
        self.conn.execute_batch(
            "DROP TABLE IF EXISTS names; DROP TABLE IF EXISTS turns_tri; DROP TABLE IF EXISTS turns_fts;
             DROP TABLE IF EXISTS turns; DROP TABLE IF EXISTS sessions;",
        )?;
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    /// Runs `f` so that all of its writes land or none do. Savepoints nest,
    /// so `f` may call `add_sessions` and `remove_session`. Outside any
    /// transaction it takes the write lock first, so waiting for another
    /// writer happens before `f` reads and can't end on a stale snapshot.
    pub fn atomic<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let (begin, done, undo) = if self.conn.is_autocommit() {
            ("BEGIN IMMEDIATE", "COMMIT", "ROLLBACK")
        } else {
            (
                "SAVEPOINT atomic",
                "RELEASE atomic",
                "ROLLBACK TO atomic; RELEASE atomic",
            )
        };
        self.conn.execute_batch(begin)?;
        let res = f(self).and_then(|v| {
            self.conn.execute_batch(done)?;
            Ok(v)
        });
        match res {
            Ok(v) => Ok(v),
            Err(e) => match self.conn.execute_batch(undo) {
                Ok(()) => Err(e),
                Err(re) => Err(e.context(format!("rollback failed: {re}"))),
            },
        }
    }

    pub fn add_sessions(&mut self, docs: &[SessionDoc]) -> Result<usize> {
        let tx = self.conn.savepoint()?;
        let mut added = 0;
        {
            let mut ins_session = tx.prepare_cached(
                "INSERT INTO sessions(doc_id, user_id, ts, ts_text) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(user_id, doc_id) DO NOTHING",
            )?;
            let mut ins_turn = tx.prepare_cached(
                "INSERT INTO turns(sid, turn_no, speaker, text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut ins_fts = tx.prepare_cached(
                "INSERT INTO turns_fts(rowid, speaker, text) VALUES (?1, ?2, ?3)",
            )?;
            let mut ins_tri =
                tx.prepare_cached("INSERT INTO turns_tri(rowid, text) VALUES (?1, ?2)")?;
            let mut ins_name =
                tx.prepare_cached("INSERT INTO names(turn_id, name) VALUES (?1, ?2)")?;
            for d in docs {
                let user = d.user_id.clone().unwrap_or_default();
                let ts = d.timestamp.as_deref().and_then(parse_ts);
                if ins_session.execute(params![d.id, user, ts, d.timestamp])? == 0 {
                    continue;
                }
                let sid = tx.last_insert_rowid();
                for (i, t) in d.turns.iter().enumerate() {
                    ins_turn.execute(params![sid, i as i64, t.speaker, t.text])?;
                    let tid = tx.last_insert_rowid();
                    ins_fts.execute(params![tid, t.speaker, t.text])?;
                    ins_tri.execute(params![tid, t.text])?;
                    for name in extract_names(&t.text) {
                        ins_name.execute(params![tid, name])?;
                    }
                }
                added += 1;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    pub fn match_turns(
        &self,
        fts: Fts,
        m: &str,
        user: &str,
        w: Option<Window>,
        limit: usize,
    ) -> Result<Vec<i64>> {
        let table = fts.table();
        let rank = fts.rank();
        let mut vals: Vec<Value> = vec![Value::Text(m.to_string()), Value::Text(user.to_string())];
        let window_sql = match w {
            Some(w) => {
                vals.push(Value::Integer(w.start));
                vals.push(Value::Integer(w.end));
                "AND s.ts BETWEEN ? AND ?"
            }
            None => "",
        };
        vals.push(Value::Integer(limit as i64));
        let sql = format!(
            "SELECT t.id FROM {table} JOIN turns t ON t.id = {table}.rowid JOIN sessions s ON s.sid = t.sid
             WHERE {table} MATCH ? AND s.user_id = ? {window_sql} ORDER BY {rank} LIMIT ?"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        ids_vec(&mut st, vals)
    }

    /// Does any turn of this user hold the term, after stemming?
    pub fn has_term(&self, term: &str, user: &str) -> Result<bool> {
        let mut st = self.conn.prepare_cached(
            "SELECT 1 FROM turns_fts JOIN turns t ON t.id = turns_fts.rowid JOIN sessions s ON s.sid = t.sid
             WHERE turns_fts MATCH ?1 AND s.user_id = ?2 LIMIT 1",
        )?;
        Ok(st.exists(params![format!("\"{term}\""), user])?)
    }

    /// Question terms that name a speaker of this user's turns. Exact match on the
    /// lowercased speaker words, so a stem like "assists" does not count as "assistant".
    pub fn speaker_terms(&self, terms: &[String], user: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached(
            "SELECT DISTINCT t.speaker FROM turns t JOIN sessions s ON s.sid = t.sid WHERE s.user_id = ?1",
        )?;
        let mut words: Vec<String> = Vec::new();
        for sp in st.query_map(params![user], |r| r.get::<_, String>(0))? {
            words.extend(crate::query::terms(&sp?));
        }
        Ok(terms
            .iter()
            .filter(|t| words.contains(t))
            .cloned()
            .collect())
    }

    /// Turns by these speakers, newest session first.
    pub fn speaker_turns(&self, m: &str, user: &str, limit: usize) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT t.id FROM turns_fts JOIN turns t ON t.id = turns_fts.rowid JOIN sessions s ON s.sid = t.sid
             WHERE turns_fts MATCH ?1 AND s.user_id = ?2 ORDER BY s.ts DESC, t.sid DESC, t.turn_no LIMIT ?3",
        )?;
        ids_vec(
            &mut st,
            vec![
                Value::Text(m.to_string()),
                Value::Text(user.to_string()),
                Value::Integer(limit as i64),
            ],
        )
    }

    pub fn name_turns(&self, terms: &[String], user: &str, limit: usize) -> Result<Vec<i64>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT n.turn_id FROM names n JOIN turns t ON t.id = n.turn_id JOIN sessions s ON s.sid = t.sid
             WHERE s.user_id = ? AND n.name IN ({}) GROUP BY n.turn_id ORDER BY COUNT(*) DESC, n.turn_id LIMIT ?",
            marks(terms.len())
        );
        let mut vals: Vec<Value> = vec![Value::Text(user.to_string())];
        vals.extend(terms.iter().map(|t| Value::Text(t.clone())));
        vals.push(Value::Integer(limit as i64));
        let mut st = self.conn.prepare_cached(&sql)?;
        ids_vec(&mut st, vals)
    }

    pub fn window_turns(&self, user: &str, w: Window, limit: usize) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT t.id FROM turns t JOIN sessions s ON s.sid = t.sid
             WHERE s.user_id = ? AND s.ts BETWEEN ? AND ? AND t.turn_no = 0 ORDER BY s.ts DESC LIMIT ?",
        )?;
        ids_vec(
            &mut st,
            vec![
                Value::Text(user.to_string()),
                Value::Integer(w.start),
                Value::Integer(w.end),
                Value::Integer(limit as i64),
            ],
        )
    }

    pub fn newest_first(&self, ids: &[i64]) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT t.id FROM turns t JOIN sessions s ON s.sid = t.sid WHERE t.id IN ({}) ORDER BY s.ts DESC, t.id",
            marks(ids.len())
        );
        let mut st = self.conn.prepare(&sql)?;
        ids_vec(&mut st, ids.iter().map(|i| Value::Integer(*i)).collect())
    }

    /// All of this user's sessions, newest first.
    pub fn sessions_newest_first(&self, user: &str) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT sid FROM sessions WHERE user_id = ?1 ORDER BY ts IS NULL, ts DESC, sid DESC",
        )?;
        ids_vec(&mut st, vec![Value::Text(user.to_string())])
    }

    pub fn turn_sessions(&self, ids: &[i64]) -> Result<HashMap<i64, (i64, i64)>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT id, sid, turn_no FROM turns WHERE id IN ({})",
            marks(ids.len())
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            params_from_iter(ids.iter().map(|i| Value::Integer(*i))),
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
                ))
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    pub fn session(&self, sid: i64) -> Result<SessionRow> {
        let (doc_id, ts, ts_text) = self.conn.query_row(
            "SELECT doc_id, ts, ts_text FROM sessions WHERE sid = ?1",
            params![sid],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )?;
        let mut st = self.conn.prepare_cached(
            "SELECT turn_no, speaker, text FROM turns WHERE sid = ?1 ORDER BY turn_no",
        )?;
        let turns = st
            .query_map(params![sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<(i64, String, String)>>>()?;
        Ok(SessionRow {
            doc_id,
            ts,
            ts_text,
            turns,
        })
    }

    /// Removes one session and all its turns from the tables and both full-text
    /// indexes. The full-text tables use external content, so each row is
    /// deleted with FTS5's 'delete' command and its original values.
    pub fn remove_session(&mut self, user: &str, doc_id: &str) -> Result<bool> {
        let tx = self.conn.savepoint()?;
        let sid: Option<i64> = tx
            .query_row(
                "SELECT sid FROM sessions WHERE user_id = ?1 AND doc_id = ?2",
                params![user, doc_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(sid) = sid else {
            return Ok(false);
        };
        let rows: Vec<(i64, String, String)> = {
            let mut st = tx.prepare("SELECT id, speaker, text FROM turns WHERE sid = ?1")?;
            st.query_map(params![sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, speaker, text) in rows {
            tx.execute(
                "INSERT INTO turns_fts(turns_fts, rowid, speaker, text) VALUES ('delete', ?1, ?2, ?3)",
                params![id, speaker, text],
            )?;
            tx.execute(
                "INSERT INTO turns_tri(turns_tri, rowid, text) VALUES ('delete', ?1, ?2)",
                params![id, text],
            )?;
            tx.execute("DELETE FROM names WHERE turn_id = ?1", params![id])?;
        }
        tx.execute("DELETE FROM turns WHERE sid = ?1", params![sid])?;
        tx.execute("DELETE FROM sessions WHERE sid = ?1", params![sid])?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TurnIn;

    fn doc(id: &str, user: &str, ts: &str, texts: &[&str]) -> SessionDoc {
        SessionDoc {
            id: id.to_string(),
            user_id: Some(user.to_string()),
            timestamp: Some(ts.to_string()),
            turns: texts
                .iter()
                .enumerate()
                .map(|(i, t)| TurnIn {
                    speaker: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    text: t.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn ingest_counts_sessions_and_skips_repeats() {
        let mut ix = Index::open(":memory:").unwrap();
        let d = doc(
            "s1",
            "u1",
            "2023-01-01T10:00:00+00:00",
            &["I adopted a dog named Rex", "Nice!"],
        );
        assert_eq!(ix.add_sessions(std::slice::from_ref(&d)).unwrap(), 1);
        assert_eq!(ix.add_sessions(&[d]).unwrap(), 0);
        let n: i64 = ix
            .conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn stem_and_trigram_and_names_find_turns_for_one_user_only() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "s1",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["We compared Hindsight with other tools"],
            ),
            doc(
                "s2",
                "u2",
                "2023-01-01T10:00:00+00:00",
                &["Hindsight came up again"],
            ),
        ])
        .unwrap();
        let stem = ix
            .match_turns(Fts::Stem, "\"compare\"", "u1", None, 10)
            .unwrap();
        assert_eq!(stem.len(), 1);
        let tri = ix
            .match_turns(
                Fts::Trigram,
                "\"hin\" OR \"ind\" OR \"ndi\" OR \"dis\" OR \"isg\" OR \"sgh\" OR \"ght\"",
                "u1",
                None,
                10,
            )
            .unwrap();
        assert_eq!(tri.len(), 1);
        let names = ix.name_turns(&["hindsight".to_string()], "u1", 10).unwrap();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn speaker_name_is_searchable() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[SessionDoc {
            id: "c1".to_string(),
            user_id: Some("u1".to_string()),
            timestamp: Some("2023-05-01T10:00:00+00:00".to_string()),
            turns: vec![
                TurnIn {
                    speaker: "Caroline".to_string(),
                    text: "I went to a support group".to_string(),
                },
                TurnIn {
                    speaker: "Melanie".to_string(),
                    text: "That sounds lovely".to_string(),
                },
            ],
        }])
        .unwrap();
        let hits = ix
            .match_turns(Fts::Stem, "\"caroline\"", "u1", None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        let text: String = ix
            .conn
            .query_row("SELECT text FROM turns WHERE id = ?1", [hits[0]], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(text, "I went to a support group");
        assert!(ix.has_term("caroline", "u1").unwrap());
        let text_only = ix
            .match_turns(Fts::Stem, "text : (\"caroline\")", "u1", None, 10)
            .unwrap();
        assert!(text_only.is_empty());
        let terms = vec!["caroline".to_string(), "support".to_string()];
        assert_eq!(ix.speaker_terms(&terms, "u1").unwrap(), vec!["caroline"]);
        assert!(ix.speaker_terms(&terms, "u2").unwrap().is_empty());
    }

    #[test]
    fn reset_empties_everything() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[doc("s1", "u1", "2023-01-01", &["hello there"])])
            .unwrap();
        ix.reset().unwrap();
        let n: i64 = ix
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn session_loads_turns_in_order() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[doc("s1", "u1", "2023-01-01", &["first", "second"])])
            .unwrap();
        let sid: i64 = ix
            .conn
            .query_row("SELECT sid FROM sessions", [], |r| r.get(0))
            .unwrap();
        let s = ix.session(sid).unwrap();
        assert_eq!(s.doc_id, "s1");
        assert_eq!(
            s.turns.iter().map(|t| t.2.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn atomic_reports_a_failed_rollback() {
        let mut ix = Index::open(":memory:").unwrap();
        let err = ix
            .atomic(|ix| -> Result<()> {
                ix.conn.execute_batch("COMMIT")?;
                anyhow::bail!("boom")
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("rollback failed"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn remove_session_takes_a_session_out_of_every_search() {
        let mut ix = Index::open(":memory:").unwrap();
        ix.add_sessions(&[
            doc(
                "s1",
                "u1",
                "2023-01-01T10:00:00+00:00",
                &["We compared Hindsight with other tools"],
            ),
            doc(
                "s2",
                "u1",
                "2023-01-02T10:00:00+00:00",
                &["Lunch was pasta"],
            ),
        ])
        .unwrap();
        assert!(ix.remove_session("u1", "s1").unwrap());
        assert!(!ix.remove_session("u1", "s1").unwrap());
        assert!(
            ix.match_turns(Fts::Stem, "\"compared\"", "u1", None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            ix.match_turns(Fts::Trigram, "\"hin\"", "u1", None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            ix.name_turns(&["hindsight".to_string()], "u1", 10)
                .unwrap()
                .is_empty()
        );
        let n: i64 = ix
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        ix.add_sessions(&[doc(
            "s1",
            "u1",
            "2023-01-03T10:00:00+00:00",
            &["Hindsight again"],
        )])
        .unwrap();
        assert_eq!(
            ix.match_turns(Fts::Stem, "\"hindsight\"", "u1", None, 10)
                .unwrap()
                .len(),
            1
        );
    }
}
