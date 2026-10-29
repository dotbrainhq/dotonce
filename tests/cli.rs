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
    assert!(
        out.contains("1 private rule set hidden; run this inside a private folder to see it"),
        "{out}"
    );
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
    // A binary that stops reading would hold the pty open, so the wait has
    // a deadline too.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{args:?} still running 20 s after the typed line: {}",
                String::from_utf8_lossy(&seen)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
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

const REPLIES: &str = "[when]\nprompt = [\"draft\"]\nskills = [\"acme-writer\"]\n[remind]\ntext = \"Links go inline as anchor text.\"\n";

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
        &hook(&h, "skill", skill("s1", "Skill", "acme-tools:acme-writer")),
        "PostToolUse",
    );
    assert!(ctx.contains("Links go inline as anchor text."), "{ctx}");
    assert!(
        hook(&h, "skill", skill("s1", "Skill", "acme-writer"))
            .trim()
            .is_empty()
    );
    assert!(
        hook(&h, "skill", skill("s2", "Read", "acme-writer"))
            .trim()
            .is_empty()
    );
    assert!(
        hook(&h, "skill", skill("s2", "Skill", "other-skill"))
            .trim()
            .is_empty()
    );
    let exp = serde_json::json!({"session_id": "s3", "prompt_id": "p1", "hook_event_name": "UserPromptExpansion", "expansion_type": "slash_command", "command_name": "acme-writer", "command_args": "", "command_source": "user", "prompt": "/acme-writer", "cwd": "/tmp"});
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
                "[when]\nskills = [\"acme-writer\"]\n[remind]\ntext = \"{}\"\n",
                long(word)
            ),
        );
        approve(&h, id, "warn");
    }
    let skill = serde_json::json!({"session_id": "s1", "prompt_id": "p1", "tool_name": "Skill", "tool_input": {"skill": "acme-writer"}, "cwd": "/tmp"});
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

const ACME_MAIL: &str = "[when]\nskills = [\"acme-writer\"]\n[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"no-stock-wait-opener\"\nforbid = [\"sorry for the wait\"]\nwhy = \"Open with a line from the thread.\"\n[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline as anchor text.\"\n";
const BROKEN: &str = "Here is the draft:\n\n```markdown\nHi Sam,\n\nSorry for the wait. The guide is at https://example.com/guide\n```";
const BROKEN_AGAIN: &str = "Second try:\n\n```markdown\nHi Sam,\n\nSorry for the wait. More at https://example.com/more\n```";
const FIXED: &str = "Fixed the opener and the link.\n\n```markdown\nHi Sam,\n\nThanks for the details. The [setup guide](https://example.com/guide) covers it.\n```";

fn acme_home(name: &str, level: &str) -> PathBuf {
    let h = home(name);
    add_set(&h, "acme-mail", ACME_MAIL);
    approve(&h, "acme-mail", level);
    h
}

fn use_skill(h: &Path, sid: &str, pid: &str) {
    hook(
        h,
        "skill",
        serde_json::json!({"session_id": sid, "prompt_id": pid, "tool_name": "Skill", "tool_input": {"skill": "acme-writer"}, "cwd": "/tmp"}),
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
    let h = acme_home("stopfix", "fix");
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
fn a_pattern_that_does_not_compile_skips_only_its_check_in_hooks() {
    let h = home("badpattern");
    let body = format!(
        "{ACME_MAIL}[[check]]\nid = \"no-todo\"\nforbid_regex = '('\nwhy = \"No placeholders.\"\n"
    );
    add_set(&h, "acme-mail", &body);
    approve(&h, "acme-mail", "fix");
    use_skill(&h, "s1", "p1");
    let msg = sent_back(&stop(&h, "s1", "p1", BROKEN, false));
    assert!(msg.contains("- no-stock-wait-opener: "), "{msg}");
    assert!(!msg.contains("no-todo"), "{msg}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(
        log.contains("check no-todo: forbid_regex") && log.contains("skipped"),
        "{log}"
    );
    let (out, _, ok) = run_full(&h, &h, &["rules", "check"], None);
    assert!(!ok);
    assert!(out.contains("check no-todo: forbid_regex"), "{out}");
    let f = h.join("reply.md");
    std::fs::write(&f, BROKEN).unwrap();
    for args in [
        &["rules", "test", "acme-mail", &f.to_string_lossy()][..],
        &["rules", "approve", "acme-mail"],
    ] {
        let (_, err, ok) = run_full(&h, &h, args, Some("acme-mail\n"));
        assert!(!ok, "{args:?}");
        assert!(
            err.contains("has an error") && err.contains("forbid_regex"),
            "{args:?}: {err}"
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_second_broken_draft_is_let_through_and_a_missing_one_is_unverified() {
    let h = acme_home("stopbroken", "fix");
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
fn a_corrected_draft_that_breaks_another_check_logs_the_new_catch() {
    let h = acme_home("recheckother-check", "fix");
    use_skill(&h, "s1", "p1");
    let first = "Draft:\n```markdown\nHi Sam,\nSorry for the wait.\n```";
    let msg = sent_back(&stop(&h, "s1", "p1", first, false));
    assert!(
        msg.contains("no-stock-wait-opener") && !msg.contains("inline-links"),
        "setup: {msg}"
    );
    let corrected = "Fixed the opener.\n\n```markdown\nHi Sam,\nThanks for the details. More at https://example.com/guide\n```";
    assert!(stop(&h, "s1", "p1", corrected, true).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[
            ("no-stock-wait-opener", "caught"),
            ("no-stock-wait-opener", "fixed"),
            ("inline-links", "caught"),
        ]),
        "the corrected draft's new failure is logged as caught"
    );
    assert!(stop(&h, "s1", "p1", corrected, true).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl").len(),
        3,
        "a later Stop in the turn logs nothing again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn another_stop_hook_continuing_the_turn_does_not_count_as_ours() {
    let h = acme_home("stopother", "fix");
    use_skill(&h, "s1", "p1");
    assert!(!sent_back(&stop(&h, "s1", "p1", BROKEN, true)).is_empty());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn warn_sets_only_log_and_unused_sets_check_nothing() {
    let h = acme_home("stopwarn", "warn");
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
    let h = acme_home("stoprecord", "fix");
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
    let h = acme_home("stopnosid", "fix");
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
    let h = acme_home("stopcontinue", "fix");
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
        serde_json::json!({"type": "attachment", "attachment": {"type": "hook_additional_context", "content": [format!("{}\n- inline-links: ...", dotonce::reply::SEND_BACK_START)]}}).to_string(),
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
fn display_hook_labels_a_broken_draft() {
    let h = acme_home("display", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let input = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "turn_id": "p1", "message_id": mid, "index": i, "final": last, "delta": delta});
    let batch =
        |mid: &str, i: u32, last: bool, delta: &str| display_out(&h, input(mid, i, last, delta));
    assert_eq!(
        batch("m1", 0, false, "Draft:\n```markdown\nHi Sam,\n"),
        None,
        "the draft's first lines show at once"
    );
    assert_eq!(
        batch("m1", 1, true, "Sorry for the wait.\n```\n").as_deref(),
        Some(
            "Sorry for the wait.\n```\n\ndotonce: the draft above breaks your no-stock-wait-opener rule. A corrected one follows.\n\n"
        ),
        "the label follows the closing fence"
    );
    assert_eq!(
        batch(
            "m2",
            0,
            true,
            "Draft:\n```markdown\nHi Sam,\nThanks for the details.\n```\n"
        ),
        None,
        "a passing draft gets no label"
    );
    assert_eq!(
        hook(&h, "display", input("m3", 0, true, "Plain reply.\n")),
        ""
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn display_hook_never_holds_text() {
    let h = acme_home("displaysafe", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let batch = |input: serde_json::Value| display_out(&h, input);
    let at = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "message_id": mid, "index": i, "final": last, "delta": delta});
    let open = "Draft:\n```markdown\nHi Sam,\n";

    assert_eq!(batch(at("m1", 0, false, open)), None);
    let approvals = h.join("dotonce/approvals.json");
    let saved = std::fs::read_to_string(&approvals).unwrap();
    std::fs::write(&approvals, "not json").unwrap();
    assert_eq!(
        batch(at("m1", 1, false, "Sorry for the wait.\n")),
        None,
        "a hook error shows the batch as it is"
    );
    std::fs::write(&approvals, saved).unwrap();
    assert_eq!(
        batch(at("m1", 2, true, "```\n")),
        None,
        "a draft missing a batch is not labeled"
    );
    assert!(
        std::fs::read_to_string(h.join("dotonce/errors.log"))
            .unwrap()
            .contains("display")
    );

    assert_eq!(batch(at("m2", 0, false, open)), None);
    assert_eq!(batch(at("m2", 2, false, "Sorry for the wait.\n")), None);
    assert_eq!(
        batch(at("m2", 3, true, "```\n")),
        None,
        "a skipped batch leaves the draft unlabeled"
    );

    assert_eq!(batch(at("m4", 0, false, open)), None);
    assert_eq!(
        batch(at("m4", 1, true, "Sorry for the wait.\n")),
        None,
        "a fence still open at the last batch gets no label"
    );
    assert!(
        !h.join("dotonce/sessions/s1.display.json").exists(),
        "the last batch leaves no draft copy"
    );

    let no_final =
        |delta: &str| serde_json::json!({"session_id": "s1", "message_id": "m3", "delta": delta});
    assert_eq!(
        batch(no_final(open)),
        None,
        "without a final flag each batch is the last"
    );
    assert_eq!(batch(no_final("Sorry for the wait.\n```\n")), None);
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_corrupt_display_file_still_shows_the_text() {
    let h = acme_home("displaycorrupt", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let at = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": mid, "index": i, "final": last, "delta": delta});
    assert_eq!(
        display_out(&h, at("m1", 0, false, "Draft:\n```markdown\nHi Sam,\n")),
        None
    );
    let file = h.join("dotonce/sessions/s1.display.json");
    assert!(file.exists(), "setup");
    std::fs::write(&file, "not json").unwrap();
    assert_eq!(
        display_out(&h, at("m1", 1, false, "Sorry for the wait.\n")),
        None,
        "the batch shows as it is"
    );
    assert_eq!(
        display_out(&h, at("m1", 2, true, "```\nDone.\n")),
        None,
        "the draft's start is lost, so no label"
    );
    std::fs::write(&file, "not json").unwrap();
    assert_eq!(
        display_out(&h, at("m2", 0, true, STOCK_DRAFT)),
        labeled(),
        "a new message checks again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_draft_split_across_batches_is_checked_whole() {
    let h = acme_home("displaysplit", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let at = |i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": "m1", "index": i, "final": last, "delta": delta});
    assert_eq!(
        display_out(
            &h,
            at(
                0,
                false,
                "Draft:\n```markdown\nHi Sam,\nThe guide is at https://example.com/guide\n"
            )
        ),
        None
    );
    assert_eq!(display_out(&h, at(1, false, "Sorry for the wait.\n")), None);
    assert_eq!(
        display_out(&h, at(2, true, "Thanks.\n```\n\nLet me know.\n")).as_deref(),
        Some(
            "Thanks.\n```\n\ndotonce: the draft above breaks your no-stock-wait-opener and inline-links rules. A corrected one follows.\n\n\nLet me know.\n"
        ),
        "lines from all three batches are checked at the close"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn text_after_a_labeled_fence_stays_its_own_paragraph() {
    let h = acme_home("displayparagraph", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let label =
        "dotonce: the draft above breaks your no-stock-wait-opener rule. A corrected one follows.";
    for (mid, after) in [("m1", "Done.\n"), ("m2", "---\n"), ("m3", "===\n")] {
        let delta = format!("```markdown\nSorry for the wait.\n```\n{after}");
        assert_eq!(
            display_out(
                &h,
                serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": mid, "index": 0, "final": true, "delta": delta})
            ),
            Some(format!(
                "```markdown\nSorry for the wait.\n```\n\n{label}\n\n{after}"
            )),
            "a blank line keeps {after:?} off the label's line"
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_fence_split_mid_line_is_read_whole() {
    let h = acme_home("displaymidline", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let at = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": mid, "index": i, "final": last, "delta": delta});
    let label =
        "dotonce: the draft above breaks your no-stock-wait-opener rule. A corrected one follows.";
    assert_eq!(
        display_out(
            &h,
            at("m1", 0, false, "```markdown\nSorry for the wait.\n```")
        ),
        None,
        "a batch ending mid-line isn't read as a whole line"
    );
    assert_eq!(
        display_out(&h, at("m1", 1, false, "bash\nmore\n")),
        None,
        "```bash is not a closing fence"
    );
    assert_eq!(
        display_out(&h, at("m1", 2, true, "```\nDone.\n")),
        Some(format!("```\n\n{label}\n\nDone.\n"))
    );
    assert_eq!(
        display_out(
            &h,
            at("m2", 0, false, "```markdown\nSorry for the wait.\n``")
        ),
        None
    );
    assert_eq!(
        display_out(&h, at("m2", 1, true, "`\nDone.\n")),
        Some(format!("`\n\n{label}\n\nDone.\n")),
        "a closing fence split as `` and `\\n is still found"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

/// Where a label starts on screen. A blank line always follows it.
const LABEL_START: &str = "\ndotonce: the draft above breaks your ";

/// What a display hook call puts on screen, or None when the batch shows as
/// it is. Every output must be the batch's own text unchanged, with at most
/// label lines added after it.
fn display_out(h: &Path, input: serde_json::Value) -> Option<String> {
    let out = hook(h, "display", input.clone());
    if out.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "MessageDisplay");
    let shown = v["hookSpecificOutput"]["displayContent"]
        .as_str()
        .unwrap()
        .to_string();
    let delta = input["delta"].as_str().unwrap_or_default();
    let mut bare = shown.clone();
    while let Some(i) = bare.find(LABEL_START) {
        let end = bare[i + 1..]
            .find('\n')
            .map(|e| i + 1 + e + 1)
            .unwrap_or_else(|| panic!("a label ends its line: {shown:?}"));
        assert_eq!(
            bare.get(end..end + 1),
            Some("\n"),
            "a blank line follows the label: {shown:?}"
        );
        bare.replace_range(i..end + 1, "");
    }
    // At the last batch, a closing fence without a newline of its own gets
    // one before its label.
    if bare != delta && shown.ends_with("\n\n") && !delta.ends_with('\n') {
        assert_eq!(bare, format!("{delta}\n"), "{shown:?}");
        bare.pop();
    }
    assert_eq!(
        bare, delta,
        "the batch's own text shows unchanged: {shown:?}"
    );
    Some(shown)
}

#[test]
fn no_label_after_the_turn_was_sent_back() {
    let h = acme_home("displayagain", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    assert!(!sent_back(&stop(&h, "s1", "p1", BROKEN, false)).is_empty());
    let again = "Second try:\n```markdown\nHi Sam,\nSorry for the wait.\n```\n";
    assert_eq!(
        display_out(
            &h,
            serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": "m2", "index": 0, "final": true, "delta": again})
        ),
        None,
        "Stop won't send this draft back, so it shows unlabeled"
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
    assert_eq!(
        out,
        format!(
            "{again}\ndotonce: the draft above breaks your no-stock-wait-opener rule. A corrected one follows.\n\n"
        ),
        "a new turn labels again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_new_message_resets_the_draft_copy() {
    let h = acme_home("displaystop", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let at = |mid: &str, i: u32, last: bool, delta: &str| serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": mid, "index": i, "final": last, "delta": delta});
    assert_eq!(
        display_out(
            &h,
            at(
                "m1",
                0,
                false,
                "Draft:\n```markdown\nHi Sam,\nSorry for the wait.\n"
            )
        ),
        None
    );
    let files: Vec<String> = std::fs::read_dir(h.join("dotonce/sessions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        files.contains(&"s1.display.json".to_string())
            && !files.iter().any(|f| f.ends_with(".tmp")),
        "the draft copy is saved through a renamed temp file: {files:?}"
    );
    assert_eq!(
        display_out(
            &h,
            at("m2", 0, false, "Plain reply.\n```markdown\nHi Sam,\n")
        ),
        None,
        "nothing from the stopped message shows again"
    );
    assert_eq!(
        display_out(&h, at("m2", 1, true, "Thanks for the details.\n```\n")),
        None,
        "the stopped message's lines are not part of this draft"
    );
    assert_eq!(display_out(&h, at("m3", 0, true, STOCK_DRAFT)), labeled());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn the_recheck_cuts_at_the_continuation_or_is_unverified() {
    let user = serde_json::json!({"type": "user", "message": {"role": "user", "content": "Draft the reply"}, "promptId": "p1"}).to_string();
    let said = |t: &str| {
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": t}]}}).to_string()
    };
    // The record keeps the broken message as two text entries, so the last
    // message at send-back matches no single entry.
    let (intro, draft) = BROKEN.split_once("\n\n").unwrap();
    let continuation = [
        serde_json::json!({"type": "attachment", "attachment": {"type": "hook_success", "stdout": "{}"}}).to_string(),
        serde_json::json!({"type": "attachment", "attachment": {"type": "hook_additional_context", "content": [format!("{}\n- inline-links: ...", dotonce::reply::SEND_BACK_START)]}}).to_string(),
        serde_json::json!({"type": "system", "subtype": "stop_hook_summary"}).to_string(),
    ];
    for (name, marker, want) in [
        ("recheckcut", true, "fixed"),
        ("recheckunverified", false, "unverified"),
    ] {
        let h = acme_home(name, "fix");
        use_skill(&h, "s1", "p1");
        let record = h.join("session.jsonl");
        let first = [user.clone(), said(intro), said(draft)];
        std::fs::write(&record, first.join("\n")).unwrap();
        assert!(!sent_back(&stop_with(&h, "s1", "p1", BROKEN, false, &record)).is_empty());
        let mut later = first.to_vec();
        if marker {
            later.extend(continuation.iter().cloned());
        }
        later.push(said(FIXED));
        std::fs::write(&record, later.join("\n")).unwrap();
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
                ("no-stock-wait-opener", want),
                ("inline-links", want),
            ]),
            "{name}"
        );
        std::fs::remove_dir_all(&h).unwrap();
    }
}

#[test]
fn the_recheck_prefers_the_continuation_to_a_repeated_message() {
    let h = acme_home("recheckrepeat", "fix");
    use_skill(&h, "s1", "p1");
    let user = serde_json::json!({"type": "user", "message": {"role": "user", "content": "Draft the reply"}, "promptId": "p1"}).to_string();
    let said = |t: &str| {
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": t}]}}).to_string()
    };
    let first = [user, said(BROKEN), said("Saved.")];
    let record = h.join("session.jsonl");
    std::fs::write(&record, first.join("\n")).unwrap();
    assert!(!sent_back(&stop_with(&h, "s1", "p1", "Saved.", false, &record)).is_empty());
    let mut later = first.to_vec();
    later.push(serde_json::json!({"type": "attachment", "attachment": {"type": "hook_additional_context", "content": [format!("{}\n- inline-links: ...", dotonce::reply::SEND_BACK_START)]}}).to_string());
    later.push(said(FIXED));
    later.push(said("Saved."));
    std::fs::write(&record, later.join("\n")).unwrap();
    assert!(
        stop_with(&h, "s1", "p1", "Saved.", true, &record)
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
        "the second Saved. is not where the send-back continued"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_warn_catch_is_logged_once_per_turn() {
    let h = acme_home("stopwarnonce", "warn");
    use_skill(&h, "s1", "p1");
    assert!(stop(&h, "s1", "p1", BROKEN, false).trim().is_empty());
    assert!(
        stop(&h, "s1", "p1", BROKEN, true).trim().is_empty(),
        "another Stop hook continued the turn"
    );
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[
            ("no-stock-wait-opener", "caught"),
            ("inline-links", "caught")
        ])
    );
    use_skill(&h, "s1", "p2");
    assert!(stop(&h, "s1", "p2", BROKEN, false).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl").len(),
        4,
        "the next turn logs again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

/// Registers `dotonce hook stop` as a Stop hook in the test home's Claude
/// Code user settings, so the display hook may label drafts.
fn register_stop(h: &Path) {
    let dir = h.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let settings = serde_json::json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/opt/acme/bin/dotonce hook stop", "timeout": 5}]}]}});
    std::fs::write(dir.join("settings.json"), settings.to_string()).unwrap();
}

const STOCK_DRAFT: &str = "Draft:\n```markdown\nHi Sam,\nSorry for the wait.\n```\n";

/// Shows `STOCK_DRAFT` as one whole message and returns what went on screen,
/// or None when it showed as it is.
fn show_draft(h: &Path, cwd: &Path, sid: &str, pid: &str, mid: &str) -> Option<String> {
    display_out(
        h,
        serde_json::json!({"session_id": sid, "prompt_id": pid, "message_id": mid, "index": 0, "final": true, "delta": STOCK_DRAFT, "cwd": cwd}),
    )
}

/// What the screen shows when `STOCK_DRAFT` is labeled.
fn labeled() -> Option<String> {
    Some(
        "Draft:\n```markdown\nHi Sam,\nSorry for the wait.\n```\n\ndotonce: the draft above breaks your no-stock-wait-opener rule. A corrected one follows.\n\n"
            .to_string(),
    )
}

#[test]
fn a_broken_config_never_labels_a_draft() {
    let h = acme_home("displaycfg", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    use_skill(&h, "s2", "p1");
    assert_eq!(show_draft(&h, &h, "s2", "p1", "m1"), labeled(), "setup");
    std::fs::write(h.join("dotonce/config.toml"), "not = [toml").unwrap();
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m1"),
        None,
        "Stop can't load the config, so it won't send back: the draft shows"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

const STOCK_CHECK: &str = "[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"no-stock-wait-opener\"\nforbid = [\"sorry for the wait\"]\nwhy = \"Open with a line from the thread.\"\n";

#[test]
fn a_folder_set_is_not_labeled_once_the_cwd_left_the_folder() {
    let check = STOCK_CHECK;
    let h = home("displayfolder");
    register_stop(&h);
    let work = h.join("work");
    std::fs::create_dir_all(&work).unwrap();
    add_set(
        &h,
        "folder-reply",
        &format!("[when]\nfolders = [{:?}]\n{check}", work.to_string_lossy()),
    );
    approve(&h, "folder-reply", "fix");
    for sid in ["s1", "s2"] {
        hook(
            &h,
            "prompt",
            serde_json::json!({"session_id": sid, "prompt_id": "p1", "prompt": "hello", "cwd": work}),
        );
    }
    assert_eq!(show_draft(&h, &work, "s2", "p1", "m1"), labeled(), "setup");
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m1"),
        None,
        "outside the folder Stop drops the set"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_private_set_is_not_labeled_outside_the_private_paths() {
    let check = STOCK_CHECK;
    let (h, proj) = home_with_private_path("displayprivate");
    register_stop(&h);
    add_set(
        &h,
        "secret-reply",
        &format!("private = true\n[when]\nskills = [\"acme-writer\"]\n{check}"),
    );
    approve(&h, "secret-reply", "fix");
    for sid in ["s1", "s2"] {
        hook(
            &h,
            "skill",
            serde_json::json!({"session_id": sid, "prompt_id": "p1", "tool_name": "Skill", "tool_input": {"skill": "acme-writer"}, "cwd": proj}),
        );
    }
    assert_eq!(show_draft(&h, &proj, "s2", "p1", "m1"), labeled(), "setup");
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m1"),
        None,
        "outside the private paths Stop drops the set"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_paused_set_is_not_labeled() {
    let h = acme_home("displaypaused", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    use_skill(&h, "s2", "p1");
    assert_eq!(show_draft(&h, &h, "s2", "p1", "m1"), labeled(), "setup");
    let file = h.join("dotonce/sessions/s1.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    state["paused"] = serde_json::json!(["acme-mail"]);
    std::fs::write(&file, state.to_string()).unwrap();
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m1"),
        None,
        "Stop drops a paused set"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_warn_set_never_labels_a_draft() {
    let h = acme_home("displaywarn", "warn");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m1"),
        None,
        "Stop only logs a warn set, so no corrected draft follows"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn the_label_names_every_failing_check() {
    let h = acme_home("displayids", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let draft = "```markdown\nHi Sam,\nSorry for the wait. The guide is at https://example.com/guide\n```\n";
    assert_eq!(
        display_out(
            &h,
            serde_json::json!({"session_id": "s1", "prompt_id": "p1", "message_id": "m1", "index": 0, "final": true, "delta": draft, "cwd": h}),
        ),
        Some(format!(
            "{draft}\ndotonce: the draft above breaks your no-stock-wait-opener and inline-links rules. A corrected one follows.\n\n"
        ))
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_config_broken_before_stop_sends_nothing_back() {
    let h = acme_home("stopcfg", "fix");
    use_skill(&h, "s1", "p1");
    std::fs::write(h.join("dotonce/config.toml"), "not = [toml").unwrap();
    let out = stop(&h, "s1", "p1", STOCK_DRAFT, false);
    assert!(out.trim().is_empty(), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn stop_sends_nothing_back_for_a_set_no_longer_at_fix() {
    for change in ["warn", "edited", "unapproved"] {
        let h = acme_home(&format!("stopleft{change}"), "fix");
        use_skill(&h, "s1", "p1");
        match change {
            "warn" => approve(&h, "acme-mail", "warn"),
            "edited" => edit_set(&h, "acme-mail"),
            _ => std::fs::write(h.join("dotonce/approvals.json"), "{\"sets\": {}}").unwrap(),
        }
        let out = stop(&h, "s1", "p1", STOCK_DRAFT, false);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
        assert!(
            v.get("hookSpecificOutput").is_none() && v.get("systemMessage").is_none(),
            "{change}: {out}"
        );
        std::fs::remove_dir_all(&h).unwrap();
    }
}

#[test]
fn a_failed_stop_save_sends_nothing_back() {
    use std::os::unix::fs::PermissionsExt;
    let h = acme_home("stopsave", "fix");
    use_skill(&h, "s1", "p1");
    let sessions = h.join("dotonce/sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = stop(&h, "s1", "p1", STOCK_DRAFT, false);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        out.trim().is_empty(),
        "without its state saved, Stop sends nothing back: {out}"
    );
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(!log.contains("Sam"), "the draft is never logged: {log}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn display_labels_only_when_dotonce_runs_at_stop() {
    let h = acme_home("displaynostop", "fix");
    for sid in ["s1", "s2", "s3"] {
        use_skill(&h, sid, "p1");
    }
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m1"),
        None,
        "no Stop hook: no corrected draft follows, so no label"
    );
    let settings = |dir: &Path, file: &str, command: &str| {
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let v = serde_json::json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": command}]}]}});
        std::fs::write(dir.join(".claude").join(file), v.to_string()).unwrap();
    };
    settings(&h, "settings.json", "/opt/acme/bin/notify stop");
    assert_eq!(
        show_draft(&h, &h, "s2", "p1", "m1"),
        None,
        "another Stop hook doesn't count"
    );
    let proj = h.join("proj");
    settings(&proj, "settings.local.json", "dotonce hook stop");
    assert_eq!(
        show_draft(&h, &proj, "s3", "p1", "m1"),
        labeled(),
        "the project's local settings count"
    );
    register_stop(&h);
    assert_eq!(
        show_draft(&h, &h, "s1", "p1", "m2"),
        labeled(),
        "the settings are read again at each call"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn display_never_writes_the_session_state() {
    let h = acme_home("displaystate", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let file = h.join("dotonce/sessions/s1.json");
    let before = std::fs::read_to_string(&file).unwrap();
    assert_eq!(show_draft(&h, &h, "s1", "p1", "m1"), labeled());
    assert_eq!(
        display_out(
            &h,
            serde_json::json!({"session_id": "s1", "prompt_id": "p2", "message_id": "m2", "index": 0, "final": false, "delta": "```markdown\nHi Sam,\n"})
        ),
        None
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "the session state Stop keeps is left byte for byte as it was"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn disabled_hooks_mean_no_label() {
    let h = acme_home("displaydisabled", "fix");
    register_stop(&h);
    for sid in ["s1", "s2", "s3"] {
        use_skill(&h, sid, "p1");
    }
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    assert_eq!(show_draft(&h, &proj, "s3", "p1", "m1"), labeled(), "setup");
    let local = proj.join(".claude/settings.local.json");
    std::fs::write(&local, r#"{"disableAllHooks": true}"#).unwrap();
    assert_eq!(
        show_draft(&h, &proj, "s1", "p1", "m1"),
        None,
        "the project's local settings turn every hook off"
    );
    std::fs::write(&local, r#"{"disableAllHooks": false}"#).unwrap();
    assert_eq!(show_draft(&h, &proj, "s1", "p1", "m2"), labeled());
    let user = h.join(".claude/settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&user).unwrap()).unwrap();
    settings["disableAllHooks"] = serde_json::json!(true);
    std::fs::write(&user, settings.to_string()).unwrap();
    assert_eq!(
        show_draft(&h, &proj, "s2", "p1", "m1"),
        None,
        "the file that adds the Stop hook also turns hooks off"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn an_unreadable_settings_file_means_no_label() {
    let h = acme_home("displaybadsettings", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    use_skill(&h, "s2", "p1");
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    std::fs::write(proj.join(".claude/settings.json"), "{not json").unwrap();
    assert_eq!(
        show_draft(&h, &proj, "s1", "p1", "m1"),
        None,
        "a broken file"
    );
    std::fs::remove_file(proj.join(".claude/settings.json")).unwrap();
    std::fs::create_dir_all(proj.join(".claude/settings.json")).unwrap();
    assert_eq!(
        show_draft(&h, &proj, "s2", "p1", "m1"),
        None,
        "a file that can't be read"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn another_hooks_added_context_is_not_the_send_back() {
    let h = acme_home("recheckother", "fix");
    use_skill(&h, "s1", "p1");
    let user = serde_json::json!({"type": "user", "message": {"role": "user", "content": "Draft the reply"}, "promptId": "p1"}).to_string();
    // The prompt hook's context, at the start of the turn.
    let other = serde_json::json!({"type": "attachment", "attachment": {"type": "hook_additional_context", "content": ["<dotonce>\nRules from your rule sets"]}}).to_string();
    let said = |t: &str| {
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": t}]}}).to_string()
    };
    let (intro, draft) = BROKEN.split_once("\n\n").unwrap();
    let first = [user, other, said(intro), said(draft)];
    let record = h.join("session.jsonl");
    std::fs::write(&record, first.join("\n")).unwrap();
    assert!(!sent_back(&stop_with(&h, "s1", "p1", BROKEN, false, &record)).is_empty());
    let mut later = first.to_vec();
    later.push(said(FIXED));
    std::fs::write(&record, later.join("\n")).unwrap();
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
            ("no-stock-wait-opener", "unverified"),
            ("inline-links", "unverified"),
        ]),
        "the turn's other added context is not where the send-back continued"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

const GUARD: &str = "dotonce: rule files, approvals and rule commands only change from the user's own terminal. Ask the user to run it there.";

/// Runs the PreToolUse hook and returns the deny reason, or None when the
/// call may run.
fn pre(h: &Path, cwd: &Path, tool: &str, input: serde_json::Value) -> Option<String> {
    let out = hook(
        h,
        "pre-tool",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input, "cwd": cwd}),
    );
    if out.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    Some(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn guard_keeps_rule_files_and_approvals_out_of_reach_with_no_rule_sets() {
    let h = home("guard");
    let proj = h.join("proj");
    let d = h.join("dotonce");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&d).unwrap();
    let p = |rel: &str| d.join(rel).to_string_lossy().to_string();
    let denied = [
        (
            "Write",
            serde_json::json!({"file_path": p("rules/acme.toml"), "content": "id = \"acme\""}),
        ),
        (
            "Edit",
            serde_json::json!({"file_path": p("approvals.json"), "old_string": "a", "new_string": "b"}),
        ),
        (
            "MultiEdit",
            serde_json::json!({"file_path": p("rules/acme.toml"), "edits": [{"old_string": "a", "new_string": "b"}]}),
        ),
        (
            "NotebookEdit",
            serde_json::json!({"notebook_path": p("rules/x.ipynb"), "new_source": "x"}),
        ),
        (
            "Bash",
            serde_json::json!({"command": "cat > ~/dotonce/rules/acme.toml"}),
        ),
        (
            "Bash",
            serde_json::json!({"command": "echo '{}' > $HOME/dotonce/approvals.json"}),
        ),
        (
            "Bash",
            serde_json::json!({"command": format!("rm {}", p("rules/acme.toml"))}),
        ),
        (
            "Bash",
            serde_json::json!({"command": "cd ~/dotonce && rm rules/acme.toml"}),
        ),
    ];
    for (tool, input) in denied {
        let reason =
            pre(&h, &proj, tool, input.clone()).unwrap_or_else(|| panic!("{tool} {input}"));
        assert_eq!(reason, GUARD, "{tool} {input}");
    }
    assert!(
        pre(
            &h,
            &d,
            "Write",
            serde_json::json!({"file_path": "rules/acme.toml", "content": "x"})
        )
        .is_some()
    );
    let allowed = [
        (
            "Write",
            serde_json::json!({"file_path": proj.join("a.txt"), "content": "x"}),
        ),
        (
            "Bash",
            serde_json::json!({"command": "grep -rn \"dotonce rules approve\" src"}),
        ),
        (
            "Bash",
            serde_json::json!({"command": "git commit -m \"Add the dotonce rules approve command\""}),
        ),
        ("Bash", serde_json::json!({"command": "ls ~/dotonce/notes"})),
        (
            "Read",
            serde_json::json!({"file_path": p("rules/acme.toml")}),
        ),
    ];
    for (tool, input) in allowed {
        assert_eq!(pre(&h, &proj, tool, input.clone()), None, "{tool} {input}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

const CODE_CHECKS: &str = "[[check]]\nid = \"no-force-push\"\non = \"tool\"\ntool = \"Bash\"\nforbid_regex = 'git\\s+push\\b.*(--force|-f\\b)'\nwhy = \"Never force push. Ask first.\"\n[[check]]\nid = \"no-todo-in-rust\"\non = \"tool\"\ntool = \"Write\"\npath = [\"*.rs\"]\nforbid = [\"TODO\"]\nwhy = \"No TODOs in Rust files.\"\n[[check]]\nid = \"no-unwrap\"\non = \"tool\"\ntool = \"Edit\"\nforbid = [\"unwrap()\"]\nwhy = \"Handle the error.\"\n[[check]]\nid = \"no-unwrap-multi\"\non = \"tool\"\ntool = \"MultiEdit\"\nforbid = [\"unwrap()\"]\nwhy = \"Handle the error.\"\n[[check]]\nid = \"no-print\"\non = \"tool\"\ntool = \"NotebookEdit\"\nforbid = [\"print(\"]\nwhy = \"Use the logger.\"\n";

fn code_home(name: &str, level: &str) -> (PathBuf, PathBuf) {
    let h = home(name);
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    add_set(
        &h,
        "code-safety",
        &format!(
            "[when]\nfolders = [{:?}]\n{CODE_CHECKS}",
            proj.to_string_lossy()
        ),
    );
    approve(&h, "code-safety", level);
    (h, proj)
}

#[test]
fn tool_checks_read_each_tool_field_and_deny_at_fix() {
    let (h, proj) = code_home("toolfix", "fix");
    let bash = |cmd: &str| serde_json::json!({"command": cmd});
    assert_eq!(
        pre(&h, &proj, "Bash", bash("git push --force origin main")).as_deref(),
        Some("no-force-push: Never force push. Ask first.")
    );
    assert!(pre(&h, &proj, "Bash", bash("git push -f")).is_some());
    assert_eq!(pre(&h, &proj, "Bash", bash("git push origin main")), None);
    let rs = proj.join("src/main.rs");
    assert!(
        pre(
            &h,
            &proj,
            "Write",
            serde_json::json!({"file_path": rs, "content": "// TODO: later"})
        )
        .unwrap()
        .starts_with("no-todo-in-rust")
    );
    assert_eq!(
        pre(
            &h,
            &proj,
            "Write",
            serde_json::json!({"file_path": proj.join("notes.md"), "content": "TODO"})
        ),
        None
    );
    assert!(
        pre(
            &h,
            &proj,
            "Edit",
            serde_json::json!({"file_path": rs, "old_string": "a", "new_string": "x.unwrap()"})
        )
        .unwrap()
        .starts_with("no-unwrap")
    );
    assert!(pre(&h, &proj, "MultiEdit", serde_json::json!({"file_path": rs, "edits": [{"old_string": "a", "new_string": "ok"}, {"old_string": "b", "new_string": "y.unwrap()"}]})).unwrap().starts_with("no-unwrap-multi"));
    assert!(
        pre(
            &h,
            &proj,
            "NotebookEdit",
            serde_json::json!({"notebook_path": proj.join("n.ipynb"), "new_source": "print(1)"})
        )
        .unwrap()
        .starts_with("no-print")
    );
    assert_eq!(
        pre(&h, &h, "Bash", bash("git push --force")),
        None,
        "outside the folder the set is off"
    );
    let got = outcomes(&h, "catches.jsonl");
    assert_eq!(got.len(), 6, "{got:?}");
    assert!(got.iter().all(|(_, o)| o == "denied"), "{got:?}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn tool_checks_at_warn_only_log() {
    let (h, proj) = code_home("toolwarn", "warn");
    assert_eq!(
        pre(
            &h,
            &proj,
            "Bash",
            serde_json::json!({"command": "git push --force"})
        ),
        None
    );
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[("no-force-push", "caught")])
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_test_runs_tool_checks_too() {
    let (h, _) = code_home("tooltest", "warn");
    let file = h.join("cmd.txt");
    std::fs::write(&file, "git push --force origin main").unwrap();
    let f = file.to_string_lossy().to_string();
    let (out, ok) = run(
        &h,
        &["rules", "test", "code-safety", &f, "--tool", "Bash"],
        None,
    );
    assert!(ok, "{out}");
    assert!(out.contains("no-force-push"), "{out}");
    std::fs::write(&file, "// TODO").unwrap();
    let (out, _) = run(
        &h,
        &[
            "rules",
            "test",
            "code-safety",
            &f,
            "--tool",
            "Write",
            "--path",
            "/code/a.rs",
        ],
        None,
    );
    assert!(out.contains("no-todo-in-rust"), "{out}");
    let (out, _) = run(
        &h,
        &[
            "rules",
            "test",
            "code-safety",
            &f,
            "--tool",
            "Write",
            "--path",
            "/code/a.md",
        ],
        None,
    );
    assert!(out.contains("passes"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn guard_covers_last_live_and_rule_changing_commands() {
    let h = home("guardextra");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let last = h
        .join("dotonce/last-live.json")
        .to_string_lossy()
        .to_string();
    let bash = |cmd: &str| serde_json::json!({"command": cmd});
    let denied = [
        (
            "Write",
            serde_json::json!({"file_path": last, "content": "{}"}),
        ),
        (
            "Edit",
            serde_json::json!({"file_path": last, "old_string": "a", "new_string": "b"}),
        ),
        ("Bash", bash("echo '{}' > ~/dotonce/last-live.json")),
        ("Bash", bash("dotonce rules approve acme")),
        (
            "Bash",
            bash("env -u CLAUDECODE dotonce rules level acme fix"),
        ),
        (
            "Bash",
            bash("script -q /dev/null ~/.cargo/bin/dotonce rules off acme"),
        ),
        ("Bash", bash("cd /tmp && dotonce rules pause acme")),
    ];
    for (tool, input) in denied {
        let reason =
            pre(&h, &proj, tool, input.clone()).unwrap_or_else(|| panic!("{tool} {input}"));
        assert_eq!(reason, GUARD, "{tool} {input}");
    }
    for cmd in [
        "grep -rn \"dotonce rules approve\" src",
        "git commit -m \"Add the dotonce rules approve command\"",
        "dotonce rules list",
    ] {
        assert_eq!(pre(&h, &proj, "Bash", bash(cmd)), None, "{cmd}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs the PreToolUse hook with extra environment variables and returns
/// the deny reason, or None.
fn pre_env(
    h: &Path,
    cwd: &Path,
    tool: &str,
    input: serde_json::Value,
    env: &[(&str, &str)],
) -> Option<String> {
    let json = serde_json::json!({"session_id": "s1", "prompt_id": "p1", "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input, "cwd": cwd});
    let (out, _, ok) = run_env(h, cwd, &["hook", "pre-tool"], Some(&json.to_string()), env);
    assert!(ok);
    if out.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    Some(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn guard_follows_dotonce_home() {
    let h = home("guardhome");
    let proj = h.join("proj");
    let custom = h.join("elsewhere/dot");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&custom).unwrap();
    let env = [("DOTONCE_HOME", custom.to_str().unwrap())];
    let write = |p: PathBuf| serde_json::json!({"file_path": p, "content": "x"});
    for name in ["rules/acme.toml", "approvals.json", "last-live.json"] {
        assert!(
            pre_env(&h, &proj, "Write", write(custom.join(name)), &env).is_some(),
            "{name}"
        );
    }
    let cmd = format!("rm {}", custom.join("last-live.json").display());
    assert!(pre_env(&h, &proj, "Bash", serde_json::json!({"command": cmd}), &env).is_some());
    assert_eq!(
        pre_env(
            &h,
            &proj,
            "Write",
            write(h.join("dotonce/rules/acme.toml")),
            &env
        ),
        None,
        "the default home is not the home now"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_repeated_warn_tool_catch_is_logged_once_per_turn() {
    let (h, proj) = code_home("toolwarnonce", "warn");
    let push = serde_json::json!({"command": "git push --force"});
    assert_eq!(pre(&h, &proj, "Bash", push.clone()), None);
    assert_eq!(pre(&h, &proj, "Bash", push.clone()), None);
    assert_eq!(
        outcomes(&h, "catches.jsonl"),
        pairs(&[("no-force-push", "caught")])
    );
    let next = serde_json::json!({"session_id": "s1", "prompt_id": "p2", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": push, "cwd": proj});
    assert!(hook(&h, "pre-tool", next).trim().is_empty());
    assert_eq!(
        outcomes(&h, "catches.jsonl").len(),
        2,
        "the next turn logs again"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn guard_denies_rule_commands_through_shells_wrappers_and_pipes() {
    let h = home("guardshell");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let bash = |cmd: &str| serde_json::json!({"command": cmd});
    for cmd in [
        "bash -c 'dotonce rules approve acme'",
        "/bin/sh -c \"dotonce rules level acme fix\"",
        "eval \"dotonce rules off acme\"",
        "env -S 'dotonce rules pause acme'",
        "sudo dotonce rules approve acme",
        "echo acme | xargs dotonce rules approve",
        "script -q /dev/null dotonce rules off acme",
        "echo 'dotonce rules approve acme' | sh",
        "bash <<'EOF'\ndotonce rules approve acme\nEOF",
    ] {
        assert_eq!(
            pre(&h, &proj, "Bash", bash(cmd)).as_deref(),
            Some(GUARD),
            "{cmd}"
        );
    }
    for cmd in [
        "grep -rn \"dotonce rules approve\" src",
        "git commit -m \"Add the dotonce rules approve command\"",
        "echo \"run dotonce rules approve to approve\"",
        "dotonce rules list",
        "dotonce rules check",
    ] {
        assert_eq!(pre(&h, &proj, "Bash", bash(cmd)), None, "{cmd}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_missing_session_id_is_logged_only_with_rule_sets() {
    let input = |cwd: &Path| serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}, "cwd": cwd});
    let errors =
        |h: &Path| std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap_or_default();
    let h = home("nosidempty");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    assert!(hook(&h, "pre-tool", input(&proj)).trim().is_empty());
    assert_eq!(errors(&h), "", "no rule sets, nothing to log");
    std::fs::remove_dir_all(&h).unwrap();
    let (h, proj) = code_home("nosidset", "warn");
    assert!(hook(&h, "pre-tool", input(&proj)).trim().is_empty());
    assert!(errors(&h).contains("no session_id"), "{}", errors(&h));
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn guard_lets_searches_and_other_rule_commands_through() {
    let h = home("guardsearch");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let bash = |cmd: &str| serde_json::json!({"command": cmd});
    for cmd in [
        "grep -rn \"dotonce rules approve\" src | grep sh",
        "rg \"dotonce rules approve\" -t sh",
        "grep -rn \"dotonce rules off\" . | grep -v fish",
        "ps aux | grep zsh",
        "dotonce rules test x off",
        "dotonce search rules off",
    ] {
        assert_eq!(pre(&h, &proj, "Bash", bash(cmd)), None, "{cmd}");
    }
    for cmd in [
        "echo 'dotonce rules approve acme' | sudo -u sam bash",
        "dotonce rules off acme",
    ] {
        assert_eq!(
            pre(&h, &proj, "Bash", bash(cmd)).as_deref(),
            Some(GUARD),
            "{cmd}"
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn guard_catches_plain_text_changes_to_the_home() {
    let h = home("guardplain");
    let proj = h.join("proj");
    std::fs::create_dir_all(h.join("dotonce/rules")).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    let bash = |cmd: &str| serde_json::json!({"command": cmd});
    for cmd in [
        "rm ~/dotonce/*.json",
        "rm -rf ~/dotonce/r*",
        "find ~/dotonce -name '*.json' -delete",
        "rm -rf ~/dotonce",
        "mv ~/dotonce ~/old",
        "rm ~/\"dotonce\"/approvals.json",
        "rm ~/dotonce/approval\\s.json",
        "sed -i s/a/b/ ~/dotonce/rules/x.toml",
        "cd ~/dotonce && echo '{}' > approvals.json",
    ] {
        assert_eq!(
            pre(&h, &proj, "Bash", bash(cmd)).as_deref(),
            Some(GUARD),
            "{cmd}"
        );
    }
    for cmd in [
        "ls ~/dotonce",
        "cat ~/dotonce/config.toml",
        "du -sh ~/dotonce",
        "cat ~/dotonce/config.toml | grep rules",
    ] {
        assert_eq!(pre(&h, &proj, "Bash", bash(cmd)), None, "{cmd}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn guard_reads_su_sudo_shells_and_a_substituted_dotonce() {
    let h = home("guardsu");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let bash = |cmd: &str| serde_json::json!({"command": cmd});
    for cmd in [
        "su -c 'dotonce rules off acme' sam",
        "sudo -s <<< 'dotonce rules off acme'",
        "sudo -s <<EOF\ndotonce rules approve acme\nEOF",
        "$(which dotonce) rules approve acme",
        "`which dotonce` rules approve acme",
    ] {
        assert_eq!(
            pre(&h, &proj, "Bash", bash(cmd)).as_deref(),
            Some(GUARD),
            "{cmd}"
        );
    }
    assert_eq!(
        pre(&h, &proj, "Bash", bash("$(which dotonce) rules list")),
        None
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_test_takes_only_the_five_tool_names() {
    let (h, _) = code_home("tooltestnames", "warn");
    let file = h.join("cmd.txt");
    std::fs::write(&file, "git push --force origin main").unwrap();
    let f = file.to_string_lossy().to_string();
    for tool in ["Read", "bash", "Grep", ""] {
        let (_, err, ok) = run_full(
            &h,
            &h,
            &["rules", "test", "code-safety", &f, "--tool", tool],
            None,
        );
        assert!(!ok, "{tool}");
        assert!(err.contains("possible values"), "{tool}: {err}");
    }
    for tool in ["Bash", "Write", "Edit", "MultiEdit", "NotebookEdit"] {
        let (out, err, ok) = run_full(
            &h,
            &h,
            &["rules", "test", "code-safety", &f, "--tool", tool],
            None,
        );
        assert!(ok, "{tool}: {out}{err}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn pre_tool_reads_the_session_file_only_with_rule_sets() {
    let input = |cwd: &Path| serde_json::json!({"session_id": "///", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}, "cwd": cwd});
    let errors =
        |h: &Path| std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap_or_default();
    let h = home("presessionempty");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    assert!(hook(&h, "pre-tool", input(&proj)).trim().is_empty());
    assert_eq!(errors(&h), "", "no rule sets, no session file read");
    std::fs::remove_dir_all(&h).unwrap();
    let (h, proj) = code_home("presessionset", "warn");
    assert!(hook(&h, "pre-tool", input(&proj)).trim().is_empty());
    assert!(
        errors(&h).contains("no usable session_id"),
        "{}",
        errors(&h)
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn tool_check_paths_match_the_normalized_path() {
    let h = home("toolnorm");
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join("docs")).unwrap();
    std::fs::create_dir_all(proj.join("src")).unwrap();
    let src = proj.join("src").to_string_lossy().to_string();
    add_set(
        &h,
        "src-todo",
        &format!(
            "[when]\nfolders = [{:?}]\n[[check]]\nid = \"no-todo-in-src\"\non = \"tool\"\ntool = \"Write\"\npath = [{:?}]\nforbid = [\"TODO\"]\nwhy = \"No TODOs in src.\"\n",
            proj.to_string_lossy(),
            format!("{src}/*.rs")
        ),
    );
    approve(&h, "src-todo", "fix");
    let write = |p: &str| serde_json::json!({"file_path": p, "content": "// TODO"});
    let docs = proj.join("docs");
    assert_eq!(
        pre(&h, &docs, "Write", write("../src/a.rs")).as_deref(),
        Some("no-todo-in-src: No TODOs in src.")
    );
    assert_eq!(
        pre(&h, &proj, "Write", write("./src/./b.rs")).as_deref(),
        Some("no-todo-in-src: No TODOs in src.")
    );
    assert_eq!(pre(&h, &docs, "Write", write("a.rs")), None);
    std::fs::remove_dir_all(&h).unwrap();
}

/// Writes catch log lines: (days ago, set, check, outcome).
fn write_catches(h: &Path, file: &str, rows: &[(i64, &str, &str, &str)]) {
    let now = chrono::Utc::now().timestamp();
    let lines: Vec<String> = rows
        .iter()
        .map(|(days, set, check, outcome)| {
            serde_json::json!({"time": dotonce::catches::timestamp(now - days * 86_400), "session": "s1", "set": set, "check": check, "level": "fix", "outcome": outcome}).to_string()
        })
        .collect();
    std::fs::create_dir_all(h.join("dotonce")).unwrap();
    std::fs::write(h.join("dotonce").join(file), lines.join("\n") + "\n").unwrap();
}

#[test]
fn rules_log_stats_and_list_hide_private_sets_outside_private_paths() {
    let (h, proj) = home_with_private_path("stats");
    add_set(&h, "acme-replies", "");
    write_catches(
        &h,
        "catches.jsonl",
        &[
            (40, "acme-replies", "inline-links", "caught"),
            (10, "acme-replies", "inline-links", "caught"),
            (10, "acme-replies", "inline-links", "fixed"),
            (2, "acme-replies", "inline-links", "denied"),
            (1, "acme-replies", "inline-links", "caught"),
        ],
    );
    write_catches(
        &h,
        "catches-private.jsonl",
        &[(1, "secret-notes", "quiet", "caught")],
    );
    let (out, ok) = run(&h, &["rules", "log"], None);
    assert!(ok, "{out}");
    assert!(
        out.contains("acme-replies/inline-links  fix  denied"),
        "{out}"
    );
    assert!(!out.contains("secret-notes"), "{out}");
    let (out, _) = run(&h, &["rules", "log", "--limit", "1"], None);
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(out.contains("caught"), "{out}");
    let (out, _) = run_in(&h, &proj, &["rules", "log"], None);
    assert!(out.contains("secret-notes/quiet"), "{out}");
    let (out, ok) = run(&h, &["stats"], None);
    assert!(ok, "{out}");
    let row: Vec<&str> = out
        .lines()
        .find(|l| l.starts_with("acme-replies/inline-links"))
        .expect(&out)
        .split_whitespace()
        .collect();
    assert_eq!(
        row,
        [
            "acme-replies/inline-links",
            "2",
            "3",
            "1",
            "0",
            "0",
            "1",
            "0.7"
        ]
    );
    assert!(!out.contains("secret-notes"), "{out}");
    let (out, _) = run_in(&h, &proj, &["stats"], None);
    assert!(out.contains("secret-notes/quiet"), "{out}");
    let (out, _) = run(&h, &["rules", "list"], None);
    let today = chrono::Utc::now() - chrono::Duration::days(1);
    assert!(
        out.contains(&format!("last catch: {}", today.format("%Y-%m-%d"))),
        "{out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_why_shows_what_turned_each_set_on_and_pause_needs_a_terminal() {
    let h = home("why");
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    add_set(
        &h,
        "code-safety",
        &format!("[when]\nfolders = [{:?}]\n", proj.to_string_lossy()),
    );
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "code-safety", "warn");
    approve(&h, "acme-replies", "warn");
    hook(
        &h,
        "skill",
        serde_json::json!({"session_id": "s1", "prompt_id": "p1", "tool_name": "Skill", "tool_input": {"skill": "acme-writer"}, "cwd": proj}),
    );
    let (out, ok) = run_in(&h, &proj, &["rules", "why"], None);
    assert!(ok, "{out}");
    assert!(out.contains("session s1"), "{out}");
    assert!(
        out.contains("acme-replies") && out.contains("skill acme-writer"),
        "{out}"
    );
    assert!(
        out.contains("code-safety") && out.contains("folder"),
        "{out}"
    );
    let (out, _) = run_in(&h, &h, &["rules", "why"], None);
    assert!(
        !out.contains("code-safety"),
        "the folder set is off outside its folder: {out}"
    );
    let (_, err, ok) = run_full(&h, &proj, &["rules", "pause", "acme-replies"], None);
    assert!(!ok);
    assert!(err.contains("terminal"), "{err}");
    let state = std::fs::read_to_string(h.join("dotonce/sessions/s1.json")).unwrap();
    assert!(state.contains("\"paused\":[]"), "{state}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_lists_missing_hooks_and_never_writes_settings() {
    let h = home("doctor");
    let (out, ok) = run(&h, &["doctor"], None);
    assert!(ok, "{out}");
    assert!(out.contains("missing  Stop"), "{out}");
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    let settings = serde_json::json!({"hooks": {
        "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "/opt/dotonce hook prompt"}]}],
        "SessionStart": [{"hooks": [{"type": "command", "command": "/opt/dotonce hook session-start"}]}],
        "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "/opt/dotonce hook skill"}]}]
    }})
    .to_string();
    let file = h.join(".claude/settings.json");
    std::fs::write(&file, &settings).unwrap();
    let (out, ok) = run(&h, &["doctor"], None);
    assert!(ok, "{out}");
    assert!(
        out.contains("ok       UserPromptSubmit") && out.contains("ok       SessionStart"),
        "{out}"
    );
    for e in ["Stop", "PostToolUse", "UserPromptExpansion", "PreToolUse"] {
        assert!(out.contains(&format!("missing  {e}")), "{e}: {out}");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), settings);
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn hook_config_prints_every_hook_and_doctor_accepts_it() {
    let h = home("hookcfg");
    let (out, ok) = run(&h, &["hook-config"], None);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let want = [
        ("UserPromptSubmit", "prompt", None),
        ("SessionStart", "session-start", None),
        ("Stop", "stop", None),
        ("PostToolUse", "skill", Some("Skill")),
        ("UserPromptExpansion", "expansion", None),
        (
            "PreToolUse",
            "pre-tool",
            Some("Bash|Write|Edit|MultiEdit|NotebookEdit"),
        ),
        ("MessageDisplay", "display", None),
    ];
    for (event, arg, matcher) in want {
        let g = &v["hooks"][event][0];
        let cmd = g["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.ends_with(&format!("dotonce hook {arg}")), "{cmd}");
        match matcher {
            Some(m) => assert_eq!(g["matcher"], m),
            None => assert!(g.get("matcher").is_none(), "{event}"),
        }
    }
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    std::fs::write(h.join(".claude/settings.json"), &out).unwrap();
    let (out, _) = run(&h, &["doctor"], None);
    assert!(!out.contains("missing"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_warns_when_settings_point_dotonce_home_elsewhere() {
    let h = home("doctorenv");
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    let user = h.join(".claude/settings.json");
    let local = h.join(".claude/settings.local.json");
    let a = serde_json::json!({"env": {"DOTONCE_HOME": "/elsewhere/a"}}).to_string();
    let b = serde_json::json!({"env": {"DOTONCE_HOME": "/elsewhere/b"}}).to_string();
    std::fs::write(&user, &a).unwrap();
    std::fs::write(&local, &b).unwrap();
    let (out, ok) = run_in(&h, &h, &["doctor"], None);
    assert!(ok, "{out}");
    for (file, value) in [(&user, "/elsewhere/a"), (&local, "/elsewhere/b")] {
        let line = out
            .lines()
            .find(|l| l.starts_with("warning") && l.contains(&*file.to_string_lossy()))
            .unwrap_or_else(|| panic!("no warning for {}: {out}", file.display()));
        assert!(
            line.contains("DOTONCE_HOME") && line.contains(value),
            "{line}"
        );
    }
    assert_eq!(std::fs::read_to_string(&user).unwrap(), a);
    assert_eq!(std::fs::read_to_string(&local).unwrap(), b);
    std::fs::write(&user, "{}").unwrap();
    std::fs::remove_file(&local).unwrap();
    let (out, _) = run_in(&h, &h, &["doctor"], None);
    assert!(!out.contains("DOTONCE_HOME"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_pause_is_refused_under_an_ai_agent() {
    let h = home("pauseagent");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    hook(
        &h,
        "skill",
        serde_json::json!({"session_id": "s1", "tool_name": "Skill", "tool_input": {"skill": "acme-writer"}, "cwd": h}),
    );
    let file = h.join("dotonce/sessions/s1.json");
    let before = std::fs::read(&file).unwrap();
    for var in AGENT_VARS {
        let (_, err, ok) = run_env(
            &h,
            &h,
            &["rules", "pause", "acme-replies"],
            Some("acme-replies\n"),
            &[(var, "1")],
        );
        assert!(!ok, "{var}");
        assert!(err.contains("not through an AI agent"), "{var}: {err}");
    }
    assert_eq!(std::fs::read(&file).unwrap(), before);
    std::fs::remove_dir_all(&h).unwrap();
}

#[cfg(unix)]
#[test]
fn rules_pause_from_a_terminal_pauses_the_newest_session() {
    let h = home("pausepty");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    for sid in ["s-old", "s-new"] {
        hook(
            &h,
            "skill",
            serde_json::json!({"session_id": sid, "tool_name": "Skill", "tool_input": {"skill": "acme-writer"}, "cwd": h}),
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let Some((out, ok)) = run_pty(
        &h,
        &["rules", "pause", "acme-replies"],
        "acme-replies\n",
        || {},
    ) else {
        eprintln!("python3 isn't installed, skipping");
        return;
    };
    assert!(ok, "{out}");
    assert!(
        out.contains("paused acme-replies for session s-new"),
        "{out}"
    );
    let read = |sid: &str| -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(h.join(format!("dotonce/sessions/{sid}.json"))).unwrap(),
        )
        .unwrap()
    };
    let new = read("s-new");
    assert_eq!(new["paused"], serde_json::json!(["acme-replies"]), "{new}");
    assert_eq!(new["active"], serde_json::json!([]), "{new}");
    assert_eq!(read("s-old")["paused"], serde_json::json!([]));
    let (out, _) = run_in(&h, &h, &["rules", "why"], None);
    assert!(out.contains("session s-new"), "{out}");
    assert!(
        out.contains("acme-replies") && out.contains("paused for this session"),
        "{out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn printed_ids_and_times_lose_terminal_control_characters() {
    let h = home("ctrl");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    let json = h.join("dotonce/approvals.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    v["sets"]["acme-replies"]["changed"] = serde_json::json!("2026-10-28\u{1b}[2K");
    std::fs::write(&json, v.to_string()).unwrap();
    write_catches(
        &h,
        "catches.jsonl",
        &[(1, "acme-replies", "inline\u{1b}[2K-links", "caught")],
    );
    std::fs::create_dir_all(h.join("dotonce/sessions")).unwrap();
    std::fs::write(
        h.join("dotonce/sessions/s1.json"),
        serde_json::json!({"active": [{"id": "acme-replies", "by": "skill", "what": "acme\u{1b}[2K-writer"}], "paused": ["gone\u{1b}[2K"]}).to_string(),
    )
    .unwrap();
    for args in [
        &["rules", "list"][..],
        &["rules", "log"],
        &["stats"],
        &["rules", "why"],
    ] {
        let (out, ok) = run_in(&h, &h, args, None);
        assert!(ok, "{args:?}: {out}");
        assert!(!out.contains('\u{1b}'), "{args:?}: {out:?}");
        assert!(
            out.contains("[2K"),
            "{args:?} should keep the printable text: {out:?}"
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

/// Writes a session state file straight into the sessions folder.
fn write_session(h: &Path, sid: &str, state: serde_json::Value) {
    let dir = h.join("dotonce/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{sid}.json")), state.to_string()).unwrap();
}

#[test]
fn rules_why_hides_private_sets_outside_private_paths() {
    let (h, proj) = home_with_private_path("whyprivate");
    add_set(&h, "acme-replies", "");
    add_set(&h, "quiet-active", "private = true\n");
    add_set(&h, "quiet-paused", "private = true\n");
    for id in ["acme-replies", "quiet-active", "quiet-paused"] {
        approve(&h, id, "warn");
    }
    write_session(
        &h,
        "s1",
        serde_json::json!({
            "active": [{"id": "quiet-active", "by": "skill", "what": "acme-writer"}],
            "paused": ["acme-replies", "quiet-paused", "removed-set"]
        }),
    );
    let (out, ok) = run_in(&h, &h, &["rules", "why"], None);
    assert!(ok, "{out}");
    assert!(
        out.contains("acme-replies") && out.contains("paused for this session"),
        "{out}"
    );
    for hidden in ["quiet-active", "quiet-paused", "removed-set"] {
        assert!(!out.contains(hidden), "{hidden} shows outside: {out}");
    }
    let (out, ok) = run_in(&h, &proj, &["rules", "why"], None);
    assert!(ok, "{out}");
    for shown in [
        "quiet-active",
        "quiet-paused",
        "removed-set",
        "acme-replies",
    ] {
        assert!(out.contains(shown), "{shown} is missing inside: {out}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_pause_and_why_need_a_session_that_exists() {
    let h = home("nosession");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    write_session(&h, "s1", serde_json::json!({}));
    let sessions = h.join("dotonce/sessions");
    let list = || {
        let mut names: Vec<String> = std::fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let before = list();
    for (given, shown) in [("nope", "nope"), ("../x", "x")] {
        let (_, err, ok) = run_full(
            &h,
            &h,
            &["rules", "pause", "acme-replies", "--session", given],
            Some("acme-replies\n"),
        );
        assert!(!ok, "{given}");
        assert!(err.contains(&format!("no session {shown}")), "{err}");
        let (out, err, ok) = run_full(&h, &h, &["rules", "why", "--session", given], None);
        assert!(!ok, "{given}: {out}");
        assert!(err.contains(&format!("no session {shown}")), "{err}");
        assert!(!out.contains("session "), "{out}");
    }
    assert_eq!(list(), before);
    assert!(!h.join("dotonce/x.json").exists());
    let (out, ok) = run_in(&h, &h, &["rules", "why", "--session", "s1"], None);
    assert!(ok && out.contains("session s1"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

/// A settings file holding one dotonce hook per event.
fn settings_with_hooks(name: &str) -> String {
    let h = home(name);
    let (out, ok) = run(&h, &["hook-config"], None);
    assert!(ok);
    std::fs::remove_dir_all(&h).unwrap();
    out
}

#[test]
fn doctor_finds_hooks_in_project_settings_and_names_the_file() {
    let h = home("doctorproj");
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    let all = settings_with_hooks("hookcfgsrc1");
    let project = proj.join(".claude/settings.json");
    std::fs::write(&project, &all).unwrap();
    let (out, ok) = run_in(&h, &proj, &["doctor"], None);
    assert!(ok, "{out}");
    assert!(!out.contains("missing"), "{out}");
    for line in out.lines().filter(|l| l.starts_with("ok ")) {
        assert!(line.contains(&*project.to_string_lossy()), "{line}");
    }
    assert_eq!(
        out.lines().filter(|l| l.starts_with("ok ")).count(),
        7,
        "{out}"
    );
    // Some hooks in the user file, the rest in the local file.
    let mut v: serde_json::Value = serde_json::from_str(&all).unwrap();
    let mut local = serde_json::json!({"hooks": {}});
    for event in ["Stop", "PreToolUse", "MessageDisplay"] {
        local["hooks"][event] = v["hooks"].as_object_mut().unwrap().remove(event).unwrap();
    }
    std::fs::write(h.join(".claude/settings.json"), v.to_string()).unwrap();
    std::fs::write(proj.join(".claude/settings.local.json"), local.to_string()).unwrap();
    std::fs::remove_file(&project).unwrap();
    let (out, _) = run_in(&h, &proj, &["doctor"], None);
    assert!(!out.contains("missing"), "{out}");
    let stop = out.lines().find(|l| l.contains("Stop")).expect(&out);
    assert!(stop.contains("settings.local.json"), "{stop}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_warns_about_a_broken_settings_file_and_goes_on() {
    let h = home("doctorbroken");
    let proj = h.join("proj");
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    let user = h.join(".claude/settings.json");
    std::fs::write(&user, "{ not json").unwrap();
    std::fs::write(
        proj.join(".claude/settings.json"),
        settings_with_hooks("hookcfgsrc2"),
    )
    .unwrap();
    let (out, ok) = run_in(&h, &proj, &["doctor"], None);
    assert!(ok, "{out}");
    let warn = out
        .lines()
        .find(|l| l.starts_with("warning") && l.contains(&*user.to_string_lossy()))
        .unwrap_or_else(|| panic!("no warning for the broken file: {out}"));
    assert!(warn.contains("JSON"), "{warn}");
    assert!(!out.contains("missing"), "{out}");
    assert!(out.contains("rule sets: 0 loaded"), "{out}");
    assert_eq!(std::fs::read_to_string(&user).unwrap(), "{ not json");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_list_last_catch_counts_only_caught_and_denied_with_real_times() {
    let h = home("lastcatch");
    add_set(&h, "acme-replies", "");
    add_set(&h, "acme-notes", "");
    add_set(&h, "acme-docs", "");
    let now = chrono::Utc::now().timestamp();
    let line = |time: &str, set: &str, outcome: &str| {
        serde_json::json!({"time": time, "session": "s1", "set": set, "check": "c", "level": "fix", "outcome": outcome}).to_string()
    };
    let day = |n: i64| dotonce::catches::timestamp(now - n * 86_400);
    let lines = [
        line(&day(5), "acme-replies", "caught"),
        line(&day(1), "acme-replies", "fixed"),
        line(&day(2), "acme-replies", "still_broken"),
        line("yesterday", "acme-replies", "denied"),
        line(&day(3), "acme-notes", "unverified"),
        line(&day(4), "acme-docs", "denied"),
    ];
    std::fs::write(h.join("dotonce/catches.jsonl"), lines.join("\n") + "\n").unwrap();
    let (out, ok) = run(&h, &["rules", "list"], None);
    assert!(ok, "{out}");
    let row = |id: &str| {
        out.lines()
            .find(|l| l.starts_with(id))
            .expect(&out)
            .to_string()
    };
    let date = |n: i64| {
        (chrono::Utc::now() - chrono::Duration::days(n))
            .format("%Y-%m-%d")
            .to_string()
    };
    assert!(
        row("acme-replies").contains(&format!("last catch: {}", date(5))),
        "{out}"
    );
    assert!(row("acme-notes").contains("last catch: none"), "{out}");
    assert!(
        row("acme-docs").contains(&format!("last catch: {}", date(4))),
        "{out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_log_with_a_zero_limit_says_nothing_is_shown() {
    let h = home("loglimit0");
    write_catches(&h, "catches.jsonl", &[(1, "acme-replies", "c", "caught")]);
    let (out, ok) = run(&h, &["rules", "log", "--limit", "0"], None);
    assert!(ok, "{out}");
    assert_eq!(out.trim(), "no catches shown (limit 0)");
    std::fs::remove_dir_all(&h).unwrap();
}

/// A home with one public set and five sets that count as private outside
/// `private_paths`, each by a different source: the loaded file, a broken
/// file saying `private = true`, `"private" = true` or `private.x`, and the
/// last session's record. Every private one names "launch".
fn home_with_quiet_sets(name: &str) -> (PathBuf, PathBuf) {
    let (h, proj) = home_with_private_path(name);
    add_set(
        &h,
        "acme-replies",
        "[[check]]\nid = \"inline-links\"\nkind = \"bare-url\"\nwhy = \"Links go inline.\"\n",
    );
    add_set(
        &h,
        "quiet-launch",
        "private = true\n[[check]]\nid = \"no-launch\"\nforbid = [\"launch day\"]\nwhy = \"Keep the launch quiet.\"\n",
    );
    add_set(
        &h,
        "quiet-broken",
        "private = true\ncolour = \"launch day\"\n",
    );
    add_set(
        &h,
        "quoted-launch",
        "\"private\" = true\ncolour = \"launch day\"\n",
    );
    add_set(&h, "dotted-launch", "private.x = \"launch day\"\n");
    add_set(
        &h,
        "was-launch",
        "[[check]]\nid = \"launch\"\nforbid = [\"x\"]\nwhy = \"w\"\n",
    );
    for id in ["acme-replies", "quiet-launch", "was-launch"] {
        approve(&h, id, "warn");
    }
    std::fs::write(
        h.join("dotonce/last-live.json"),
        serde_json::json!({"live": {}, "private": {"was-launch": true, "acme-replies": false}})
            .to_string(),
    )
    .unwrap();
    (h, proj)
}

const QUIET_IDS: [&str; 5] = [
    "quiet-launch",
    "quiet-broken",
    "quoted-launch",
    "dotted-launch",
    "was-launch",
];

#[test]
fn rules_check_hides_private_sets_outside_private_paths() {
    let (h, proj) = home_with_quiet_sets("checkquiet");
    let (out, err, ok) = run_full(&h, &h, &["rules", "check"], None);
    assert!(!ok, "the broken files still fail the check: {out}");
    assert!(out.contains("ok     acme-replies (1 checks)"), "{out}");
    assert!(!out.contains("launch"), "{out}");
    assert!(
        out.contains("5 private rule sets hidden; run this inside a private folder to see them"),
        "{out}"
    );
    assert!(!err.contains("launch"), "{err}");
    let (out, _, _) = run_full(&h, &proj, &["rules", "check"], None);
    for id in QUIET_IDS {
        assert!(out.contains(id), "{id} is missing inside: {out}");
    }
    assert!(out.contains("launch day"), "{out}");
    assert!(!out.contains("hidden"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_list_hides_broken_and_recorded_private_sets() {
    let (h, proj) = home_with_quiet_sets("listquiet");
    let (out, ok) = run(&h, &["rules", "list"], None);
    assert!(ok, "{out}");
    assert!(out.contains("acme-replies"), "{out}");
    assert!(!out.contains("launch"), "{out}");
    assert!(
        out.contains("5 private rule sets hidden; run this inside a private folder to see them"),
        "{out}"
    );
    let (out, _) = run_in(&h, &proj, &["rules", "list"], None);
    for id in QUIET_IDS {
        assert!(out.contains(id), "{id} is missing inside: {out}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_test_refuses_a_private_set_outside_private_paths() {
    let (h, proj) = home_with_quiet_sets("testquiet");
    let f = h.join("reply.md");
    std::fs::write(&f, "The launch day is set.\n").unwrap();
    let f = f.to_string_lossy().to_string();
    for id in QUIET_IDS {
        let (out, err, ok) = run_full(&h, &h, &["rules", "test", id, &f], None);
        assert!(!ok, "{id}: {out}");
        assert!(
            err.contains("private") && err.contains("inside a private folder"),
            "{id}: {err}"
        );
        assert!(
            !out.contains("launch") && !err.contains("launch day"),
            "{id}: {out} {err}"
        );
    }
    let (out, _, ok) = run_full(&h, &proj, &["rules", "test", "quiet-launch", &f], None);
    assert!(ok, "{out}");
    assert!(out.contains("no-launch"), "{out}");
    let (_, err, ok) = run_full(&h, &proj, &["rules", "test", "quiet-broken", &f], None);
    assert!(!ok);
    assert!(err.contains("has an error"), "{err}");
    let (out, _, ok) = run_full(&h, &h, &["rules", "test", "acme-replies", &f], None);
    assert!(ok, "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_why_log_and_stats_hide_sets_any_source_calls_private() {
    let (h, proj) = home_with_quiet_sets("whyquiet");
    write_session(
        &h,
        "s1",
        serde_json::json!({
            "active": [
                {"id": "acme-replies", "by": "skill", "what": "acme-writer"},
                {"id": "was-launch", "by": "skill", "what": "acme-writer"}
            ],
            "paused": ["quiet-launch"]
        }),
    );
    // Caught while each set was public, so the lines are in the public log.
    write_catches(
        &h,
        "catches.jsonl",
        &[
            (1, "acme-replies", "inline-links", "caught"),
            (1, "quiet-launch", "no-launch", "caught"),
            (1, "was-launch", "launch", "caught"),
            (1, "quoted-launch", "launch", "caught"),
        ],
    );
    for args in [&["rules", "why"][..], &["rules", "log"], &["stats"]] {
        let (out, ok) = run_in(&h, &h, args, None);
        assert!(ok, "{args:?}: {out}");
        assert!(out.contains("acme-replies"), "{args:?}: {out}");
        assert!(!out.contains("launch"), "{args:?}: {out}");
        let (out, ok) = run_in(&h, &proj, args, None);
        assert!(ok, "{args:?}: {out}");
        assert!(out.contains("was-launch"), "{args:?} inside: {out}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn rules_check_and_test_print_no_control_characters() {
    let h = home("ctrlcheck");
    add_set(
        &h,
        "acme-replies",
        "[[check]]\nid = \"st\\u001b[2Kock\"\nforbid = [\"sorry\"]\nwhy = \"Open\\u001b[2K plainly.\"\n",
    );
    add_set(
        &h,
        "acme-notes",
        "[[check]]\nid = \"a\"\nwhy = \"b\"\nkind = \"bare\\u001b[2K-link\"\n",
    );
    std::fs::write(h.join("dotonce/rules/odd\u{1b}[2K.toml"), "id = \"odd\"\n").unwrap();
    let (out, err, ok) = run_full(&h, &h, &["rules", "check"], None);
    assert!(!ok);
    assert!(
        !out.contains('\u{1b}') && !err.contains('\u{1b}'),
        "{out:?} {err:?}"
    );
    assert!(
        out.contains("bare[2K-link") && out.contains("odd[2K.toml"),
        "{out:?}"
    );
    let f = h.join("reply.md");
    std::fs::write(&f, "Sorry\u{1b}[2K about that.\n").unwrap();
    let f = f.to_string_lossy().to_string();
    let (out, err, ok) = run_full(&h, &h, &["rules", "test", "acme-replies", &f], None);
    assert!(ok, "{err}");
    assert!(!out.contains('\u{1b}'), "{out:?}");
    assert!(
        out.contains("st[2Kock: \"Sorry[2K about that.\"") && out.contains("Open[2K plainly."),
        "{out:?}"
    );
    let (_, err, ok) = run_full(&h, &h, &["rules", "test", "acme-notes", &f], None);
    assert!(!ok);
    assert!(
        !err.contains('\u{1b}') && err.contains("bare[2K-link"),
        "{err:?}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

fn run_with_path(home: &Path, exe: &Path, path: Option<&std::ffi::OsStr>, args: &[&str]) -> String {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(home)
        .env("DOTONCE_HOME", home.join("dotonce"))
        .env("HOME", home)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::null());
    match path {
        Some(p) => cmd.env("PATH", p),
        None => cmd.env_remove("PATH"),
    };
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{:?}", out);
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const PATH_HINT: &str = "dotonce is not on your PATH, so the short `dotonce` command won't work in a new terminal. Add its folder once:";

#[test]
fn init_and_doctor_print_a_path_hint_when_the_binary_folder_is_off_path() {
    let h = home("pathhint");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_dotonce"));
    let elsewhere = h.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let missing = h.join("no-such-folder");
    let joined = std::env::join_paths([&missing, &elsewhere]).unwrap();
    for (label, path) in [
        ("other folders", Some(joined.as_os_str())),
        ("empty PATH", Some(std::ffi::OsStr::new(""))),
        ("no PATH", None),
    ] {
        for cmd in ["init", "doctor"] {
            let out = run_with_path(&h, &exe, path, &[cmd]);
            assert!(out.contains(PATH_HINT), "{cmd} with {label}: {out}");
            assert!(
                out.contains(
                    "then open a new terminal (or run `source ~/.zshrc`). On bash, use ~/.bashrc."
                ),
                "{cmd} with {label}: {out}"
            );
            let line = out
                .lines()
                .find(|l| l.starts_with("echo 'export PATH=\""))
                .unwrap_or_else(|| panic!("{cmd} with {label}: {out}"));
            assert!(line.ends_with(":$PATH\"' >> ~/.zshrc"), "{line}");
            assert!(
                line.contains(exe.parent().unwrap().to_str().unwrap()),
                "{line}"
            );
        }
    }
    assert!(!h.join(".zshrc").exists());
    assert!(!h.join(".bashrc").exists());
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn init_and_doctor_print_no_path_hint_when_the_binary_folder_is_on_path() {
    let h = home("pathok");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_dotonce"));
    let dir = exe.parent().unwrap();
    let other = h.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let link = h.join("link");
    std::os::unix::fs::symlink(dir, &link).unwrap();
    for entries in [
        vec![dir.to_path_buf()],
        vec![other.clone(), dir.to_path_buf()],
        vec![other.clone(), link.clone()],
    ] {
        let path = std::env::join_paths(&entries).unwrap();
        for cmd in ["init", "doctor"] {
            let out = run_with_path(&h, &exe, Some(&path), &[cmd]);
            assert!(
                !out.contains("not on your PATH"),
                "{cmd} {entries:?}: {out}"
            );
            assert!(!out.contains("export PATH"), "{cmd} {entries:?}: {out}");
        }
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn path_hint_writes_home_when_the_binary_is_under_home() {
    let h = home("pathhome");
    let bin = h.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("dotonce");
    std::fs::copy(env!("CARGO_BIN_EXE_dotonce"), &exe).unwrap();
    let elsewhere = h.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for cmd in ["init", "doctor"] {
        let out = run_with_path(&h, &exe, Some(elsewhere.as_os_str()), &[cmd]);
        assert!(
            out.contains("echo 'export PATH=\"$HOME/bin:$PATH\"' >> ~/.zshrc"),
            "{cmd}: {out}"
        );
        let line = out.lines().find(|l| l.starts_with("echo ")).unwrap();
        assert!(!line.contains(h.to_str().unwrap()), "{line}");
    }
    let on_path = run_with_path(&h, &exe, Some(bin.as_os_str()), &["doctor"]);
    assert!(!on_path.contains("not on your PATH"), "{on_path}");
    std::fs::remove_dir_all(&h).unwrap();
}

/// Prompts Claude Code sends itself, one of each kind, each with words that
/// turn on `REPLIES` and match the staging site fact.
const FROM_CLAUDE_CODE: [&str; 5] = [
    "<task-notification>\n<task-id>t1</task-id>\n<status>completed</status>\n<summary>draft the staging site reply</summary>\n</task-notification>",
    "[Subagent hand-back] draft the staging site reply is ready",
    "<agent-message from=\"acme\">draft the staging site reply</agent-message>",
    "[SYSTEM NOTIFICATION] draft the staging site reply",
    "<system-reminder>draft the staging site reply</system-reminder>",
];

#[test]
fn claude_codes_own_messages_get_no_reminders_or_facts() {
    let h = home("ownmsg");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    for (i, text) in FROM_CLAUDE_CODE.iter().enumerate() {
        let out = hook(
            &h,
            "prompt",
            serde_json::json!({"session_id": format!("s{i}"), "prompt_id": "p1", "prompt": text, "cwd": "/tmp"}),
        );
        assert!(out.trim().is_empty(), "{text}: {out}");
        let exp = serde_json::json!({"session_id": format!("e{i}"), "prompt_id": "p1", "hook_event_name": "UserPromptExpansion", "expansion_type": "slash_command", "command_name": "acme-writer", "command_args": "", "command_source": "user", "prompt": text, "cwd": "/tmp"});
        let out = hook(&h, "expansion", exp);
        assert!(out.trim().is_empty(), "{text}: {out}");
    }
    let typed = serde_json::json!({"session_id": "s9", "prompt_id": "p1", "prompt": "draft the staging site reply", "cwd": "/tmp"});
    let ctx = context(&hook(&h, "prompt", typed), "UserPromptSubmit");
    assert!(
        ctx.contains("Links go inline") && ctx.contains("acme.example.com"),
        "a typed prompt still gets both: {ctx}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_typed_prompt_that_pastes_claude_codes_markers_keeps_its_context() {
    let h = home("ownpaste");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    // The fact search reads pasted words too, and a long paste of words no
    // fact has drowns the fact, so the pastes here are short.
    let typed = "draft the reply for the staging site acme.example.com";
    for (i, text) in [
        "<task-notification>",
        "[Subagent hand-back]",
        "<agent-message>",
        "[SYSTEM NOTIFICATION]",
        "<system-reminder>",
    ]
    .iter()
    .enumerate()
    {
        for (j, prompt) in [
            format!("{typed}\n```\n{text}\n```"),
            format!("{typed}\n<pasted_content>{text}</pasted_content>"),
            format!("{typed}\n> {text}"),
        ]
        .iter()
        .enumerate()
        {
            let input = serde_json::json!({"session_id": format!("s{i}-{j}"), "prompt_id": "p1", "prompt": prompt, "cwd": "/tmp"});
            let ctx = context(&hook(&h, "prompt", input), "UserPromptSubmit");
            assert!(
                ctx.contains("Links go inline") && ctx.contains("acme.example.com"),
                "{prompt}: {ctx}"
            );
        }
        let exp = serde_json::json!({"session_id": format!("e{i}"), "prompt_id": "p1", "hook_event_name": "UserPromptExpansion", "expansion_type": "slash_command", "command_name": "acme-writer", "command_args": "", "command_source": "user", "prompt": format!("/acme-writer what is this?\n```\n{text}\n```"), "cwd": "/tmp"});
        let ctx = context(&hook(&h, "expansion", exp), "UserPromptExpansion");
        assert!(ctx.contains("Links go inline"), "{text}: {ctx}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn a_turn_claude_code_started_is_still_checked_and_sent_back() {
    let h = acme_home("ownstop", "fix");
    register_stop(&h);
    use_skill(&h, "s1", "p1");
    let note = serde_json::json!({"session_id": "s1", "prompt_id": "p2", "prompt": FROM_CLAUDE_CODE[0], "cwd": "/tmp"});
    assert!(
        hook(&h, "prompt", note).trim().is_empty(),
        "no reminder or fact"
    );
    assert_eq!(show_draft(&h, &h, "s1", "p2", "m1"), labeled());
    let back = sent_back(&stop(&h, "s1", "p2", BROKEN, false));
    assert!(back.contains("no-stock-wait-opener"), "{back}");
    assert!(
        outcomes(&h, "catches.jsonl")
            .iter()
            .any(|(c, o)| c == "no-stock-wait-opener" && o == "caught"),
        "the catch is logged"
    );
    assert!(
        stop(&h, "s1", "p2", BROKEN_AGAIN, true).trim().is_empty(),
        "sent back once"
    );
    for (i, text) in FROM_CLAUDE_CODE.iter().enumerate() {
        let pid = format!("q{i}");
        let note = serde_json::json!({"session_id": "s1", "prompt_id": pid, "prompt": text, "cwd": "/tmp"});
        assert!(hook(&h, "prompt", note).trim().is_empty(), "{text}");
        assert!(
            !sent_back(&stop(&h, "s1", &pid, BROKEN, false)).is_empty(),
            "{text}"
        );
    }
    std::fs::remove_dir_all(&h).unwrap();
}

const CHAT: &str = "[when]\nprompt = [\"draft\"]\ncheck_on_prompt = true\n[remind]\ntext = \"Keep chat replies short.\"\n[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"no-stock-wait-opener\"\nforbid = [\"sorry for the wait\"]\nwhy = \"Open with a line from the thread.\"\n";

#[test]
fn a_prompt_word_reminder_comes_once_per_session_and_checks_run_every_turn() {
    let h = home("remindonce");
    add_set(&h, "acme-chat", CHAT);
    approve(&h, "acme-chat", "fix");
    let prompt = |sid: &str, pid: &str, text: &str| {
        hook(
            &h,
            "prompt",
            serde_json::json!({"session_id": sid, "prompt_id": pid, "prompt": text, "cwd": "/tmp"}),
        )
    };
    let reminded = |out: &str| out.contains("Keep chat replies short.");
    assert!(reminded(&prompt("s1", "p1", "draft it")));
    let out = prompt("s1", "p2", "draft the next one");
    assert!(!reminded(&out), "the second turn gets no reminder: {out}");
    assert!(
        !sent_back(&stop(&h, "s1", "p2", BROKEN, false)).is_empty(),
        "the set still checks this turn"
    );
    assert!(
        reminded(&prompt("s2", "p1", "draft it")),
        "a new session gets it"
    );
    hook(
        &h,
        "session-start",
        serde_json::json!({"session_id": "s1", "source": "compact", "cwd": "/tmp"}),
    );
    assert!(
        reminded(&prompt("s1", "p3", "draft a third")),
        "after compaction it comes again"
    );
    assert!(!reminded(&prompt("s1", "p4", "draft a fourth")));
    edit_set(&h, "acme-chat");
    approve(&h, "acme-chat", "fix");
    assert!(
        reminded(&prompt("s1", "p5", "draft a fifth")),
        "a changed file comes again"
    );
    assert!(!reminded(&prompt("s1", "p6", "draft a sixth")));
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn an_ignored_folder_gets_nothing_but_the_guard() {
    let (h, proj) = code_home("ignore", "fix");
    run(&h, &["init"], None);
    run(&h, &["save", "The staging site is acme.example.com"], None);
    add_set(&h, "acme-mail", ACME_MAIL);
    approve(&h, "acme-mail", "fix");
    add_set(&h, "acme-replies", REPLIES);
    approve(&h, "acme-replies", "warn");
    register_stop(&h);
    let notes = h.join("dotonce/notes");
    std::fs::write(
        h.join("dotonce/config.toml"),
        format!("ignore_paths = [{proj:?}]\n\n[[stores]]\nname = \"notes\"\npath = {notes:?}\n"),
    )
    .unwrap();
    use_skill(&h, "s1", "p1");
    let state = h.join("dotonce/sessions/s1.json");
    let before = std::fs::read_to_string(&state).unwrap();
    let at = |extra: serde_json::Value| {
        let mut v = serde_json::json!({"session_id": "s1", "prompt_id": "p2", "cwd": proj});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    let quiet = [
        (
            "prompt",
            at(serde_json::json!({"prompt": "draft the staging site reply"})),
        ),
        (
            "session-start",
            at(serde_json::json!({"source": "compact"})),
        ),
        (
            "skill",
            at(serde_json::json!({"tool_name": "Skill", "tool_input": {"skill": "acme-writer"}})),
        ),
        (
            "expansion",
            at(serde_json::json!({"command_name": "acme-writer", "prompt": "/acme-writer"})),
        ),
        (
            "stop",
            at(serde_json::json!({"last_assistant_message": BROKEN, "stop_hook_active": false})),
        ),
    ];
    for (event, input) in quiet {
        let out = hook(&h, event, input);
        assert!(out.trim().is_empty(), "{event}: {out}");
    }
    assert_eq!(show_draft(&h, &proj, "s1", "p2", "m1"), None);
    let bash = serde_json::json!({"command": "git push --force origin main"});
    assert_eq!(pre(&h, &proj, "Bash", bash), None, "no tool checks");
    let rule = h
        .join("dotonce/rules/x.toml")
        .to_string_lossy()
        .into_owned();
    let write = serde_json::json!({"file_path": rule, "content": "id = \"x\"\n"});
    assert!(
        pre(&h, &proj, "Write", write).is_some_and(|r| r.starts_with("dotonce: rule files")),
        "the guard still runs"
    );
    assert_eq!(std::fs::read_to_string(&state).unwrap(), before);
    assert!(!h.join("dotonce/catches.jsonl").exists());
    let out = hook(
        &h,
        "prompt",
        serde_json::json!({"session_id": "s2", "prompt_id": "p1", "prompt": "draft the staging site reply", "cwd": "/tmp"}),
    );
    assert!(out.contains("acme.example.com"), "outside it: {out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn prompt_words_skip_quoted_lines_and_match_a_phrase_whole() {
    let h = home("phrase");
    add_set(
        &h,
        "acme-notes",
        "[when]\nprompt = [\"internal note\"]\n[remind]\ntext = \"Internal notes stay short.\"\n",
    );
    approve(&h, "acme-notes", "warn");
    let prompt = |sid: &str, text: &str| {
        hook(
            &h,
            "prompt",
            serde_json::json!({"session_id": sid, "prompt_id": "p1", "prompt": text, "cwd": "/tmp"}),
        )
    };
    for (i, text) in [
        "The internal tool left a note on the acme order",
        "keep it internal, note the date",
        "> add an internal note for acme\nwhat does this mean?",
    ]
    .iter()
    .enumerate()
    {
        let out = prompt(&format!("s{i}"), text);
        assert!(out.trim().is_empty(), "{text:?}: {out}");
    }
    assert!(prompt("s9", "add an internal note for acme").contains("Internal notes stay short."));
    std::fs::remove_dir_all(&h).unwrap();
}
