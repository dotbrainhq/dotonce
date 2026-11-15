//! Model checks end to end: the hooks run a fake `claude` that prints a
//! canned answer and keeps what it was given, so no test reaches a model.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const SET: &str = "[when]\nfolders = [\"/tmp\"]\n[draft]\nfence = [\"\", \"markdown\"]\n[[check]]\nid = \"every-ask\"\nkind = \"model\"\nask = \"The draft answers every question in the message it replies to.\"\nwhy = \"Answer what was asked, all of it.\"\n";
const DRAFT: &str = "Here is the reply:\n\n```\nHi Sam,\n\nThanks for the details on the acme checkout page. I tested the same steps on a fresh store at example.com and the coupon applies there. Could you share the plugin list from your store so I can compare the two?\n```";
const REDRAFT: &str = "Fixed: it answers the refund question now.\n\n```\nHi Sam,\n\nThe refund for order 1001 went out this morning and shows on your card in three to five days. I tested the same coupon steps on a fresh store at example.com and it applies there. Could you share the plugin list from your store?\n```";
const COMMANDS: &str = "Run these:\n\n```\ncd ~/acme\ngit pull\nnpm install\nnpm run build\n```";
const QUOTE: &str = "Could you share the plugin list from your store so I can compare the two?";
const PROMPT: &str = "Draft the reply to Sam about the acme refund and the coupon";

/// A home with the acme-replies set at `level` and a fake `claude` in bin/.
fn setup(name: &str, level: &str) -> PathBuf {
    let h = std::env::temp_dir().join(format!("dotonce-judge-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    let rules = h.join("dotonce/rules");
    std::fs::create_dir_all(&rules).unwrap();
    let src = format!("id = \"acme-replies\"\n{SET}");
    std::fs::write(rules.join("acme-replies.toml"), &src).unwrap();
    let approvals = json!({"sets": {"acme-replies": {
        "hash": dotonce::approvals::hash(&src),
        "level": level,
        "off": false,
        "approved": "2026-10-28T00:00:00Z",
        "changed": "2026-10-28T00:00:00Z"
    }}});
    std::fs::write(h.join("dotonce/approvals.json"), approvals.to_string()).unwrap();
    let fake = h.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let script = format!(
        "#!/bin/sh\nf='{dir}'\nn=$(cat \"$f/calls\" 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > \"$f/calls\"\ncat > \"$f/stdin-$n.txt\"\nprintf '%s\\n' \"$@\" > \"$f/args-$n.txt\"\necho \"child=$DOTONCE_CHILD cwd=$(pwd)\" > \"$f/env-$n.txt\"\nif [ -f \"$f/out-$n.json\" ]; then cat \"$f/out-$n.json\"; else cat \"$f/out.json\"; fi\nexit $(cat \"$f/exit\" 2>/dev/null || echo 0)\n",
        dir = fake.display()
    );
    let bin = h.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("claude"), script).unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    answer(&h, None, &[]);
    h
}

/// Claude Code's JSON result with these breaks, for call `n` or every call.
fn answer(h: &Path, n: Option<usize>, breaks: &[(&str, &str)]) {
    let breaks: Vec<Value> = breaks
        .iter()
        .map(|(rule, quote)| json!({"rule": rule, "quote": quote, "why": "It skips the refund question."}))
        .collect();
    let out = json!({"type": "result", "subtype": "success", "is_error": false, "result": "", "usage": {"input_tokens": 3, "cache_creation_input_tokens": 1500, "cache_read_input_tokens": 0, "output_tokens": 40}, "structured_output": {"breaks": breaks}});
    let name = n.map_or("out.json".to_string(), |n| format!("out-{n}.json"));
    std::fs::write(h.join("fake").join(name), out.to_string()).unwrap();
}

fn calls(h: &Path) -> usize {
    std::fs::read_to_string(h.join("fake/calls"))
        .map(|s| s.trim().parse().unwrap())
        .unwrap_or(0)
}

fn fake_file(h: &Path, name: &str) -> String {
    std::fs::read_to_string(h.join("fake").join(name)).unwrap()
}

/// Runs a hook as the other tests do, with `path` as PATH and `extra` set.
fn run(h: &Path, event: &str, input: &Value, path: &str, extra: &[(&str, &str)]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(["hook", event])
        .current_dir(h)
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", h)
        .env("PATH", path)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_PLUGIN_ROOT")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("AI_AGENT")
        .env_remove("DOTONCE_CHILD")
        .envs(extra.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "hook {event} failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn path(h: &Path) -> String {
    format!("{}:/usr/bin:/bin", h.join("bin").display())
}

/// A session record with the typed prompt and a few tool calls.
fn transcript(h: &Path) -> PathBuf {
    let lines = [
        json!({"type": "user", "message": {"content": PROMPT}}),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": "curl -s https://example.com/acme/orders/1001"}}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
    ];
    let p = h.join("record.jsonl");
    let src: Vec<String> = lines.iter().map(Value::to_string).collect();
    std::fs::write(&p, src.join("\n") + "\n").unwrap();
    p
}

fn stop_input(sid: &str, pid: &str, last: &str, active: bool, record: &Path) -> Value {
    json!({"session_id": sid, "prompt_id": pid, "hook_event_name": "Stop", "stop_hook_active": active, "last_assistant_message": last, "transcript_path": record, "cwd": "/tmp"})
}

fn prompt(h: &Path, sid: &str, pid: &str) {
    let input = json!({"session_id": sid, "prompt_id": pid, "prompt": PROMPT, "cwd": "/tmp"});
    run(h, "prompt", &input, &path(h), &[]);
}

fn judge(h: &Path, sid: &str, pid: &str, last: &str) -> String {
    let record = transcript(h);
    run(
        h,
        "judge",
        &stop_input(sid, pid, last, false, &record),
        &path(h),
        &[],
    )
}

fn stop(h: &Path, sid: &str, pid: &str, last: &str, active: bool) -> String {
    let record = h.join("missing.jsonl");
    run(
        h,
        "stop",
        &stop_input(sid, pid, last, active, &record),
        &path(h),
        &[],
    )
}

fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "Stop");
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

/// (check, outcome) of each catch, oldest first.
fn outcomes(h: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(h.join("dotonce/catches.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (
                v["check"].as_str().unwrap().to_string(),
                v["outcome"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// The result of each judge.log line, oldest first.
fn results(h: &Path) -> Vec<String> {
    std::fs::read_to_string(h.join("dotonce/judge.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["result"].as_str().unwrap().to_string()
        })
        .collect()
}

#[test]
fn a_warn_catch_comes_back_on_the_next_turn_from_the_background_hook() {
    let h = setup("warn", "warn");
    answer(&h, None, &[("every-ask", QUOTE)]);
    prompt(&h, "s1", "p1");
    assert!(stop(&h, "s1", "p1", DRAFT, false).trim().is_empty());
    assert_eq!(
        calls(&h),
        0,
        "Stop leaves a warn set to the background hook"
    );
    let note = context(&judge(&h, "s1", "p1", DRAFT));
    assert_eq!(
        note,
        format!(
            "Your last draft may break every-ask: \"{QUOTE}\". Answer what was asked, all of it. The checker's reason: It skips the refund question.\nA second model wrote this and can be wrong. If a catch holds, fix the draft. If not, ignore it."
        )
    );
    assert_eq!(outcomes(&h), pairs(&[("every-ask", "caught")]));
    assert_eq!(results(&h), ["caught"]);
    let line: Value = serde_json::from_str(
        std::fs::read_to_string(h.join("dotonce/judge.log"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(line["sets"], json!(["acme-replies"]));
    assert_eq!(
        (line["in"].as_u64(), line["out"].as_u64()),
        (Some(1503), Some(40))
    );
    assert!(
        !std::fs::read_to_string(h.join("dotonce/judge.log"))
            .unwrap()
            .contains("Sam"),
        "no draft text in the log"
    );

    let sent = fake_file(&h, "stdin-1.txt");
    assert!(
        sent.contains("- every-ask: The draft answers every question"),
        "{sent}"
    );
    assert!(
        sent.contains(&format!("<session>\nUser, this turn: {PROMPT}\n\n")),
        "{sent}"
    );
    assert!(
        sent.contains("\nBash: curl -s https://example.com/acme/orders/1001\n"),
        "{sent}"
    );
    assert!(
        sent.contains("<draft n=\"1\">\nHi Sam,") && sent.contains(QUOTE),
        "{sent}"
    );
    assert!(
        sent.contains("<reply>\nHere is the reply:\n\n```\n[draft 1]\n```\n</reply>"),
        "the reply goes with the draft swapped for its number: {sent}"
    );
    let args = fake_file(&h, "args-1.txt");
    for want in [
        "-p\n",
        "--model\nhaiku\n",
        "--tools\n\n",
        "--strict-mcp-config\n",
        "--no-session-persistence\n",
        "--setting-sources\n\n",
        "--disable-slash-commands\n",
        "--output-format\njson\n",
        "--json-schema\n",
    ] {
        assert!(args.contains(want), "{want:?}: {args}");
    }
    assert!(args.contains("{\"disableAllHooks\":true}"), "{args}");
    let env = fake_file(&h, "env-1.txt");
    assert!(env.starts_with("child=1 cwd="), "{env}");
    assert!(env.trim_end().ends_with("dotonce-judge"), "{env}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_draft_is_judged_once_and_commands_are_not_a_draft() {
    let h = setup("once", "warn");
    prompt(&h, "s1", "p1");
    assert!(judge(&h, "s1", "p1", DRAFT).trim().is_empty());
    assert!(judge(&h, "s1", "p1", DRAFT).trim().is_empty());
    assert_eq!(calls(&h), 1, "the same draft isn't judged twice");
    assert!(judge(&h, "s1", "p2", COMMANDS).trim().is_empty());
    assert_eq!(calls(&h), 1);
    assert!(judge(&h, "s2", "p1", DRAFT).trim().is_empty());
    assert_eq!(calls(&h), 2, "another session judges it again");
    assert_eq!(results(&h), ["pass", "pass"]);
    assert!(outcomes(&h).is_empty());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_break_without_its_words_in_the_draft_is_dropped() {
    let h = setup("unquoted", "warn");
    answer(
        &h,
        None,
        &[
            ("every-ask", "I promise the refund lands tomorrow."),
            ("no-such-check", QUOTE),
        ],
    );
    prompt(&h, "s1", "p1");
    assert!(judge(&h, "s1", "p1", DRAFT).trim().is_empty());
    assert_eq!(results(&h), ["unquoted"]);
    assert!(outcomes(&h).is_empty());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn any_failure_lets_the_draft_through_and_says_why_in_the_log() {
    let h = setup("failopen", "fix");
    prompt(&h, "s1", "p1");
    let input = stop_input("s1", "p1", DRAFT, false, &h.join("missing.jsonl"));
    assert!(
        run(&h, "stop", &input, "/usr/bin:/bin", &[])
            .trim()
            .is_empty()
    );
    std::fs::write(h.join("fake/out.json"), "not json").unwrap();
    let redraft = DRAFT.replace("so I can compare the two?", "to compare?");
    assert!(stop(&h, "s1", "p1", &redraft, false).trim().is_empty());
    std::fs::write(
        h.join("fake/out.json"),
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true})
            .to_string(),
    )
    .unwrap();
    std::fs::write(h.join("fake/exit"), "1").unwrap();
    let third = DRAFT.replace("so I can compare the two?", "today?");
    assert!(stop(&h, "s1", "p1", &third, false).trim().is_empty());
    assert_eq!(
        results(&h),
        ["no-claude", "bad-output", "error-error_during_execution"]
    );
    assert!(outcomes(&h).is_empty());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn the_daily_cap_stops_the_calls() {
    let h = setup("cap", "warn");
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let sessions = h.join("dotonce/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("s1.judge.json"),
        json!({"day": day, "calls": 40, "keys": []}).to_string(),
    )
    .unwrap();
    prompt(&h, "s1", "p1");
    assert!(judge(&h, "s1", "p1", DRAFT).trim().is_empty());
    assert_eq!(calls(&h), 0);
    assert_eq!(results(&h), ["cap"]);
    std::fs::write(
        sessions.join("s1.judge.json"),
        json!({"day": "2026-01-01", "calls": 40, "keys": []}).to_string(),
    )
    .unwrap();
    judge(&h, "s1", "p1", DRAFT);
    assert_eq!(calls(&h), 1, "a new day starts the count again");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_fix_catch_goes_back_and_the_rewrite_is_judged() {
    let h = setup("fix", "fix");
    answer(&h, Some(1), &[("every-ask", QUOTE)]);
    prompt(&h, "s1", "p1");
    assert!(judge(&h, "s1", "p1", DRAFT).trim().is_empty());
    assert_eq!(calls(&h), 0, "the background hook leaves a fix set to Stop");
    let msg = context(&stop(&h, "s1", "p1", DRAFT, false));
    assert!(
        msg.contains(&format!("- every-ask: \"{QUOTE}\". Answer what was asked, all of it. The checker's reason: It skips the refund question.")),
        "{msg}"
    );
    assert!(msg.starts_with("dotonce checked the draft"), "{msg}");
    assert!(stop(&h, "s1", "p1", REDRAFT, true).trim().is_empty());
    assert_eq!(calls(&h), 2);
    assert_eq!(
        outcomes(&h),
        pairs(&[("every-ask", "caught"), ("every-ask", "fixed")])
    );

    answer(
        &h,
        Some(3),
        &[(
            "every-ask",
            "Could you share the plugin list from your store before Friday?",
        )],
    );
    answer(
        &h,
        Some(4),
        &[(
            "every-ask",
            "Could you share the plugin list from your store?",
        )],
    );
    prompt(&h, "s1", "p2");
    let again = DRAFT.replace("so I can compare the two?", "before Friday?");
    context(&stop(&h, "s1", "p2", &again, false));
    let rewrite = REDRAFT.replace("this morning", "at noon");
    assert!(stop(&h, "s1", "p2", &rewrite, true).trim().is_empty());
    assert_eq!(calls(&h), 4);
    assert_eq!(
        outcomes(&h)[2..],
        pairs(&[("every-ask", "caught"), ("every-ask", "still_broken")])
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_rewrite_the_judge_cant_read_is_unverified() {
    let h = setup("unverified", "fix");
    answer(&h, None, &[("every-ask", QUOTE)]);
    prompt(&h, "s1", "p1");
    context(&stop(&h, "s1", "p1", DRAFT, false));
    assert!(stop(&h, "s1", "p1", COMMANDS, true).trim().is_empty());
    assert_eq!(calls(&h), 1);
    assert_eq!(
        outcomes(&h),
        pairs(&[("every-ask", "caught"), ("every-ask", "unverified")])
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn the_judges_own_claude_run_is_never_checked() {
    let h = setup("child", "fix");
    answer(&h, None, &[("every-ask", QUOTE)]);
    let record = transcript(&h);
    for event in ["stop", "judge", "prompt"] {
        let out = run(
            &h,
            event,
            &stop_input("s1", "p1", DRAFT, false, &record),
            &path(&h),
            &[("DOTONCE_CHILD", "1")],
        );
        assert!(out.trim().is_empty(), "{event}: {out}");
    }
    assert_eq!(calls(&h), 0);
    assert!(results(&h).is_empty() && outcomes(&h).is_empty());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_says_whether_the_judge_can_run_and_what_it_did() {
    let h = setup("doctor", "warn");
    let doctor = |p: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_dotonce"))
            .arg("doctor")
            .current_dir(&h)
            .env("DOTONCE_HOME", h.join("dotonce"))
            .env("HOME", &h)
            .env("PATH", p)
            .env_remove("CLAUDE_CONFIG_DIR")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let out = doctor("/usr/bin:/bin");
    assert!(
        out.contains(
            "warning  model checks in 1 set(s) let every draft through: no claude command"
        ),
        "{out}"
    );
    let out = doctor(&path(&h));
    let bin = h.join("bin/claude");
    assert!(
        out.contains(&format!(
            "ok       model checks in 1 set(s), judged by haiku through {}",
            bin.display()
        )),
        "{out}"
    );
    assert!(!out.contains("last 7 days"), "{out}");
    answer(&h, None, &[("every-ask", QUOTE)]);
    prompt(&h, "s1", "p1");
    judge(&h, "s1", "p1", DRAFT);
    let input = stop_input("s2", "p1", DRAFT, false, &transcript(&h));
    run(&h, "judge", &input, "/usr/bin:/bin", &[]);
    let out = doctor(&path(&h));
    assert!(
        out.contains("model checks, last 7 days: 1 judged, 1 caught, 1 let through (1 no-claude)"),
        "{out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_test_says_model_checks_run_only_at_stop() {
    let h = setup("rulestest", "warn");
    let file = h.join("draft.md");
    std::fs::write(&file, DRAFT).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(["rules", "test", "acme-replies"])
        .arg(&file)
        .current_dir(&h)
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", &h)
        .env("PATH", path(&h))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("passes: no check fails"), "{out}");
    assert!(
        out.contains("note     every-ask only run at Stop, where a second model judges the draft: rules test doesn't call it"),
        "{out}"
    );
    assert_eq!(calls(&h), 0);
    std::fs::remove_dir_all(&h).unwrap();
}
