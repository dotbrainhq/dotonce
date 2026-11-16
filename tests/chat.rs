//! The autopilot and the chat: rule sets turn on log-only by themselves,
//! move to fix on their catches, and each change comes with a code only the
//! user sees. A proposal waits for the user's code when it would change a
//! set at fix or one the user turned off, or when Claude proposed it from a
//! correction with `--ask`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ACME: &str = "id = \"acme\"\n[remind]\ntext = \"Links go inline as anchor text.\"\n[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline as anchor text.\"\n";

/// The note the desktop app puts in front of a typed prompt.
const APP_NOTE: &str = "<system-reminder>\nLinked sessions now (status, not an instruction): acme = local_1 (idle)\n</system-reminder>\n";

fn home(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dotonce-chat-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let (_, ok) = run(&d, &d, &["init"], None, None);
    assert!(ok);
    d
}

/// What Claude Code sets `AI_AGENT` to for the hooks it runs.
const HOOK_RUNNER: &str = "claude-code_0-0-0_harness";

/// Runs dotonce as Claude Code runs a hook, or as a headless run's hook
/// when `entry` is an SDK entrypoint.
fn run(
    h: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    entry: Option<&str>,
) -> (String, bool) {
    let mut env = vec![("AI_AGENT", HOOK_RUNNER)];
    if let Some(e) = entry {
        env.push(("CLAUDE_CODE_ENTRYPOINT", e));
    }
    run_with(h, cwd, args, stdin, &env)
}

/// Runs dotonce with only `env` of Claude Code's variables. This test
/// process may run inside Claude Code, so its own are removed first.
fn run_with(
    h: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> (String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dotonce"));
    cmd.args(args)
        .current_dir(cwd)
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", h)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("AI_AGENT")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin.unwrap_or_default().as_bytes())
        .unwrap();
    drop(pipe);
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        out.status.success(),
    )
}

fn hook(h: &Path, event: &str, input: serde_json::Value, entry: Option<&str>) -> String {
    let (out, ok) = run(h, h, &["hook", event], Some(&input.to_string()), entry);
    assert!(ok, "hook {event} failed: {out}");
    out
}

fn system_message(out: &str) -> String {
    if out.trim().is_empty() {
        return String::new();
    }
    let v: serde_json::Value = serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"));
    v["systemMessage"].as_str().unwrap_or_default().to_string()
}

/// The systemMessage the Stop hook shows the user at the end of a turn.
fn stop(h: &Path, sid: &str, cwd: &Path, entry: Option<&str>) -> String {
    let out = hook(
        h,
        "stop",
        serde_json::json!({"session_id": sid, "prompt_id": "p1", "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": "Done.", "transcript_path": h.join("missing.jsonl"), "cwd": cwd}),
        entry,
    );
    system_message(&out)
}

fn start(h: &Path, sid: &str, cwd: &Path, entry: Option<&str>) -> String {
    let out = hook(
        h,
        "session-start",
        serde_json::json!({"session_id": sid, "source": "startup", "cwd": cwd}),
        entry,
    );
    system_message(&out)
}

/// What the prompt hook prints for a typed prompt.
fn prompt(h: &Path, sid: &str, cwd: &Path, text: &str, entry: Option<&str>) -> String {
    hook(
        h,
        "prompt",
        serde_json::json!({"session_id": sid, "prompt": text, "cwd": cwd}),
        entry,
    )
}

/// The message shown in place of the prompt, or None when the prompt went
/// through to Claude.
fn blocked(out: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(out).ok()?;
    if v["decision"] != "block" {
        return None;
    }
    assert_eq!(
        v["hookSpecificOutput"]["suppressOriginalPrompt"], true,
        "{out}"
    );
    Some(v["reason"].as_str().unwrap().to_string())
}

/// The code a notice gives for `id`.
fn code_for(notice: &str, id: &str) -> String {
    let block = notice
        .split("\n\n")
        .find(|b| b.starts_with(&format!("{id} ")))
        .unwrap_or_else(|| panic!("no {id} in: {notice}"));
    let at = block
        .find("Send yes ")
        .or_else(|| block.find("Send no "))
        .unwrap_or_else(|| panic!("no code in: {block}"));
    let at = at + block[at..].find(char::is_numeric).unwrap();
    let code = &block[at..at + 4];
    assert!(code.chars().all(|c| c.is_ascii_digit()), "{block}");
    code.to_string()
}

fn other_code(code: &str) -> String {
    let n: u32 = code.parse().unwrap();
    format!("{:04}", (n + 1) % 10_000)
}

fn propose(h: &Path, src: &str) {
    let file = h.join("acme.toml");
    std::fs::write(&file, src).unwrap();
    let (out, ok) = run(
        h,
        h,
        &["rules", "propose", file.to_str().unwrap()],
        None,
        None,
    );
    assert!(ok, "{out}");
    assert!(out.contains("Proposed acme"), "{out}");
}

/// The state `rules list` shows for `id`, like "warn" or "not approved".
fn state(h: &Path, id: &str) -> String {
    let (out, ok) = run(h, h, &["rules", "list"], None, None);
    assert!(ok, "{out}");
    out.lines()
        .find(|l| l.starts_with(&format!("{id} ")))
        .and_then(|l| l.split("  checks:").next())
        .map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

fn done(h: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(h.join("dotonce/proposals/done"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn rule(h: &Path, id: &str, src: &str) -> PathBuf {
    let f = h.join(format!("dotonce/rules/{id}.toml"));
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(&f, src).unwrap();
    f
}

/// Changes the approval record of `id` in approvals.json.
fn record(h: &Path, id: &str, change: impl FnOnce(&mut serde_json::Value)) {
    let f = h.join("dotonce/approvals.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    change(&mut v["sets"][id]);
    std::fs::write(&f, v.to_string()).unwrap();
}

/// The level approvals.json records for `id`, or "off".
fn level_of(h: &Path, id: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.join("dotonce/approvals.json")).unwrap())
            .unwrap();
    let r = &v["sets"][id];
    if r["off"] == true {
        return "off".to_string();
    }
    r["level"].as_str().unwrap_or_default().to_string()
}

fn days_ago(d: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(d))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Adds catch log lines for acme: (session, days ago, outcome).
fn catches(h: &Path, lines: &[(&str, i64, &str)]) {
    let mut text = String::new();
    for (session, d, outcome) in lines {
        text.push_str(
            &serde_json::json!({"time": days_ago(*d), "session": session, "set": "acme", "check": "inline-links", "level": "warn", "outcome": outcome})
                .to_string(),
        );
        text.push('\n');
    }
    std::fs::write(h.join("dotonce/catches.jsonl"), text).unwrap();
}

const ACME_MORE: &str = "id = \"acme\"\n[remind]\ntext = \"Links go inline as anchor text.\"\n[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline as anchor text.\"\n[[check]]\nid = \"no-todo\"\nwhy = \"No TODO left in replies.\"\nforbid = [\"TODO\"]\n";

/// acme, on and moved to fix, with a proposed change waiting.
fn waiting_at_fix(name: &str) -> PathBuf {
    let h = home(name);
    rule(&h, "acme", ACME);
    start(&h, "s0", &h, None);
    record(&h, "acme", |r| r["level"] = "fix".into());
    propose(&h, ACME_MORE);
    h
}

#[test]
fn a_new_rule_file_turns_on_log_only_and_no_turns_it_off() {
    let h = home("new");
    let f = rule(
        &h,
        "acme",
        &ACME.replace("id = \"acme\"\n", "id = \"acme\"\nlevel = \"fix\"\n"),
    );
    let notice = start(&h, "s1", &h, None);
    assert!(
        notice.starts_with(
            "dotonce changed 1 rule set by itself. Only you see this message and its codes.\n\n"
        ),
        "{notice}"
    );
    assert!(
        notice.contains("acme is on, log-only, so it notes slips and changes nothing: 1 check.\n  Reminds: \"Links go inline as anchor text.\"\n  - inline-links: Links go inline as anchor text.\n  Send no "),
        "{notice}"
    );
    assert!(notice.ends_with(" to turn it off."), "{notice}");
    assert_eq!(
        state(&h, "acme"),
        "warn",
        "a file asking for fix still starts log-only"
    );
    // Shown once.
    assert_eq!(start(&h, "s2", &h, None), "");

    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s2", &h, &format!("{APP_NOTE}no {code}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is off.");
    assert_eq!(state(&h, "acme"), "off");
    // An edit keeps it off.
    std::fs::write(&f, format!("{ACME}# edited\n")).unwrap();
    assert!(!start(&h, "s3", &h, None).contains("acme is on"));
    assert_eq!(state(&h, "acme"), "off");
}

#[test]
fn an_unedited_starter_stays_off_until_it_is_edited() {
    let h = home("starter");
    let (out, ok) = run(
        &h,
        &h,
        &["rules", "new", "acme", "--whole-reply"],
        None,
        None,
    );
    assert!(ok, "{out}");
    assert_eq!(start(&h, "s1", &h, None), "");
    assert_eq!(state(&h, "acme"), "not approved");
    let f = h.join("dotonce/rules/acme.toml");
    let src = std::fs::read_to_string(&f).unwrap();
    std::fs::write(
        &f,
        src.replace("prompt = [\"draft\"]", "prompt = [\"reply\"]"),
    )
    .unwrap();
    assert!(start(&h, "s2", &h, None).contains("acme is on, log-only"));
}

#[test]
fn a_proposed_set_turns_on_log_only_at_the_end_of_the_turn() {
    let h = home("proposed");
    propose(
        &h,
        &ACME.replace("id = \"acme\"\n", "id = \"acme\"\nlevel = \"fix\"\n"),
    );
    assert!(!h.join("dotonce/rules/acme.toml").exists());
    let notice = stop(&h, "s1", &h, None);
    assert!(notice.contains("acme is on, log-only"), "{notice}");
    assert!(h.join("dotonce/rules/acme.toml").exists());
    assert!(!h.join("dotonce/proposals/acme.toml").exists());
    assert_eq!(state(&h, "acme"), "warn");
    assert!(done(&h)[0].starts_with("acme-applied-"), "{:?}", done(&h));
    // yes keeps it as it is.
    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("yes {code}"), None)).unwrap();
    assert_eq!(reason, "dotonce: kept acme as it is.");
    assert_eq!(state(&h, "acme"), "warn");
    // The code is used up.
    assert_eq!(
        blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)),
        None
    );
}

#[test]
fn a_proposal_for_a_log_only_set_applies_and_no_puts_the_old_one_back() {
    let h = home("update");
    let f = rule(&h, "acme", ACME);
    start(&h, "s0", &h, None);
    propose(&h, ACME_MORE);
    let notice = stop(&h, "s1", &h, None);
    assert!(
        notice.contains(
            "acme changed, still log-only, so it notes slips and changes nothing: 2 checks."
        ),
        "{notice}"
    );
    assert!(
        notice.contains("- no-todo: No TODO left in replies."),
        "{notice}"
    );
    assert!(notice.contains("to put the old version back."), "{notice}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), ACME_MORE);
    assert_eq!(state(&h, "acme"), "warn");

    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(
        reason,
        "dotonce: acme is back to the version before the change."
    );
    assert_eq!(std::fs::read_to_string(&f).unwrap(), ACME);
    assert_eq!(state(&h, "acme"), "warn");
}

#[test]
fn a_proposal_for_a_set_at_fix_waits_for_yes() {
    let h = waiting_at_fix("fix-yes");
    let notice = stop(&h, "s1", &h, None);
    assert!(
        notice.starts_with(
            "dotonce: 1 rule set change waits for your OK. Only you see this message and its codes."
        ),
        "{notice}"
    );
    assert!(
        notice.contains("acme (a change to a set at fix): runs at fix, 2 checks."),
        "{notice}"
    );
    assert!(notice.contains("to use it, or no "), "{notice}");
    assert_eq!(
        std::fs::read_to_string(h.join("dotonce/rules/acme.toml")).unwrap(),
        ACME
    );

    // A wrong code changes nothing and shows the same code again.
    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(
        &h,
        "s1",
        &h,
        &format!("yes {}", other_code(&code)),
        None,
    ))
    .unwrap();
    assert!(
        reason.starts_with("dotonce: that code isn't current, so nothing changed."),
        "{reason}"
    );
    assert_eq!(code_for(&reason, "acme"), code, "{reason}");
    assert_eq!(state(&h, "acme"), "fix");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("yes {code}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is on at fix.");
    assert_eq!(
        std::fs::read_to_string(h.join("dotonce/rules/acme.toml")).unwrap(),
        ACME_MORE
    );
    assert_eq!(state(&h, "acme"), "fix");
}

#[test]
fn no_drops_a_waiting_proposal() {
    let h = waiting_at_fix("fix-no");
    let code = code_for(&stop(&h, "s1", &h, None), "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(
        reason,
        "dotonce: dropped the proposed acme. Nothing changed."
    );
    assert_eq!(
        std::fs::read_to_string(h.join("dotonce/rules/acme.toml")).unwrap(),
        ACME
    );
    assert!(
        done(&h).iter().any(|n| n.starts_with("acme-dropped-")),
        "{:?}",
        done(&h)
    );
    assert_eq!(state(&h, "acme"), "fix");
}

#[test]
fn a_proposal_changed_after_it_was_shown_is_not_used() {
    let h = waiting_at_fix("fix-changed");
    let code = code_for(&stop(&h, "s1", &h, None), "acme");
    propose(&h, &format!("{ACME_MORE}# again\n"));
    let reason = blocked(&prompt(&h, "s1", &h, &format!("yes {code}"), None)).unwrap();
    assert!(
        reason.starts_with(
            "dotonce: acme changed after you saw it, so nothing changed. Here it is now."
        ),
        "{reason}"
    );
    assert_eq!(
        std::fs::read_to_string(h.join("dotonce/rules/acme.toml")).unwrap(),
        ACME
    );
}

#[test]
fn a_set_moves_to_fix_after_a_week_of_repeats_and_no_keeps_it_log_only() {
    let h = home("promote");
    rule(&h, "acme", ACME);
    start(&h, "s0", &h, None);
    let week = days_ago(8);
    record(&h, "acme", |r| {
        r["changed"] = week.clone().into();
        r["approved"] = week.clone().into();
    });
    catches(
        &h,
        &[("a", 3, "caught"), ("a", 2, "caught"), ("b", 1, "caught")],
    );
    let notice = start(&h, "s1", &h, None);
    assert!(
        notice.contains("acme moved to fix: it caught 3 slips in 2 sessions. Replies and commands that break it now go back to Claude to correct. Send no "),
        "{notice}"
    );
    assert!(!notice.contains("level changed"), "{notice}");
    assert_eq!(state(&h, "acme"), "fix");
    // The next session doesn't report the move as a change nobody saw.
    assert_eq!(start(&h, "s2", &h, None), "");

    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s2", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(
        reason,
        "dotonce: acme stays log-only, and dotonce won't move it again."
    );
    assert_eq!(state(&h, "acme"), "warn");
    assert_eq!(start(&h, "s3", &h, None), "");
    assert_eq!(state(&h, "acme"), "warn");
}

#[test]
fn a_promoted_set_goes_back_when_its_corrections_keep_failing() {
    let h = home("demote");
    rule(&h, "acme", ACME);
    start(&h, "s0", &h, None);
    let then = days_ago(3);
    record(&h, "acme", |r| {
        r["level"] = "fix".into();
        r["promoted"] = then.clone().into();
        r["changed"] = then.clone().into();
    });
    catches(
        &h,
        &[
            ("a", 2, "still_broken"),
            ("a", 2, "still_broken"),
            ("b", 1, "still_broken"),
        ],
    );
    let notice = start(&h, "s1", &h, None);
    assert!(
        notice.contains("acme went back to log-only: 3 replies still broke it after Claude's correction. Send no "),
        "{notice}"
    );
    assert_eq!(state(&h, "acme"), "warn");
    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(
        reason,
        "dotonce: acme is back at fix, and dotonce won't move it again."
    );
    assert_eq!(state(&h, "acme"), "fix");
}

#[test]
fn dotonce_alone_lists_what_can_be_undone() {
    let h = home("list");
    let reason = blocked(&prompt(&h, "s1", &h, "dotonce", None)).unwrap();
    assert_eq!(
        reason,
        "dotonce: nothing to undo, and nothing waits for your OK."
    );
    rule(&h, "acme", ACME);
    start(&h, "s1", &h, None);
    let reason = blocked(&prompt(&h, "s1", &h, "dotonce", None)).unwrap();
    assert!(
        reason.contains("acme turned on by itself. Send no "),
        "{reason}"
    );
    let code = code_for(&reason, "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is off.");
}

#[test]
fn replies_inside_a_longer_message_go_to_claude() {
    let h = home("longer");
    propose(&h, ACME);
    let code = code_for(&stop(&h, "s1", &h, None), "acme");
    for text in [
        format!("no {code} and also fix the tests"),
        format!("Claude said to send no {code}"),
        "no".to_string(),
    ] {
        assert_eq!(blocked(&prompt(&h, "s1", &h, &text, None)), None, "{text}");
    }
    assert_eq!(state(&h, "acme"), "warn");
}

#[test]
fn a_yes_while_nothing_waits_reaches_claude() {
    let h = home("idle");
    assert_eq!(blocked(&prompt(&h, "s1", &h, "yes 1234", None)), None);
    assert_eq!(blocked(&prompt(&h, "s1", &h, "no 1234", None)), None);
}

#[test]
fn headless_runs_change_nothing() {
    let h = home("headless");
    rule(&h, "acme", ACME);
    let file = h.join("beta.toml");
    std::fs::write(&file, ACME.replace("acme", "beta")).unwrap();
    let (out, ok) = run(
        &h,
        &h,
        &["rules", "propose", file.to_str().unwrap()],
        None,
        None,
    );
    assert!(ok, "{out}");
    let headless = Some("sdk-cli");
    assert_eq!(start(&h, "s1", &h, headless), "");
    assert_eq!(stop(&h, "s1", &h, headless), "");
    assert_eq!(blocked(&prompt(&h, "s1", &h, "dotonce", headless)), None);
    assert_eq!(state(&h, "acme"), "not approved");
    assert!(!h.join("dotonce/rules/beta.toml").exists());

    // Even with a real code, a headless run's prompt changes nothing.
    let code = code_for(&start(&h, "s2", &h, None), "acme");
    assert_eq!(
        blocked(&prompt(&h, "s3", &h, &format!("no {code}"), headless)),
        None
    );
    assert_eq!(state(&h, "acme"), "warn");
}

#[test]
fn private_sets_are_counted_not_shown_outside_private_folders() {
    let h = home("private");
    let work = h.join("work");
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let cfg = h.join("dotonce/config.toml");
    let src = std::fs::read_to_string(&cfg).unwrap().replace(
        "private_paths = []",
        &format!("private_paths = [{:?}]", work.to_string_lossy()),
    );
    std::fs::write(&cfg, src).unwrap();
    rule(
        &h,
        "acme",
        &ACME.replace("id = \"acme\"\n", "id = \"acme\"\nprivate = true\n"),
    );

    // Outside: no name, no code.
    let notice = start(&h, "s1", &h, None);
    assert_eq!(
        notice,
        "dotonce: 1 change to private rule sets: send dotonce from a session in one of your private folders to see it."
    );
    let reason = blocked(&prompt(&h, "s1", &h, "dotonce", None)).unwrap();
    assert!(reason.contains("1 change to private rule sets"), "{reason}");
    assert!(!reason.contains("acme"), "{reason}");

    // Inside: shown, and its code works there only.
    let reason = blocked(&prompt(&h, "s2", &proj, "dotonce", None)).unwrap();
    let code = code_for(&reason, "acme");
    assert_eq!(
        blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)),
        None
    );
    assert_eq!(level_of(&h, "acme"), "warn");
    let reason = blocked(&prompt(&h, "s2", &proj, &format!("no {code}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is off.");
}

#[test]
fn an_invalid_proposal_is_refused() {
    let h = home("invalid");
    let file = h.join("bad.toml");
    for src in [
        "id = \"../acme\"\n",
        "id = \"acme\"\nlevel = \"block\"\n",
        "not toml [",
    ] {
        std::fs::write(&file, src).unwrap();
        let (_, ok) = run(
            &h,
            &h,
            &["rules", "propose", file.to_str().unwrap()],
            None,
            None,
        );
        assert!(!ok, "{src}");
    }
    assert_eq!(stop(&h, "s1", &h, None), "");
}

#[test]
fn claude_running_the_hook_itself_changes_nothing() {
    let h = home("shell");
    rule(&h, "acme", ACME);
    let stop_input = serde_json::json!({"session_id": "s1", "prompt_id": "p2", "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": "Done.", "cwd": h}).to_string();
    let shell: [&[(&str, &str)]; 2] = [
        // Claude's own shell, where it runs commands.
        &[
            ("AI_AGENT", "claude-code_2-1-325_agent"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-desktop"),
        ],
        // A Claude Code that doesn't mark its hooks.
        &[("CLAUDE_CODE_ENTRYPOINT", "cli")],
    ];
    for env in shell {
        for args in [["hook", "stop"], ["hook", "session-start"]] {
            let input = if args[1] == "stop" { &stop_input } else { "{}" };
            let (out, ok) = run_with(&h, &h, &args, Some(input), env);
            assert!(ok);
            assert!(!out.contains("Send no"), "{env:?} {args:?}: {out}");
        }
    }
    assert_eq!(state(&h, "acme"), "not approved");
    // A real code from the real hook can't be used from Claude's shell.
    let code = code_for(&start(&h, "s1", &h, None), "acme");
    let no = serde_json::json!({"session_id": "s1", "prompt": format!("no {code}"), "cwd": h})
        .to_string();
    let (out, ok) = run_with(&h, &h, &["hook", "prompt"], Some(&no), shell[0]);
    assert!(ok);
    assert_eq!(blocked(&out), None, "{out}");
    assert_eq!(state(&h, "acme"), "warn");
}

/// A rule Claude writes from a correction, on in the test home.
fn asked_rule(h: &Path) -> String {
    format!(
        "id = \"acme\"\n[when]\nfolders = [\"{}\"]\n[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline as anchor text.\"\n",
        h.display()
    )
}

/// `rules propose FILE --ask`, with `extra` flags after it.
fn propose_ask(h: &Path, src: &str, extra: &[&str]) -> String {
    let file = h.join("acme.toml");
    std::fs::write(&file, src).unwrap();
    let mut args = vec!["rules", "propose", file.to_str().unwrap(), "--ask"];
    args.extend_from_slice(extra);
    let (out, ok) = run(h, h, &args, None, None);
    assert!(ok, "{out}");
    out
}

/// A session record in the test home's Claude Code folder: one prompt and
/// one reply in `cwd`. Returns its path.
fn transcript(h: &Path, sid: &str, cwd: &Path, reply: Option<&str>) -> PathBuf {
    let dashed: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = h.join(".claude/projects").join(dashed);
    std::fs::create_dir_all(&dir).unwrap();
    let now = days_ago(0);
    let mut text = serde_json::json!({"type": "user", "isSidechain": false, "cwd": cwd, "sessionId": sid, "timestamp": now, "message": {"role": "user", "content": "Add the docs link to the acme readme."}}).to_string() + "\n";
    if let Some(r) = reply {
        text += &(serde_json::json!({"type": "assistant", "isSidechain": false, "cwd": cwd, "sessionId": sid, "timestamp": now, "message": {"role": "assistant", "content": [{"type": "text", "text": r}]}}).to_string() + "\n");
    }
    let f = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&f, text).unwrap();
    f
}

fn proposals(h: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(h.join("dotonce/proposals"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn an_asked_rule_waits_for_a_yes_and_turns_on_at_fix() {
    let h = home("ask-yes");
    transcript(
        &h,
        "old-session",
        &h,
        Some("The steps are at https://example.com/docs today."),
    );
    let out = propose_ask(
        &h,
        &asked_rule(&h),
        &["--note", "Links go inline, as asked."],
    );
    assert!(out.contains("Proposed acme: 1 check."), "{out}");
    assert!(
        out.contains("waits for the user's yes, and turns on at fix"),
        "{out}"
    );
    assert!(out.contains("Last 14 days: 1 hit in 1 session"), "{out}");
    assert!(out.contains("https://example.com/docs"), "{out}");
    assert_eq!(proposals(&h), ["acme.ask.json", "acme.toml"]);

    // The end of the turn shows it and leaves it waiting.
    let notice = stop(&h, "s1", &h, None);
    assert!(
        notice.contains("acme (a rule from your correction): turns on at fix, 1 check."),
        "{notice}"
    );
    assert!(
        notice.contains(&format!("  Scope: {}.", h.display())),
        "{notice}"
    );
    assert!(
        notice.contains("  Last 14 days: 1 hit in 1 session"),
        "{notice}"
    );
    assert!(
        notice.contains("  Claude's note: Links go inline, as asked."),
        "{notice}"
    );
    assert!(notice.contains("Send yes "), "{notice}");
    assert_eq!(state(&h, "acme"), "");
    // Another session's turn ends: it shows the same ask with the same code.
    let first = code_for(&notice, "acme");
    let notice = stop(&h, "s2", &h, None);
    assert_eq!(code_for(&notice, "acme"), first, "{notice}");
    assert_eq!(
        state(&h, "acme"),
        "",
        "the autopilot turned on an asked rule"
    );

    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s2", &h, &format!("yes {code}"), None)).unwrap();
    assert!(
        reason.contains("dotonce: acme is on at fix. Send no "),
        "{reason}"
    );
    assert!(reason.contains("within 7 days to turn it off."), "{reason}");
    assert_eq!(level_of(&h, "acme"), "fix");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.join("dotonce/approvals.json")).unwrap())
            .unwrap();
    assert!(
        !v["sets"]["acme"]["promoted"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "a yes at fix must still let replies that stay broken move it back"
    );
    assert!(proposals(&h).is_empty(), "{:?}", proposals(&h));
    let archived = done(&h);
    assert!(
        archived
            .iter()
            .any(|n| n.starts_with("acme-applied-") && n.ends_with(".toml"))
    );
    assert!(
        archived
            .iter()
            .any(|n| n.starts_with("acme-applied-") && n.ends_with(".ask.json"))
    );

    // `dotonce` lists it as the user's change, and its code undoes it.
    let list = blocked(&prompt(&h, "s2", &h, "dotonce", None)).unwrap();
    assert!(list.contains("dotonce changed 1 rule set."), "{list}");
    assert!(list.contains("acme turned on after your yes."), "{list}");
    let code = code_for(&list, "acme");
    let reason = blocked(&prompt(&h, "s2", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is off.");
    assert_eq!(level_of(&h, "acme"), "off");
}

/// The code after "Send no " in a reply.
fn undo_code(reply: &str) -> String {
    let at = reply.find("Send no ").unwrap_or_else(|| panic!("{reply}")) + "Send no ".len();
    reply[at..at + 4].to_string()
}

/// Whether any string in the JSON value is `text`.
fn holds(v: &serde_json::Value, text: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s == text,
        serde_json::Value::Array(a) => a.iter().any(|x| holds(x, text)),
        serde_json::Value::Object(o) => o.values().any(|x| holds(x, text)),
        _ => false,
    }
}

#[test]
fn a_yes_from_an_earlier_notice_works_after_another_notice_shows_it() {
    let h = home("ask-earlier");
    propose_ask(&h, &asked_rule(&h), &[]);
    let first = code_for(&stop(&h, "s1", &h, None), "acme");

    // A second session shows the same ask at its start, and again when the
    // user sends dotonce or a wrong code. The code stays the same.
    let again = start(&h, "s2", &h, None);
    assert_eq!(code_for(&again, "acme"), first, "{again}");
    let list = blocked(&prompt(&h, "s2", &h, "dotonce", None)).unwrap();
    assert_eq!(code_for(&list, "acme"), first, "{list}");
    let wrong = blocked(&prompt(
        &h,
        "s2",
        &h,
        &format!("yes {}", other_code(&first)),
        None,
    ))
    .unwrap();
    assert!(wrong.contains("that code isn't current"), "{wrong}");
    assert_eq!(code_for(&wrong, "acme"), first, "{wrong}");
    assert_eq!(state(&h, "acme"), "");

    // chat.json keeps only a salted hash of it.
    let store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.join("dotonce/chat.json")).unwrap())
            .unwrap();
    assert!(!holds(&store, &first), "{store}");

    // The yes from the first session's notice works in the second.
    let reason = blocked(&prompt(&h, "s2", &h, &format!("yes {first}"), None)).unwrap();
    assert!(
        reason.contains("dotonce: acme is on at fix. Send no "),
        "{reason}"
    );
    assert_eq!(level_of(&h, "acme"), "fix");

    // So does the undo code from that reply, after dotonce lists it again
    // in another session.
    let undo = undo_code(&reason);
    let list = blocked(&prompt(&h, "s3", &h, "dotonce", None)).unwrap();
    assert_eq!(code_for(&list, "acme"), undo, "{list}");
    let reason = blocked(&prompt(&h, "s3", &h, &format!("no {undo}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is off.");
}

#[test]
fn a_proposal_that_changes_between_notices_gets_a_new_code() {
    let h = waiting_at_fix("fix-new-code");
    let first = code_for(&stop(&h, "s1", &h, None), "acme");
    propose(&h, &format!("{ACME_MORE}# again\n"));
    let second = code_for(&start(&h, "s2", &h, None), "acme");
    assert_ne!(second, first);
    let reason = blocked(&prompt(&h, "s2", &h, &format!("yes {first}"), None)).unwrap();
    assert!(reason.contains("that code isn't current"), "{reason}");
    assert_eq!(code_for(&reason, "acme"), second, "{reason}");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("yes {second}"), None)).unwrap();
    assert_eq!(reason, "dotonce: acme is on at fix.");
}

#[test]
fn an_asked_change_to_a_live_set_names_it_and_undoes_to_the_old_version() {
    let h = home("ask-replace");
    let f = rule(&h, "acme", &asked_rule(&h));
    start(&h, "s0", &h, None);
    assert_eq!(level_of(&h, "acme"), "warn");
    let out = propose_ask(
        &h,
        &format!(
            "{}\n[[check]]\nid = \"no-todo\"\nwhy = \"No TODO left in replies.\"\nforbid = [\"TODO\"]\n",
            asked_rule(&h)
        ),
        &[],
    );
    assert!(
        out.contains("Overlap: acme is on now at warn, with 1 check: a yes replaces it."),
        "{out}"
    );
    let notice = stop(&h, "s1", &h, None);
    assert!(
        notice.contains("  Overlap: acme is on now at warn"),
        "{notice}"
    );
    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("yes {code}"), None)).unwrap();
    assert!(
        reason.contains("within 7 days to put the old version back."),
        "{reason}"
    );
    assert_eq!(level_of(&h, "acme"), "fix");
    assert!(std::fs::read_to_string(&f).unwrap().contains("no-todo"));

    let code = reason[reason.find("Send no ").unwrap() + 8..][..4].to_string();
    let reason = blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)).unwrap();
    assert!(
        reason.contains("back to the version before the change"),
        "{reason}"
    );
    assert!(!std::fs::read_to_string(&f).unwrap().contains("no-todo"));
}

#[test]
fn no_drops_an_asked_rule() {
    let h = home("ask-no");
    propose_ask(&h, &asked_rule(&h), &[]);
    let notice = stop(&h, "s1", &h, None);
    let code = code_for(&notice, "acme");
    let reason = blocked(&prompt(&h, "s1", &h, &format!("no {code}"), None)).unwrap();
    assert_eq!(
        reason,
        "dotonce: dropped the proposed acme. Nothing changed."
    );
    assert!(proposals(&h).is_empty(), "{:?}", proposals(&h));
    let archived = done(&h);
    assert!(
        archived
            .iter()
            .any(|n| n.starts_with("acme-dropped-") && n.ends_with(".toml"))
    );
    assert!(
        archived
            .iter()
            .any(|n| n.starts_with("acme-dropped-") && n.ends_with(".ask.json"))
    );
    stop(&h, "s2", &h, None);
    assert_eq!(state(&h, "acme"), "");
}

#[test]
fn an_asked_rule_with_no_answer_goes_after_seven_days() {
    let h = home("ask-expire");
    propose_ask(&h, &asked_rule(&h), &[]);
    let f = h.join("dotonce/proposals/acme.ask.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    v["created"] = days_ago(8).into();
    std::fs::write(&f, v.to_string()).unwrap();

    let notice = stop(&h, "s1", &h, None);
    assert!(!notice.contains("acme"), "{notice}");
    assert!(proposals(&h).is_empty(), "{:?}", proposals(&h));
    let archived = done(&h);
    assert!(
        archived
            .iter()
            .any(|n| n.starts_with("acme-expired-") && n.ends_with(".toml"))
    );
    assert!(
        archived
            .iter()
            .any(|n| n.starts_with("acme-expired-") && n.ends_with(".ask.json"))
    );
    assert_eq!(state(&h, "acme"), "");
}

#[test]
fn a_plain_propose_after_an_ask_takes_the_autopilot_path() {
    let h = home("ask-plain");
    propose_ask(&h, &asked_rule(&h), &[]);
    propose(&h, &asked_rule(&h));
    assert_eq!(proposals(&h), ["acme.toml"]);
    assert!(done(&h).iter().any(|n| n.starts_with("acme-superseded-")));
    let notice = stop(&h, "s1", &h, None);
    assert!(
        notice.contains("dotonce changed 1 rule set by itself."),
        "{notice}"
    );
    assert_eq!(level_of(&h, "acme"), "warn");
}

#[test]
fn a_note_needs_ask_and_a_bad_pattern_fails_at_propose() {
    let h = home("ask-flags");
    let file = h.join("acme.toml");
    std::fs::write(&file, asked_rule(&h)).unwrap();
    let (_, ok) = run(
        &h,
        &h,
        &["rules", "propose", file.to_str().unwrap(), "--note", "x"],
        None,
        None,
    );
    assert!(!ok);
    std::fs::write(
        &file,
        format!(
            "id = \"acme\"\n[when]\nfolders = [\"{}\"]\n[[check]]\nid = \"look\"\nwhy = \"x\"\nforbid_regex = '(?=acme)'\n",
            h.display()
        ),
    )
    .unwrap();
    let (_, ok) = run(
        &h,
        &h,
        &["rules", "propose", file.to_str().unwrap(), "--ask"],
        None,
        None,
    );
    assert!(
        !ok,
        "a pattern Rust regex can't compile must fail at propose"
    );
    assert!(!h.join("dotonce/proposals/acme.ask.json").exists());
}

/// What the prompt hook adds for `text`, with `transcript` as the session.
fn prompt_with(h: &Path, text: &str, transcript: &Path, entry: Option<&str>) -> String {
    hook(
        h,
        "prompt",
        serde_json::json!({"session_id": "s1", "prompt": text, "cwd": h, "transcript_path": transcript}),
        entry,
    )
}

#[test]
fn a_correction_after_a_reply_gets_the_rule_note() {
    let h = home("note");
    let fresh = transcript(&h, "fresh", &h, None);
    let replied = transcript(&h, "replied", &h, Some("Here is the acme readme."));
    let note = "This prompt may be a correction.";
    let correction = "Why did you paste a bare link?";

    assert!(!prompt_with(&h, correction, &fresh, None).contains(note));
    let out = prompt_with(&h, correction, &replied, None);
    assert!(out.contains(note), "{out}");
    assert!(out.contains("rules propose FILE --ask --note"), "{out}");
    assert!(
        !prompt_with(
            &h,
            "Add a link to example.com in the acme readme.",
            &replied,
            None
        )
        .contains(note)
    );
    assert!(!prompt_with(&h, correction, &replied, Some("sdk-cli")).contains(note));
}
