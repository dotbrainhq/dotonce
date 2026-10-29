use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoreCfg {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_stores")]
    pub stores: Vec<StoreCfg>,
    #[serde(default)]
    pub private_paths: Vec<String>,
    /// Folders dotonce stays out of: its hooks add nothing and change nothing
    /// there, except the guard that keeps rule files out of Claude's reach.
    #[serde(default)]
    pub ignore_paths: Vec<String>,
    #[serde(default = "default_save_store")]
    pub save_store: String,
    #[serde(default = "default_budget")]
    pub prompt_budget: usize,
    #[serde(default = "default_budget")]
    pub session_budget: usize,
}

fn default_stores() -> Vec<StoreCfg> {
    home_dir()
        .map(|h| StoreCfg {
            name: "notes".to_string(),
            path: h.join("notes").to_string_lossy().into_owned(),
        })
        .into_iter()
        .collect()
}

fn default_save_store() -> String {
    "notes".to_string()
}

fn default_budget() -> usize {
    1_500
}

/// The most tokens a hook block may use. Claude Code cuts additionalContext
/// at 10,000 characters, and at about four characters a token this keeps the
/// whole block under that.
pub const MAX_BUDGET: usize = 2_000;

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").expect("an empty config parses")
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `~/.dotonce`, or `DOTONCE_HOME` when set. With neither variable set there
/// is no home, rather than a `.dotonce` in whatever folder dotonce runs in.
pub fn home_dir() -> Result<PathBuf> {
    if let Some(h) = env_path("DOTONCE_HOME") {
        return Ok(h);
    }
    match env_path("HOME") {
        Some(h) => Ok(h.join(".dotonce")),
        None => bail!("can't find the dotonce home: set HOME or DOTONCE_HOME"),
    }
}

pub fn expand(p: &str) -> PathBuf {
    let home = || PathBuf::from(std::env::var("HOME").unwrap_or_default());
    if p == "~" {
        return home();
    }
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// Resolves `.` and `..` without touching the disk.
pub fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The real path when it exists, the lexically cleaned path otherwise.
pub fn resolve(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| lexical_normalize(p))
}

/// True when `cwd` is `entry` or inside it, by real path. `entry` may start
/// with `~/`; an empty or relative entry matches nothing.
pub fn path_inside(cwd: &Path, entry: &str) -> bool {
    let p = entry.trim();
    if p.is_empty() {
        return false;
    }
    let e = expand(p);
    e.is_absolute() && resolve(cwd).starts_with(resolve(&e))
}

impl Config {
    pub fn parse(src: &str) -> Result<Self> {
        let mut c: Self = toml::from_str(src)?;
        c.prompt_budget = c.prompt_budget.min(MAX_BUDGET);
        c.session_budget = c.session_budget.min(MAX_BUDGET);
        Ok(c)
    }

    pub fn store(&self, name: &str) -> Option<&StoreCfg> {
        self.stores.iter().find(|s| s.name == name)
    }

    pub fn private_allowed(&self, cwd: &Path) -> bool {
        self.private_paths.iter().any(|p| path_inside(cwd, p))
    }

    pub fn ignored(&self, cwd: &Path) -> bool {
        self.ignore_paths.iter().any(|p| path_inside(cwd, p))
    }
}

pub fn load() -> Result<Config> {
    Ok(load_file()?.unwrap_or_default())
}

/// The config file, or None when there isn't one yet.
pub fn load_file() -> Result<Option<Config>> {
    let path = home_dir()?.join("config.toml");
    if !path.exists() {
        return Ok(None);
    }
    let src = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg = Config::parse(&src).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(cfg))
}

const TEMPLATE: &str = "# dotonce configuration.
# Stores are folders of markdown facts. New facts from `dotonce save` go to save_store.
save_store = \"notes\"

# Facts marked private are only shown when Claude Code runs inside one of these folders.
private_paths = []

# dotonce stays out of these folders: its hooks add nothing and change nothing there.
# The guard that keeps rule files out of Claude's reach still runs.
ignore_paths = []

# Token budgets for what the hooks add to Claude's context, at most 2000 each.
prompt_budget = 1500
session_budget = 1500

[[stores]]
name = \"notes\"
path = \"NOTES_PATH\"
";

/// The folder of the running binary, when no entry of `PATH` resolves to it.
/// A missing `PATH` counts as not found.
pub fn folder_missing_from_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    missing_from(exe.parent()?, std::env::var_os("PATH").as_deref())
}

fn missing_from(folder: &Path, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let real = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let found = path.is_some_and(|p| {
        std::env::split_paths(p)
            .filter(|e| !e.as_os_str().is_empty())
            .any(|e| fs::canonicalize(e).is_ok_and(|e| e == real))
    });
    (!found).then(|| folder.to_path_buf())
}

/// The shell line that adds `folder` to `PATH`, written with `$HOME` when the
/// folder is under `home`.
fn path_export_line(folder: &Path, home: Option<&Path>) -> String {
    let under = |f: &Path, h: &Path| f.strip_prefix(h).ok().map(Path::to_path_buf);
    let home = home.filter(|h| !h.as_os_str().is_empty());
    let rel = home.and_then(|h| {
        let (rf, rh) = (
            fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf()),
            fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()),
        );
        under(folder, h).or_else(|| under(&rf, &rh))
    });
    let text = match rel {
        Some(r) if r.as_os_str().is_empty() => "$HOME".to_string(),
        Some(r) => format!("$HOME/{}", dq_escape(&r.to_string_lossy())),
        None => dq_escape(&folder.to_string_lossy()),
    };
    let line = format!("export PATH=\"{text}:$PATH\"");
    format!("echo '{}' >> ~/.zshrc", line.replace('\'', "'\\''"))
}

/// Escapes what a double-quoted shell string would expand, and drops control
/// characters so a folder name can't rewrite the terminal.
fn dq_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if matches!(c, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The PATH hint `init` and `doctor` print, or nothing when the binary's
/// folder is already on `PATH`. Never edits a shell file.
pub fn path_hint() -> Option<String> {
    let folder = folder_missing_from_path()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Some(format!(
        "dotonce is not on your PATH, so the short `dotonce` command won't work in a new terminal. Add its folder once:\n{}\nthen open a new terminal (or run `source ~/.zshrc`). On bash, use ~/.bashrc.",
        path_export_line(&folder, home.as_deref())
    ))
}

/// Creates the home folder, a notes store and a config file if there isn't one.
pub fn init() -> Result<PathBuf> {
    let home = home_dir()?;
    let notes = home.join("notes");
    fs::create_dir_all(&notes)?;
    let path = home.join("config.toml");
    if !path.exists() {
        fs::write(
            &path,
            TEMPLATE.replace("NOTES_PATH", &notes.to_string_lossy()),
        )?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_line_uses_home_and_quotes_odd_folders() {
        let home = Path::new("/nonexistent-home/user");
        assert_eq!(
            path_export_line(Path::new("/nonexistent-home/user/.cargo/bin"), Some(home)),
            "echo 'export PATH=\"$HOME/.cargo/bin:$PATH\"' >> ~/.zshrc"
        );
        assert_eq!(
            path_export_line(Path::new("/opt/tools/bin"), Some(home)),
            "echo 'export PATH=\"/opt/tools/bin:$PATH\"' >> ~/.zshrc"
        );
        assert_eq!(
            path_export_line(Path::new("/opt/tools/bin"), None),
            "echo 'export PATH=\"/opt/tools/bin:$PATH\"' >> ~/.zshrc"
        );
        assert_eq!(
            path_export_line(Path::new("/opt/it's $x/bin"), None),
            "echo 'export PATH=\"/opt/it'\\''s \\$x/bin:$PATH\"' >> ~/.zshrc"
        );
    }

    #[test]
    fn missing_from_needs_an_entry_that_resolves_to_the_folder() {
        let d = std::env::temp_dir().join(format!("dotonce-pathunit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let (bin, other) = (d.join("bin"), d.join("other"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&other).unwrap();
        let join = |v: &[&Path]| std::env::join_paths(v).unwrap();
        assert_eq!(missing_from(&bin, None), Some(bin.clone()));
        assert!(missing_from(&bin, Some(&join(&[&other]))).is_some());
        assert!(missing_from(&bin, Some(&join(&[&other, &bin]))).is_none());
        let dotted = bin.join("..").join("bin");
        assert!(missing_from(&bin, Some(&join(&[&dotted]))).is_none());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn parses_stores_and_privacy() {
        let c = Config::parse(
            "save_store = \"kb\"\nprivate_paths = [\"/work/private\"]\nprompt_budget = 900\n\n[[stores]]\nname = \"kb\"\npath = \"/data/kb\"\n",
        )
        .unwrap();
        assert_eq!(c.save_store, "kb");
        assert_eq!(c.prompt_budget, 900);
        assert_eq!(c.session_budget, 1_500);
        assert_eq!(c.store("kb").unwrap().path, "/data/kb");
        assert!(c.store("nope").is_none());
        assert!(c.private_allowed(Path::new("/work/private/app")));
        assert!(!c.private_allowed(Path::new("/work/public")));
    }

    #[test]
    fn ignore_paths_follow_the_private_paths_rules() {
        let c = Config::parse("ignore_paths = [\"/work/agents\", \"\", \"rel/dir\"]\n").unwrap();
        assert!(c.ignored(Path::new("/work/agents")));
        assert!(c.ignored(Path::new("/work/agents/run-1")));
        assert!(!c.ignored(Path::new("/work/agentsX")));
        assert!(!c.ignored(Path::new("/work/agents/../other")));
        assert!(!c.ignored(Path::new("rel/dir")));
        assert!(!Config::default().ignored(Path::new("/work/agents")));
        assert!(TEMPLATE.contains("\nignore_paths = []\n"));
        let err = Config::parse("[[stores]]\nname = \"kb\"\npath = \"/kb\"\nignore_paths = []\n")
            .unwrap_err();
        assert!(format!("{err:#}").contains("ignore_paths"), "{err:#}");
    }

    #[test]
    fn private_paths_inside_a_store_is_an_error() {
        let err = Config::parse(
            "[[stores]]\nname = \"kb\"\npath = \"/data/kb\"\nprivate_paths = [\"/work\"]\n",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("private_paths"), "{err:#}");
    }

    #[test]
    fn unknown_top_level_key_is_an_error() {
        let err = Config::parse("private_path = [\"/work\"]\n").unwrap_err();
        assert!(format!("{err:#}").contains("private_path"), "{err:#}");
    }

    #[test]
    fn budgets_are_capped() {
        let c = Config::parse("prompt_budget = 50000\nsession_budget = 2001\n").unwrap();
        assert_eq!(c.prompt_budget, MAX_BUDGET);
        assert_eq!(c.session_budget, MAX_BUDGET);
        let c = Config::parse("prompt_budget = 800\n").unwrap();
        assert_eq!(c.prompt_budget, 800);
    }

    #[test]
    fn empty_config_has_one_notes_store() {
        let c = Config::default();
        assert_eq!(c.save_store, "notes");
        assert_eq!(c.stores.len(), 1);
        assert!(c.stores[0].path.ends_with("notes"));
        assert!(c.private_paths.is_empty());
    }

    #[test]
    fn tilde_paths_expand_from_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~/notes"), PathBuf::from(home).join("notes"));
        assert_eq!(expand("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn path_inside_does_not_match_a_prefix_look_alike() {
        assert!(path_inside(Path::new("/a/work"), "/a/work"));
        assert!(path_inside(Path::new("/a/work/app"), "/a/work"));
        assert!(!path_inside(Path::new("/a/workshop"), "/a/work"));
        assert!(!path_inside(Path::new("/a/workshop/app"), "/a/work"));
    }

    #[test]
    fn path_inside_resolves_dot_dot() {
        assert!(!path_inside(Path::new("/a/work/../other"), "/a/work"));
        assert!(path_inside(Path::new("/a/other/../work/app"), "/a/work"));
        assert!(path_inside(Path::new("/a/work/./app"), "/a/work/../work"));
    }

    #[cfg(unix)]
    #[test]
    fn path_inside_follows_a_symlinked_folder() {
        let base = std::env::temp_dir().join(format!("dotonce-inside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real = base.join("real/app");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(base.join("other")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let entry = base.join("real").to_string_lossy().into_owned();
        assert!(path_inside(&base.join("link/app"), &entry));
        let link_entry = base.join("link").to_string_lossy().into_owned();
        assert!(path_inside(&real, &link_entry));
        assert!(!path_inside(&base.join("other"), &entry));
        fs::remove_dir_all(&base).unwrap();
    }

    fn cfg_with_private(entries: &[&str]) -> Config {
        let list: Vec<String> = entries.iter().map(|e| format!("{e:?}")).collect();
        Config::parse(&format!("private_paths = [{}]\n", list.join(", "))).unwrap()
    }

    #[test]
    fn empty_private_entry_allows_nothing() {
        let c = cfg_with_private(&[""]);
        assert!(!c.private_allowed(Path::new("/work/private")));
        let c = cfg_with_private(&["   "]);
        assert!(!c.private_allowed(Path::new("/work/private")));
    }

    #[test]
    fn relative_private_entry_allows_nothing() {
        let c = cfg_with_private(&["work/private"]);
        assert!(!c.private_allowed(Path::new("work/private/app")));
        assert!(!c.private_allowed(Path::new("/work/private/app")));
    }

    #[test]
    fn sibling_with_the_same_prefix_is_rejected() {
        let c = cfg_with_private(&["/work/private"]);
        assert!(!c.private_allowed(Path::new("/work/privateX")));
        assert!(c.private_allowed(Path::new("/work/private")));
    }

    #[test]
    fn dotdot_segments_are_resolved() {
        let c = cfg_with_private(&["/work/private"]);
        assert!(!c.private_allowed(Path::new("/work/private/../public")));
        assert!(c.private_allowed(Path::new("/work/public/../private/app")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dirs_match_by_real_path() {
        let base = std::env::temp_dir().join(format!("dotonce-cfg-{}", std::process::id()));
        let real = base.join("real");
        let link = base.join("link");
        fs::create_dir_all(real.join("app")).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let via_link = cfg_with_private(&[link.to_str().unwrap()]);
        assert!(via_link.private_allowed(&real.join("app")));
        assert!(via_link.private_allowed(&link.join("app")));

        let via_real = cfg_with_private(&[real.to_str().unwrap()]);
        assert!(via_real.private_allowed(&real.join("app")));
        assert!(via_real.private_allowed(&link.join("app")));

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn bare_tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~"), PathBuf::from(home));
    }
}
