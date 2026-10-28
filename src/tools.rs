//! PreToolUse: rule sets' tool checks, and the always-on guard that keeps
//! Claude from editing rule files, approvals.json or last-live.json, and
//! from running the `dotonce rules` commands that change approvals.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::catches::{self, Catch, Outcome};
use crate::checks::{Failure, failing_line};
use crate::config::{home_dir, lexical_normalize, resolve};
use crate::hook::{prompt_id, session_id, str_field};
use crate::rules::{Level, On, RuleSet};
use crate::session;

pub const GUARD_REASON: &str = "dotonce: rule files and approvals.json only change from the user's own terminal. Ask the user to make this change and approve it with dotonce rules approve.";

const FILE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];
/// The files in the dotonce home that only the user changes. last-live.json
/// feeds the session-start notice's level-change lines.
const GUARDED_FILES: [&str; 2] = ["approvals.json", "last-live.json"];
/// The `dotonce rules` subcommands that change what checks run.
const RULE_CHANGES: [&str; 4] = ["approve", "level", "off", "pause"];
/// Words that run the command after them.
const RUNS_NEXT: [&str; 13] = [
    "!", "{", "if", "then", "else", "elif", "do", "while", "until", "exec", "command", "nohup",
    "time",
];
/// env options that take the next word as their value.
const ENV_VALUE_OPTS: [&str; 7] = [
    "-u",
    "--unset",
    "-C",
    "--chdir",
    "-P",
    "-S",
    "--split-string",
];

pub fn tool_text(tool: &str, input: &Value) -> Option<(Option<String>, String)> {
    let s = |k: &str| str_field(input, k).to_string();
    match tool {
        "Bash" => Some((None, s("command"))),
        "Write" => Some((Some(s("file_path")), s("content"))),
        "Edit" => Some((Some(s("file_path")), s("new_string"))),
        "MultiEdit" => {
            let parts: Vec<&str> = input
                .get("edits")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|e| str_field(e, "new_string")).collect())
                .unwrap_or_default();
            Some((Some(s("file_path")), parts.join("\n")))
        }
        "NotebookEdit" => Some((Some(s("notebook_path")), s("new_source"))),
        _ => None,
    }
}

/// The set's tool checks for this tool, on the path (when the tool has one)
/// and the body.
pub fn run_tool(set: &RuleSet, tool: &str, path: Option<&str>, body: &str) -> Vec<Failure> {
    let text = match path {
        Some(p) => format!("{p}\n{body}"),
        None => body.to_string(),
    };
    set.checks
        .iter()
        .filter(|c| c.on == On::Tool && c.tool.as_deref() == Some(tool))
        .filter(|c| match (&c.paths, path) {
            (None, _) => true,
            (Some(g), Some(p)) => g.is_match(p),
            (Some(_), None) => false,
        })
        .filter_map(|c| {
            failing_line(c, &text).map(|line| Failure {
                set: set.id.clone(),
                check: c.id.clone(),
                line,
                why: c.why.clone(),
            })
        })
        .collect()
}

/// The real path of the longest existing part of `p`, with the rest as
/// written, so a new file under a symlinked folder still matches the folder.
fn resolve_deep(p: &Path) -> PathBuf {
    let p = lexical_normalize(p);
    let mut base = p.as_path();
    let mut rest = Vec::new();
    loop {
        if base.exists() {
            let mut out = resolve(base);
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => return p.clone(),
        }
    }
}

fn forms(p: &Path) -> [PathBuf; 2] {
    [lexical_normalize(p), resolve_deep(p)]
}

fn guarded_path(p: &Path, home: &Path) -> bool {
    let rules = forms(&home.join("rules"));
    let files: Vec<PathBuf> = GUARDED_FILES
        .iter()
        .flat_map(|n| forms(&home.join(n)))
        .collect();
    forms(p)
        .iter()
        .any(|f| rules.iter().any(|r| f.starts_with(r)) || files.contains(f))
}

/// True when a shell command names the rules folder or a guarded file.
pub fn mentions(cmd: &str, home: &Path, cwd: &Path, user_home: Option<&Path>) -> bool {
    let mut bases: Vec<String> = forms(home)
        .iter()
        .map(|h| h.to_string_lossy().into_owned())
        .collect();
    let rest = user_home.and_then(|u| {
        lexical_normalize(home)
            .strip_prefix(u)
            .ok()
            .map(|r| r.to_string_lossy().into_owned())
    });
    if let Some(rest) = rest {
        for pre in ["~", "$HOME", "${HOME}"] {
            bases.push(format!("{pre}/{rest}"));
        }
    }
    let named = ["rules", GUARDED_FILES[0], GUARDED_FILES[1]];
    let mut needles: Vec<String> = bases
        .iter()
        .flat_map(|b| named.iter().map(move |n| format!("{b}/{n}")))
        .collect();
    needles.extend(named.iter().map(|n| format!(".dotonce/{n}")));
    if needles.iter().any(|n| cmd.contains(n.as_str())) {
        return true;
    }
    let names_target = ["rules", "approvals", "last-live"]
        .iter()
        .any(|w| cmd.contains(w));
    let home_forms = forms(home);
    let at_home = bases.iter().any(|b| cmd.contains(b.as_str()))
        || forms(cwd)
            .iter()
            .any(|c| home_forms.iter().any(|h| c.starts_with(h)));
    names_target && at_home
}

/// True when the call would change a rule file or approvals.json.
pub fn guard(tool: &str, input: &Value, cwd: &Path, home: &Path, user_home: Option<&Path>) -> bool {
    if FILE_TOOLS.contains(&tool) {
        let key = if tool == "NotebookEdit" {
            "notebook_path"
        } else {
            "file_path"
        };
        let p = str_field(input, key);
        return !p.is_empty() && guarded_path(&cwd.join(p), home);
    }
    let cmd = str_field(input, "command");
    tool == "Bash" && (mentions(cmd, home, cwd, user_home) || runs_rule_change(cmd))
}

#[derive(PartialEq)]
enum Ctx {
    Paren,
    Tick,
    Quote,
}

/// Splits a shell command into its simple commands, each a list of words
/// with quotes taken off. A quoted string stays one word, so text that only
/// quotes a command never reads as running it; `$(...)` and backticks
/// inside double quotes still run, so they start a command of their own.
fn simple_commands(cmd: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut stack: Vec<Ctx> = Vec::new();
    let mut chars = cmd.chars().peekable();
    let end_word = |word: &mut Option<String>, cur: &mut Vec<String>| {
        if let Some(w) = word.take() {
            cur.push(w);
        }
    };
    let end_cmd = |word: &mut Option<String>, cur: &mut Vec<String>, out: &mut Vec<Vec<String>>| {
        end_word(word, cur);
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    while let Some(c) = chars.next() {
        if stack.last() == Some(&Ctx::Quote) {
            match c {
                '"' => {
                    stack.pop();
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        word.get_or_insert_default().push(n);
                    }
                }
                '`' => {
                    end_cmd(&mut word, &mut cur, &mut out);
                    stack.push(Ctx::Tick);
                }
                '$' if chars.peek() == Some(&'(') => {
                    chars.next();
                    end_cmd(&mut word, &mut cur, &mut out);
                    stack.push(Ctx::Paren);
                }
                _ => word.get_or_insert_default().push(c),
            }
            continue;
        }
        match c {
            '\n' | ';' | '|' | '&' | '(' => end_cmd(&mut word, &mut cur, &mut out),
            c if c.is_whitespace() => end_word(&mut word, &mut cur),
            ')' => {
                end_cmd(&mut word, &mut cur, &mut out);
                if stack.last() == Some(&Ctx::Paren) {
                    stack.pop();
                }
            }
            '`' => {
                end_cmd(&mut word, &mut cur, &mut out);
                if stack.last() == Some(&Ctx::Tick) {
                    stack.pop();
                } else {
                    stack.push(Ctx::Tick);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                end_cmd(&mut word, &mut cur, &mut out);
                stack.push(Ctx::Paren);
            }
            '\'' => {
                let w = word.get_or_insert_default();
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    w.push(n);
                }
            }
            '"' => {
                word.get_or_insert_default();
                stack.push(Ctx::Quote);
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(n) => word.get_or_insert_default().push(n),
            },
            '#' if word.is_none() => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                end_cmd(&mut word, &mut cur, &mut out);
            }
            _ => word.get_or_insert_default().push(c),
        }
    }
    end_cmd(&mut word, &mut cur, &mut out);
    out
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Where the command a simple command runs starts, past variable
/// assignments, `env` and its options, and words such as `exec`.
fn command_start(words: &[String]) -> usize {
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let w = w.as_str();
        i += 1;
        if w == "env" {
            while let Some(o) = words.get(i).map(String::as_str) {
                if o == "--" {
                    i += 1;
                    break;
                }
                if ENV_VALUE_OPTS.contains(&o) {
                    i += 2;
                } else if o.starts_with('-') || is_assignment(o) {
                    i += 1;
                } else {
                    break;
                }
            }
        } else if RUNS_NEXT.contains(&w) {
            while words.get(i).is_some_and(|o| o.starts_with('-')) {
                i += if words[i] == "-a" { 2 } else { 1 };
            }
        } else if !is_assignment(w) {
            return i - 1;
        }
    }
    i
}

/// True when the command runs `dotonce rules` approve, level, off or pause.
/// Approvals already refuse under Claude Code's variables; this stops an
/// agent that unsets them.
fn runs_rule_change(cmd: &str) -> bool {
    simple_commands(cmd).iter().any(|words| {
        let changes_at = |i: usize| {
            words.get(i + 1).is_some_and(|w| w == "rules")
                && words
                    .get(i + 2)
                    .is_some_and(|w| RULE_CHANGES.contains(&w.as_str()))
        };
        // A path to the binary runs it wherever it stands, as under `script`.
        if words
            .iter()
            .enumerate()
            .any(|(i, w)| w.ends_with("/dotonce") && changes_at(i))
        {
            return true;
        }
        let i = command_start(words);
        words.get(i).is_some_and(|w| w == "dotonce") && changes_at(i)
    })
}

fn deny(reason: &str) -> String {
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}})
        .to_string()
}

pub fn pre_tool(v: &Value, cwd: &Path, now: i64) -> Result<Option<String>> {
    let tool = str_field(v, "tool_name");
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let home = home_dir()?;
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    if guard(tool, &input, cwd, &home, user_home.as_deref()) {
        return Ok(Some(deny(GUARD_REASON)));
    }
    let Some((path, body)) = tool_text(tool, &input) else {
        return Ok(None);
    };
    let sid = session_id(v)?;
    let dir = session::dir()?;
    let before = session::load(&dir, sid)?;
    let session::Now {
        live,
        mut state,
        turn,
    } = session::checks_now(&before, cwd, prompt_id(v))?;
    if live.is_empty() {
        return Ok(None);
    }
    let abs = path
        .filter(|p| !p.is_empty())
        .map(|p| cwd.join(p).to_string_lossy().into_owned());
    let time = catches::timestamp(now);
    let mut reasons = Vec::new();
    for l in session::checking(&state, &live) {
        for f in run_tool(&l.set, tool, abs.as_deref(), &body) {
            let outcome = if l.level == Level::Fix {
                reasons.push(format!("{}: {}", f.check, f.why));
                Outcome::Denied
            } else if state.first_log(&turn, &l.set.id, &f.check) {
                Outcome::Caught
            } else {
                continue;
            };
            let c = Catch {
                time: time.clone(),
                session: sid.to_string(),
                set: l.set.id.clone(),
                check: f.check,
                level: l.level,
                outcome,
            };
            catches::record(&c, l.set.private);
        }
    }
    if state != before {
        session::save(&dir, sid, &state)?;
    }
    Ok((!reasons.is_empty()).then(|| deny(&reasons.join("; "))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse;

    #[test]
    fn tool_text_reads_each_tools_fields() {
        let s = |v: &str| v.to_string();
        assert_eq!(
            tool_text("Bash", &json!({"command": "git push"})),
            Some((None, s("git push")))
        );
        assert_eq!(
            tool_text("Write", &json!({"file_path": "/a.rs", "content": "x"})),
            Some((Some(s("/a.rs")), s("x")))
        );
        assert_eq!(
            tool_text(
                "Edit",
                &json!({"file_path": "/a.rs", "old_string": "a", "new_string": "b"})
            ),
            Some((Some(s("/a.rs")), s("b")))
        );
        assert_eq!(
            tool_text(
                "MultiEdit",
                &json!({"file_path": "/a.rs", "edits": [{"old_string": "a", "new_string": "b"}, {"new_string": "c"}]})
            ),
            Some((Some(s("/a.rs")), s("b\nc")))
        );
        assert_eq!(
            tool_text(
                "NotebookEdit",
                &json!({"notebook_path": "/n.ipynb", "new_source": "print(1)"})
            ),
            Some((Some(s("/n.ipynb")), s("print(1)")))
        );
        assert_eq!(tool_text("Read", &json!({"file_path": "/a.rs"})), None);
    }

    #[test]
    fn tool_checks_match_the_tool_and_the_path() {
        let set = parse(
            "id = \"t\"\n[[check]]\nid = \"todo\"\non = \"tool\"\ntool = \"Write\"\npath = [\"*.rs\"]\nforbid = [\"TODO\"]\nwhy = \"w\"\n[[check]]\nid = \"reply\"\nforbid = [\"TODO\"]\nwhy = \"w\"\n",
            Path::new("/r/t.toml"),
        )
        .unwrap();
        assert_eq!(
            run_tool(&set, "Write", Some("/code/a.rs"), "// TODO").len(),
            1
        );
        assert!(run_tool(&set, "Write", Some("/code/a.md"), "// TODO").is_empty());
        assert!(run_tool(&set, "Edit", Some("/code/a.rs"), "// TODO").is_empty());
        assert!(run_tool(&set, "Write", None, "// TODO").is_empty());
        let f = run_tool(&set, "Write", Some("/code/TODO.rs"), "fine");
        assert_eq!(f[0].line, "/code/TODO.rs");
        assert_eq!(f[0].check, "todo");
    }

    #[test]
    fn mentions_finds_guarded_paths_and_passes_quoted_commands() {
        let home = Path::new("/fake/sam/.dotonce");
        let user = Some(Path::new("/fake/sam"));
        let cwd = Path::new("/fake/sam/code");
        for cmd in [
            "cat > /fake/sam/.dotonce/rules/a.toml",
            "echo x > ~/.dotonce/approvals.json",
            "rm $HOME/.dotonce/rules/a.toml",
            "vim ${HOME}/.dotonce/rules/a.toml",
            "cd ~/.dotonce && rm rules/a.toml",
            "cp a.toml .dotonce/rules/",
        ] {
            assert!(mentions(cmd, home, cwd, user), "{cmd}");
        }
        for cmd in [
            "grep -rn \"dotonce rules approve\" src",
            "git commit -m \"Add the dotonce rules approve command\"",
            "ls ~/.dotonce/notes",
            "cargo test rules",
        ] {
            assert!(!mentions(cmd, home, cwd, user), "{cmd}");
        }
        assert!(mentions(
            "rm rules/a.toml",
            home,
            Path::new("/fake/sam/.dotonce"),
            user
        ));
        assert!(!mentions("rm rules/a.toml", home, cwd, None));
    }

    #[test]
    fn last_live_is_guarded_like_approvals() {
        let home = Path::new("/fake/sam/.dotonce");
        let user = Some(Path::new("/fake/sam"));
        let cwd = Path::new("/fake/sam/code");
        for cmd in [
            "echo '{}' > /fake/sam/.dotonce/last-live.json",
            "echo '{}' > ~/.dotonce/last-live.json",
            "rm $HOME/.dotonce/last-live.json",
            "vim ${HOME}/.dotonce/last-live.json",
            "cp x.json .dotonce/last-live.json",
            "cd ~/.dotonce && rm last-live.json",
        ] {
            assert!(mentions(cmd, home, cwd, user), "{cmd}");
            assert!(
                guard("Bash", &json!({"command": cmd}), cwd, home, user),
                "{cmd}"
            );
        }
        assert!(mentions("rm last-live.json", home, home, user));
        assert!(!mentions("rm last-live.json", home, cwd, user));
        for tool in FILE_TOOLS {
            let key = if tool == "NotebookEdit" {
                "notebook_path"
            } else {
                "file_path"
            };
            let input = json!({key: "/fake/sam/.dotonce/last-live.json"});
            assert!(guard(tool, &input, cwd, home, user), "{tool}");
        }
        let rel = json!({"file_path": "../.dotonce/last-live.json"});
        assert!(guard("Write", &rel, cwd, home, user));
        let other = json!({"file_path": "/fake/sam/.dotonce/notes/last-live.json"});
        assert!(!guard("Write", &other, cwd, home, user));
    }

    #[test]
    fn guarded_files_match_by_real_path() {
        let tmp = std::env::temp_dir().join(format!("dotonce-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let real = tmp.join("real");
        std::fs::create_dir_all(real.join("dot")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.join("link")).unwrap();
        let home = real.join("dot");
        let cwd = Path::new("/elsewhere");
        for name in ["last-live.json", "approvals.json", "rules/new.toml"] {
            let input = json!({"file_path": tmp.join("link/dot").join(name)});
            assert!(guard("Write", &input, cwd, &home, None), "{name}");
        }
        let input = json!({"file_path": tmp.join("link/dot/notes/a.md")});
        assert!(!guard("Write", &input, cwd, &home, None));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn rule_changing_commands_are_denied_where_they_run() {
        for cmd in [
            "dotonce rules approve x",
            "env -u CLAUDECODE dotonce rules level x fix",
            "script -q /dev/null ~/.cargo/bin/dotonce rules off x",
            "cd /tmp && dotonce rules pause x",
            "true; dotonce rules approve x",
            "false || dotonce rules off x",
            "echo x | dotonce rules approve x",
            "echo $(dotonce rules approve x)",
            "echo `dotonce rules approve x`",
            "echo \"$(dotonce rules level x fix)\"",
            "exec dotonce rules approve x",
            "CLAUDECODE= dotonce rules approve x",
            "env -i HOME=/fake/sam dotonce rules approve x",
            "/usr/local/bin/dotonce rules level x fix",
            "\"dotonce\" rules approve x",
            "(dotonce rules off x)",
            "ls\ndotonce rules approve x",
        ] {
            assert!(runs_rule_change(cmd), "{cmd}");
            let input = json!({"command": cmd});
            let (home, cwd) = (Path::new("/fake/sam/.dotonce"), Path::new("/fake/sam/code"));
            assert!(guard("Bash", &input, cwd, home, None), "{cmd}");
        }
        for cmd in [
            "grep -rn \"dotonce rules approve\" src",
            "git commit -m \"Add the dotonce rules approve command\"",
            "git commit -m 'dotonce rules off x'",
            "echo dotonce rules approve x",
            "dotonce rules list",
            "dotonce rules test x reply.md",
            "dotonce search rules",
            "cargo test rules",
            "echo hi # ; dotonce rules approve x",
        ] {
            assert!(!runs_rule_change(cmd), "{cmd}");
        }
    }
}
