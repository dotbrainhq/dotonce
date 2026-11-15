//! Which skills a session has loaded, for the `skill-loaded` check: the
//! names the Skill and slash-command hooks recorded, and, only when a draft
//! shows up with none of them, the loads the session record holds.

use std::cell::OnceCell;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::expand;
use crate::session::skill_match;

const LOADED_MARK: &str = "Base directory for this skill: ";

/// The skill's name without a plugin prefix or a leading slash.
pub fn base(name: &str) -> &str {
    let n = name.trim().trim_start_matches('/');
    n.rsplit(':').next().unwrap_or(n)
}

/// Whether a skill counts as a writing skill when a check names none: its
/// name starts with `writing` or ends with `reply`, `replies` or
/// `descriptions`.
pub fn default_match(name: &str) -> bool {
    let b = base(name).to_ascii_lowercase();
    b.starts_with("writing")
        || ["reply", "replies", "descriptions"]
            .iter()
            .any(|end| b.ends_with(end))
}

/// Whether `name` is one of `list`, or a writing skill by name when the
/// list is empty.
pub fn wanted(list: &[String], name: &str) -> bool {
    if list.is_empty() {
        default_match(name)
    } else {
        skill_match(list, name).is_some()
    }
}

/// The skill a `Read` of `path` loads: `<dir>/skills/<name>/SKILL.md`.
fn read_skill(path: &str) -> Option<String> {
    let p = Path::new(path);
    if p.file_name()? != "SKILL.md" {
        return None;
    }
    let dir = p.parent()?;
    (dir.parent()?.file_name()? == "skills")
        .then(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))?
}

fn record_text(v: &Value) -> String {
    match v.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The skills one session record entry loads: a `Skill` call, a `Read` of a
/// skill's `SKILL.md`, or the note Claude Code adds when a skill loads.
fn loads_in(v: &Value) -> Vec<String> {
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|b| {
                let input = b.get("input")?;
                match b.get("name").and_then(Value::as_str)? {
                    "Skill" => input
                        .get("skill")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    "Read" => read_skill(input.get("file_path").and_then(Value::as_str)?),
                    _ => None,
                }
            })
            .collect(),
        Some("user") if v.get("isMeta").and_then(Value::as_bool) == Some(true) => {
            let text = record_text(v);
            text.trim_start()
                .strip_prefix(LOADED_MARK)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|dir| Path::new(dir.trim_end_matches('/')).file_name())
                .map(|n| vec![n.to_string_lossy().into_owned()])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// The skills the session record loads from byte `from` on, in order, each
/// once, and where its last whole line ends, to read from next time. A line
/// still being written waits for then. A record that can't be read loads
/// none; one shorter than `from` was replaced, so it's read from the start.
pub fn from_record(path: &Path, from: u64) -> (Vec<String>, u64) {
    let Ok(f) = File::open(path) else {
        return (Vec::new(), from);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or_default();
    let mut at = if from > len { 0 } else { from };
    let mut r = BufReader::new(f);
    if r.seek(SeekFrom::Start(at)).is_err() {
        return (Vec::new(), from);
    }
    let mut out: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(n) if n > 0 && buf.ends_with(b"\n") => at += n as u64,
            _ => break,
        }
        let line = String::from_utf8_lossy(&buf);
        if !(line.contains("\"Skill\"") || line.contains("SKILL.md") || line.contains(LOADED_MARK))
        {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for s in loads_in(&v) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    (out, at)
}

/// The folders Claude Code loads skills from: the user's, each project's up
/// from `cwd`, and the installed plugins'.
fn skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand("~/.claude"));
    let mut dirs = vec![config.join("skills")];
    dirs.extend(cwd.ancestors().map(|a| a.join(".claude/skills")));
    // plugins/cache/<marketplace>/<plugin>/<version>/skills
    let mut level = vec![config.join("plugins/cache")];
    for _ in 0..3 {
        level = level
            .iter()
            .filter_map(|d| fs::read_dir(d).ok())
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
    }
    dirs.extend(level.into_iter().map(|p| p.join("skills")));
    dirs
}

/// The installed skills a check without its own list counts, by name,
/// sorted, each once.
pub fn installed_writing(cwd: &Path) -> Vec<String> {
    let mut out: Vec<String> = skill_dirs(cwd)
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .filter(|e| e.path().join("SKILL.md").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| default_match(n))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What one hook call knows about the session's skills. The session record
/// and the skill folders are read at most once, and only when asked.
pub struct Seen<'a> {
    recorded: &'a [String],
    /// The session record, and the byte its earlier reads ended at.
    record: Option<(&'a Path, u64)>,
    cwd: &'a Path,
    read: OnceCell<(Vec<String>, u64)>,
    installed: OnceCell<Vec<String>>,
}

impl<'a> Seen<'a> {
    pub fn new(recorded: &'a [String], record: Option<(&'a Path, u64)>, cwd: &'a Path) -> Self {
        Seen {
            recorded,
            record,
            cwd,
            read: OnceCell::new(),
            installed: OnceCell::new(),
        }
    }

    /// Whether one of `list` (or a writing skill, for an empty list) has
    /// loaded in the session.
    pub fn any(&self, list: &[String]) -> bool {
        self.recorded.iter().any(|s| wanted(list, s))
            || self.record.is_some_and(|(r, from)| {
                self.read
                    .get_or_init(|| from_record(r, from))
                    .0
                    .iter()
                    .any(|s| wanted(list, s))
            })
    }

    /// The skills a check asks for, to name in its message: its own list,
    /// or the installed writing skills. Empty means there's nothing to load,
    /// so the check passes.
    pub fn choices(&self, list: &[String]) -> Vec<String> {
        if list.is_empty() {
            self.installed
                .get_or_init(|| installed_writing(self.cwd))
                .clone()
        } else {
            list.to_vec()
        }
    }

    /// The skills read from the session record and where the read ended,
    /// when it was read.
    pub fn read_from_record(&self) -> Option<&(Vec<String>, u64)> {
        self.read.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writing_skills_go_by_name_without_a_list() {
        for n in [
            "writing-notes",
            "acme:writing-mail",
            "/support-reply",
            "forum-replies",
            "pr-descriptions",
        ] {
            assert!(default_match(n), "{n}");
        }
        for n in [
            "code-review",
            "rewriting-tool",
            "replying-bot",
            "description",
        ] {
            assert!(!default_match(n), "{n}");
        }
        let list = vec!["acme-writer".to_string()];
        assert!(wanted(&list, "acme:acme-writer"));
        assert!(!wanted(&list, "writing-notes"));
        assert!(wanted(&[], "writing-notes"));
    }

    #[test]
    fn the_record_shows_each_way_a_skill_loads() {
        let lines = [
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "acme:acme-writer"}}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "/home/sam/.claude/skills/writing-notes/SKILL.md"}}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "/home/sam/notes/SKILL.md"}}]}}),
            json!({"type": "user", "isMeta": true, "message": {"content": [{"type": "text", "text": "Base directory for this skill: /home/sam/.claude/skills/forum-replies\n\n# Forum"}]}}),
            json!({"type": "user", "message": {"content": "Base directory for this skill: /x/not-meta"}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "side-skill"}}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "acme:acme-writer"}}]}}),
        ];
        let dir = std::env::temp_dir().join(format!("dotonce-skills-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let rec = dir.join("record.jsonl");
        let mut text: Vec<String> = lines.iter().map(Value::to_string).collect();
        text.insert(2, "not json \"Skill\"".to_string());
        let whole = text.join("\n") + "\n";
        fs::write(&rec, &whole).unwrap();
        let (names, end) = from_record(&rec, 0);
        assert_eq!(
            names,
            vec!["acme:acme-writer", "writing-notes", "forum-replies"]
        );
        assert_eq!(end, whole.len() as u64);
        assert_eq!(from_record(&dir.join("missing.jsonl"), 7), (Vec::new(), 7));
        let none: Vec<String> = Vec::new();
        let seen = Seen::new(&none, Some((&rec, 0)), &dir);
        assert!(seen.any(&["acme-writer".to_string()]));
        assert!(seen.any(&[]));
        assert!(!seen.any(&["side-skill".to_string()]));
        assert_eq!(seen.read_from_record().unwrap().0.len(), 3);

        // The next read starts where this one ended, and a line still being
        // written waits for the one after.
        let more = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "pr-descriptions"}}]}}).to_string();
        fs::write(&rec, format!("{whole}{more}")).unwrap();
        assert_eq!(from_record(&rec, end), (Vec::new(), end));
        fs::write(&rec, format!("{whole}{more}\n")).unwrap();
        let (names, next) = from_record(&rec, end);
        assert_eq!(names, vec!["pr-descriptions"]);
        assert!(next > end);
        // A record shorter than where the last read ended was replaced.
        fs::write(&rec, format!("{more}\n")).unwrap();
        assert_eq!(from_record(&rec, next).0, vec!["pr-descriptions"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_recorded_skill_needs_no_read() {
        let rec = vec!["writing-notes".to_string()];
        let seen = Seen::new(&rec, Some((Path::new("/missing.jsonl"), 0)), Path::new("/"));
        assert!(seen.any(&[]));
        assert!(seen.read_from_record().is_none());
        assert_eq!(
            seen.choices(&["acme-writer".to_string()]),
            vec!["acme-writer"]
        );
    }
}
