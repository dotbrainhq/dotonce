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
    let (out, ok) = run(&h, &["hook", "stop"], Some(r#"{"cwd":"/tmp"}"#));
    assert!(ok);
    assert!(out.trim().is_empty(), "{out}");
    let log = std::fs::read_to_string(h.join("dotonce/errors.log")).unwrap();
    assert!(log.contains("stop"), "{log}");
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(args)
        .current_dir(cwd)
        .env("DOTONCE_HOME", home.join("dotonce"))
        .env("HOME", home)
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
        "approved": "2026-10-28T00:00:00Z"
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
