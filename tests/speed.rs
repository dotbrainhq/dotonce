use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

use dotonce::index::Index;
use dotonce::model::{SessionDoc, TurnIn};
use dotonce::retrieve::{Options, retrieve};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize
    }
}

fn vocab() -> Vec<String> {
    let syll = [
        "ka", "lo", "mi", "ne", "ru", "sa", "to", "vi", "ze", "po", "qu", "fe", "gi", "ho", "ju",
        "be",
    ];
    let mut v = Vec::new();
    for a in syll {
        for b in syll {
            for c in ["n", "r", "s", "t"] {
                v.push(format!("{a}{b}{c}"));
            }
        }
    }
    v
}

#[test]
#[ignore = "run with: cargo test --release --test speed -- --ignored --nocapture"]
fn retrieve_p95_under_50ms_at_100k_turns() {
    let words = vocab();
    let mut rng = Lcg(42);
    let mut ix = Index::open(":memory:").unwrap();
    let docs: Vec<SessionDoc> = (0..2_000)
        .map(|s| SessionDoc {
            id: format!("s{s}"),
            user_id: Some("u".to_string()),
            timestamp: Some(format!(
                "2023-{:02}-{:02}T10:00:00+00:00",
                s % 12 + 1,
                s % 28 + 1
            )),
            turns: (0..50)
                .map(|_| TurnIn {
                    speaker: "user".to_string(),
                    text: (0..30)
                        .map(|_| words[rng.next() % words.len()].as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                })
                .collect(),
        })
        .collect();
    let t0 = Instant::now();
    ix.add_sessions(&docs).unwrap();
    println!("ingest 100k turns: {:?}", t0.elapsed());

    let opt = Options {
        budget_tokens: 1_500,
        ..Options::default()
    };
    let mut times: Vec<f64> = (0..200)
        .map(|_| {
            let q = (0..4)
                .map(|_| words[rng.next() % words.len()].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let t = Instant::now();
            retrieve(&ix, &q, "u", None, &opt).unwrap();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let p50 = times[100];
    let p95 = times[190];
    println!("retrieve p50 {p50:.1} ms, p95 {p95:.1} ms");
    assert!(p95 < 50.0, "p95 {p95:.1} ms");
}

#[test]
#[ignore = "run with: cargo test --release --test speed -- --ignored --nocapture"]
fn stop_hook_p95_under_50ms_with_50_rule_sets() {
    let h = std::env::temp_dir().join(format!("dotonce-speed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    let rules = h.join("dotonce/rules");
    let proj = h.join("proj");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    let mut approvals = serde_json::Map::new();
    for i in 0..50 {
        let id = format!("set-{i:02}");
        let level = if i % 2 == 0 { "fix" } else { "warn" };
        let src = format!(
            "id = \"{id}\"\nlevel = \"{level}\"\n[when]\nfolders = [{proj:?}]\nskills = [\"skill-{i}\"]\nprompt = [\"word{i}\"]\n[draft]\nfence = [\"markdown\", \"draft\"]\n[remind]\ntext = \"Reminder {i}.\"\n[[check]]\nid = \"forbid\"\nforbid = [\"stock phrase {i}\", \"sorry for the wait\"]\nwhy = \"w\"\n[[check]]\nid = \"regex\"\nforbid_regex = '\\bTODO{i}\\b'\nwhy = \"w\"\n[[check]]\nid = \"require\"\nrequire = [\"Thanks,\"]\nwhy = \"w\"\n[[check]]\nid = \"links\"\nkind = \"bare-url\"\nwhy = \"w\"\n[[check]]\nid = \"name\"\nrepeat_of = '^Hi (\\w+),'\nmax = 0\nwhy = \"w\"\n[[check]]\nid = \"paragraphs\"\nmax_paragraphs = 6\nwhy = \"w\"\n[[check]]\nid = \"words\"\nmax_words = 300\nwhy = \"w\"\n"
        );
        std::fs::write(rules.join(format!("{id}.toml")), &src).unwrap();
        approvals.insert(
            id,
            serde_json::json!({"hash": dotonce::approvals::hash(&src), "level": level, "off": false, "approved": "2026-10-28T00:00:00Z"}),
        );
    }
    std::fs::write(
        h.join("dotonce/approvals.json"),
        serde_json::json!({"sets": approvals}).to_string(),
    )
    .unwrap();
    let words = vocab();
    let mut rng = Lcg(7);
    let mut record = String::new();
    for t in 0..200 {
        record.push_str(&serde_json::json!({"type": "user", "message": {"role": "user", "content": format!("prompt {t}")}, "promptId": format!("old{t}")}).to_string());
        record.push('\n');
        let text: Vec<&str> = (0..200)
            .map(|_| words[rng.next() % words.len()].as_str())
            .collect();
        record.push_str(&serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text.join(" ")}]}}).to_string());
        record.push('\n');
    }
    let transcript = h.join("record.jsonl");
    std::fs::write(&transcript, record).unwrap();
    let draft = "```markdown\nHi Sam,\n\nSorry for the wait. See https://example.com/guide\n\nThanks, Sam\n```";
    let mut times: Vec<f64> = Vec::new();
    for i in 0..100 {
        let input = serde_json::json!({"session_id": "speed", "prompt_id": format!("p{i}"), "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": draft, "transcript_path": transcript, "cwd": proj}).to_string();
        let t = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
            .args(["hook", "stop"])
            .env("DOTONCE_HOME", h.join("dotonce"))
            .env("HOME", &h)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(out.status.success());
        if i == 0 {
            let s = String::from_utf8_lossy(&out.stdout);
            assert!(
                s.contains("additionalContext"),
                "the first Stop sends back: {s}"
            );
        }
    }
    times.sort_by(f64::total_cmp);
    let (p50, p95) = (times[50], times[95]);
    println!("stop hook with 50 rule sets: p50 {p50:.1} ms, p95 {p95:.1} ms");
    std::fs::remove_dir_all(&h).unwrap();
    assert!(p95 < 50.0, "p95 {p95:.1} ms");
}

/// Writes 50 approved sets that never turn on here: each needs a skill
/// nobody uses. Returns the home.
fn inactive_home(name: &str, sets: usize) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!("dotonce-speed-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    let rules = h.join("dotonce/rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::create_dir_all(h.join("proj")).unwrap();
    let mut approvals = serde_json::Map::new();
    for i in 0..sets {
        let id = format!("set-{i:02}");
        let src = format!(
            "id = \"{id}\"\nlevel = \"fix\"\n[when]\nskills = [\"skill-{i}\"]\n[draft]\nfence = [\"markdown\"]\n[remind]\ntext = \"Reminder {i}.\"\n[[check]]\nid = \"forbid\"\nforbid = [\"stock phrase {i}\"]\nwhy = \"w\"\n[[check]]\nid = \"regex\"\nforbid_regex = '\\bTODO{i}\\b|\\w+ \\w+ \\w+ (alpha|beta|gamma){{2,9}}'\nwhy = \"w\"\n[[check]]\nid = \"name\"\nrepeat_of = '^Hi (\\w+),'\nmax = 0\nwhy = \"w\"\n[[check]]\nid = \"push\"\non = \"tool\"\ntool = \"Bash\"\nforbid_regex = 'git\\s+push\\b.*(--force|-f\\b){i}'\nwhy = \"w\"\n"
        );
        std::fs::write(rules.join(format!("{id}.toml")), &src).unwrap();
        approvals.insert(
            id,
            serde_json::json!({"hash": dotonce::approvals::hash(&src), "level": "fix", "off": false, "approved": "2026-10-28T00:00:00Z"}),
        );
    }
    std::fs::write(
        h.join("dotonce/approvals.json"),
        serde_json::json!({"sets": approvals}).to_string(),
    )
    .unwrap();
    h
}

/// p50 and p95 in ms of 60 runs of `dotonce hook <event>` with this input.
fn time_hook(h: &std::path::Path, event: &str, input: &serde_json::Value) -> (f64, f64) {
    let input = input.to_string();
    let mut times: Vec<f64> = (0..60)
        .map(|_| {
            let t = Instant::now();
            let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
                .args(["hook", event])
                .env("DOTONCE_HOME", h.join("dotonce"))
                .env("HOME", h)
                .env_remove("CLAUDECODE")
                .env_remove("CLAUDE_CODE_ENTRYPOINT")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            assert!(child.wait_with_output().unwrap().status.success());
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    (times[30], times[57])
}

#[test]
#[ignore = "run with: cargo test --release --test speed -- --ignored --nocapture"]
fn inactive_sets_cost_about_what_no_sets_cost() {
    let none = inactive_home("none", 0);
    let fifty = inactive_home("fifty", 50);
    let inputs = |h: &std::path::Path| {
        let cwd = h.join("proj");
        [
            (
                "pre-tool",
                serde_json::json!({"session_id": "speed", "prompt_id": "p1", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "git push origin main"}, "cwd": cwd}),
            ),
            (
                "display",
                serde_json::json!({"session_id": "speed", "prompt_id": "p1", "hook_event_name": "MessageDisplay", "message_id": "m1", "index": 0, "delta": "```markdown\nHi Sam,\n```\n", "final": true, "cwd": cwd}),
            ),
            (
                "prompt",
                serde_json::json!({"session_id": "speed", "prompt_id": "p1", "hook_event_name": "UserPromptSubmit", "prompt": "Draft a note to Sam", "cwd": cwd}),
            ),
        ]
    };
    let mut slow = Vec::new();
    for ((event, a), (_, b)) in inputs(&none).into_iter().zip(inputs(&fifty)) {
        let (n50, n95) = time_hook(&none, event, &a);
        let (f50, f95) = time_hook(&fifty, event, &b);
        println!(
            "{event}: no sets p50 {n50:.1} ms p95 {n95:.1} ms; 50 inactive sets p50 {f50:.1} ms p95 {f95:.1} ms"
        );
        if f50 > n50 + 10.0 {
            slow.push(event);
        }
    }
    std::fs::remove_dir_all(&none).unwrap();
    std::fs::remove_dir_all(&fifty).unwrap();
    assert!(
        slow.is_empty(),
        "over 10 ms slower with 50 inactive sets: {slow:?}"
    );
}
