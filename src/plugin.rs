//! The dotonce plugin for Claude Code: finding its installs for `doctor`,
//! the hooks file of the plugin that started a hook, and the `claude plugin`
//! commands that would turn it off, which the guard denies.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;

/// A dotonce plugin as Claude Code installed it.
pub struct Plugin {
    /// `dotonce@<marketplace>`, its key in `enabledPlugins`.
    pub id: String,
    /// Its hooks.json, or Null when that can't be read.
    pub hooks: Value,
    /// Turned on in the settings doctor read. The last file that names it
    /// wins, as in Claude Code: local over project over user.
    pub on: bool,
}

/// The dotonce plugins in Claude Code's list of installed plugins. Of a
/// plugin's installs, the one for this folder counts first, then the
/// user-wide one.
pub fn installed(settings: &[(&PathBuf, Value)], cwd: &Path) -> Vec<Plugin> {
    let list = config::expand("~/.claude/plugins/installed_plugins.json");
    let Some(v) = fs::read_to_string(list)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return Vec::new();
    };
    let here = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let for_here = |i: &&Value| {
        i.get("projectPath")
            .and_then(Value::as_str)
            .is_some_and(|p| fs::canonicalize(p).is_ok_and(|p| p == here))
    };
    let user = |i: &&Value| i.get("scope").and_then(Value::as_str) == Some("user");
    let mut out = Vec::new();
    for (id, installs) in v
        .get("plugins")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if id.split('@').next() != Some("dotonce") {
            continue;
        }
        let installs = installs.as_array().map(Vec::as_slice).unwrap_or_default();
        let Some(pick) = installs
            .iter()
            .find(for_here)
            .or_else(|| installs.iter().find(user))
            .or_else(|| installs.first())
        else {
            continue;
        };
        let hooks = pick
            .get("installPath")
            .and_then(Value::as_str)
            .and_then(|p| fs::read_to_string(Path::new(p).join("hooks/hooks.json")).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let on = settings
            .iter()
            .rev()
            .find_map(|(_, s)| s.get("enabledPlugins")?.get(id)?.as_bool())
            .unwrap_or(false);
        out.push(Plugin {
            id: id.clone(),
            hooks,
            on,
        });
    }
    out
}

/// The hooks.json of the plugin that started this hook. Claude Code sets
/// CLAUDE_PLUGIN_ROOT for a plugin's hooks only.
pub fn hooks_here() -> Option<Value> {
    let root = std::env::var_os("CLAUDE_PLUGIN_ROOT").filter(|r| !r.is_empty())?;
    let src = fs::read_to_string(Path::new(&root).join("hooks/hooks.json")).ok()?;
    serde_json::from_str(&src).ok()
}

pub const OFF_REASON: &str = "dotonce: Claude can't turn off or remove the dotonce plugin, or turn off all plugins. If the user wants that, they run the command in their own terminal or use /plugin.";

fn names_dotonce(w: &str) -> bool {
    w == "dotonce" || w.starts_with("dotonce@")
}

/// True when the words run `claude plugin` disable, uninstall or remove on
/// dotonce, disable with `--all`, or `marketplace remove` on dotonce. Like
/// the rule-change check, `claude` can stand anywhere, after `sudo` or `env`.
pub(crate) fn turns_off(words: &[String]) -> bool {
    words.iter().enumerate().any(|(i, w)| {
        if w != "claude" && !w.ends_with("/claude") {
            return false;
        }
        let rest = &words[i + 1..];
        let mut args = rest
            .iter()
            .map(String::as_str)
            .filter(|w| !w.starts_with('-'));
        if !matches!(args.next(), Some("plugin" | "plugins")) {
            return false;
        }
        let all = rest.iter().any(|w| {
            w == "--all" || (w.starts_with('-') && !w.starts_with("--") && w.contains('a'))
        });
        match args.next() {
            Some("disable") => all || args.any(names_dotonce),
            Some("uninstall" | "remove") => args.any(names_dotonce),
            Some("marketplace") => {
                matches!(args.next(), Some("remove" | "rm")) && args.any(names_dotonce)
            }
            _ => false,
        }
    })
}

/// True when text read on stdin by a shell names `claude plugin`, a way to
/// turn a plugin off, and dotonce or `--all`.
pub(crate) fn names_off(text: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .collect();
    words.contains(&"claude")
        && words.iter().any(|w| ["plugin", "plugins"].contains(w))
        && words
            .iter()
            .any(|w| ["disable", "uninstall", "remove", "rm"].contains(w))
        && words.iter().any(|w| ["dotonce", "--all", "-a"].contains(w))
}
