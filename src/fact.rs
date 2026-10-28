//! A fact is one markdown file: frontmatter between `---` lines, then a body.
//! The frontmatter reader handles a common subset of YAML:
//! `key: value`, inline lists `[a, b]`, and block lists of `- item` lines.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fact {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: Vec<String>,
    pub tags: Vec<String>,
    pub learned: Option<String>,
    pub updated: Option<String>,
    pub private: bool,
    pub pinned: bool,
    pub replaced_by: Option<String>,
}

enum Value {
    One(String),
    Many(Vec<String>),
}

/// A trailing comment is `#` after whitespace (or a value that is only a comment).
fn is_comment(rest: &str) -> bool {
    let r = rest.trim_start();
    r.is_empty() || (r.len() < rest.len() && r.starts_with('#'))
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('"') {
        let mut out = String::new();
        let mut chars = t.char_indices().skip(1).peekable();
        while let Some((i, c)) = chars.next() {
            match (c, chars.peek()) {
                ('\\', Some((_, '"' | '\\'))) => out.push(chars.next().map_or(c, |(_, n)| n)),
                ('"', _) => {
                    if is_comment(&t[i + 1..]) {
                        return out;
                    }
                    break;
                }
                _ => out.push(c),
            }
        }
    } else if let Some(inner) = t.strip_prefix('\'') {
        if let Some(end) = inner.find('\'') {
            if is_comment(&inner[end + 1..]) {
                return inner[..end].to_string();
            }
        }
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        return t[1..t.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].to_string();
    }
    strip_comment(t).to_string()
}

/// Drops a trailing ` #` comment from an unquoted value.
fn strip_comment(t: &str) -> &str {
    if t.starts_with('#') {
        return "";
    }
    let mut prev = None;
    for (i, c) in t.char_indices() {
        if c == '#' && prev.is_some_and(char::is_whitespace) {
            return t[..i].trim_end();
        }
        prev = Some(c);
    }
    t
}

/// Skips a byte order mark and any leading blank lines.
fn start_of_content(src: &str) -> &str {
    let s = src.strip_prefix('\u{feff}').unwrap_or(src);
    let lead = s.len() - s.trim_start().len();
    let cut = s[..lead].rfind('\n').map_or(0, |i| i + 1);
    &s[cut..]
}

/// Splits the leading frontmatter from the body. The closing marker is a line
/// that is exactly `---`. Returns `None` when the file does not open with a
/// frontmatter block or never closes it.
fn find_front(src: &str) -> Option<(&str, &str)> {
    let src = start_of_content(src);
    let rest = src
        .strip_prefix("---\n")
        .or_else(|| src.strip_prefix("---\r\n"))?;
    let mut pos = 0;
    while pos <= rest.len() {
        let line_end = rest[pos..].find('\n').map(|i| pos + i);
        let next = line_end.map_or(rest.len(), |i| i + 1);
        let line = &rest[pos..line_end.unwrap_or(rest.len())];
        if line.strip_suffix('\r').unwrap_or(line) == "---" {
            return Some((&rest[..pos], &rest[next..]));
        }
        if line_end.is_none() {
            break;
        }
        pos = next;
    }
    None
}

/// True when the file opens a frontmatter block (starts with `---`) but never
/// closes it. Such a file can hide a `privacy: private` line, so callers
/// should skip it rather than treat it as public.
pub fn has_broken_front(src: &str) -> bool {
    start_of_content(src).starts_with("---") && find_front(src).is_none()
}

fn is_truthy(v: &str) -> bool {
    v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
}

/// `privacy: public`, `privacy: work` or an empty `privacy:`.
fn plainly_public(v: &str) -> bool {
    v.is_empty() || v.eq_ignore_ascii_case("public") || v.eq_ignore_ascii_case("work")
}

/// `private: false`, `no`, `off` or `0`.
fn plainly_false(v: &str) -> bool {
    ["false", "no", "off", "0"]
        .iter()
        .any(|f| v.eq_ignore_ascii_case(f))
}

/// Returns the keys (unquoted, lowercased, last one wins), the body, and
/// whether the fact is private. Privacy fails closed: any `privacy` or
/// `private` key whose value is not plainly public or plainly false makes the
/// fact private, and so does a block scalar or a value that runs onto more
/// lines, since this reader cannot be sure what YAML would make of them.
fn split_front(src: &str) -> (BTreeMap<String, Value>, String, bool) {
    let mut map = BTreeMap::new();
    let Some((front, body)) = find_front(src) else {
        return (map, src.to_string(), false);
    };
    let body = body.to_string();
    let mut entries: Vec<(String, Value)> = Vec::new();
    let mut in_list = false;
    let mut private = false;
    for line in front.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let (Some(item), true) = (t.strip_prefix("- "), in_list) {
            if let Some((_, Value::Many(v))) = entries.last_mut() {
                v.push(unquote(item));
            }
            continue;
        }
        let under_privacy = entries
            .last()
            .is_some_and(|(k, _)| k == "privacy" || k == "private");
        if under_privacy && (t.len() < line.len() || !line.contains(':')) {
            private = true;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = unquote(k).to_lowercase();
        let v = v.trim();
        if v.is_empty() || v.starts_with('#') {
            entries.push((k, Value::Many(Vec::new())));
            in_list = true;
        } else if v.starts_with('[') && v.rfind(']').is_some_and(|e| is_comment(&v[e + 1..])) {
            let end = v.rfind(']').unwrap_or(v.len() - 1);
            let items = v[1..end]
                .split(',')
                .map(unquote)
                .filter(|s| !s.is_empty())
                .collect();
            entries.push((k, Value::Many(items)));
            in_list = false;
        } else {
            entries.push((k, Value::One(unquote(v))));
            in_list = false;
        }
    }
    for (k, v) in entries {
        let values: Vec<&str> = match &v {
            Value::One(s) => vec![s.as_str()],
            Value::Many(items) => items.iter().map(String::as_str).collect(),
        };
        match k.as_str() {
            "privacy" => private |= !values.iter().all(|s| plainly_public(s)),
            "private" => private |= values.is_empty() || !values.iter().all(|s| plainly_false(s)),
            _ => {}
        }
        map.insert(k, v);
    }
    (map, body, private)
}

pub fn parse(src: &str) -> Fact {
    let (m, body, private) = split_front(src);
    let one = |k: &str| match m.get(k) {
        Some(Value::One(s)) => Some(s.clone()),
        Some(Value::Many(v)) => v.first().cloned(),
        None => None,
    };
    let many = |k: &str| match m.get(k) {
        Some(Value::Many(v)) => v.clone(),
        Some(Value::One(s)) => vec![s.clone()],
        None => Vec::new(),
    };
    let truthy = |k: &str| one(k).is_some_and(|v| is_truthy(&v));
    Fact {
        name: one("name").unwrap_or_default(),
        description: one("description").unwrap_or_default(),
        body: body.trim().to_string(),
        source: many("sources").into_iter().chain(many("source")).collect(),
        tags: many("tags"),
        learned: one("learned"),
        updated: one("updated"),
        private,
        pinned: truthy("pinned"),
        replaced_by: one("replaced_by"),
    }
}

/// Makes a value safe for one frontmatter line: no newlines, and double-quoted
/// when a plain scalar would be misread on the way back in.
fn scalar(v: &str) -> String {
    let v = v.replace(['\n', '\r'], " ");
    let risky = v.is_empty()
        || v.starts_with(['[', '"', '\'', '-'])
        || v.contains(": ")
        || v.contains('#')
        || v != v.trim();
    if risky {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v
    }
}

pub fn to_markdown(f: &Fact) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("name: {}\n", scalar(&f.name)));
    s.push_str(&format!("description: {}\n", scalar(&f.description)));
    if !f.source.is_empty() {
        s.push_str("sources:\n");
        for x in &f.source {
            s.push_str(&format!("  - {}\n", scalar(x)));
        }
    }
    if !f.tags.is_empty() {
        s.push_str("tags:\n");
        for t in &f.tags {
            s.push_str(&format!("  - {}\n", scalar(t)));
        }
    }
    if let Some(l) = &f.learned {
        s.push_str(&format!("learned: {}\n", scalar(l)));
    }
    if let Some(u) = &f.updated {
        s.push_str(&format!("updated: {}\n", scalar(u)));
    }
    if f.private {
        s.push_str("privacy: private\n");
    }
    if f.pinned {
        s.push_str("pinned: true\n");
    }
    if let Some(r) = &f.replaced_by {
        s.push_str(&format!("replaced_by: {}\n", scalar(r)));
    }
    s.push_str("---\n");
    if !f.body.is_empty() {
        s.push_str(&f.body);
        s.push('\n');
    }
    s
}

pub fn slug(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(8)
        .map(str::to_lowercase)
        .collect();
    let s: String = words.join("-").chars().take(60).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "note".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB_ENTRY: &str = "---
name: run-linter-first
description: Sam's run-the-linter rule applies to hotfixes too: never skip it
type: feedback
privacy: private
sources:
  - Claude Code memory, project acme
  - \"Sam, 2026-09-02\"
learned: 2026-08-11
updated: 2026-09-02
tags: [claude-memory, writing]
---
On August 11, 2026, Sam flagged a lint skip.

Second paragraph.
";

    #[test]
    fn parses_a_typical_entry() {
        let f = parse(KB_ENTRY);
        assert_eq!(f.name, "run-linter-first");
        assert_eq!(
            f.description,
            "Sam's run-the-linter rule applies to hotfixes too: never skip it"
        );
        assert_eq!(
            f.source,
            vec!["Claude Code memory, project acme", "Sam, 2026-09-02"]
        );
        assert_eq!(f.tags, vec!["claude-memory", "writing"]);
        assert_eq!(f.learned.as_deref(), Some("2026-08-11"));
        assert_eq!(f.updated.as_deref(), Some("2026-09-02"));
        assert!(f.private);
        assert!(!f.pinned);
        assert_eq!(
            f.body,
            "On August 11, 2026, Sam flagged a lint skip.\n\nSecond paragraph."
        );
    }

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let f = parse("Just a note.\n");
        assert_eq!(f.name, "");
        assert_eq!(f.body, "Just a note.");
    }

    #[test]
    fn writing_then_parsing_gives_the_same_fact() {
        let f = Fact {
            name: "staging-site".into(),
            description: "The staging site is staging.example.com".into(),
            body: "Use it for client demos.".into(),
            source: vec!["Sam, 2026-10-27".into()],
            tags: vec!["setup".into()],
            learned: Some("2026-10-27".into()),
            updated: Some("2026-10-27".into()),
            private: true,
            pinned: true,
            replaced_by: Some("staging-site-2".into()),
        };
        assert_eq!(parse(&to_markdown(&f)), f);
    }

    #[test]
    fn slug_is_short_lowercase_words() {
        assert_eq!(
            slug("The staging site is example.com, use it!"),
            "the-staging-site-is-example-com-use-it"
        );
        assert_eq!(slug("!!!"), "note");
    }

    #[test]
    fn closing_marker_must_be_exactly_three_dashes() {
        let f = parse("---\nprivacy: private\n----\nmore: x\n---x\nname: a\n---\nbody\n");
        assert!(f.private);
        assert_eq!(f.name, "a");
        assert_eq!(f.body, "body");
    }

    #[test]
    fn closing_marker_accepts_crlf_and_end_of_input() {
        let f = parse("---\r\nname: a\r\nprivacy: private\r\n---\r\nbody\r\n");
        assert_eq!(f.name, "a");
        assert!(f.private);
        assert_eq!(f.body, "body");

        let f = parse("---\nname: a\n---");
        assert_eq!(f.name, "a");
        assert_eq!(f.body, "");
    }

    #[test]
    fn empty_frontmatter_parses() {
        let f = parse("---\n---\nJust a body.\n");
        assert_eq!(f.name, "");
        assert_eq!(f.body, "Just a body.");
        assert!(!has_broken_front("---\n---\nJust a body.\n"));
    }

    #[test]
    fn broken_front_means_opened_but_never_closed() {
        assert!(has_broken_front("---\nname: a\nprivacy: private\n"));
        assert!(has_broken_front("---\nname: a\n----\nbody\n"));
        assert!(has_broken_front("---\nname: a\n---x\nbody\n"));
        assert!(!has_broken_front("---\nname: a\n---\nbody\n"));
        assert!(!has_broken_front("---\nname: a\n---"));
        assert!(!has_broken_front("Just a note.\n"));
        assert!(!has_broken_front(""));
    }

    #[test]
    fn privacy_and_booleans_ignore_case() {
        assert!(parse("---\nprivacy: Private\n---\n").private);
        assert!(parse("---\nprivacy: PRIVATE\n---\n").private);
        assert!(parse("---\nprivate: True\n---\n").private);
        assert!(parse("---\nprivate: YES\n---\n").private);
        assert!(parse("---\npinned: YES\n---\n").pinned);
        assert!(parse("---\npinned: TRUE\n---\n").pinned);
        assert!(!parse("---\nprivacy: public\n---\n").private);
        assert!(!parse("---\npinned: no\n---\n").pinned);
    }

    #[test]
    fn awkward_text_round_trips() {
        let f = Fact {
            name: "- starts with a dash".into(),
            description: "[draft] fix bug [urgent]".into(),
            body: "Body.".into(),
            source: vec![
                "\"quoted\" source".into(),
                "back\\slash: and colon".into(),
                "  padded  ".into(),
                "it's #1".into(),
            ],
            tags: vec!["a, b".into(), "[x]".into(), "plain".into()],
            learned: Some("'odd' date".into()),
            updated: Some("2026-10-27".into()),
            private: false,
            pinned: false,
            replaced_by: Some("- other".into()),
        };
        assert_eq!(parse(&to_markdown(&f)), f);
    }

    #[test]
    fn newlines_in_name_and_description_become_spaces() {
        let f = Fact {
            name: "two\nlines".into(),
            description: "first\nsecond".into(),
            source: vec!["x\nprivacy: public".into()],
            private: true,
            ..Fact::default()
        };
        let back = parse(&to_markdown(&f));
        assert_eq!(back.name, "two lines");
        assert_eq!(back.description, "first second");
        assert_eq!(back.source, vec!["x privacy: public"]);
        assert!(back.private);
    }

    #[test]
    fn tags_are_written_as_a_block_list() {
        let f = Fact {
            tags: vec!["a".into(), "b".into()],
            ..Fact::default()
        };
        assert!(to_markdown(&f).contains("tags:\n  - a\n  - b\n"));
        assert_eq!(parse("---\ntags: [a, b]\n---\n").tags, vec!["a", "b"]);
    }

    #[test]
    fn frontmatter_after_bom_or_blank_lines_is_recognised() {
        for src in [
            "\u{feff}---\nprivacy: private\nname: a\n---\nbody\n",
            "\n\n---\nprivacy: private\nname: a\n---\nbody\n",
            "\u{feff}\n  \n---\r\nprivacy: private\r\nname: a\r\n---\r\nbody\r\n",
        ] {
            let f = parse(src);
            assert!(f.private, "{src:?}");
            assert_eq!(f.name, "a");
            assert_eq!(f.body.trim(), "body");
            assert!(find_front(src).is_some());
            assert!(!has_broken_front(src));
        }
        assert!(has_broken_front("\u{feff}---\nprivacy: private\n"));
        assert!(has_broken_front("\n\n---\nprivacy: private\n"));
        assert!(!has_broken_front("\n\nJust a note.\n"));
    }

    #[test]
    fn trailing_comment_is_stripped_from_unquoted_values() {
        assert!(parse("---\nprivacy: private # note\n---\n").private);
        assert!(parse("---\nprivacy: private\t# note\n---\n").private);
        assert!(parse("---\npinned: true # keep\n---\n").pinned);
        assert_eq!(parse("---\nname: abc # n\n---\n").name, "abc");
        assert_eq!(parse("---\nname: a#b\n---\n").name, "a#b");
        assert_eq!(parse("---\ntags: [a, b] # t\n---\n").tags, vec!["a", "b"]);
        assert_eq!(
            parse("---\ntags:\n  - a # x\n  - b\n---\n").tags,
            vec!["a", "b"]
        );
    }

    #[test]
    fn quoted_values_keep_a_hash_and_allow_a_trailing_comment() {
        assert_eq!(parse("---\nname: \"a # b\"\n---\n").name, "a # b");
        assert_eq!(parse("---\nname: 'a # b'\n---\n").name, "a # b");
        assert_eq!(parse("---\nname: \"a # b\" # c\n---\n").name, "a # b");
        assert!(parse("---\nprivacy: \"private\" # note\n---\n").private);
        assert!(parse("---\nprivacy: 'private' # note\n---\n").private);
    }

    #[test]
    fn keys_match_case_insensitively() {
        assert!(parse("---\nPrivacy: private\n---\n").private);
        assert!(parse("---\nPRIVATE: true\n---\n").private);
        assert!(parse("---\nPinned: yes\n---\n").pinned);
        assert_eq!(parse("---\nName: x\n---\n").name, "x");
    }

    #[test]
    fn privacy_fails_closed_on_duplicate_keys() {
        assert!(parse("---\nprivacy: private\nprivacy: public\n---\n").private);
        assert!(parse("---\nprivacy: public\nprivacy: private\n---\n").private);
        assert!(parse("---\nprivate: true\nprivate: false\n---\n").private);
        assert!(parse("---\nprivate: false\nprivate: true\n---\n").private);
        assert!(parse("---\nprivacy: public\nprivate: true\n---\n").private);
        assert!(parse("---\nprivate: false\nPrivacy: private\n---\n").private);
        assert!(parse("---\nprivacy:\n  - public\n  - private\n---\n").private);
        assert!(!parse("---\nprivacy: public\nprivate: false\n---\n").private);
    }

    #[test]
    fn privacy_value_on_the_next_indented_line_is_private() {
        assert!(parse("---\nname: a\nprivacy:\n  private\n---\n").private);
    }

    #[test]
    fn privacy_block_scalar_is_private() {
        assert!(parse("---\nname: a\nprivacy: >\n  private\n---\n").private);
        assert!(parse("---\nname: a\nprivacy: |\n  private\n---\n").private);
    }

    #[test]
    fn double_quoted_privacy_key_is_read() {
        assert!(parse("---\n\"privacy\": private\n---\n").private);
    }

    #[test]
    fn single_quoted_privacy_key_is_read() {
        assert!(parse("---\n'privacy': private\n---\n").private);
    }

    #[test]
    fn tagged_privacy_value_is_private() {
        assert!(parse("---\nprivacy: !!str private\n---\n").private);
    }

    #[test]
    fn private_on_is_private() {
        assert!(parse("---\nprivate: on\n---\n").private);
    }

    #[test]
    fn private_one_is_private() {
        assert!(parse("---\nprivate: 1\n---\n").private);
    }

    #[test]
    fn privacy_public_stays_public() {
        assert!(!parse("---\nname: a\nprivacy: public\n---\n").private);
    }

    #[test]
    fn privacy_work_stays_public() {
        assert!(!parse("---\nname: a\nprivacy: work\n---\n").private);
    }

    #[test]
    fn private_false_stays_public() {
        assert!(!parse("---\nname: a\nprivate: false\n---\n").private);
    }
}
