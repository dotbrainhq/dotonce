//! Bash commands that only read. The guard lets one of these name the rules
//! folder and the guarded files, so Claude can look at them without the
//! user's help.

use crate::tools::simple_commands;

/// Commands that only read files or print text. None of them has an option
/// that writes a file or runs another command, apart from the `find` and
/// `file` options `reads` checks.
const READERS: [&str; 27] = [
    "cat", "ls", "head", "tail", "grep", "egrep", "fgrep", "wc", "stat", "diff", "cmp", "file",
    "find", "du", "jq", "echo", "printf", "pwd", "cd", "basename", "dirname", "readlink",
    "realpath", "test", "[", "true", "false",
];

/// `find` actions that delete, run a command or write a file, as prefixes:
/// `-exec` covers `-execdir`, `-ok` covers `-okdir`, and `-fprint` covers
/// `-fprint0` and `-fprintf`.
const FIND_ACTIONS: [&str; 5] = ["-delete", "-exec", "-ok", "-fprint", "-fls"];

/// True when every command in `cmd` only reads: a reader called by its bare
/// name, with no `NAME=value` before it, and no output redirected to a file.
/// `$(...)`, backticks and here-doc bodies count as commands of their own,
/// and the words after a `$(...)` still belong to the command it sits in.
/// Give it the command as written, not folded, so `file -C` stays `-C`.
pub fn only_reads(cmd: &str) -> bool {
    let cmds = simple_commands(cmd);
    let mut words: Vec<Vec<&str>> = vec![Vec::new(); cmds.len()];
    let mut roots = Vec::with_capacity(cmds.len());
    for (k, c) in cmds.iter().enumerate() {
        let root = c.joins.and_then(|j| roots.get(j).copied()).unwrap_or(k);
        roots.push(root);
        words[root].extend(c.words.iter().map(String::as_str));
    }
    !cmds.is_empty()
        && cmds.iter().all(|c| !c.writes)
        && roots
            .iter()
            .enumerate()
            .all(|(k, root)| k != *root || reads(&words[k]))
}

/// True when the words run a reader with arguments that only read: no `find`
/// action from FIND_ACTIONS and no `file -C`. For those two, an argument the
/// shell expands could turn into such an option, so it doesn't count either.
fn reads(words: &[&str]) -> bool {
    let Some((name, args)) = words.split_first() else {
        return false;
    };
    let any = |f: fn(&str) -> bool| args.iter().any(|a| f(a));
    READERS.contains(name)
        && match *name {
            "find" => !any(expands) && !any(|a| FIND_ACTIONS.iter().any(|p| a.starts_with(p))),
            "file" => {
                !any(expands)
                    && !any(|a| {
                        a.starts_with("--c")
                            || (a.starts_with('-') && !a.starts_with("--") && a.contains('C'))
                    })
            }
            _ => true,
        }
}

/// True when the shell may turn the argument into other words: a `$` or
/// backtick expansion, or a brace list. `$HOME/` and `${HOME}/` at the start
/// become a path, so they don't count.
fn expands(a: &str) -> bool {
    let rest = a
        .strip_prefix("$HOME/")
        .or_else(|| a.strip_prefix("${HOME}/"))
        .unwrap_or(a);
    rest.contains(['$', '`', '{'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_with_reading_arguments_only_read() {
        for cmd in [
            "cat a.toml",
            "ls -la rules | head -5 && wc -l rules/*.toml",
            "cd rules; grep -n id *.toml 2>/dev/null || echo none",
            "find $HOME/rules -name '*.toml' -type f",
            "file -b a.toml",
            "cat $(ls rules/*.toml | head -1)",
            "head -3 `ls rules/*.toml` x.toml",
            "[ -f a.toml ] && cat a.toml >&2",
        ] {
            assert!(only_reads(cmd), "{cmd}");
        }
        for cmd in [
            "",
            "cat a.toml > b.toml",
            "cat a.toml >| b.toml",
            "cat a.toml >&b.toml",
            "cat a.toml > $(echo b.toml)",
            "cat a.toml && rm a.toml",
            "cat a.toml | sh",
            "cat $(rm a.toml)",
            "X=1 cat a.toml",
            "./cat a.toml",
            "$(echo cat) a.toml",
            "find rules -delete",
            "find rules -execdir rm {} +",
            "find rules -okdir rm {} ;",
            "find rules -fprint0 out",
            "find rules $(echo -delete)",
            "find rules ${X:--delete}",
            "find rules {-delete,}",
            "find $(echo rules) -delete",
            "file -bC -m magic",
            "file --comp -m magic",
            "file $(echo -C) -m magic",
        ] {
            assert!(!only_reads(cmd), "{cmd}");
        }
    }
}
