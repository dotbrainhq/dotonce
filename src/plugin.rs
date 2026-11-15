//! The dotonce plugin for Claude Code: finding its installs for `doctor`,
//! and the hooks file of the plugin that started a hook.

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
