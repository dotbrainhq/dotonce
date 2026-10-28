use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn home(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dotonce-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(home: &Path, args: &[&str], stdin: Option<&str>) -> (String, bool) {
    run_in(home, &std::env::current_dir().unwrap(), args, stdin)
}

fn run_in(home: &Path, cwd: &Path, args: &[&str], stdin: Option<&str>) -> (String, bool) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(args)
        .current_dir(cwd)
        .env("DOTONCE_HOME", home.join("dotonce"))
        .env("HOME", home)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        out.status.success(),
    )
}

fn context(out: &str, event: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], event);
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn init_save_and_search() {
    let h = home("save");
    assert!(run(&h, &["init"], None).1);
    let (path, ok) = run(
        &h,
        &[
            "save",
            "The staging site is acme.example.com",
            "--source",
            "test",
        ],
        None,
    );
    assert!(ok);
    assert!(path.trim().ends_with(".md"), "{path}");
    let (out, ok) = run(&h, &["search", "which staging site"], None);
    assert!(ok);
    assert!(out.contains("acme.example.com"), "{out}");
    let (json, ok) = run(&h, &["search", "which staging site", "--json"], None);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["path"], path.trim());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn index_reports_changes_and_rebuilds() {
    let h = home("index");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    let (out, ok) = run(&h, &["index"], None);
    assert!(ok);
    assert_eq!(out.trim(), "added 0, updated 0, removed 0");
    let (out, ok) = run(&h, &["index", "--rebuild"], None);
    assert!(ok);
    assert_eq!(out.trim(), "added 1, updated 0, removed 0");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn prompt_hook_adds_matching_facts() {
    let h = home("prompt");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    let (out, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#),
    );
    assert!(ok);
    let ctx = context(&out, "UserPromptSubmit");
    assert!(
        ctx.starts_with("<dotonce>") && ctx.contains("acme.example.com"),
        "{ctx}"
    );
    let (none, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"quantum chromodynamics","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(none.trim().is_empty(), "{none}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn hooks_never_fail_and_log_errors() {
    let h = home("fail");
    let (out, ok) = run(&h, &["hook", "prompt"], Some("not json"));
    assert!(ok);
    assert!(out.trim().is_empty());
    assert!(h.join("dotonce/errors.log").exists());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn unknown_hook_event_is_logged_not_fatal() {
    let h = home("event");
    let (out, ok) = run(&h, &["hook", "bogus"], Some(r#"{"cwd":"/tmp"}"#));
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("bogus"), "{log}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn hook_works_without_a_config_and_logs_a_broken_one() {
    let h = home("noconfig");
    let (out, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    assert!(!h.join("dotonce/index.sqlite").exists());
    let (out, ok) = run(
        &h,
        &["hook", "session-start"],
        Some(r#"{"source":"startup","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    assert!(!h.join("dotonce").exists());

    std::fs::create_dir_all(h.join("dotonce")).unwrap();
    std::fs::write(h.join("dotonce/config.toml"), "stores = 3\n[[").unwrap();
    let (out, ok) = run(
        &h,
        &["hook", "session-start"],
        Some(r#"{"source":"startup","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("config.toml"), "{log}");
    std::fs::remove_dir_all(&h).unwrap();
}

/// Initializes a home whose config has `private_paths` inside `[[stores]]`,
/// where it does nothing.
fn home_with_misplaced_private_paths(name: &str) -> PathBuf {
    let h = home(name);
    run(&h, &["init"], None);
    let cfg = h.join("dotonce/config.toml");
    let mut src = std::fs::read_to_string(&cfg).unwrap();
    src.push_str("private_paths = [\"/work\"]\n");
    std::fs::write(&cfg, src).unwrap();
    h
}

#[test]
fn misplaced_config_key_in_a_hook_is_logged_not_fatal() {
    let h = home_with_misplaced_private_paths("typohook");
    let (out, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("private_paths"), "{log}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn misplaced_config_key_is_a_clear_cli_error() {
    let h = home_with_misplaced_private_paths("typocli");
    let out = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(["search", "staging"])
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", &h)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("config.toml") && err.contains("private_paths"),
        "{err}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn locked_index_gives_no_output_and_logs() {
    let h = home("locked");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    let lock = rusqlite::Connection::open(h.join("dotonce/index.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
    // A new file means the hook's sync has to write, and the lock stops it.
    std::fs::write(
        h.join("dotonce/notes/second.md"),
        "---\nname: second\ndescription: The staging site moved to acme2.example.com\n---\n",
    )
    .unwrap();
    let (out, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("locked"), "{log}");
    lock.execute_batch("ROLLBACK").unwrap();
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn racing_hooks_on_a_fresh_home_both_add_facts() {
    let input = r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#;
    for round in 0..10 {
        let h = home(&format!("race{round}"));
        run(&h, &["init"], None);
        std::fs::write(
            h.join("dotonce/notes/staging.md"),
            "---\nname: staging\ndescription: The staging site is acme.example.com\n---\n",
        )
        .unwrap();
        assert!(!h.join("dotonce/index.sqlite").exists());
        let hooks: Vec<_> = (0..2)
            .map(|_| {
                let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
                    .args(["hook", "prompt"])
                    .env("DOTONCE_HOME", h.join("dotonce"))
                    .env("HOME", &h)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(input.as_bytes())
                    .unwrap();
                child
            })
            .collect();
        for child in hooks {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let out = String::from_utf8_lossy(&out.stdout);
            let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap_or_default();
            assert!(
                context(&out, "UserPromptSubmit").contains("acme.example.com"),
                "round {round}: {out} {log}"
            );
        }
        std::fs::remove_dir_all(&h).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn unreadable_store_gives_no_output() {
    use std::os::unix::fs::PermissionsExt;
    let h = home("unreadable");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    let notes = h.join("dotonce/notes");
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root reads the folder anyway, so there is nothing to test.
    if std::fs::read_dir(&notes).is_ok() {
        std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&h).unwrap();
        return;
    }
    let (out, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#),
    );
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn session_start_lists_pinned_facts_and_private_facts_stay_private() {
    let h = home("session");
    run(&h, &["init"], None);
    run(
        &h,
        &["save", "Always reply in short lines", "--pinned"],
        None,
    );
    run(
        &h,
        &["save", "The private launch date is Thursday", "--private"],
        None,
    );
    let (out, ok) = run(
        &h,
        &["hook", "session-start"],
        Some(r#"{"source":"compact","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(context(&out, "SessionStart").contains("short lines"));
    let (hidden, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"private launch date","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(!hidden.contains("Thursday"), "{hidden}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn session_start_without_pinned_facts_prints_nothing() {
    let h = home("nopinned");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    let (out, ok) = run(
        &h,
        &["hook", "session-start"],
        Some(r#"{"source":"startup","cwd":"/tmp"}"#),
    );
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_big_budget_still_fits_claude_codes_context_cap() {
    let h = home("bigbudget");
    run(&h, &["init"], None);
    let cfg = h.join("dotonce/config.toml");
    let src = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("prompt_budget = 1500", "prompt_budget = 100000")
        .replace("session_budget = 1500", "session_budget = 100000");
    std::fs::write(&cfg, src).unwrap();
    let filler = "the staging site runs acme checks ".repeat(60);
    for i in 0..8 {
        std::fs::write(
            h.join(format!("dotonce/notes/staging-{i}.md")),
            format!(
                "---\nname: staging-{i}\ndescription: Staging note {i}: {filler}\npinned: true\n---\n{filler}\n"
            ),
        )
        .unwrap();
    }
    for (event, name, input) in [
        (
            "prompt",
            "UserPromptSubmit",
            r#"{"prompt":"where does the staging site run acme checks?","cwd":"/tmp"}"#,
        ),
        (
            "session-start",
            "SessionStart",
            r#"{"source":"startup","cwd":"/tmp"}"#,
        ),
    ] {
        let (out, ok) = run(&h, &["hook", event], Some(input));
        assert!(ok);
        let ctx = context(&out, name);
        assert!(ctx.contains("staging-"), "{event}: {ctx}");
        assert!(
            ctx.chars().count() < 10_000,
            "{event}: {} chars",
            ctx.chars().count()
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn save_rejects_empty_text_and_names() {
    let h = home("emptysave");
    run(&h, &["init"], None);
    for args in [
        &["save", ""][..],
        &["save", "   "],
        &["save", "The staging site is acme.example.com", "--name", ""],
        &[
            "save",
            "The staging site is acme.example.com",
            "--name",
            " ",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_dotonce"))
            .args(args)
            .env("DOTONCE_HOME", h.join("dotonce"))
            .env("HOME", &h)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("empty"), "{args:?}: {err}");
    }
    let saved = std::fs::read_dir(h.join("dotonce/notes")).unwrap().count();
    assert_eq!(saved, 0);
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn save_hint_names_the_running_binary() {
    let h = home("hint");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    let (out, ok) = run(
        &h,
        &["hook", "prompt"],
        Some(r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#),
    );
    assert!(ok);
    let ctx = context(&out, "UserPromptSubmit");
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_dotonce")).unwrap();
    let hint = format!(
        "run: {} save \"<one line: the rule or fact>\"",
        exe.display()
    );
    assert!(ctx.contains(&hint), "{ctx}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn private_facts_show_inside_a_private_path() {
    let h = home("privpath");
    run(&h, &["init"], None);
    let work = h.join("work");
    std::fs::create_dir_all(work.join("app")).unwrap();
    let cfg = h.join("dotonce/config.toml");
    let src = std::fs::read_to_string(&cfg).unwrap().replace(
        "private_paths = []",
        &format!("private_paths = [{:?}]", work.to_string_lossy()),
    );
    std::fs::write(&cfg, src).unwrap();
    run(
        &h,
        &["save", "The private launch date is Thursday", "--private"],
        None,
    );
    let input = serde_json::json!({"prompt": "private launch date", "cwd": work.join("app")});
    let (out, ok) = run(&h, &["hook", "prompt"], Some(&input.to_string()));
    assert!(ok);
    assert!(
        context(&out, "UserPromptSubmit").contains("Thursday"),
        "{out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

/// Initializes a home whose config lists `work` as a private path.
fn home_with_private_path(name: &str) -> (PathBuf, PathBuf) {
    let h = home(name);
    run(&h, &["init"], None);
    let work = h.join("work");
    std::fs::create_dir_all(work.join("proj")).unwrap();
    let cfg = h.join("dotonce/config.toml");
    let src = std::fs::read_to_string(&cfg).unwrap().replace(
        "private_paths = []",
        &format!("private_paths = [{:?}]", work.to_string_lossy()),
    );
    std::fs::write(&cfg, src).unwrap();
    (h, work.join("proj"))
}

fn prompt_from(h: &Path, cwd: &Path, prompt: &str) -> String {
    let input = serde_json::json!({"prompt": prompt, "cwd": cwd});
    let (out, ok) = run(h, &["hook", "prompt"], Some(&input.to_string()));
    assert!(ok);
    out
}

#[test]
fn save_inside_a_private_path_is_private_unless_public() {
    let (h, proj) = home_with_private_path("savepriv");
    let (_, ok) = run_in(&h, &proj, &["save", "The launch code is tangerine"], None);
    assert!(ok);
    let out = prompt_from(&h, Path::new("/tmp"), "what is the launch code?");
    assert!(!out.contains("tangerine"), "{out}");
    let out = prompt_from(&h, &proj, "what is the launch code?");
    assert!(out.contains("tangerine"), "{out}");

    let (_, ok) = run_in(
        &h,
        &proj,
        &["save", "The release branch is mango", "--public"],
        None,
    );
    assert!(ok);
    let out = prompt_from(&h, Path::new("/tmp"), "which release branch?");
    assert!(out.contains("mango"), "{out}");

    let (_, ok) = run_in(&h, &proj, &["save", "x", "--public", "--private"], None);
    assert!(!ok);
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs dotonce in `cwd` with neither HOME nor DOTONCE_HOME set.
fn run_homeless(cwd: &Path, args: &[&str], stdin: &str) -> (String, String, bool) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(args)
        .current_dir(cwd)
        .env_remove("HOME")
        .env_remove("DOTONCE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

#[test]
fn no_home_never_writes_into_the_project() {
    let proj = home("nohome");
    for event in ["prompt", "session-start", "stop"] {
        let (out, _, ok) = run_homeless(
            &proj,
            &["hook", event],
            r#"{"prompt":"where is the staging site?","cwd":"/tmp"}"#,
        );
        assert!(ok, "{event}");
        assert!(out.trim().is_empty(), "{event}: {out}");
    }
    let (_, _, ok) = run_homeless(&proj, &["hook", "prompt"], "not json");
    assert!(ok);
    for args in [
        &["init"][..],
        &["save", "The staging site is acme.example.com"],
        &["search", "staging"],
        &["index"],
    ] {
        let (_, err, ok) = run_homeless(&proj, args, "");
        assert!(!ok, "{args:?}");
        assert!(err.contains("HOME"), "{args:?}: {err}");
    }
    let left: Vec<_> = std::fs::read_dir(&proj).unwrap().flatten().collect();
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir_all(&proj).unwrap();
}

#[test]
fn hook_config_points_at_this_binary() {
    let h = home("config");
    let (out, ok) = run(&h, &["hook-config"], None);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    for (event, arg) in [
        ("UserPromptSubmit", "prompt"),
        ("SessionStart", "session-start"),
    ] {
        let cmd = v["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.ends_with(&format!("dotonce hook {arg}")), "{cmd}");
        assert!(
            Path::new(cmd.split(" hook ").next().unwrap()).is_absolute(),
            "{cmd}"
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs dotonce in `cwd` and returns stdout, stderr and whether it exited 0.
fn run_full(home: &Path, cwd: &Path, args: &[&str], stdin: Option<&str>) -> (String, String, bool) {
    run_env(home, cwd, args, stdin, &[])
}

/// The variables Claude Code sets for its Bash tool. This test process runs
/// inside Claude Code too, so every run removes them unless a test sets one.
const AGENT_VARS: [&str; 2] = ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"];

/// Like `run_full`, with extra environment variables set.
fn run_env(
    home: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> (String, String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dotonce"));
    cmd.args(args)
        .current_dir(cwd)
        .env("DOTONCE_HOME", home.join("dotonce"))
        .env("HOME", home);
    for v in AGENT_VARS {
        cmd.env_remove(v);
    }
    let mut child = cmd
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(s) = stdin {
        let _ = child.stdin.take().unwrap().write_all(s.as_bytes());
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

/// Writes `<home>/dotonce/rules/<id>.toml` with `id = "<id>"` and then `body`.
fn add_set(h: &Path, id: &str, body: &str) {
    let dir = h.join("dotonce/rules");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.toml")),
        format!("id = \"{id}\"\n{body}"),
    )
    .unwrap();
}

#[test]
fn rules_new_writes_a_starter_that_checks_clean() {
    let h = home("rulesnew");
    let (out, ok) = run(&h, &["rules", "new", "acme-replies", "--drafts"], None);
    assert!(ok, "{out}");
    let path = h.join("dotonce/rules/acme-replies.toml");
    assert_eq!(out.trim().lines().last().unwrap(), path.to_string_lossy());
    let src = std::fs::read_to_string(&path).unwrap();
    assert!(src.contains("[draft]\nfence"), "{src}");
    let (out, ok) = run(&h, &["rules", "check"], None);
    assert!(ok, "{out}");
    assert!(out.contains("ok     acme-replies (7 checks)"), "{out}");
    let (_, err, ok) = run_full(&h, &h, &["rules", "new", "acme-replies", "--drafts"], None);
    assert!(!ok);
    assert!(err.contains("already exists"), "{err}");
    let (_, err, ok) = run_full(&h, &h, &["rules", "new", "Acme Replies"], None);
    assert!(!ok);
    assert!(err.contains("a-z"), "{err}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_new_asks_drafts_or_whole_replies() {
    let h = home("rulesask");
    let (out, ok) = run(&h, &["rules", "new", "acme-chat"], Some("w\n"));
    assert!(ok, "{out}");
    assert!(out.contains("whole replies"), "{out}");
    let src = std::fs::read_to_string(h.join("dotonce/rules/acme-chat.toml")).unwrap();
    assert!(!src.contains("\n[draft]"), "{src}");
    let (_, ok) = run(&h, &["rules", "new", "acme-notes"], Some("\n"));
    assert!(ok);
    let src = std::fs::read_to_string(h.join("dotonce/rules/acme-notes.toml")).unwrap();
    assert!(src.contains("\n[draft]"), "{src}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_check_lists_broken_files_and_fails() {
    let h = home("rulescheck");
    let (out, ok) = run(&h, &["rules", "check"], None);
    assert!(ok, "{out}");
    assert!(out.contains("no rule sets in"), "{out}");
    add_set(&h, "good", "");
    add_set(&h, "bad", "[[check]]\nid = \"a\"\nforbid = [\"x\"]\n");
    let (out, err, ok) = run_full(&h, &h, &["rules", "check"], None);
    assert!(!ok);
    assert!(out.contains("ok     good (0 checks)"), "{out}");
    assert!(
        out.contains("bad.toml") && out.contains("check a has no why"),
        "{out}"
    );
    assert!(err.contains("1 rule file"), "{err}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_test_runs_a_sets_checks_on_a_file() {
    let h = home("rulestest");
    add_set(
        &h,
        "acme-replies",
        "[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"no-stock-wait-opener\"\nforbid = [\"sorry for the wait\"]\nwhy = \"Open with a line from the thread.\"\n[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline as anchor text.\"\n",
    );
    let file = h.join("reply.md");
    std::fs::write(
        &file,
        "Draft:\n```markdown\nHi Sam,\n\nSorry for the wait. See https://example.com/guide\n```\n",
    )
    .unwrap();
    let f = file.to_string_lossy().to_string();
    let (out, ok) = run(&h, &["rules", "test", "acme-replies", &f], None);
    assert!(ok, "{out}");
    assert!(
        out.contains("no-stock-wait-opener: \"Sorry for the wait. See https://example.com/guide\""),
        "{out}"
    );
    assert!(out.contains("  Links go inline as anchor text."), "{out}");
    let (out, ok) = run(&h, &["rules", "test", "acme-replies", &f, "--json"], None);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["drafts"], 1);
    assert_eq!(v["failures"].as_array().unwrap().len(), 2);
    assert_eq!(v["failures"][1]["check"], "inline-links");
    std::fs::write(&file, "No drafts here.").unwrap();
    let (out, _) = run(&h, &["rules", "test", "acme-replies", &f], None);
    assert!(out.contains("no drafts in"), "{out}");
    std::fs::write(&file, "```markdown\nHi Sam,\nThanks for the details.\n```").unwrap();
    let (out, _) = run(&h, &["rules", "test", "acme-replies", &f], None);
    assert!(out.contains("passes"), "{out}");
    let (_, err, ok) = run_full(&h, &h, &["rules", "test", "missing", &f], None);
    assert!(!ok);
    assert!(err.contains("no rule set missing"), "{err}");
    std::fs::remove_dir_all(&h).unwrap();
}

/// Records an approval the way `dotonce rules approve` does, for a file's text.
fn approve_src(h: &Path, id: &str, src: &str, level: &str) {
    let path = h.join("dotonce/approvals.json");
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({"sets": {}}));
    v["sets"][id] = serde_json::json!({
        "hash": dotonce::approvals::hash(src),
        "level": level,
        "off": false,
        "approved": "2026-10-28T00:00:00Z",
        "changed": "2026-10-28T00:00:00Z"
    });
    std::fs::create_dir_all(h.join("dotonce")).unwrap();
    std::fs::write(path, v.to_string()).unwrap();
}

/// Approves `<home>/dotonce/rules/<id>.toml` as it is now.
fn approve(h: &Path, id: &str, level: &str) {
    let src = std::fs::read_to_string(h.join(format!("dotonce/rules/{id}.toml"))).unwrap();
    approve_src(h, id, &src, level);
}

#[test]
fn approval_needs_a_terminal_and_tracks_changes() {
    let h = home("approve");
    add_set(
        &h,
        "acme-replies",
        "[[check]]\nid = \"c\"\nwhy = \"w\"\nforbid = [\"x\"]\n",
    );
    let (_, err, ok) = run_full(
        &h,
        &h,
        &["rules", "approve", "acme-replies"],
        Some("acme-replies\n"),
    );
    assert!(!ok);
    assert!(err.contains("terminal"), "{err}");
    assert!(!h.join("dotonce/approvals.json").exists());
    for args in [
        &["rules", "level", "acme-replies", "fix"][..],
        &["rules", "off", "acme-replies"],
    ] {
        let (_, err, ok) = run_full(&h, &h, args, Some("acme-replies\n"));
        assert!(!ok, "{args:?}");
        assert!(
            err.contains("terminal") || err.contains("isn't approved"),
            "{args:?}: {err}"
        );
    }
    let (out, ok) = run(&h, &["rules", "list"], None);
    assert!(ok);
    assert!(
        out.contains("acme-replies") && out.contains("not approved"),
        "{out}"
    );
    approve(&h, "acme-replies", "warn");
    let (out, _) = run(&h, &["rules", "list"], None);
    assert!(out.contains("warn") && out.contains("checks: 1"), "{out}");
    assert!(out.contains("changed 2026-10-28T00:00:00Z"), "{out}");
    let (_, err, ok) = run_full(
        &h,
        &h,
        &["rules", "level", "acme-replies", "fix"],
        Some("acme-replies\n"),
    );
    assert!(!ok);
    assert!(err.contains("terminal"), "{err}");
    let f = h.join("dotonce/rules/acme-replies.toml");
    let mut src = std::fs::read_to_string(&f).unwrap();
    src.push_str("# edited\n");
    std::fs::write(&f, src).unwrap();
    let (out, _) = run(&h, &["rules", "list"], None);
    assert!(out.contains("changed since approval"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_list_hides_private_sets_outside_private_paths() {
    let (h, proj) = home_with_private_path("listpriv");
    add_set(&h, "secret-notes", "private = true\n");
    add_set(&h, "acme-replies", "");
    let (out, _) = run(&h, &["rules", "list"], None);
    assert!(out.contains("acme-replies"), "{out}");
    assert!(!out.contains("secret-notes"), "{out}");
    assert!(out.contains("1 private set(s) hidden"), "{out}");
    let (out, _) = run_in(&h, &proj, &["rules", "list"], None);
    assert!(out.contains("secret-notes"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rule_changes_are_refused_under_an_ai_agent() {
    let h = home("agent");
    add_set(&h, "acme-replies", "");
    let json = h.join("dotonce/approvals.json");
    for var in AGENT_VARS {
        let (_, err, ok) = run_env(
            &h,
            &h,
            &["rules", "approve", "acme-replies"],
            Some("acme-replies\n"),
            &[(var, "1")],
        );
        assert!(!ok, "{var}");
        assert!(
            err.contains("Run this in your own terminal, not through an AI agent."),
            "{var}: {err}"
        );
        assert!(!json.exists(), "{var}");
    }
    approve(&h, "acme-replies", "warn");
    let before = std::fs::read(&json).unwrap();
    for var in AGENT_VARS {
        for args in [
            &["rules", "level", "acme-replies", "fix"][..],
            &["rules", "off", "acme-replies"],
        ] {
            let (_, err, ok) = run_env(&h, &h, args, Some("acme-replies\n"), &[(var, "1")]);
            assert!(!ok, "{var} {args:?}");
            assert!(
                err.contains("not through an AI agent"),
                "{var} {args:?}: {err}"
            );
        }
    }
    assert_eq!(std::fs::read(&json).unwrap(), before);
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_off_on_an_approved_set_needs_a_terminal() {
    let h = home("offterm");
    add_set(&h, "acme-replies", "");
    approve(&h, "acme-replies", "warn");
    let json = h.join("dotonce/approvals.json");
    let before = std::fs::read(&json).unwrap();
    let (_, err, ok) = run_full(
        &h,
        &h,
        &["rules", "off", "acme-replies"],
        Some("acme-replies\n"),
    );
    assert!(!ok);
    assert!(
        err.contains("turn acme-replies off only works in a terminal"),
        "{err}"
    );
    assert_eq!(std::fs::read(&json).unwrap(), before);
    let (out, _) = run(&h, &["rules", "list"], None);
    assert!(out.contains("warn"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs a command under a pseudo-terminal, copying its output to stdout and
/// stdin to it, so tests can reach the approve, level and off prompts.
#[cfg(unix)]
const PTY_DRIVER: &str = r#"
import os, pty, select, sys
pid, fd = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:])
ins = [0]
while True:
    r, _, _ = select.select([fd] + ins, [], [], 30)
    if not r:
        break
    if fd in r:
        try:
            b = os.read(fd, 4096)
        except OSError:
            b = b""
        if not b:
            break
        os.write(1, b)
    if 0 in r:
        b = os.read(0, 4096)
        if b:
            os.write(fd, b)
        else:
            ins = []
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
"#;

/// Runs dotonce under a pty, waits for the confirmation prompt, calls
/// `meanwhile`, then types `typed`. Returns the output and whether it exited
/// 0, or None when python3 isn't installed.
#[cfg(unix)]
fn run_pty(
    h: &Path,
    args: &[&str],
    typed: &str,
    meanwhile: impl FnOnce(),
) -> Option<(String, bool)> {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let mut cmd = Command::new("python3");
    cmd.args(["-I", "-c", PTY_DRIVER, env!("CARGO_BIN_EXE_dotonce")])
        .args(args)
        .current_dir(h)
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", h);
    for v in AGENT_VARS {
        cmd.env_remove(v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&seen).contains("press Return: ") {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(b) => seen.extend(b),
            Err(_) => {
                let _ = child.kill();
                panic!(
                    "no prompt from {args:?}: {}",
                    String::from_utf8_lossy(&seen)
                );
            }
        }
    }
    meanwhile();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(typed.as_bytes()).unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    seen.extend(rx.try_iter().flatten());
    Some((String::from_utf8_lossy(&seen).to_string(), status.success()))
}

/// A `changed` time that is RFC 3339 and from the last minute.
#[cfg(unix)]
fn assert_recent(t: &serde_json::Value) {
    let s = t.as_str().unwrap_or_else(|| panic!("no changed time: {t}"));
    let at = chrono::DateTime::parse_from_rfc3339(s).unwrap();
    let age = chrono::Utc::now().signed_duration_since(at);
    assert!(age.num_seconds().abs() < 60, "{s}");
}

#[cfg(unix)]
#[test]
fn changes_made_while_a_prompt_waits_are_kept() {
    let h = home("waits");
    add_set(&h, "acme-replies", "");
    let json = h.join("dotonce/approvals.json");
    let read = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap()
    };
    let Some((out, ok)) = run_pty(
        &h,
        &["rules", "approve", "acme-replies"],
        "acme-replies\n",
        || approve_src(&h, "beta", "id = \"beta\"\n", "warn"),
    ) else {
        eprintln!("python3 isn't installed, skipping");
        return;
    };
    assert!(ok && out.contains("approved acme-replies"), "{out}");
    let v = read();
    assert_eq!(v["sets"]["acme-replies"]["level"], "warn", "{v}");
    assert_recent(&v["sets"]["acme-replies"]["changed"]);
    assert!(v["sets"]["beta"].is_object(), "{v}");
    let (out, ok) = run_pty(
        &h,
        &["rules", "level", "acme-replies", "fix"],
        "acme-replies\n",
        || approve_src(&h, "gamma", "id = \"gamma\"\n", "warn"),
    )
    .unwrap();
    assert!(ok, "{out}");
    let v = read();
    assert_eq!(v["sets"]["acme-replies"]["level"], "fix", "{v}");
    assert_recent(&v["sets"]["acme-replies"]["changed"]);
    assert!(v["sets"]["gamma"].is_object(), "{v}");
    let (out, ok) = run_pty(
        &h,
        &["rules", "off", "acme-replies"],
        "acme-replies\n",
        || approve_src(&h, "delta", "id = \"delta\"\n", "warn"),
    )
    .unwrap();
    assert!(ok, "{out}");
    let v = read();
    assert_eq!(v["sets"]["acme-replies"]["off"], true, "{v}");
    assert_recent(&v["sets"]["acme-replies"]["changed"]);
    for id in ["beta", "gamma", "delta"] {
        assert!(v["sets"][id].is_object(), "{id}: {v}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn approve_shows_the_checks_and_that_an_off_set_turns_on() {
    let h = home("summary");
    add_set(
        &h,
        "acme-replies",
        "[[check]]\nid = \"no-x\"\nwhy = \"x reads badly\"\nforbid = [\"x\"]\n",
    );
    approve(&h, "acme-replies", "warn");
    let json = h.join("dotonce/approvals.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    v["sets"]["acme-replies"]["off"] = serde_json::json!(true);
    std::fs::write(&json, v.to_string()).unwrap();
    let f = h.join("dotonce/rules/acme-replies.toml");
    std::fs::write(&f, std::fs::read_to_string(&f).unwrap() + "# edited\n").unwrap();
    let (out, err, ok) = run_full(
        &h,
        &h,
        &["rules", "approve", "acme-replies"],
        Some("acme-replies\n"),
    );
    assert!(!ok);
    assert!(err.contains("terminal"), "{err}");
    assert!(out.contains("  no-x (forbid): x reads badly"), "{out}");
    assert!(out.contains("changed since the last approval"), "{out}");
    assert!(out.contains("approving it turns it back on"), "{out}");
    #[cfg(unix)]
    {
        approve(&h, "acme-replies", "warn");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        v["sets"]["acme-replies"]["off"] = serde_json::json!(true);
        std::fs::write(&json, v.to_string()).unwrap();
        let (out, _) = run(&h, &["rules", "list"], None);
        assert!(out.contains(" off "), "{out}");
        if let Some((out, ok)) = run_pty(
            &h,
            &["rules", "approve", "acme-replies"],
            "acme-replies\n",
            || {},
        ) {
            assert!(ok, "{out}");
            assert!(out.contains("approving it turns it back on"), "{out}");
            let (out, _) = run(&h, &["rules", "list"], None);
            assert!(out.contains("warn") && !out.contains(" off "), "{out}");
        }
    }
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs one hook with a JSON input and returns its stdout.
fn hook(h: &Path, event: &str, input: serde_json::Value) -> String {
    let (out, ok) = run(h, &["hook", event], Some(&input.to_string()));
    assert!(ok, "hook {event} failed");
    out
}

const REPLIES: &str = "[when]\nprompt = [\"draft\"]\nskills = [\"writing-support\"]\n[remind]\ntext = \"Links go inline as anchor text.\"\n";

#[test]
fn prompt_hook_adds_reminders_before_facts() {
    let h = home("remindprompt");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    let input = serde_json::json!({"session_id": "s1", "prompt_id": "p1", "prompt": "draft the staging site reply", "cwd": "/tmp"});
    let ctx = context(&hook(&h, "prompt", input.clone()), "UserPromptSubmit");
    let rule = ctx
        .find("[acme-replies]\nLinks go inline as anchor text.")
        .expect(&ctx);
    let fact = ctx.find("acme.example.com").expect(&ctx);
    assert!(
        ctx.starts_with("<dotonce>\nRules from your rule sets"),
        "{ctx}"
    );
    assert!(rule < fact, "{ctx}");
    assert_eq!(ctx.matches("<dotonce>").count(), 1, "{ctx}");
    let ctx = context(&hook(&h, "prompt", input), "UserPromptSubmit");
    assert!(
        !ctx.contains("Links go inline"),
        "the same turn gets the reminder once: {ctx}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_need_approval_and_skip_pasted_text_and_project_folders() {
    let h = home("remindgate");
    add_set(&h, "acme-replies", REPLIES);
    let input = |prompt: &str, sid: &str| serde_json::json!({"session_id": sid, "prompt_id": "p1", "prompt": prompt, "cwd": "/tmp"});
    assert!(
        hook(&h, "prompt", input("draft it", "s1"))
            .trim()
            .is_empty()
    );
    assert!(!h.join("dotonce/sessions").exists());
    approve(&h, "acme-replies", "warn");
    assert!(
        hook(&h, "prompt", input("```\ndraft\n```\nwhat now?", "s2"))
            .trim()
            .is_empty()
    );
    let ctx = context(
        &hook(&h, "prompt", input("draft it", "s3")),
        "UserPromptSubmit",
    );
    assert!(ctx.contains("Links go inline"), "{ctx}");
    std::fs::remove_dir_all(&h).unwrap();

    let h = home("remindproject");
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join(".dotonce/rules")).unwrap();
    let src = format!("id = \"acme-replies\"\n{REPLIES}");
    std::fs::write(proj.join(".dotonce/rules/acme-replies.toml"), &src).unwrap();
    approve_src(&h, "acme-replies", &src, "fix");
    let input = serde_json::json!({"session_id": "s1", "prompt_id": "p1", "prompt": "draft it", "cwd": proj});
    let (out, ok) = run_in(&h, &proj, &["hook", "prompt"], Some(&input.to_string()));
    assert!(ok);
    assert!(
        out.trim().is_empty(),
        "a repo's own rules are never read: {out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn skill_and_expansion_hooks_turn_sets_on_and_compaction_reminds_again() {
    let h = home("remindskill");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    let skill = |sid: &str, tool: &str, name: &str| serde_json::json!({"session_id": sid, "prompt_id": "p1", "hook_event_name": "PostToolUse", "tool_name": tool, "tool_input": {"skill": name}, "cwd": "/tmp"});
    let ctx = context(
        &hook(
            &h,
            "skill",
            skill("s1", "Skill", "acme-tools:writing-support"),
        ),
        "PostToolUse",
    );
    assert!(ctx.contains("Links go inline as anchor text."), "{ctx}");
    assert!(
        hook(&h, "skill", skill("s1", "Skill", "writing-support"))
            .trim()
            .is_empty()
    );
    assert!(
        hook(&h, "skill", skill("s2", "Read", "writing-support"))
            .trim()
            .is_empty()
    );
    assert!(
        hook(&h, "skill", skill("s2", "Skill", "other-skill"))
            .trim()
            .is_empty()
    );
    let exp = serde_json::json!({"session_id": "s3", "prompt_id": "p1", "hook_event_name": "UserPromptExpansion", "expansion_type": "slash_command", "command_name": "writing-support", "command_args": "", "command_source": "user", "prompt": "/writing-support", "cwd": "/tmp"});
    let ctx = context(&hook(&h, "expansion", exp), "UserPromptExpansion");
    assert!(ctx.contains("Links go inline"), "{ctx}");
    let out = hook(
        &h,
        "session-start",
        serde_json::json!({"session_id": "s1", "source": "compact", "cwd": "/tmp"}),
    );
    assert!(
        context(&out, "SessionStart").contains("Links go inline"),
        "{out}"
    );
    let out = hook(
        &h,
        "session-start",
        serde_json::json!({"session_id": "s1", "source": "resume", "cwd": "/tmp"}),
    );
    assert!(out.trim().is_empty(), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn session_start_reports_broken_rule_files_and_cleans_old_sessions() {
    let h = home("sessionrules");
    add_set(&h, "broken", "[[check]]\nid = \"a\"\nforbid = [\"x\"]\n");
    let sessions = h.join("dotonce/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for name in ["old.json", "fresh.json"] {
        std::fs::write(sessions.join(name), "{}").unwrap();
    }
    std::fs::File::options()
        .write(true)
        .open(sessions.join("old.json"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400))
        .unwrap();
    let out = hook(
        &h,
        "session-start",
        serde_json::json!({"session_id": "s1", "source": "startup", "cwd": "/tmp"}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let msg = v["systemMessage"].as_str().unwrap();
    assert!(
        msg.contains("broken.toml") && msg.contains("check a has no why"),
        "{msg}"
    );
    assert!(msg.contains("dotonce rules check"), "{msg}");
    assert!(v.get("hookSpecificOutput").is_none(), "{out}");
    assert!(!sessions.join("old.json").exists());
    assert!(sessions.join("fresh.json").exists());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn private_sets_only_remind_inside_private_paths() {
    let (h, proj) = home_with_private_path("remindpriv");
    add_set(
        &h,
        "secret-notes",
        "private = true\n[when]\nprompt = [\"draft\"]\n[remind]\ntext = \"Keep the launch quiet.\"\n",
    );
    approve(&h, "secret-notes", "warn");
    let out = hook(
        &h,
        "prompt",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "prompt": "draft it", "cwd": "/tmp"}),
    );
    assert!(!out.contains("launch quiet"), "{out}");
    let out = hook(
        &h,
        "prompt",
        serde_json::json!({"session_id": "s2", "prompt_id": "p1", "prompt": "draft it", "cwd": proj}),
    );
    assert!(out.contains("Keep the launch quiet."), "{out}");
    let state = std::fs::read_to_string(h.join("dotonce/sessions/s2.json")).unwrap();
    assert!(!state.contains("draft"), "{state}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn hook_path_never_writes_to_stderr() {
    let h = home("stderr");
    for args in [
        &["hook"][..],
        &["hook", "prompt", "--bogus"],
        &["hook", "bogus"],
        &["hook", "prompt"],
    ] {
        let (out, err, ok) = run_full(&h, &h, args, Some("not json"));
        assert!(ok, "{args:?}");
        assert!(err.is_empty(), "{args:?}: {err}");
        assert!(out.trim().is_empty(), "{args:?}: {out}");
    }
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("hook arguments"), "{log}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn hook_argument_errors_with_stderr_closed_exit_0() {
    let h = home("stderrclosed");
    for args in [&["hook"][..], &["hook", "prompt"]] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dotonce"));
        cmd.args(args)
            .env("DOTONCE_HOME", h.join("dotonce"))
            .env("HOME", &h);
        for v in AGENT_VARS {
            cmd.env_remove(v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Claude Code may close stderr: any write to it now fails.
        drop(child.stderr.take());
        let _ = child.stdin.take().unwrap().write_all(b"not json");
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", out.status);
    }
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("hook arguments"), "{log}");
    assert!(!log.contains("panic"), "{log}");
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs session-start for a session and returns its systemMessage, or "".
fn start_message(h: &Path, sid: &str, cwd: &Path) -> String {
    let out = hook(
        h,
        "session-start",
        serde_json::json!({"session_id": sid, "source": "startup", "cwd": cwd}),
    );
    if out.trim().is_empty() {
        return String::new();
    }
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    v["systemMessage"].as_str().unwrap_or_default().to_string()
}

#[test]
fn session_start_names_approved_sets_that_stopped_running() {
    let h = home("sessionlive");
    let tmp = Path::new("/tmp");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    assert_eq!(start_message(&h, "s1", tmp), "");
    assert!(h.join("dotonce/last-live.json").exists());
    assert_eq!(start_message(&h, "s2", tmp), "", "nothing changed");
    approve(&h, "acme-replies", "fix");
    let msg = start_message(&h, "s3", tmp);
    assert!(
        msg.contains("acme-replies") && msg.contains("from warn to fix"),
        "{msg}"
    );
    assert_eq!(start_message(&h, "s4", tmp), "");
    let f = h.join("dotonce/rules/acme-replies.toml");
    let mut src = std::fs::read_to_string(&f).unwrap();
    src.push_str("# edited\n");
    std::fs::write(&f, &src).unwrap();
    let changed = "Changed since approval, run `dotonce rules approve <id>`: acme-replies.";
    assert_eq!(start_message(&h, "s5", tmp), changed);
    assert_eq!(
        start_message(&h, "s6", tmp),
        changed,
        "reported every session"
    );
    approve(&h, "acme-replies", "fix");
    let path = h.join("dotonce/approvals.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["sets"]["acme-replies"]["off"] = serde_json::json!(true);
    std::fs::write(&path, v.to_string()).unwrap();
    assert_eq!(start_message(&h, "s7", tmp), "Off: acme-replies.");
    std::fs::remove_file(&f).unwrap();
    assert_eq!(start_message(&h, "s8", tmp), "Missing: acme-replies.");
    std::fs::write(&f, "id = \"acme-replies\"\n[[check]]\n").unwrap();
    let msg = start_message(&h, "s9", tmp);
    assert!(msg.starts_with("File errors: acme-replies.toml ("), "{msg}");
    assert_eq!(msg.matches("acme-replies").count(), 1, "{msg}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn session_start_hides_private_set_ids_outside_private_paths() {
    let (h, proj) = home_with_private_path("sessionlivepriv");
    let tmp = Path::new("/tmp");
    add_set(
        &h,
        "secret-notes",
        "private = true\n[remind]\ntext = \"Keep the launch quiet.\"\n",
    );
    approve(&h, "secret-notes", "warn");
    assert_eq!(start_message(&h, "s1", &proj), "");
    let f = h.join("dotonce/rules/secret-notes.toml");
    let mut src = std::fs::read_to_string(&f).unwrap();
    src.push_str("# edited\n");
    std::fs::write(&f, &src).unwrap();
    assert_eq!(start_message(&h, "s2", tmp), "Not running and private: 1.");
    let msg = start_message(&h, "s3", &proj);
    assert!(
        msg.contains("Changed since approval") && msg.contains("secret-notes"),
        "{msg}"
    );
    std::fs::remove_file(&f).unwrap();
    assert_eq!(start_message(&h, "s4", tmp), "Not running and private: 1.");
    std::fs::remove_dir_all(&h).unwrap();
}

/// Turns an approved set off the way `dotonce rules off` does.
fn turn_off(h: &Path, id: &str) {
    let path = h.join("dotonce/approvals.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["sets"][id]["off"] = serde_json::json!(true);
    std::fs::write(&path, v.to_string()).unwrap();
}

fn edit_set(h: &Path, id: &str) {
    let f = h.join(format!("dotonce/rules/{id}.toml"));
    let mut src = std::fs::read_to_string(&f).unwrap();
    src.push_str("# edited\n");
    std::fs::write(&f, src).unwrap();
}

#[test]
fn session_start_groups_the_notice_by_reason() {
    let (h, proj) = home_with_private_path("grouped");
    let tmp = Path::new("/tmp");
    let body = "[remind]\ntext = \"Keep it short.\"\n";
    let private = "private = true\n[remind]\ntext = \"Keep it short.\"\n";
    let public = [
        "off-one",
        "off-two",
        "edited",
        "gone",
        "broken-approved",
        "paused-one",
        "level-up",
        "steady",
    ];
    for id in public {
        add_set(&h, id, body);
        approve(&h, id, "warn");
    }
    for id in ["quiet-off", "quiet-edited", "quiet-broken"] {
        add_set(&h, id, private);
        approve(&h, id, "warn");
    }
    assert_eq!(start_message(&h, "s0", tmp), "");
    for id in ["off-one", "off-two", "quiet-off"] {
        turn_off(&h, id);
    }
    edit_set(&h, "edited");
    edit_set(&h, "quiet-edited");
    std::fs::remove_file(h.join("dotonce/rules/gone.toml")).unwrap();
    add_set(&h, "broken-approved", "[[check]]\n");
    add_set(&h, "quiet-broken", "private = true\n[[check]]\n");
    add_set(&h, "broken-new", "[[check]]\n");
    approve(&h, "level-up", "fix");
    std::fs::create_dir_all(h.join("dotonce/sessions")).unwrap();
    std::fs::write(
        h.join("dotonce/sessions/s1.json"),
        r#"{"paused":["paused-one"]}"#,
    )
    .unwrap();
    let msg = start_message(&h, "s1", tmp);
    println!("SAMPLE (12 sets, outside private_paths):\n{msg}");
    let lines: Vec<&str> = msg.lines().collect();
    assert_eq!(
        lines[..6],
        [
            "Rule set level-up: level changed from warn to fix since the last session.",
            "Off: off-one, off-two.",
            "Changed since approval, run `dotonce rules approve <id>`: edited.",
            "Missing: gone.",
            "Paused in this session: paused-one.",
            "Not running and private: 2.",
        ],
        "{msg}"
    );
    assert_eq!(lines.len(), 7, "{msg}");
    let files = lines[6];
    assert!(
        files.starts_with("File errors: broken-approved.toml (")
            && files.contains("; broken-new.toml (")
            && files.contains(
                ". 1 private rule set file has errors. Run `dotonce rules check` for details."
            ),
        "{files}"
    );
    assert_eq!(msg.matches("broken-approved").count(), 1, "{msg}");
    assert!(!msg.contains("quiet") && !msg.contains("steady"), "{msg}");
    let msg = start_message(&h, "s2", &proj);
    assert!(
        msg.contains("Off: off-one, off-two, quiet-off.")
            && msg.contains(": edited, quiet-edited.")
            && msg.contains("quiet-broken.toml (")
            && !msg.contains("private"),
        "inside private_paths every set is named: {msg}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn broken_private_files_are_only_named_inside_private_paths() {
    let (h, proj) = home_with_private_path("brokenpriv");
    let tmp = Path::new("/tmp");
    add_set(
        &h,
        "secret-notes",
        "[remind]\ntext = \"Keep the launch quiet.\"\n",
    );
    approve(&h, "secret-notes", "warn");
    assert_eq!(start_message(&h, "s1", tmp), "");
    // The record says public, but the broken file now says private.
    add_set(
        &h,
        "secret-notes",
        "private = true\n[[check]]\nid = \"x\"\nforbid = [\"launch day\"]\n",
    );
    let msg = start_message(&h, "s2", tmp);
    assert_eq!(
        msg,
        "1 private rule set file has errors. Run `dotonce rules check` for details.",
    );
    let msg = start_message(&h, "s3", &proj);
    assert!(msg.contains("secret-notes.toml ("), "{msg}");
    // The record now says private, so a fix to the flag alone doesn't name it.
    add_set(&h, "secret-notes", "[[check]]\n");
    assert!(!start_message(&h, "s4", tmp).contains("secret-notes"));
    // A file that can't be read is hidden, whatever the record says.
    std::fs::remove_file(h.join("dotonce/rules/secret-notes.toml")).unwrap();
    add_set(&h, "open-notes", "[remind]\ntext = \"Keep it short.\"\n");
    approve(&h, "open-notes", "warn");
    start_message(&h, "s5", tmp);
    add_set(&h, "open-notes", "[[check]]\n");
    assert!(start_message(&h, "s6", tmp).contains("File errors: open-notes.toml ("));
    let f = h.join("dotonce/rules/open-notes.toml");
    let mut perm = std::fs::metadata(&f).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o000);
    std::fs::set_permissions(&f, perm.clone()).unwrap();
    let msg = start_message(&h, "s7", tmp);
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o644);
    std::fs::set_permissions(&f, perm).unwrap();
    assert!(
        !msg.contains("open-notes") && msg.contains("1 private rule set file has errors"),
        "{msg}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_changed_file_cannot_make_a_private_set_public() {
    let (h, proj) = home_with_private_path("changedpriv");
    let tmp = Path::new("/tmp");
    add_set(
        &h,
        "secret-notes",
        "private = true\n[remind]\ntext = \"Keep the launch quiet.\"\n",
    );
    approve(&h, "secret-notes", "warn");
    assert_eq!(start_message(&h, "s1", &proj), "");
    add_set(
        &h,
        "secret-notes",
        "private = false\n[remind]\ntext = \"Keep the launch quiet.\"\n",
    );
    assert_eq!(start_message(&h, "s2", tmp), "Not running and private: 1.");
    assert_eq!(
        start_message(&h, "s3", tmp),
        "Not running and private: 1.",
        "the changed file doesn't overwrite the record"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_reminder_left_out_at_the_cap_comes_with_the_next_prompt() {
    let h = home("remindcap");
    // About 1,100 tokens each: two don't fit under the 2,000-token cap.
    let long = |word: &str| format!("{word} ").repeat(880);
    for (id, word) in [("first-set", "alpha"), ("second-set", "bravo")] {
        add_set(
            &h,
            id,
            &format!(
                "[when]\nskills = [\"writing-support\"]\n[remind]\ntext = \"{}\"\n",
                long(word)
            ),
        );
        approve(&h, id, "warn");
    }
    let skill = serde_json::json!({"session_id": "s1", "prompt_id": "p1", "tool_name": "Skill", "tool_input": {"skill": "writing-support"}, "cwd": "/tmp"});
    let ctx = context(&hook(&h, "skill", skill), "PostToolUse");
    assert!(
        ctx.contains("[first-set]") && !ctx.contains("[second-set]"),
        "{ctx}"
    );
    let out = hook(
        &h,
        "prompt",
        serde_json::json!({"session_id": "s1", "prompt_id": "p2", "prompt": "what now?", "cwd": "/tmp"}),
    );
    let ctx = context(&out, "UserPromptSubmit");
    assert!(
        ctx.contains("[second-set]") && !ctx.contains("[first-set]"),
        "{ctx}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_check_warns_about_a_reminder_too_long_to_fit() {
    let h = home("checklong");
    let text = "alpha ".repeat(1_300);
    add_set(&h, "wordy", &format!("[remind]\ntext = \"{text}\"\n"));
    add_set(&h, "short", "[remind]\ntext = \"Keep it short.\"\n");
    let (out, ok) = run(&h, &["rules", "check"], None);
    assert!(ok, "{out}");
    assert!(
        out.contains("warn   wordy: its reminder is about") && out.contains("over 1,800"),
        "{out}"
    );
    assert!(!out.contains("warn   short"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn compaction_gives_back_only_this_turns_prompt_sets() {
    let h = home("compactturn");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    add_set(
        &h,
        "acme-folder",
        &format!(
            "[when]\nfolders = [{:?}]\n[remind]\ntext = \"Run the tests first.\"\n",
            proj.to_string_lossy()
        ),
    );
    approve(&h, "acme-folder", "warn");
    let out = hook(
        &h,
        "prompt",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "prompt": "draft it", "cwd": proj}),
    );
    let ctx = context(&out, "UserPromptSubmit");
    assert!(
        ctx.contains("Links go inline") && ctx.contains("Run the tests first"),
        "{ctx}"
    );
    let compact = |pid: Option<&str>| {
        let mut v = serde_json::json!({"session_id": "s1", "source": "compact", "cwd": proj});
        if let Some(p) = pid {
            v["prompt_id"] = serde_json::json!(p);
        }
        context(&hook(&h, "session-start", v), "SessionStart")
    };
    let ctx = compact(None);
    assert!(
        ctx.contains("Run the tests first") && !ctx.contains("Links go inline"),
        "no current turn, so no prompt sets: {ctx}"
    );
    let ctx = compact(Some("p1"));
    assert!(
        ctx.contains("Links go inline"),
        "this turn's prompt set: {ctx}"
    );
    let ctx = compact(Some("p2"));
    assert!(
        ctx.contains("Run the tests first") && !ctx.contains("Links go inline"),
        "a later turn drops the earlier turn's prompt set: {ctx}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_facts_failure_still_gives_the_reminders() {
    let h = home("factsfail");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    let lock = rusqlite::Connection::open(h.join("dotonce/index.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
    // A new file means the hook's sync has to write, and the lock stops it.
    std::fs::write(
        h.join("dotonce/notes/second.md"),
        "---\nname: second\ndescription: The staging site moved to acme2.example.com\n---\n",
    )
    .unwrap();
    let out = hook(
        &h,
        "prompt",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "prompt": "draft the staging site reply", "cwd": "/tmp"}),
    );
    let ctx = context(&out, "UserPromptSubmit");
    assert!(ctx.contains("Links go inline"), "{ctx}");
    assert!(
        !ctx.contains("example.com"),
        "no facts until the index syncs: {ctx}"
    );
    let out = hook(
        &h,
        "session-start",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "source": "compact", "cwd": "/tmp"}),
    );
    assert!(
        context(&out, "SessionStart").contains("Links go inline"),
        "{out}"
    );
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("facts") && log.contains("locked"), "{log}");
    lock.execute_batch("ROLLBACK").unwrap();
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn an_unusable_session_id_still_gives_the_notice() {
    let h = home("badsid");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    turn_off(&h, "acme-replies");
    for source in ["startup", "compact"] {
        let out = hook(
            &h,
            "session-start",
            serde_json::json!({"session_id": "!!!", "source": source, "cwd": "/tmp"}),
        );
        let v: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{source}: {e}: {out}"));
        assert_eq!(v["systemMessage"], "Off: acme-replies.", "{source}: {out}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

const TICKET: &str = "[when]\nskills = [\"writing-support\"]\n[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"no-stock-wait-opener\"\nforbid = [\"sorry for the wait\"]\nwhy = \"Open with a line from the thread.\"\n[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline as anchor text.\"\n";
const BROKEN: &str = "Here is the draft:\n\n```markdown\nHi Sam,\n\nSorry for the wait. The guide is at https://example.com/guide\n```";
const BROKEN_AGAIN: &str = "Second try:\n\n```markdown\nHi Sam,\n\nSorry for the wait. More at https://example.com/more\n```";
const FIXED: &str = "Fixed the opener and the link.\n\n```markdown\nHi Sam,\n\nThanks for the details. The [setup guide](https://example.com/guide) covers it.\n```";

fn ticket_home(name: &str, level: &str) -> PathBuf {
    let h = home(name);
    add_set(&h, "ticket-reply", TICKET);
    approve(&h, "ticket-reply", level);
    h
}

fn use_skill(h: &Path, sid: &str, pid: &str) {
    hook(
        h,
        "skill",
        serde_json::json!({"session_id": sid, "prompt_id": pid, "tool_name": "Skill", "tool_input": {"skill": "writing-support"}, "cwd": "/tmp"}),
    );
}

fn stop_with(
    h: &Path,
    sid: &str,
    pid: &str,
    last: &str,
    active: bool,
    transcript: &Path,
) -> String {
    hook(
        h,
        "stop",
        serde_json::json!({"session_id": sid, "prompt_id": pid, "hook_event_name": "Stop", "stop_hook_active": active, "last_assistant_message": last, "transcript_path": transcript, "cwd": "/tmp"}),
    )
}

fn stop(h: &Path, sid: &str, pid: &str, last: &str, active: bool) -> String {
    stop_with(h, sid, pid, last, active, &h.join("missing.jsonl"))
}

fn sent_back(out: &str) -> String {
    context(out, "Stop")
}

/// (check id, outcome) of each line in a catch log, oldest first.
fn outcomes(h: &Path, file: &str) -> Vec<(String, String)> {
    std::fs::read_to_string(h.join("dotonce").join(file))
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
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

#[test]
fn fix_set_sends_a_broken_draft_back_once_and_logs_the_fix() {
    let h = ticket_home("stopfix", "fix");
    use_skill(&h, "s1", "p1");
    let msg = sent_back(&stop(&h, "s1", "p1", BROKEN, false));
    assert!(
        msg.contains("- no-stock-wait-opener: \"Sorry for the wait. The guide is at https://example.com/guide\". Open with a line from the thread."),
        "{msg}"
    );
    assert!(msg.contains("- inline-links: "), "{msg}");
    assert!(
        msg.ends_with(
            "Write the draft again with these fixed. Add one line saying what you fixed."
        ),
        "{msg}"
    );
    assert!(stop(&h, "s1", "p1", FIXED, true).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[
            ("no-stock-wait-opener", "caught"),
            ("inline-links", "caught"),
            ("no-stock-wait-opener", "fixed"),
            ("inline-links", "fixed"),
        ])
    );
    assert!(
        stop(&h, "s1", "p1", BROKEN_AGAIN, true).trim().is_empty(),
        "a turn is sent back once"
    );
    assert_eq!(outcomes(&h, "catches.jsonl").len(), 4);
    let log = std::fs::read_to_string(h.join("dotonce/catches.jsonl")).unwrap();
    assert!(
        !log.contains("Sam") && !log.contains("example.com"),
        "{log}"
    );
    assert!(
        !sent_back(&stop(&h, "s1", "p2", BROKEN, false)).is_empty(),
        "the next turn is checked again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_second_broken_draft_is_let_through_and_a_missing_one_is_unverified() {
    let h = ticket_home("stopbroken", "fix");
    use_skill(&h, "s1", "p1");
    assert!(!stop(&h, "s1", "p1", BROKEN, false).trim().is_empty());
    assert!(stop(&h, "s1", "p1", BROKEN_AGAIN, true).trim().is_empty());
    use_skill(&h, "s2", "p1");
    assert!(!stop(&h, "s2", "p1", BROKEN, false).trim().is_empty());
    assert!(stop(&h, "s2", "p1", "Saved.", true).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[
            ("no-stock-wait-opener", "caught"),
            ("inline-links", "caught"),
            ("no-stock-wait-opener", "still_broken"),
            ("inline-links", "still_broken"),
            ("no-stock-wait-opener", "caught"),
            ("inline-links", "caught"),
            ("no-stock-wait-opener", "unverified"),
            ("inline-links", "unverified"),
        ])
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn another_stop_hook_continuing_the_turn_does_not_count_as_ours() {
    let h = ticket_home("stopother", "fix");
    use_skill(&h, "s1", "p1");
    assert!(!sent_back(&stop(&h, "s1", "p1", BROKEN, true)).is_empty());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn warn_sets_only_log_and_unused_sets_check_nothing() {
    let h = ticket_home("stopwarn", "warn");
    use_skill(&h, "s1", "p1");
    assert!(stop(&h, "s1", "p1", BROKEN, false).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[
            ("no-stock-wait-opener", "caught"),
            ("inline-links", "caught")
        ])
    );
    assert!(
        stop(&h, "s9", "p1", BROKEN, false).trim().is_empty(),
        "no skill, no checks"
    );
    assert_eq!(outcomes(&h, "catches.jsonl").len(), 2);
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn stop_reads_earlier_drafts_of_the_turn_from_the_session_record() {
    let h = ticket_home("stoprecord", "fix");
    use_skill(&h, "s1", "p1");
    let lines = [
        serde_json::json!({"type": "user", "message": {"role": "user", "content": "Draft the old one"}, "promptId": "p0"}).to_string(),
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "```markdown\nHi Sam,\nOld link https://example.com/old\n```"}]}}).to_string(),
        serde_json::json!({"type": "user", "message": {"role": "user", "content": "Draft the reply"}, "promptId": "p1"}).to_string(),
        "not json".to_string(),
        serde_json::json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "```markdown\nSide link https://example.com/side\n```"}]}}).to_string(),
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "```markdown\nHi Sam,\n\nSorry for the wait.\n```"}]}}).to_string(),
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Write", "input": {}}]}}).to_string(),
        serde_json::json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}, "promptId": "p1"}).to_string(),
    ];
    let record = h.join("session.jsonl");
    std::fs::write(&record, lines.join("\n")).unwrap();
    let msg = sent_back(&stop_with(&h, "s1", "p1", "Saved.", false, &record));
    assert!(msg.contains("no-stock-wait-opener"), "{msg}");
    assert!(
        !msg.contains("inline-links"),
        "the old turn and the sidechain aren't checked: {msg}"
    );
    use_skill(&h, "s2", "p1");
    let msg = sent_back(&stop_with(&h, "s2", "p1", BROKEN, false, &h));
    assert!(
        msg.contains("inline-links"),
        "an unreadable record falls back to the last message: {msg}"
    );
    use_skill(&h, "s3", "p1");
    let out = hook(
        &h,
        "stop",
        serde_json::json!({"session_id": "s3", "prompt_id": "p1", "last_assistant_message": BROKEN, "cwd": "/tmp"}),
    );
    assert!(sent_back(&out).contains("inline-links"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn private_sets_log_to_their_own_file() {
    let (h, proj) = home_with_private_path("stoppriv");
    add_set(
        &h,
        "secret-notes",
        &format!(
            "private = true\n[when]\nfolders = [{:?}]\n[[check]]\nid = \"quiet\"\nforbid = [\"launch date\"]\nwhy = \"Keep it quiet.\"\n",
            proj.to_string_lossy()
        ),
    );
    approve(&h, "secret-notes", "warn");
    let out = hook(
        &h,
        "stop",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "last_assistant_message": "The launch date is Friday.", "cwd": proj}),
    );
    assert!(out.trim().is_empty(), "{out}");
    assert_eq!(
        outcomes(&h, "catches-private.jsonl"),
        pairs(&[("quiet", "caught")])
    );
    assert!(!h.join("dotonce/catches.jsonl").exists());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn stop_without_a_session_id_fails_open() {
    let h = ticket_home("stopnosid", "fix");
    let out = hook(
        &h,
        "stop",
        serde_json::json!({"last_assistant_message": BROKEN, "cwd": "/tmp"}),
    );
    assert!(out.trim().is_empty(), "{out}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("session_id"), "{log}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn the_stop_continuation_in_the_session_record_is_not_a_new_turn() {
    let h = ticket_home("stopcontinue", "fix");
    use_skill(&h, "s1", "p1");
    let user = serde_json::json!({"type": "user", "message": {"role": "user", "content": "Draft the reply"}, "promptId": "p1"}).to_string();
    let said = |t: &str| {
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": t}]}}).to_string()
    };
    let record = h.join("session.jsonl");
    std::fs::write(&record, [user.clone(), said(BROKEN)].join("\n")).unwrap();
    assert!(!sent_back(&stop_with(&h, "s1", "p1", BROKEN, false, &record)).is_empty());
    let continued = [
        user,
        said(BROKEN),
        serde_json::json!({"type": "attachment", "attachment": {"type": "hook_success", "stdout": "{}"}}).to_string(),
        serde_json::json!({"type": "attachment", "attachment": {"type": "hook_additional_context", "content": ["dotonce checked the draft"]}}).to_string(),
        serde_json::json!({"type": "system", "subtype": "stop_hook_summary"}).to_string(),
        said(FIXED),
    ];
    std::fs::write(&record, continued.join("\n")).unwrap();
    assert!(
        stop_with(&h, "s1", "p1", FIXED, true, &record)
            .trim()
            .is_empty()
    );
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[
            ("no-stock-wait-opener", "caught"),
            ("inline-links", "caught"),
            ("no-stock-wait-opener", "fixed"),
            ("inline-links", "fixed"),
        ]),
        "the old draft before the send-back isn't checked again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn display_hook_holds_a_broken_draft_back() {
    let h = ticket_home("display", "fix");
    use_skill(&h, "s1", "p1");
    let shown = |out: &str| -> Option<String> {
        if out.trim().is_empty() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "MessageDisplay");
        Some(
            v["hookSpecificOutput"]["displayContent"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let batch = |mid: &str, i: u32, last: bool, delta: &str| {
        hook(
            &h,
            "display",
            serde_json::json!({"session_id": "s1", "turn_id": "p1", "message_id": mid, "index": i, "final": last, "delta": delta}),
        )
    };
    assert_eq!(
        shown(&batch("m1", 0, false, "Draft:\n```markdown\nHi Sam,\n")).as_deref(),
        Some("Draft:\n")
    );
    let out = shown(&batch("m1", 1, true, "Sorry for the wait.\n```\n")).unwrap();
    assert!(out.starts_with("(dotonce held back a draft"), "{out}");
    assert_eq!(batch("m2", 0, true, "Plain reply.\n"), "");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn display_hook_never_loses_held_lines() {
    let h = ticket_home("displaysafe", "fix");
    use_skill(&h, "s1", "p1");
    let batch = |input: serde_json::Value| -> Option<String> {
        let out = hook(&h, "display", input);
        if out.trim().is_empty() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        Some(
            v["hookSpecificOutput"]["displayContent"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let at = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "message_id": mid, "index": i, "final": last, "delta": delta});
    let open = "Draft:\n```markdown\nHi Sam,\n";
    let held = "```markdown\nHi Sam,\nSorry for the wait.\n";

    assert_eq!(batch(at("m1", 0, false, open)).as_deref(), Some("Draft:\n"));
    let approvals = h.join("dotonce/approvals.json");
    let saved = std::fs::read_to_string(&approvals).unwrap();
    std::fs::write(&approvals, "not json").unwrap();
    assert_eq!(
        batch(at("m1", 1, false, "Sorry for the wait.\n")).as_deref(),
        Some(held),
        "a hook error shows what was held"
    );
    std::fs::write(&approvals, saved).unwrap();
    assert_eq!(
        batch(at("m1", 2, true, "```\n")),
        None,
        "the rest of the message shows as it is"
    );
    assert!(
        std::fs::read_to_string(h.join("dotonce/errors.log"))
            .unwrap()
            .contains("display")
    );

    assert_eq!(batch(at("m2", 0, false, open)).as_deref(), Some("Draft:\n"));
    assert_eq!(
        batch(at("m2", 2, false, "Sorry for the wait.\n")).as_deref(),
        Some(held),
        "a skipped batch shows what was held"
    );
    assert_eq!(batch(at("m2", 3, true, "```\n")), None);

    let no_final =
        |delta: &str| serde_json::json!({"session_id": "s1", "message_id": "m3", "delta": delta});
    assert_eq!(
        batch(no_final(open)),
        None,
        "without a final flag nothing is held past the batch"
    );
    assert_eq!(batch(no_final("Sorry for the wait.\n```\n")), None);
    std::fs::remove_dir_all(&h).unwrap();
}

/// What a display hook call puts on screen, or None when the batch shows as it is.
fn display_out(h: &Path, input: serde_json::Value) -> Option<String> {
    let out = hook(h, "display", input);
    if out.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "MessageDisplay");
    Some(
        v["hookSpecificOutput"]["displayContent"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn a_draft_after_the_send_back_always_shows() {
    let h = ticket_home("displayagain", "fix");
    use_skill(&h, "s1", "p1");
    assert!(!sent_back(&stop(&h, "s1", "p1", BROKEN, false)).is_empty());
    let again = "Second try:\n```markdown\nHi Sam,\nSorry for the wait.\n```\n";
    assert_eq!(
        display_out(
            &h,
            serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": "m2", "index": 0, "final": true, "delta": again})
        ),
        None,
        "Stop won't send this draft back, so it shows in full"
    );
    assert_eq!(
        display_out(
            &h,
            serde_json::json!({"session_id": "s1", "message_id": "m3", "index": 0, "final": true, "delta": again})
        ),
        None,
        "without a prompt_id the session's turn is used"
    );
    use_skill(&h, "s1", "p2");
    let out = display_out(
        &h,
        serde_json::json!({"session_id": "s1", "prompt_id": "p2", "message_id": "m4", "index": 0, "final": true, "delta": again}),
    )
    .unwrap();
    assert!(out.contains("(dotonce held back a draft"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn an_interrupted_draft_shows_with_the_next_message() {
    let h = ticket_home("displaystop", "fix");
    use_skill(&h, "s1", "p1");
    let at = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": mid, "index": i, "final": last, "delta": delta});
    let note = "(dotonce: an earlier draft, not checked because the reply was interrupted)\n";
    assert_eq!(
        display_out(
            &h,
            at(
                "m1",
                0,
                false,
                "Draft:\n```markdown\nHi Sam,\nSorry for the wa"
            )
        )
        .as_deref(),
        Some("Draft:\n")
    );
    assert_eq!(
        display_out(
            &h,
            at("m2", 0, false, "Plain reply.\n```markdown\nHi Sam,\n")
        ),
        Some(format!(
            "{note}```markdown\nHi Sam,\nSorry for the wa\nPlain reply.\n"
        )),
        "the held lines show first, then the new message, whose draft is held"
    );
    assert_eq!(
        display_out(&h, at("m2", 1, true, "Thanks for the details.\n```\n")).as_deref(),
        Some("```markdown\nHi Sam,\nThanks for the details.\n```\n")
    );
    assert_eq!(
        display_out(&h, at("m3", 0, true, "Plain reply.\n")),
        None,
        "nothing is shown twice"
    );
    std::fs::remove_dir_all(&h).unwrap();
}
