use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
}

impl Server {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
            .arg("bench-serve")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, out }
    }

    fn call(&mut self, line: &str) -> String {
        writeln!(self.stdin.as_mut().unwrap(), "{line}").unwrap();
        let mut reply = String::new();
        self.out.read_line(&mut reply).unwrap();
        reply
    }

    fn stop(mut self) {
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

fn session(id: &str, user: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"id":"{id}","user_id":"{user}","timestamp":"{ts}","turns":[{{"speaker":"user","text":"{text}"}}]}}"#
    )
}

fn ingest(docs: &[String]) -> String {
    format!(r#"{{"op":"ingest","docs":[{}]}}"#, docs.join(","))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dotonce-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn ingest_then_retrieve_over_stdio() {
    let mut s = Server::start(&["--dir", ":memory:"]);
    let line = s.call(&ingest(&[session(
        "s1",
        "u1",
        "2023-01-01T10:00:00+00:00",
        "I adopted a dog named Rex",
    )]));
    assert!(line.contains(r#""ingested":1"#), "{line}");

    let line = s.call(r#"{"op":"retrieve","query":"What is my dog called?","user_id":"u1"}"#);
    assert!(line.contains(r#""id":"s1""#), "{line}");

    let line = s.call("not json");
    assert!(line.contains(r#""ok":false"#), "{line}");

    assert_eq!(s.call(r#"{"op":"reset"}"#).trim(), r#"{"ok":true}"#);
    let line = s.call(r#"{"op":"retrieve","query":"What is my dog called?","user_id":"u1"}"#);
    assert_eq!(line.trim(), r#"{"ok":true,"sessions":[]}"#);
    s.stop();
}

#[test]
fn one_users_sessions_do_not_change_another_users_ranking() {
    let mut s = Server::start(&["--dir", ":memory:"]);
    s.call(&ingest(&[
        session(
            "a1",
            "A",
            "2023-01-01T10:00:00+00:00",
            "apple apple apple fruit salad tonight",
        ),
        session(
            "a2",
            "A",
            "2023-01-02T10:00:00+00:00",
            "banana bread with a lot of extra filler words tonight",
        ),
    ]));
    let q = r#"{"op":"retrieve","query":"apple banana","user_id":"A"}"#;
    let before = s.call(q);
    assert!(before.contains(r#""id":"a1""#), "{before}");
    let b: Vec<String> = (0..50)
        .map(|i| {
            session(
                &format!("b{i}"),
                "B",
                "2023-01-03T10:00:00+00:00",
                &format!("apple number {i}"),
            )
        })
        .collect();
    assert!(s.call(&ingest(&b)).contains(r#""ingested":50"#));
    assert_eq!(s.call(q), before);
    s.stop();
}

#[test]
fn dir_keeps_one_file_per_user_and_fresh_wipes_it() {
    let dir = temp_dir("per-user");
    let d = dir.to_str().unwrap();
    let q = r#"{"op":"retrieve","query":"What is my dog called?","user_id":"u/1"}"#;

    let mut s = Server::start(&["--dir", d, "--fresh"]);
    s.call(&ingest(&[
        session(
            "s1",
            "u/1",
            "2023-01-01T10:00:00+00:00",
            "I adopted a dog named Rex",
        ),
        session(
            "s2",
            "u_1",
            "2023-01-01T10:00:00+00:00",
            "I adopted a cat named Tom",
        ),
    ]));
    s.stop();
    let files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".sqlite"))
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files.iter().all(|f| f.starts_with("u_1_")), "{files:?}");

    let mut s = Server::start(&["--dir", d]);
    assert!(s.call(q).contains(r#""id":"s1""#));
    s.stop();

    let mut s = Server::start(&["--dir", d, "--fresh"]);
    assert_eq!(s.call(q).trim(), r#"{"ok":true,"sessions":[]}"#);
    s.stop();
    std::fs::remove_dir_all(&dir).unwrap();
}
