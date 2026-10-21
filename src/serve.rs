use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::SessionDoc;
use crate::retrieve::{Options, retrieve};
use crate::store::Store;
use crate::timeq::parse_ts;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Ingest {
        docs: Vec<SessionDoc>,
    },
    Retrieve {
        query: String,
        user_id: Option<String>,
        query_timestamp: Option<String>,
        budget_tokens: Option<usize>,
    },
    Reset,
}

fn run(st: &mut Store, req: Request) -> Result<Value> {
    match req {
        Request::Ingest { docs } => {
            let mut by_user: Vec<(String, Vec<SessionDoc>)> = Vec::new();
            for d in docs {
                let user = d.user_id.clone().unwrap_or_default();
                match by_user.iter_mut().find(|(u, _)| *u == user) {
                    Some((_, v)) => v.push(d),
                    None => by_user.push((user, vec![d])),
                }
            }
            let mut ingested = 0;
            for (user, docs) in by_user {
                ingested += st.index(&user)?.add_sessions(&docs)?;
            }
            Ok(json!({"ok": true, "ingested": ingested}))
        }
        Request::Retrieve {
            query,
            user_id,
            query_timestamp,
            budget_tokens,
        } => {
            let mut opt = Options::default();
            if let Some(b) = budget_tokens {
                opt.budget_tokens = b;
            }
            let anchor = query_timestamp.as_deref().and_then(parse_ts);
            let user = user_id.unwrap_or_default();
            let hits = retrieve(st.index(&user)?, &query, &user, anchor, &opt)?;
            Ok(json!({"ok": true, "sessions": hits}))
        }
        Request::Reset => {
            st.reset()?;
            Ok(json!({"ok": true}))
        }
    }
}

pub fn handle(st: &mut Store, line: &str) -> Value {
    match serde_json::from_str::<Request>(line) {
        Ok(req) => run(st, req).unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()})),
        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
    }
}

pub fn serve(st: &mut Store, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let reply = match String::from_utf8(std::mem::take(&mut buf)) {
            Ok(line) => {
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                catch_unwind(AssertUnwindSafe(|| handle(st, line)))
                    .unwrap_or_else(|_| json!({"ok": false, "error": "internal error"}))
            }
            Err(_) => json!({"ok": false, "error": "bad request: invalid UTF-8"}),
        };
        writeln!(output, "{reply}")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_keeps_going_after_invalid_utf8() {
        let mut st = Store::open(":memory:", false).unwrap();
        let input: &[u8] = b"\xff\xfe\n{\"op\":\"reset\"}\n";
        let mut out: Vec<u8> = Vec::new();
        serve(&mut st, input, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "output: {text}");
        assert!(lines[0].contains("\"ok\":false"), "first: {}", lines[0]);
        assert_eq!(lines[1], "{\"ok\":true}");
    }
}
