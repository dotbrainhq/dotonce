//! Rule set files: one TOML file per set in `~/.dotonce/rules/`. Parsing is
//! strict, and a file with any error is skipped whole, never half-loaded.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::config::{expand, home_dir};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Remind,
    #[default]
    Warn,
    Fix,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Remind => "remind",
            Level::Warn => "warn",
            Level::Fix => "fix",
        }
    }

    pub fn parse(s: &str) -> Result<Level> {
        match s.trim() {
            "remind" => Ok(Level::Remind),
            "warn" => Ok(Level::Warn),
            "fix" => Ok(Level::Fix),
            other => bail!("unknown level {other}: use remind, warn or fix"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub prompt: Vec<String>,
    /// Prompt words turn on the checks too, not only the reminder.
    #[serde(default)]
    pub check_on_prompt: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    #[serde(default)]
    pub fence: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remind {
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum On {
    #[default]
    Reply,
    Tool,
}

/// The tools a tool check can name.
pub const TOOLS: [&str; 5] = ["Bash", "Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Every pattern this process has compiled, by its source, so a pattern
/// shared by several sets compiles once.
static COMPILED: LazyLock<Mutex<HashMap<String, Result<Regex, String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A check's regex, kept as its source and compiled on first use, so a hook
/// pays only for the patterns it runs. Commands compile every pattern up
/// front with `validate`.
#[derive(Debug, Clone)]
pub struct Pattern {
    src: String,
    /// Where the pattern is, such as `check a: forbid_regex`, for errors.
    what: String,
    /// The groups the pattern must have, counting the whole match.
    groups: Option<usize>,
    re: OnceLock<Result<Regex, String>>,
}

impl Pattern {
    fn new(src: String, what: String, groups: Option<usize>) -> Pattern {
        Pattern {
            src,
            what,
            groups,
            re: OnceLock::new(),
        }
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    /// The compiled pattern, or why it can't be used.
    pub fn regex(&self) -> Result<&Regex, &str> {
        self.re
            .get_or_init(|| {
                let re = compiled(&self.src).map_err(|e| format!("{}: {e}", self.what))?;
                match self.groups {
                    Some(n) if re.captures_len() != n => Err(format!(
                        "{}: needs exactly one group in its pattern, around the text to count",
                        self.what
                    )),
                    _ => Ok(re),
                }
            })
            .as_ref()
            .map_err(String::as_str)
    }
}

/// Patterns run with multi-line on, so `^` and `$` match at each line.
fn compiled(src: &str) -> Result<Regex, String> {
    let mut cache = COMPILED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = cache.get(src) {
        return r.clone();
    }
    let r = RegexBuilder::new(src)
        .multi_line(true)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| e.to_string());
    cache.insert(src.to_string(), r.clone());
    r
}

#[derive(Debug, Clone)]
pub enum CheckKind {
    Forbid(Vec<String>),
    ForbidRegex(Pattern),
    Require(Vec<String>),
    BareUrl,
    RepeatOf { pattern: Pattern, max: usize },
    MaxParagraphs(usize),
    MaxWords(usize),
}

impl CheckKind {
    pub fn name(&self) -> &'static str {
        match self {
            CheckKind::Forbid(_) => "forbid",
            CheckKind::ForbidRegex(_) => "forbid_regex",
            CheckKind::Require(_) => "require",
            CheckKind::BareUrl => "bare-url",
            CheckKind::RepeatOf { .. } => "repeat_of",
            CheckKind::MaxParagraphs(_) => "max_paragraphs",
            CheckKind::MaxWords(_) => "max_words",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub id: String,
    pub why: String,
    pub on: On,
    pub tool: Option<String>,
    pub paths: Option<GlobSet>,
    pub kind: CheckKind,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    pub id: String,
    pub level: Level,
    pub private: bool,
    pub when: When,
    pub draft: Option<Draft>,
    pub remind: Option<Remind>,
    pub checks: Vec<Check>,
    pub file: PathBuf,
    /// The file's text as read, for the approval hash.
    pub src: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileError {
    pub file: PathBuf,
    pub error: String,
}

impl FileError {
    /// One line for a status message: the file name, and the error's first
    /// and last lines, since a TOML error puts its reason last.
    pub fn line(&self) -> String {
        format!("{}: {}", self.name(), self.reason())
    }

    /// The file's name, such as `notes.toml`.
    pub fn name(&self) -> String {
        self.file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The error's first and last lines, since a TOML error puts its reason last.
    pub fn reason(&self) -> String {
        let lines: Vec<&str> = self
            .error
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        match lines.as_slice() {
            [] => String::new(),
            [one] => one.to_string(),
            [first, .., last] => format!("{first}: {last}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Loaded {
    pub sets: Vec<RuleSet>,
    pub errors: Vec<FileError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSet {
    id: Option<String>,
    #[serde(default)]
    level: Level,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    when: When,
    draft: Option<Draft>,
    remind: Option<Remind>,
    #[serde(default, rename = "check")]
    checks: Vec<RawCheck>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    id: Option<String>,
    why: Option<String>,
    #[serde(default)]
    on: On,
    tool: Option<String>,
    #[serde(default)]
    path: Vec<String>,
    forbid: Option<Vec<String>>,
    forbid_regex: Option<String>,
    require: Option<Vec<String>>,
    kind: Option<String>,
    repeat_of: Option<String>,
    max: Option<usize>,
    max_paragraphs: Option<usize>,
    max_words: Option<usize>,
}

pub fn rules_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join("rules"))
}

/// The text without control characters other than tab, for printing to a
/// terminal: a TOML escape in a rule file could otherwise move the cursor or
/// erase a line, and hide the "changed" or "off" lines. C1 controls go too,
/// since some terminals read U+009B as an escape.
pub fn printable(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\t' || !c.is_control())
        .collect()
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn phrases(list: Vec<String>, id: &str, field: &str) -> Result<Vec<String>> {
    let out: Vec<String> = list
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if out.is_empty() {
        bail!("check {id}: {field} needs at least one phrase");
    }
    Ok(out)
}

fn check(raw: RawCheck, n: usize) -> Result<Check> {
    let id = raw
        .id
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("check {n} has no id"))?;
    let why = raw.why.filter(|s| !s.trim().is_empty()).with_context(|| {
        format!("check {id} has no why: say why the rule exists, Claude sees it with the failure")
    })?;
    let kinds = [
        raw.forbid.is_some(),
        raw.forbid_regex.is_some(),
        raw.require.is_some(),
        raw.kind.is_some(),
        raw.repeat_of.is_some(),
        raw.max_paragraphs.is_some(),
        raw.max_words.is_some(),
    ]
    .into_iter()
    .filter(|k| *k)
    .count();
    if kinds == 0 {
        bail!(
            "check {id} has no kind: give one of forbid, forbid_regex, require, kind = \"bare-url\", repeat_of, max_paragraphs or max_words"
        );
    }
    if kinds > 1 {
        bail!("check {id} has {kinds} kinds: give exactly one");
    }
    if raw.max.is_some() && raw.repeat_of.is_none() {
        bail!("check {id}: max only goes with repeat_of");
    }
    let kind = if let Some(list) = raw.forbid {
        CheckKind::Forbid(phrases(list, &id, "forbid")?)
    } else if let Some(re) = raw.forbid_regex {
        CheckKind::ForbidRegex(Pattern::new(re, format!("check {id}: forbid_regex"), None))
    } else if let Some(list) = raw.require {
        CheckKind::Require(phrases(list, &id, "require")?)
    } else if let Some(k) = raw.kind {
        if k != "bare-url" {
            bail!("check {id}: unknown kind {k}: the only named kind is bare-url");
        }
        CheckKind::BareUrl
    } else if let Some(re) = raw.repeat_of {
        let pattern = Pattern::new(re, format!("check {id}: repeat_of"), Some(2));
        let max = raw
            .max
            .with_context(|| format!("check {id}: repeat_of needs max"))?;
        CheckKind::RepeatOf { pattern, max }
    } else if let Some(n) = raw.max_paragraphs {
        CheckKind::MaxParagraphs(n)
    } else if let Some(n) = raw.max_words {
        CheckKind::MaxWords(n)
    } else {
        bail!("check {id} has no kind");
    };
    match raw.on {
        On::Reply => {
            if raw.tool.is_some() || !raw.path.is_empty() {
                bail!("check {id}: tool and path only go with on = \"tool\"");
            }
        }
        On::Tool => {
            let tool = raw
                .tool
                .as_deref()
                .with_context(|| format!("check {id}: on = \"tool\" needs tool"))?;
            if !TOOLS.contains(&tool) {
                bail!(
                    "check {id}: unknown tool {tool}: use one of {}",
                    TOOLS.join(", ")
                );
            }
            if tool == "Bash" && !raw.path.is_empty() {
                bail!("check {id}: path doesn't go with Bash, which has no file path");
            }
        }
    }
    let paths = if raw.path.is_empty() {
        None
    } else {
        let mut b = GlobSetBuilder::new();
        for g in &raw.path {
            b.add(
                Glob::new(&expand(g).to_string_lossy())
                    .with_context(|| format!("check {id}: path {g}"))?,
            );
        }
        Some(b.build()?)
    };
    Ok(Check {
        id,
        why,
        on: raw.on,
        tool: raw.tool,
        paths,
        kind,
    })
}

/// Parses one rule set file and compiles every pattern in it. `file` is
/// where it was read from: the set's id must match its name.
pub fn parse(src: &str, file: &Path) -> Result<RuleSet> {
    let set = parse_lazy(src, file)?;
    validate(&set)?;
    Ok(set)
}

/// Compiles every pattern in the set, so a bad one is an error here rather
/// than a skipped check in a hook.
pub fn validate(set: &RuleSet) -> Result<()> {
    for c in &set.checks {
        if let CheckKind::ForbidRegex(p) | CheckKind::RepeatOf { pattern: p, .. } = &c.kind {
            if let Err(e) = p.regex() {
                bail!("{e}");
            }
        }
    }
    Ok(())
}

/// Parses one rule set file, leaving its patterns to compile on first use.
fn parse_lazy(src: &str, file: &Path) -> Result<RuleSet> {
    let raw: RawSet = toml::from_str(src)?;
    let id = raw
        .id
        .filter(|s| !s.trim().is_empty())
        .context("the set has no id")?;
    if !valid_id(&id) {
        bail!("id {id:?} can only use a-z, 0-9, - and _");
    }
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem != id {
        bail!("id {id} doesn't match the file name {stem}.toml: name the file {id}.toml");
    }
    if raw
        .remind
        .as_ref()
        .is_some_and(|r| r.text.trim().is_empty())
    {
        bail!("[remind] text is empty: write the reminder or drop the table");
    }
    let draft = match raw.draft {
        Some(d) => {
            let fence: Vec<String> = d.fence.iter().map(|f| f.trim().to_lowercase()).collect();
            if fence.is_empty() {
                bail!(
                    "[draft] fence is empty: list the info strings that mark a draft, or drop [draft] to check whole replies"
                );
            }
            Some(Draft { fence })
        }
        None => None,
    };
    let mut checks: Vec<Check> = Vec::new();
    for (i, rc) in raw.checks.into_iter().enumerate() {
        let c = check(rc, i + 1)?;
        if checks.iter().any(|x| x.id == c.id) {
            bail!("two checks have the id {}", c.id);
        }
        checks.push(c);
    }
    Ok(RuleSet {
        id,
        level: raw.level,
        private: raw.private,
        when: raw.when,
        draft,
        remind: raw.remind,
        checks,
        file: file.to_path_buf(),
        src: src.to_string(),
    })
}

/// Every `*.toml` file in `dir`, sorted by name, with every pattern
/// compiled: a set with a bad pattern is an error. For commands.
pub fn load_dir_checked(dir: &Path) -> Result<Loaded> {
    let mut out = load_dir(dir)?;
    let mut sets = Vec::new();
    for s in std::mem::take(&mut out.sets) {
        match validate(&s) {
            Ok(()) => sets.push(s),
            Err(e) => out.errors.push(FileError {
                file: s.file.clone(),
                error: format!("{e:#}"),
            }),
        }
    }
    out.errors.sort_by(|a, b| a.file.cmp(&b.file));
    out.sets = sets;
    Ok(out)
}

/// Every `*.toml` file in `dir`, sorted by name, with patterns left to
/// compile on first use, for hooks. Names starting with `.` or `_` are
/// skipped, as in stores. A missing folder has no sets.
pub fn load_dir(dir: &Path) -> Result<Loaded> {
    let mut out = Loaded::default();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            name.ends_with(".toml") && !name.starts_with('.') && !name.starts_with('_')
        })
        .collect();
    files.sort();
    for file in files {
        // A link could point the folder at a file outside it, so it's an
        // error rather than a set.
        let linked = fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_symlink());
        let res = if linked {
            Err(anyhow::anyhow!("symlinked rule files aren't loaded"))
        } else {
            fs::read_to_string(&file)
                .map_err(anyhow::Error::from)
                .and_then(|src| parse_lazy(&src, &file))
        };
        match res {
            Ok(s) => out.sets.push(s),
            Err(e) => out.errors.push(FileError {
                file,
                error: format!("{e:#}"),
            }),
        }
    }
    Ok(out)
}

/// The rule sets, patterns compiled on first use: the hooks' loader.
pub fn load() -> Result<Loaded> {
    load_dir(&rules_dir()?)
}

/// The rule sets with every pattern compiled: the commands' loader.
pub fn load_checked() -> Result<Loaded> {
    load_dir_checked(&rules_dir()?)
}

const STARTER: &str = r##"# dotonce rule set. See the README, "Rules and reply checks".
# A set does nothing until you approve it in a terminal: dotonce rules approve SET_ID
id = "SET_ID"
# remind: only add the reminder. warn: also run the checks and log what fails.
# fix: also send a failing draft back once, and deny a tool call that fails.
level = "warn"
# true: the set is only on when Claude Code runs inside one of your private_paths.
private = false

# Any one match turns the set on.
[when]
# Skills Claude uses, with or without a plugin prefix. On for the rest of the session.
skills = ["example-skill"]
# Folders, such as "~/work/support". On while Claude works inside one.
folders = []
# Words in your prompt. Only adds the reminder, for that turn.
prompt = ["draft"]
# true: prompt words also turn on the checks.
check_on_prompt = false

DRAFT_TABLE
[remind]
text = """
Write the reminder Claude sees when this set turns on.
"""

# Each check has an id, a why, and exactly one kind.
[[check]]
id = "no-stock-opener"
forbid = ["sorry for the wait", "sorry for the delay"]
why = "Open with a line that fits this reply, not a stock phrase."

[[check]]
id = "no-placeholder"
forbid_regex = '\[(TODO|NAME)\]'
why = "Fill in every placeholder."

[[check]]
id = "sign-off"
require = ["Thanks,", "Best,"]
why = "End with a sign-off."

[[check]]
id = "inline-links"
kind = "bare-url"
why = "Links go inline as anchor text."

[[check]]
id = "name-once"
repeat_of = '^Hi (\w+),'
max = 0
why = "Use the name once, in the greeting."

[[check]]
id = "short"
max_paragraphs = 5
why = "Five paragraphs at most."

[[check]]
id = "length"
max_words = 300
why = "Under 300 words."

# A tool check runs before a tool call instead of on the reply:
# [[check]]
# id = "no-force-push"
# on = "tool"
# tool = "Bash"
# forbid_regex = 'git\s+push\b.*(--force|-f\b)'
# why = "Never force push. Ask first."
"##;

const DRAFT_TABLE: &str = "# Drafts are fenced blocks with these info strings, like ```markdown.\n# \"\" means a fence with no info string. Drop this table to check whole\n# replies instead.\n[draft]\nfence = [\"markdown\", \"draft\"]\n";

const WHOLE_TABLE: &str = "# No [draft] table, so the checks read the whole reply. To check only\n# fenced drafts, add:\n# [draft]\n# fence = [\"markdown\", \"draft\"]\n";

/// A commented starter file with one example of each check kind.
pub fn starter(id: &str, whole: bool) -> String {
    let table = if whole { WHOLE_TABLE } else { DRAFT_TABLE };
    STARTER
        .replace("SET_ID", id)
        .replace("DRAFT_TABLE\n", table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &str) -> Result<RuleSet> {
        parse(src, Path::new("/rules/ticket-reply.toml"))
    }

    fn err(src: &str) -> String {
        format!("{:#}", p(src).unwrap_err())
    }

    const FULL: &str = r#"id = "ticket-reply"
level = "fix"
private = false

[when]
skills = ["writing-support"]
folders = ["~/work/support"]
prompt = ["draft", "reply"]

[draft]
fence = ["Markdown", "draft"]

[remind]
text = "Links go inline."

[[check]]
id = "no-stock-wait-opener"
forbid = ["sorry for the wait"]
why = "Open with a line from the thread."

[[check]]
id = "inline-links"
kind = "bare-url"
why = "Links go inline as anchor text."

[[check]]
id = "no-todo"
forbid_regex = '\bTODO\b'
why = "No placeholders."

[[check]]
id = "sign-off"
require = ["Thanks,"]
why = "End with a sign-off."

[[check]]
id = "name-once"
repeat_of = '^Hi (\w+),'
max = 0
why = "Use the name once."

[[check]]
id = "short"
max_paragraphs = 4
why = "Four paragraphs at most."

[[check]]
id = "length"
max_words = 200
why = "Under 200 words."

[[check]]
id = "no-force-push"
on = "tool"
tool = "Bash"
forbid_regex = 'git\s+push\b.*(--force|-f\b)'
why = "Never force push."

[[check]]
id = "no-todo-in-rust"
on = "tool"
tool = "Write"
path = ["*.rs"]
forbid = ["TODO"]
why = "No TODOs in Rust files."
"#;

    #[test]
    fn parses_every_check_kind() {
        let s = p(FULL).unwrap();
        assert_eq!(s.id, "ticket-reply");
        assert_eq!(s.level, Level::Fix);
        assert!(!s.private);
        assert_eq!(s.when.skills, vec!["writing-support"]);
        assert_eq!(s.when.prompt, vec!["draft", "reply"]);
        assert!(!s.when.check_on_prompt);
        assert_eq!(s.draft.as_ref().unwrap().fence, vec!["markdown", "draft"]);
        assert_eq!(s.remind.as_ref().unwrap().text, "Links go inline.");
        let kinds: Vec<&str> = s.checks.iter().map(|c| c.kind.name()).collect();
        assert_eq!(
            kinds,
            [
                "forbid",
                "bare-url",
                "forbid_regex",
                "require",
                "repeat_of",
                "max_paragraphs",
                "max_words",
                "forbid_regex",
                "forbid"
            ]
        );
        assert_eq!(s.checks[7].on, On::Tool);
        assert_eq!(s.checks[7].tool.as_deref(), Some("Bash"));
        let globs = s.checks[8].paths.as_ref().unwrap();
        assert!(globs.is_match("/code/src/main.rs"));
        assert!(!globs.is_match("/code/notes.md"));
        assert_eq!(s.src, FULL);
        assert_eq!(s.file, PathBuf::from("/rules/ticket-reply.toml"));
    }

    #[test]
    fn level_defaults_to_warn_and_tables_are_optional() {
        let s = p("id = \"ticket-reply\"\n").unwrap();
        assert_eq!(s.level, Level::Warn);
        assert!(s.checks.is_empty());
        assert!(s.draft.is_none());
        assert!(s.remind.is_none());
        assert_eq!(s.when, When::default());
    }

    #[test]
    fn strict_parsing_rejects_bad_files() {
        let base = "id = \"ticket-reply\"\n";
        let check = |rest: &str| format!("{base}[[check]]\nid = \"a\"\nwhy = \"b\"\n{rest}");
        let cases = [
            (format!("{base}colour = 1\n"), "colour"),
            (format!("{base}[when]\nskils = [\"x\"]\n"), "skils"),
            (format!("{base}[[checks]]\nid = \"a\"\n"), "checks"),
            (check(""), "no kind"),
            (check("forbid = [\"x\"]\nmax_words = 3\n"), "2 kinds"),
            (
                check("forbid = [\"x\"]\nmax = 3\n"),
                "max only goes with repeat_of",
            ),
            (
                format!("{base}[[check]]\nforbid = [\"x\"]\nwhy = \"b\"\n"),
                "check 1 has no id",
            ),
            (
                format!("{base}[[check]]\nid = \"a\"\nforbid = [\"x\"]\n"),
                "check a has no why",
            ),
            (
                format!("{base}[[check]]\nid = \"a\"\nwhy = \" \"\nforbid = [\"x\"]\n"),
                "check a has no why",
            ),
            (check("kind = \"bare-link\"\n"), "bare-link"),
            (check("repeat_of = 'Hi \\w+'\nmax = 0\n"), "one group"),
            (check("repeat_of = 'Hi (\\w+)'\n"), "needs max"),
            (check("forbid_regex = '('\n"), "check a: forbid_regex"),
            (check("forbid = []\n"), "at least one phrase"),
            (check("forbid = [\"  \"]\n"), "at least one phrase"),
            (check("on = \"tool\"\nforbid = [\"x\"]\n"), "needs tool"),
            (
                check("on = \"tool\"\ntool = \"Read\"\nforbid = [\"x\"]\n"),
                "unknown tool Read",
            ),
            (
                check("tool = \"Bash\"\nforbid = [\"x\"]\n"),
                "only go with on",
            ),
            (
                check("on = \"tool\"\ntool = \"Bash\"\npath = [\"*.rs\"]\nforbid = [\"x\"]\n"),
                "path doesn't go with Bash",
            ),
            (
                check("on = \"tool\"\ntool = \"Write\"\npath = [\"a[\"]\nforbid = [\"x\"]\n"),
                "path a[",
            ),
            (
                format!(
                    "{base}[[check]]\nid = \"a\"\nwhy = \"b\"\nforbid = [\"x\"]\n[[check]]\nid = \"a\"\nwhy = \"c\"\nforbid = [\"y\"]\n"
                ),
                "two checks have the id a",
            ),
            (
                format!("{base}[draft]\nfence = []\n"),
                "[draft] fence is empty",
            ),
            (
                format!("{base}[remind]\ntext = \"\"\n"),
                "[remind] text is empty",
            ),
            ("level = \"warn\"\n".to_string(), "no id"),
            ("id = \"other\"\n".to_string(), "file name"),
            (format!("{base}level = \"block\"\n"), "block"),
        ];
        for (src, want) in cases {
            let e = err(&src);
            assert!(e.contains(want), "want {want:?} in: {e}");
        }
    }

    #[test]
    fn patterns_compile_on_first_use_and_commands_compile_them_all() {
        let bad = "id = \"ticket-reply\"\n[[check]]\nid = \"a\"\nwhy = \"b\"\nforbid_regex = '('\n[[check]]\nid = \"c\"\nwhy = \"d\"\nrepeat_of = 'Hi \\w+'\nmax = 0\n";
        let file = Path::new("/rules/ticket-reply.toml");
        let s = parse_lazy(bad, file).unwrap();
        let e = format!("{:#}", validate(&s).unwrap_err());
        assert!(e.contains("check a: forbid_regex"), "{e}");
        let CheckKind::RepeatOf { pattern, .. } = &s.checks[1].kind else {
            panic!("repeat_of")
        };
        assert!(pattern.regex().unwrap_err().contains("one group"));
        assert!(parse(bad, file).is_err());

        let dir = std::env::temp_dir().join(format!("dotonce-lazy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ticket-reply.toml"), bad).unwrap();
        fs::write(dir.join("good.toml"), "id = \"good\"\n").unwrap();
        let hooks = load_dir(&dir).unwrap();
        assert_eq!(hooks.sets.len(), 2);
        assert!(hooks.errors.is_empty());
        let cmds = load_dir_checked(&dir).unwrap();
        let ids: Vec<&str> = cmds.sets.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["good"]);
        assert_eq!(cmds.errors.len(), 1);
        assert!(cmds.errors[0].error.contains("forbid_regex"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pattern_shared_by_sets_compiles_once() {
        let src = format!("cache-probe-{}", std::process::id());
        COMPILED
            .lock()
            .unwrap()
            .insert(src.clone(), Err("from the cache".to_string()));
        let p = Pattern::new(src.clone(), "check a: forbid_regex".to_string(), None);
        assert!(p.regex().unwrap_err().contains("from the cache"));
        let fresh = format!("shared-{}", std::process::id());
        let a = Pattern::new(fresh.clone(), "a".to_string(), None);
        let b = Pattern::new(fresh.clone(), "b".to_string(), None);
        assert!(a.regex().unwrap().is_match(&fresh));
        assert!(COMPILED.lock().unwrap().contains_key(&fresh));
        assert!(b.regex().unwrap().is_match(&fresh));
        assert_eq!(b.source(), fresh);
    }

    #[test]
    fn ids_use_plain_characters() {
        assert!(valid_id("ticket-reply_2"));
        assert!(!valid_id("Ticket"));
        assert!(!valid_id("a b"));
        assert!(!valid_id(""));
        assert!(!valid_id("../x"));
        let e = format!(
            "{:#}",
            parse("id = \"A b\"\n", Path::new("/r/A b.toml")).unwrap_err()
        );
        assert!(e.contains("a-z"), "{e}");
    }

    #[test]
    fn a_broken_file_is_skipped_whole_and_reported() {
        let dir = std::env::temp_dir().join(format!("dotonce-rules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("good.toml"), "id = \"good\"\n").unwrap();
        fs::write(
            dir.join("bad.toml"),
            "id = \"bad\"\n[[check]]\nid = \"a\"\nwhy = \"b\"\nforbid = [\"x\"]\n[[check]]\nid = \"c\"\nforbid = [\"y\"]\n",
        )
        .unwrap();
        fs::write(dir.join("notes.md"), "not a rule set").unwrap();
        fs::write(dir.join(".hidden.toml"), "broken = [").unwrap();
        fs::write(dir.join("_draft.toml"), "broken = [").unwrap();
        let l = load_dir(&dir).unwrap();
        let ids: Vec<&str> = l.sets.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["good"]);
        assert_eq!(l.errors.len(), 1);
        assert!(l.errors[0].file.ends_with("bad.toml"));
        assert!(
            l.errors[0].error.contains("check c has no why"),
            "{}",
            l.errors[0].error
        );
        assert!(
            l.errors[0].line().starts_with("bad.toml: "),
            "{}",
            l.errors[0].line()
        );
        assert!(load_dir(&dir.join("missing")).unwrap().sets.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_rule_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("dotonce-rules-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let rules = dir.join("rules");
        fs::create_dir_all(&rules).unwrap();
        fs::write(dir.join("acme.toml"), "id = \"acme\"\n").unwrap();
        std::os::unix::fs::symlink(dir.join("acme.toml"), rules.join("acme.toml")).unwrap();
        std::os::unix::fs::symlink(dir.join("missing.toml"), rules.join("gone.toml")).unwrap();
        fs::write(rules.join("beta.toml"), "id = \"beta\"\n").unwrap();
        let l = load_dir(&rules).unwrap();
        let ids: Vec<&str> = l.sets.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["beta"]);
        assert_eq!(l.errors.len(), 2, "{:?}", l.errors);
        assert!(l.errors[0].file.ends_with("acme.toml"));
        assert!(l.errors[1].file.ends_with("gone.toml"));
        for e in &l.errors {
            assert_eq!(e.error, "symlinked rule files aren't loaded");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_error_lines_keep_the_reason() {
        let e = FileError {
            file: PathBuf::from("/r/x.toml"),
            error: "TOML parse error at line 2, column 1\n  |\n2 | colour = 1\n  | ^^^^^^\nunknown field `colour`\n".to_string(),
        };
        assert_eq!(
            e.line(),
            "x.toml: TOML parse error at line 2, column 1: unknown field `colour`"
        );
    }

    #[test]
    fn starter_files_parse_both_ways() {
        for whole in [false, true] {
            let src = starter("acme-replies", whole);
            let s = parse(&src, Path::new("/r/acme-replies.toml")).unwrap();
            assert_eq!(s.id, "acme-replies");
            assert_eq!(s.level, Level::Warn);
            assert_eq!(s.draft.is_none(), whole);
            let kinds: Vec<&str> = s.checks.iter().map(|c| c.kind.name()).collect();
            assert_eq!(
                kinds,
                [
                    "forbid",
                    "forbid_regex",
                    "require",
                    "bare-url",
                    "repeat_of",
                    "max_paragraphs",
                    "max_words"
                ]
            );
        }
    }

    #[test]
    fn levels_parse_and_print() {
        for l in [Level::Remind, Level::Warn, Level::Fix] {
            assert_eq!(Level::parse(l.as_str()).unwrap(), l);
        }
        assert!(Level::parse("block").is_err());
    }

    #[test]
    fn printable_drops_terminal_control_characters_but_keeps_tabs() {
        assert_eq!(
            printable("keep\u{1b}[2K it\tshort\r\n\u{7f}\u{7}"),
            "keep[2K it\tshort"
        );
        assert_eq!(printable("a\u{9b}2Kb"), "a2Kb");
        assert_eq!(printable("Links go inline."), "Links go inline.");
    }
}
