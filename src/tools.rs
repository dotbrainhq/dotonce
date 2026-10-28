//! PreToolUse: rule sets' tool checks, and the always-on guard that keeps
//! Claude from editing rule files, approvals.json or last-live.json, and
//! from running the `dotonce rules` commands that change approvals.

use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;

use anyhow::Result;
use serde_json::{Value, json};

use crate::catches::{self, Catch, Outcome};
use crate::checks::{Failure, failing_line};
use crate::config::{home_dir, lexical_normalize, resolve};
use crate::hook::{prompt_id, session_id, str_field};
use crate::rules::{Level, On, RuleSet};
use crate::session;

pub const GUARD_REASON: &str = "dotonce: rule files, approvals and rule commands only change from the user's own terminal. Ask the user to run it there.";

const FILE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];
/// The files in the dotonce home that only the user changes. last-live.json
/// feeds the session-start notice's level-change lines.
const GUARDED_FILES: [&str; 2] = ["approvals.json", "last-live.json"];
/// The `dotonce rules` subcommands that change what checks run.
const RULE_CHANGES: [&str; 4] = ["approve", "level", "off", "pause"];
/// Shells whose `-c` argument, or stdin, runs as a command.
const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "dash", "ksh", "fish"];
/// Shell options that take the next word as their value.
const SHELL_VALUE_OPTS: [&str; 6] = ["-o", "+o", "-O", "+O", "--rcfile", "--init-file"];
/// How many quoted commands deep (`sh -c`, `eval`, `env -S`) the guard
/// reads. One quoted deeper is denied when it names dotonce and rules.
const MAX_DEPTH: usize = 3;
/// env options that take the next word as their value.
const ENV_VALUE_OPTS: [&str; 5] = ["-u", "--unset", "-C", "--chdir", "-P"];

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
    /// A here-doc body with an unquoted delimiter: only `$(...)` and
    /// backticks in it run.
    Body,
}

/// One simple command: its words with quotes taken off, and what it reads
/// on stdin from here-docs and here-strings.
#[derive(Default)]
struct Simple {
    words: Vec<String>,
    stdin: Vec<String>,
    /// The index of the first command of the pipeline that feeds this one.
    piped_from: Option<usize>,
}

/// A here-doc waiting for its body: the command's index, the delimiter,
/// whether the delimiter was quoted, and whether `<<-` strips tabs.
type Doc = (usize, String, bool, bool);

#[derive(Default)]
struct Lexer {
    out: Vec<Simple>,
    cur: Simple,
    word: Option<String>,
    here_string: bool,
    after_pipe: bool,
    pipe_start: usize,
    docs: Vec<Doc>,
}

impl Lexer {
    fn push(&mut self, c: char) {
        self.word.get_or_insert_default().push(c);
    }

    fn end_word(&mut self) {
        if let Some(w) = self.word.take() {
            if std::mem::take(&mut self.here_string) {
                self.cur.stdin.push(w);
            } else {
                self.cur.words.push(w);
            }
        }
    }

    fn end_cmd(&mut self) {
        self.end_word();
        if !self.cur.words.is_empty() || !self.cur.stdin.is_empty() {
            if std::mem::take(&mut self.after_pipe) {
                self.cur.piped_from = Some(self.pipe_start);
            }
            self.out.push(std::mem::take(&mut self.cur));
        }
    }

    /// Ends the command and its pipeline: `;`, `&`, `&&`, `||`, `(` or a
    /// newline.
    fn end_list(&mut self) {
        self.end_cmd();
        self.after_pipe = false;
        self.pipe_start = self.out.len();
    }

    /// Reads the bodies of the here-docs opened on the line that just ended.
    fn read_docs(&mut self, chars: &mut Peekable<Chars>) {
        for (idx, delim, quoted, strip) in std::mem::take(&mut self.docs) {
            let mut body = String::new();
            while chars.peek().is_some() {
                let line: String = chars.by_ref().take_while(|c| *c != '\n').collect();
                let line = if strip {
                    line.trim_start_matches('\t').to_string()
                } else {
                    line
                };
                if line == delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if !quoted {
                let offset = self.out.len();
                self.out
                    .extend(lex(&body, vec![Ctx::Body]).into_iter().map(|mut s| {
                        s.piped_from = s.piped_from.map(|p| p + offset);
                        s
                    }));
            }
            if let Some(c) = self.out.get_mut(idx) {
                c.stdin.push(body);
            }
        }
        self.pipe_start = self.out.len();
    }
}

/// The word after `<<` or `<<-`, with quotes taken off, and whether any
/// part of it was quoted (then the body is plain text).
fn read_delimiter(chars: &mut Peekable<Chars>) -> Option<(String, bool)> {
    while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
    let mut d = String::new();
    let mut quoted = false;
    while let Some(c) = chars.next_if(|c| !c.is_whitespace() && !";|&()<>".contains(*c)) {
        match c {
            '\'' | '"' => {
                quoted = true;
                for n in chars.by_ref() {
                    if n == c {
                        break;
                    }
                    d.push(n);
                }
            }
            '\\' => {
                quoted = true;
                d.extend(chars.next());
            }
            _ => d.push(c),
        }
    }
    (!d.is_empty()).then_some((d, quoted))
}

/// Splits a shell command into its simple commands. A quoted string stays
/// one word, so text that only quotes a command never reads as running it;
/// `$(...)` and backticks inside double quotes still run, so they start a
/// command of their own.
fn simple_commands(cmd: &str) -> Vec<Simple> {
    lex(cmd, Vec::new())
}

fn lex(cmd: &str, mut stack: Vec<Ctx>) -> Vec<Simple> {
    let mut l = Lexer::default();
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(stack.last(), Some(Ctx::Quote | Ctx::Body)) {
            let body = stack.last() == Some(&Ctx::Body);
            match c {
                '"' if !body => {
                    stack.pop();
                }
                '\\' => {
                    if let Some(n) = chars.next().filter(|_| !body) {
                        l.push(n);
                    }
                }
                '`' => {
                    l.end_cmd();
                    stack.push(Ctx::Tick);
                }
                '$' if chars.peek() == Some(&'(') => {
                    chars.next();
                    l.end_cmd();
                    stack.push(Ctx::Paren);
                }
                _ if !body => l.push(c),
                _ => {}
            }
            continue;
        }
        match c {
            '\n' => {
                l.end_list();
                l.read_docs(&mut chars);
            }
            ';' | '(' => l.end_list(),
            // `2>&1` and `&>file` are redirections, not separators.
            '&' if chars.peek() == Some(&'>')
                || l.word.as_deref().is_some_and(|w| w.ends_with(['>', '<'])) =>
            {
                l.push(c)
            }
            '&' => {
                chars.next_if_eq(&'&');
                l.end_list();
            }
            '|' if chars.next_if_eq(&'|').is_some() => l.end_list(),
            '|' => {
                chars.next_if_eq(&'&');
                l.end_cmd();
                l.after_pipe = true;
            }
            '<' if chars.next_if_eq(&'<').is_some() => {
                l.end_word();
                if chars.next_if_eq(&'<').is_some() {
                    l.here_string = true;
                } else {
                    let strip = chars.next_if_eq(&'-').is_some();
                    if let Some((delim, quoted)) = read_delimiter(&mut chars) {
                        l.docs.push((l.out.len(), delim, quoted, strip));
                    }
                }
            }
            c if c.is_whitespace() => l.end_word(),
            ')' => {
                l.end_cmd();
                if stack.last() == Some(&Ctx::Paren) {
                    stack.pop();
                }
            }
            '`' => {
                l.end_cmd();
                if stack.last() == Some(&Ctx::Tick) {
                    stack.pop();
                } else {
                    stack.push(Ctx::Tick);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                l.end_cmd();
                stack.push(Ctx::Paren);
            }
            '\'' => {
                let mut w = l.word.take().unwrap_or_default();
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    w.push(n);
                }
                l.word = Some(w);
            }
            '"' => {
                l.word.get_or_insert_default();
                stack.push(Ctx::Quote);
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(n) => l.push(n),
            },
            '#' if l.word.is_none() => while chars.next_if(|n| *n != '\n').is_some() {},
            _ => l.push(c),
        }
    }
    l.end_cmd();
    l.out
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn is_dotonce(w: &str) -> bool {
    w == "dotonce" || w.ends_with("/dotonce")
}

/// True when a `dotonce` word is followed, anywhere later in the command, by
/// `rules` and then a rule change. Wherever `dotonce` stands it can be run:
/// by `sudo`, `xargs`, `script`, `nice`, `timeout` and the like.
fn runs_change(words: &[String]) -> bool {
    let mut rest = words.iter().skip_while(|w| !is_dotonce(w));
    rest.next().is_some()
        && rest.by_ref().any(|w| w == "rules")
        && rest.any(|w| RULE_CHANGES.contains(&w.as_str()))
}

/// What a shell's arguments run: a `-c` command, stdin, or a script file.
enum Shell {
    Command(String),
    Stdin,
    File,
}

fn shell_runs(args: &[String]) -> Shell {
    let (mut c, mut s) = (false, false);
    let mut i = 0;
    while let Some(a) = args.get(i).map(String::as_str) {
        i += 1;
        if a == "--" {
            break;
        } else if let Some(cmd) = a.strip_prefix("--command=") {
            return Shell::Command(cmd.to_string());
        } else if a == "--command" {
            c = true;
        } else if SHELL_VALUE_OPTS.contains(&a) {
            i += 1;
        } else if a.starts_with("--") {
        } else if a.len() > 1 && (a.starts_with('-') || a.starts_with('+')) {
            c |= a.starts_with('-') && a.contains('c');
            s |= a.starts_with('-') && a.contains('s');
        } else {
            i -= 1;
            break;
        }
    }
    match args.get(i) {
        Some(cmd) if c => Shell::Command(cmd.clone()),
        None if !c => Shell::Stdin,
        _ if s => Shell::Stdin,
        _ => Shell::File,
    }
}

/// The command `env -S` or `--split-string` runs, if any.
fn env_split(args: &[String]) -> Option<String> {
    let mut i = 0;
    while let Some(a) = args.get(i).map(String::as_str) {
        i += 1;
        if a == "-S" || a == "--split-string" {
            return args.get(i).cloned();
        } else if let Some(cmd) = a
            .strip_prefix("--split-string=")
            .or_else(|| a.strip_prefix("-S"))
        {
            return Some(cmd.to_string());
        } else if ENV_VALUE_OPTS.contains(&a) {
            i += 1;
        } else if a == "--" || !(a.starts_with('-') || is_assignment(a)) {
            return None;
        }
    }
    None
}

/// The commands a simple command runs from a quoted string (`sh -c`,
/// `eval`, `env -S`), and whether a shell in it reads stdin.
fn quoted_commands(words: &[String]) -> (Vec<String>, bool) {
    let mut cmds = Vec::new();
    let mut stdin = false;
    for (i, w) in words.iter().enumerate() {
        let rest = &words[i + 1..];
        match w.rsplit('/').next().unwrap_or(w) {
            "eval" => cmds.push(rest.join(" ")),
            "env" => cmds.extend(env_split(rest)),
            b if SHELLS.contains(&b) => match shell_runs(rest) {
                Shell::Command(c) => cmds.push(c),
                Shell::Stdin => stdin = true,
                Shell::File => {}
            },
            _ => {}
        }
    }
    (cmds, stdin)
}

/// True when text names `dotonce`, `rules` and a rule change, in any order.
fn names_change(text: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .collect();
    words.contains(&"dotonce")
        && words.contains(&"rules")
        && RULE_CHANGES.iter().any(|v| words.contains(v))
}

/// True when the command runs `dotonce rules` approve, level, off or pause.
/// Approvals already refuse under Claude Code's variables; this stops an
/// agent that unsets them. It reads command text only: a script file the
/// agent writes and then runs gets past it, and the approval's terminal
/// check covers that.
fn runs_rule_change(cmd: &str) -> bool {
    changes_at(cmd, 0)
}

fn changes_at(cmd: &str, depth: usize) -> bool {
    let cmds = simple_commands(cmd);
    cmds.iter().enumerate().any(|(k, c)| {
        if runs_change(&c.words) {
            return true;
        }
        let (quoted, stdin) = quoted_commands(&c.words);
        let nested = |q: &String| {
            if depth < MAX_DEPTH {
                changes_at(q, depth + 1)
            } else {
                q.contains("dotonce") && q.contains("rules")
            }
        };
        if quoted.iter().any(nested) {
            return true;
        }
        let feeds = c
            .piped_from
            .and_then(|p| cmds.get(p..k))
            .unwrap_or_default();
        let text: Vec<&str> = feeds
            .iter()
            .chain([c])
            .flat_map(|f| f.words.iter().chain(&f.stdin))
            .map(String::as_str)
            .collect();
        stdin && names_change(&text.join(" "))
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
            "echo dotonce rules approve x",
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
            "dotonce rules list",
            "dotonce rules test x reply.md",
            "dotonce search rules",
            "cargo test rules",
            "echo hi # ; dotonce rules approve x",
        ] {
            assert!(!runs_rule_change(cmd), "{cmd}");
        }
    }

    #[test]
    fn rule_changes_are_denied_as_arguments_and_inside_shells() {
        for cmd in [
            "sudo dotonce rules approve x",
            "echo x | xargs dotonce rules approve",
            "script -q /dev/null dotonce rules off x",
            "nice -n 5 dotonce rules level x fix",
            "timeout 5 dotonce rules pause x",
            "watch -n 1 dotonce rules approve x",
            "bash -c 'dotonce rules approve x'",
            "/bin/sh -c \"dotonce rules level x fix\"",
            "zsh -c 'dotonce rules off x'",
            "dash -c 'dotonce rules pause x'",
            "ksh -c 'dotonce rules approve x'",
            "fish -c 'dotonce rules approve x'",
            "/usr/local/bin/fish --command 'dotonce rules approve x'",
            "bash -lc 'dotonce rules approve x'",
            "bash -o pipefail -c 'dotonce rules approve x'",
            "sudo -u sam bash -c 'cd /tmp; dotonce rules approve x'",
            "echo x | xargs sh -c 'dotonce rules off x'",
            "bash -c \"sh -c 'dotonce rules approve x'\"",
            "eval \"dotonce rules off x\"",
            "eval dotonce rules off x",
            "env -S 'dotonce rules pause x'",
            "env --split-string='dotonce rules pause x'",
            "env -u CLAUDECODE -S 'dotonce rules pause x'",
        ] {
            assert!(runs_rule_change(cmd), "{cmd}");
        }
        for cmd in [
            "bash -c 'dotonce rules list'",
            "sh -c 'echo \"dotonce rules approve x\"'",
            "bash -c 'grep -rn \"dotonce rules approve\" src'",
            "eval \"echo 'dotonce rules approve x'\"",
            "env -S 'dotonce rules check'",
            "echo \"run dotonce rules approve to approve\"",
            "sudo dotonce rules list",
        ] {
            assert!(!runs_rule_change(cmd), "{cmd}");
        }
    }

    /// `sh -c` with `cmd` quoted inside it, `n` times over.
    fn nest(cmd: &str, n: usize) -> String {
        (0..n).fold(cmd.to_string(), |c, _| {
            format!("sh -c '{}'", c.replace('\'', "'\\''"))
        })
    }

    #[test]
    fn nested_shells_are_read_three_deep_and_denied_past_that() {
        assert!(runs_rule_change(&nest("dotonce rules approve x", 3)));
        assert!(!runs_rule_change(&nest("dotonce rules list", 3)));
        assert!(runs_rule_change(&nest("dotonce rules list", 4)));
        assert!(!runs_rule_change(&nest("ls src", 4)));
    }

    #[test]
    fn rule_changes_piped_or_fed_to_a_shell_are_denied() {
        for cmd in [
            "echo 'dotonce rules approve x' | sh",
            "printf '%s\\n' 'dotonce rules off x' | sudo bash",
            "echo 'dotonce rules approve x' 2>&1 | bash -s",
            "bash <<EOF\ndotonce rules approve x\nEOF",
            "bash <<'EOF'\ndotonce rules level x fix\nEOF",
            "sh <<-EOF\n\tdotonce rules off x\n\tEOF",
            "cat <<EOF | sh\ndotonce rules pause x\nEOF",
            "bash <<< 'dotonce rules approve x'",
            "cat <<EOF\n$(dotonce rules approve x)\nEOF",
        ] {
            assert!(runs_rule_change(cmd), "{cmd}");
        }
        for cmd in [
            "echo 'dotonce rules approve x' | grep approve",
            "echo 'dotonce rules list' | sh",
            "cat <<'EOF' > notes.md\nRun dotonce rules approve x in a terminal.\nEOF",
            "cat <<EOF > notes.md\ndotonce rules approve x\nEOF\nls",
            "cat <<'EOF' > notes.md\n$(dotonce rules approve x)\nEOF",
            "bash <<EOF\nls src\nEOF",
        ] {
            assert!(!runs_rule_change(cmd), "{cmd}");
        }
    }
}
