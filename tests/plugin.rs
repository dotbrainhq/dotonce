//! The Claude Code plugin: its hooks match `dotonce hook-config`, its
//! wrapper finds the binary or does nothing, its notice tells the user when
//! dotonce can't run, `doctor` reports a plugin install and hooks that run
//! twice, and the guard keeps Claude from turning the plugin off.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// The plugin folder the marketplace file points at.
fn plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin")
}

fn home(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dotonce-plugin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn write_json(p: &Path, v: &Value) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, v.to_string()).unwrap();
}

fn run_in(h: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(args)
        .current_dir(cwd)
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", h)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_PLUGIN_ROOT")
        .env_remove("AI_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "dotonce {args:?} failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Records the plugin as Claude Code does after `claude plugin install`.
fn install(h: &Path, installs: Value) {
    let list = json!({"version": 2, "plugins": {"dotonce@dotonce": installs}});
    write_json(&h.join(".claude/plugins/installed_plugins.json"), &list);
}

fn user_install(dir: &Path) -> Value {
    json!([{"scope": "user", "installPath": dir, "version": "0.0.1"}])
}

/// A copy of the plugin's hooks file without the Stop hook, like an older
/// plugin release.
fn plugin_without_stop(h: &Path) -> PathBuf {
    let dir = h.join("old-plugin");
    let mut hooks = read_json(&plugin_dir().join("hooks/hooks.json"));
    hooks["hooks"].as_object_mut().unwrap().remove("Stop");
    write_json(&dir.join("hooks/hooks.json"), &hooks);
    dir
}

const EVENTS: [&str; 7] = [
    "UserPromptSubmit",
    "SessionStart",
    "Stop",
    "PostToolUse",
    "UserPromptExpansion",
    "PreToolUse",
    "MessageDisplay",
];

#[test]
fn the_plugin_hooks_are_the_hook_config_block() {
    let h = home("hooksfile");
    let mut want: Value = serde_json::from_str(&run_in(&h, &h, &["hook-config"])).unwrap();
    let mut have = read_json(&plugin_dir().join("hooks/hooks.json"));
    let start = have["hooks"]["SessionStart"][0]["hooks"]
        .as_array_mut()
        .unwrap();
    let notice = json!({"type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/hooks/notice\"", "timeout": 5});
    assert_eq!(start.pop(), Some(notice), "the plugin adds its notice");
    for groups in want["hooks"].as_object_mut().unwrap().values_mut() {
        for g in groups.as_array_mut().unwrap() {
            let cmd = &mut g["hooks"][0]["command"];
            let arg = cmd.as_str().unwrap().rsplit_once(" hook ").unwrap().1;
            *cmd = json!(format!(
                "\"${{CLAUDE_PLUGIN_ROOT}}/hooks/dotonce\" hook {arg}"
            ));
        }
    }
    assert_eq!(have, want);
    let manifest = read_json(&plugin_dir().join(".claude-plugin/plugin.json"));
    assert_eq!(manifest["name"], "dotonce");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    let market =
        read_json(&Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude-plugin/marketplace.json"));
    assert_eq!(
        market["name"], "dotonce",
        "the install id is dotonce@dotonce"
    );
    assert_eq!(market["plugins"][0]["name"], "dotonce");
    assert_eq!(market["plugins"][0]["source"], "./plugin");
    std::fs::remove_dir_all(&h).unwrap();
}

#[cfg(unix)]
#[test]
fn the_plugin_wrapper_runs_dotonce_or_nothing() {
    let h = home("wrapper");
    let exe = Path::new(env!("CARGO_BIN_EXE_dotonce"));
    let wrap = |path: &str, home: &Path| {
        let out = Command::new(plugin_dir().join("hooks/dotonce"))
            .arg("hook-config")
            .env_clear()
            .env("PATH", path)
            .env("HOME", home)
            .env("DOTONCE_HOME", h.join("dotonce"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    };
    let on_path = format!("{}:/usr/bin:/bin", exe.parent().unwrap().display());
    let (out, _, ok) = wrap(&on_path, &h);
    assert!(ok && out.contains("hook prompt"), "dotonce on PATH: {out}");

    let cargo_bin = h.join(".cargo/bin");
    std::fs::create_dir_all(&cargo_bin).unwrap();
    std::os::unix::fs::symlink(exe, cargo_bin.join("dotonce")).unwrap();
    let (out, _, ok) = wrap("/usr/bin:/bin", &h);
    assert!(
        ok && out.contains("hook prompt"),
        "dotonce in ~/.cargo/bin: {out}"
    );

    let bare = h.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let (out, err, ok) = wrap("/usr/bin:/bin", &bare);
    assert!(
        ok && out.is_empty() && err.is_empty(),
        "no binary: the hook does nothing and exits 0: {out}{err}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs the plugin's session-start notice and returns the message it shows
/// the user, or None when it prints nothing.
#[cfg(unix)]
fn notice(script: &Path, path: &str, home: &Path) -> Option<String> {
    let out = Command::new(script)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "the notice never fails");
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    if out.trim().is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    Some(v["systemMessage"].as_str().unwrap().to_string())
}

/// A copy of the plugin's notice whose plugin.json has another version.
#[cfg(unix)]
fn plugin_at(h: &Path, version: &str) -> PathBuf {
    let dir = h.join(format!("plugin-{version}"));
    std::fs::create_dir_all(dir.join("hooks")).unwrap();
    std::fs::copy(plugin_dir().join("hooks/notice"), dir.join("hooks/notice")).unwrap();
    let mut manifest = read_json(&plugin_dir().join(".claude-plugin/plugin.json"));
    manifest["version"] = json!(version);
    let text = serde_json::to_string_pretty(&manifest).unwrap();
    std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    std::fs::write(dir.join(".claude-plugin/plugin.json"), text).unwrap();
    dir.join("hooks/notice")
}

#[cfg(unix)]
#[test]
fn the_plugin_notice_tells_the_user_when_dotonce_cant_run() {
    let h = home("notice");
    let bare = h.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let script = plugin_dir().join("hooks/notice");
    let missing = notice(&script, "/usr/bin:/bin", &bare).expect("no binary: a notice");
    assert_eq!(
        missing,
        "dotonce isn't installed, so its hooks are off. Install it from github.com/dotbrainhq/dotonce, then start a new session."
    );

    let exe = Path::new(env!("CARGO_BIN_EXE_dotonce"));
    let on_path = format!("{}:/usr/bin:/bin", exe.parent().unwrap().display());
    assert_eq!(
        notice(&script, &on_path, &bare),
        None,
        "same version: quiet"
    );

    let have = env!("CARGO_PKG_VERSION");
    let older = notice(&plugin_at(&h, "99.0.0"), &on_path, &bare).expect("plugin newer: a notice");
    assert_eq!(
        older,
        format!(
            "dotonce {have} is older than its plugin (99.0.0), so some hooks may not work. Update it, then start a new session."
        )
    );
    assert_eq!(
        notice(&plugin_at(&h, "0.0.0"), &on_path, &bare),
        None,
        "a newer binary is fine"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

/// Runs the PreToolUse hook on a Bash command and returns the deny reason.
fn pre_tool(h: &Path, cmd: &str) -> Option<String> {
    let input = json!({"session_id": "s1", "prompt_id": "p1", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": cmd}, "cwd": h});
    let mut child = Command::new(env!("CARGO_BIN_EXE_dotonce"))
        .args(["hook", "pre-tool"])
        .current_dir(h)
        .env("DOTONCE_HOME", h.join("dotonce"))
        .env("HOME", h)
        .env_remove("CLAUDE_PLUGIN_ROOT")
        .env_remove("AI_AGENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "pre-tool failed on {cmd}");
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    if out.trim().is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    Some(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn the_guard_keeps_the_plugin_on() {
    let h = home("guard");
    for cmd in [
        "claude plugin disable dotonce@dotonce",
        "claude plugin disable dotonce",
        "claude plugins disable -s user dotonce@dotonce",
        "claude plugin disable --all",
        "claude plugin disable -a",
        "claude plugin uninstall dotonce@dotonce",
        "claude plugin remove dotonce@dotonce --scope project",
        "claude plugin marketplace remove dotonce",
        "claude plugin marketplace rm dotonce",
        "/opt/acme/bin/claude plugin disable dotonce@dotonce",
        "cd /tmp && claude plugin disable dotonce@dotonce",
        "sudo claude plugin disable dotonce@dotonce",
        "env ACME=1 claude plugin uninstall dotonce",
        "sh -c 'claude plugin disable dotonce@dotonce'",
        "bash -c \"eval 'claude plugin uninstall dotonce'\"",
        "echo 'claude plugin disable dotonce@dotonce' | sh",
    ] {
        let reason = pre_tool(&h, cmd).unwrap_or_else(|| panic!("allowed: {cmd}"));
        assert!(
            reason.starts_with("dotonce: Claude can't turn off or remove the dotonce plugin"),
            "{cmd}: {reason}"
        );
    }
    for cmd in [
        "claude plugin list",
        "claude plugin details dotonce@dotonce",
        "claude plugin update dotonce@dotonce",
        "claude plugin enable dotonce@dotonce",
        "claude plugin disable acme@acme",
        "claude plugin uninstall acme@acme",
        "claude plugin marketplace remove acme",
        "grep 'claude plugin disable dotonce' README.md",
    ] {
        assert_eq!(pre_tool(&h, cmd), None, "denied: {cmd}");
    }
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_reports_the_plugin_and_hooks_that_run_twice() {
    let h = home("doctor");
    install(&h, user_install(&plugin_dir()));
    let out = run_in(&h, &h, &["doctor"]);
    assert!(
        out.contains("note     plugin dotonce@dotonce is installed but off here"),
        "{out}"
    );
    assert!(out.contains("missing  Stop"), "{out}");

    let user = h.join(".claude/settings.json");
    write_json(&user, &json!({"enabledPlugins": {"dotonce@dotonce": true}}));
    let out = run_in(&h, &h, &["doctor"]);
    for e in EVENTS {
        assert!(
            out.lines().any(|l| l.starts_with(&format!("ok       {e} "))
                && l.ends_with("in plugin dotonce@dotonce")),
            "{e}: {out}"
        );
    }
    assert!(
        !out.contains("missing") && !out.contains("twice") && !out.contains("note"),
        "{out}"
    );

    // The hooks block from hook-config added by hand as well.
    let mut both: Value = serde_json::from_str(&run_in(&h, &h, &["hook-config"])).unwrap();
    both["enabledPlugins"] = json!({"dotonce@dotonce": true});
    write_json(&user, &both);
    let before = std::fs::read_to_string(&user).unwrap();
    let out = run_in(&h, &h, &["doctor"]);
    let file = user.to_string_lossy();
    for e in EVENTS {
        assert!(
            out.lines().any(|l| l.starts_with(&format!("twice    {e} "))
                && l.ends_with(&format!("from plugin dotonce@dotonce and {file}"))),
            "{e}: {out}"
        );
    }
    let warning = out
        .lines()
        .find(|l| l.starts_with("warning  8 hook(s) run twice"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(warning.contains(&*file), "{warning}");
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        before,
        "doctor never writes"
    );

    // A narrower matcher in settings still runs the hook a second time.
    let narrow = json!({
        "enabledPlugins": {"dotonce@dotonce": true},
        "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "/opt/acme/dotonce hook pre-tool"}]}]}
    });
    write_json(&user, &narrow);
    let out = run_in(&h, &h, &["doctor"]);
    assert!(out.contains("twice    PreToolUse"), "{out}");
    assert!(out.contains("warning  1 hook(s) run twice"), "{out}");

    // The project's local settings turn the plugin off there.
    write_json(&user, &both);
    let proj = h.join("proj");
    write_json(
        &proj.join(".claude/settings.local.json"),
        &json!({"enabledPlugins": {"dotonce@dotonce": false}}),
    );
    let out = run_in(&h, &proj, &["doctor"]);
    assert!(!out.contains("twice"), "{out}");
    assert!(
        out.contains(&format!("ok       Stop (dotonce hook stop) in {file}")),
        "{out}"
    );
    assert!(out.contains("installed but off here"), "{out}");
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_names_the_plugin_when_its_hooks_fall_short() {
    let h = home("doctorold");
    let old = plugin_without_stop(&h);
    let proj = h.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // The user-wide install is an older release; this project has its own.
    install(
        &h,
        json!([
            {"scope": "user", "installPath": old, "version": "0.0.0"},
            {"scope": "project", "projectPath": proj, "installPath": plugin_dir(), "version": "0.0.1"}
        ]),
    );
    write_json(
        &h.join(".claude/settings.json"),
        &json!({"enabledPlugins": {"dotonce@dotonce": true}}),
    );
    let out = run_in(&h, &h, &["doctor"]);
    assert!(out.contains("missing  Stop"), "{out}");
    assert!(
        out.contains("2 hook(s) missing from plugin dotonce@dotonce. Update it with claude plugin update dotonce@dotonce"),
        "{out}"
    );
    let out = run_in(&h, &proj, &["doctor"]);
    assert!(
        !out.contains("missing"),
        "this folder's install counts: {out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}

#[test]
fn doctor_without_hooks_points_at_the_plugin() {
    let h = home("doctornone");
    let out = run_in(&h, &h, &["doctor"]);
    assert!(
        out.contains("claude plugin install dotonce@dotonce")
            && out.contains("dotonce hook-config"),
        "{out}"
    );
    std::fs::remove_dir_all(&h).unwrap();
}
