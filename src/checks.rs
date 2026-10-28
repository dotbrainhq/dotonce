//! Reply checks: finding drafts by CommonMark fence rules, and running each
//! check kind on plain text. No model, no network.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::rules::{Check, CheckKind, On, RuleSet};

/// A failing line is cut to this many characters in messages.
pub const LINE_CHARS: usize = 120;

static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!?\[[^\]]*\]\([^)]*\)").expect("link pattern"));
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bhttps?://\S").expect("url pattern"));
static REF_DEF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}\[[^\]]+\]:\s").expect("reference pattern"));

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub set: String,
    pub check: String,
    pub line: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub info: String,
    pub body: String,
}

struct Open<'a> {
    ch: char,
    len: usize,
    info: String,
    body: Vec<&'a str>,
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// An opening fence: up to three spaces, then three or more backticks or
/// tildes, then the info string. A backtick fence's info can't hold a backtick.
pub fn fence_open(line: &str) -> Option<(char, usize, String)> {
    if indent(line) > 3 {
        return None;
    }
    let rest = &line[indent(line)..];
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = rest[len..].trim();
    if ch == '`' && info.contains('`') {
        return None;
    }
    let word = info.split_whitespace().next().unwrap_or("").to_lowercase();
    Some((ch, len, word))
}

/// A closing fence: the same character, at least as long as the opening one,
/// and nothing after it but spaces.
pub fn fence_closes(line: &str, ch: char, len: usize) -> bool {
    if indent(line) > 3 {
        return false;
    }
    let rest = &line[indent(line)..];
    let n = rest.chars().take_while(|&c| c == ch).count();
    n >= len && rest[n..].trim().is_empty()
}

/// Top-level fenced blocks. A block closes on a fence of the same character
/// at least as long as the one that opened it, so a longer fence can hold
/// shorter ones. An unclosed block runs to the end of the text.
pub fn fenced_blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut open: Option<Open> = None;
    for line in text.lines() {
        let closes = open.as_ref().map(|o| fence_closes(line, o.ch, o.len));
        match closes {
            Some(false) => {
                if let Some(o) = open.as_mut() {
                    o.body.push(line);
                }
            }
            Some(true) => {
                if let Some(o) = open.take() {
                    out.push(Block {
                        info: o.info,
                        body: o.body.join("\n"),
                    });
                }
            }
            None => {
                if let Some((ch, len, info)) = fence_open(line) {
                    open = Some(Open {
                        ch,
                        len,
                        info,
                        body: Vec::new(),
                    });
                }
            }
        }
    }
    if let Some(o) = open {
        out.push(Block {
            info: o.info,
            body: o.body.join("\n"),
        });
    }
    out
}

/// The texts a set's reply checks read: the bodies of fenced blocks whose
/// info string is in the set's `[draft] fence` list, or the whole text when
/// the set has no `[draft]` table.
pub fn drafts(set: &RuleSet, text: &str) -> Vec<String> {
    match &set.draft {
        None if text.trim().is_empty() => Vec::new(),
        None => vec![text.to_string()],
        Some(d) => fenced_blocks(text)
            .into_iter()
            .filter(|b| d.fence.contains(&b.info))
            .map(|b| b.body)
            .collect(),
    }
}

/// The line trimmed, and cut to `LINE_CHARS` characters with "..." after.
pub fn cut(line: &str) -> String {
    let t = line.trim();
    if t.chars().count() <= LINE_CHARS {
        return t.to_string();
    }
    let mut s: String = t.chars().take(LINE_CHARS).collect();
    s.push_str("...");
    s
}

/// The whole line that holds byte `at` of `text`.
fn line_at(text: &str, at: usize) -> &str {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    text[start..end].trim_end_matches('\r')
}

/// Where `part`, a slice of `text`, starts in it.
fn offset_in(text: &str, part: &str) -> usize {
    part.as_ptr() as usize - text.as_ptr() as usize
}

fn norm(s: &str) -> String {
    s.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'")
}

/// A phrase that starts or ends with a letter or digit must not touch
/// another letter or digit there.
fn edge(outside: Option<char>, inside: Option<char>) -> bool {
    match (outside, inside) {
        (Some(o), Some(i)) if i.is_alphanumeric() => !o.is_alphanumeric(),
        _ => true,
    }
}

/// How often `phrase` appears in `line`, case-insensitive, at word boundaries.
fn count_phrase(line: &str, phrase: &str) -> usize {
    let (l, p) = (norm(line), norm(phrase));
    let (first, last) = (p.chars().next(), p.chars().next_back());
    l.match_indices(p.as_str())
        .filter(|&(i, m)| {
            edge(l[..i].chars().next_back(), first) && edge(l[i + m.len()..].chars().next(), last)
        })
        .count()
}

fn find_phrase<'a>(text: &'a str, phrase: &str) -> Option<&'a str> {
    text.lines().find(|l| count_phrase(l, phrase) > 0)
}

/// The line without its inline code spans: a run of backticks up to the
/// next run of the same length.
fn strip_code(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::new();
    let (mut i, mut last) = (0, 0);
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let n = b[i..].iter().take_while(|&&c| c == b'`').count();
        let mut j = i + n;
        let mut close = None;
        while j < b.len() {
            if b[j] == b'`' {
                let m = b[j..].iter().take_while(|&&c| c == b'`').count();
                if m == n {
                    close = Some(j + m);
                    break;
                }
                j += m;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out.push_str(&line[last..i]);
                last = end;
                i = end;
            }
            None => i += n,
        }
    }
    out.push_str(&line[last..]);
    out
}

fn bare_url(text: &str) -> Option<&str> {
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        if let Some((ch, len)) = fence {
            if fence_closes(line, ch, len) {
                fence = None;
            }
            continue;
        }
        if let Some((ch, len, _)) = fence_open(line) {
            fence = Some((ch, len));
            continue;
        }
        if REF_DEF.is_match(line) {
            continue;
        }
        let visible = LINK.replace_all(&strip_code(line), "").into_owned();
        if URL.is_match(&visible) {
            return Some(line);
        }
    }
    None
}

fn repeat<'a>(text: &'a str, re: &Regex, max: usize) -> Option<&'a str> {
    let caps = re.captures(text)?;
    let name = caps.get(1)?.as_str().trim();
    if name.is_empty() {
        return None;
    }
    let rest = &text[caps.get(0)?.end()..];
    let mut seen = 0;
    for line in rest.lines() {
        seen += count_phrase(line, name);
        if seen > max {
            return Some(line_at(text, offset_in(text, line)));
        }
    }
    None
}

/// The first line of each paragraph. Blank lines inside a fenced block don't
/// split it.
fn paragraphs(text: &str) -> Vec<&str> {
    let mut firsts = Vec::new();
    let mut in_par = false;
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        if let Some((ch, len)) = fence {
            if fence_closes(line, ch, len) {
                fence = None;
            }
            continue;
        }
        if line.trim().is_empty() {
            in_par = false;
            continue;
        }
        if !in_par {
            firsts.push(line);
            in_par = true;
        }
        if let Some((ch, len, _)) = fence_open(line) {
            fence = Some((ch, len));
        }
    }
    firsts
}

fn over_words(text: &str, max: usize) -> Option<&str> {
    let mut n = 0;
    for line in text.lines() {
        n += line
            .split_whitespace()
            .filter(|w| w.chars().any(char::is_alphanumeric))
            .count();
        if n > max {
            return Some(line);
        }
    }
    None
}

/// The line that breaks the check, cut for messages, or None when the text
/// passes.
pub fn failing_line(check: &Check, text: &str) -> Option<String> {
    match &check.kind {
        CheckKind::Forbid(list) => list.iter().find_map(|p| find_phrase(text, p)).map(cut),
        CheckKind::ForbidRegex(re) => re.find(text).map(|m| cut(line_at(text, m.start()))),
        CheckKind::Require(list) => (!list.iter().any(|p| find_phrase(text, p).is_some()))
            .then(|| format!("(missing: {})", list.join(" / "))),
        CheckKind::BareUrl => bare_url(text).map(cut),
        CheckKind::RepeatOf { pattern, max } => repeat(text, pattern, *max).map(cut),
        CheckKind::MaxParagraphs(max) => paragraphs(text).get(*max).copied().map(cut),
        CheckKind::MaxWords(max) => over_words(text, *max).map(cut),
    }
}

/// Runs the set's reply checks on each of its drafts in `text`. Each check
/// fails at most once, with the first line that breaks it.
pub fn run_reply(set: &RuleSet, text: &str) -> Vec<Failure> {
    let drafts = drafts(set, text);
    set.checks
        .iter()
        .filter(|c| c.on == On::Reply)
        .filter_map(|c| {
            drafts
                .iter()
                .find_map(|d| failing_line(c, d))
                .map(|line| Failure {
                    set: set.id.clone(),
                    check: c.id.clone(),
                    line,
                    why: c.why.clone(),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse;
    use std::path::Path;

    fn set(body: &str) -> RuleSet {
        parse(&format!("id = \"t\"\n{body}"), Path::new("/r/t.toml")).unwrap()
    }

    fn fails(kind: &str, text: &str) -> Option<String> {
        let s = set(&format!("[[check]]\nid = \"c\"\nwhy = \"w\"\n{kind}\n"));
        failing_line(&s.checks[0], text)
    }

    #[test]
    fn fence_lines_follow_commonmark() {
        assert_eq!(
            fence_open("```markdown"),
            Some(('`', 3, "markdown".to_string()))
        );
        assert_eq!(
            fence_open("~~~~ Draft for Sam"),
            Some(('~', 4, "draft".to_string()))
        );
        assert_eq!(fence_open("   ```"), Some(('`', 3, String::new())));
        assert_eq!(fence_open("    ```"), None);
        assert_eq!(fence_open("``"), None);
        assert_eq!(fence_open("```a`b"), None);
        assert!(fence_closes("```", '`', 3));
        assert!(fence_closes("`````  ", '`', 3));
        assert!(!fence_closes("```", '`', 4));
        assert!(!fence_closes("~~~", '`', 3));
        assert!(!fence_closes("```php", '`', 3));
    }

    #[test]
    fn a_longer_fence_holds_a_shorter_one() {
        let text = "intro\n````markdown\nHi Sam,\n```php\necho 1;\n```\nThanks,\n````\nafter";
        let b = fenced_blocks(text);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].info, "markdown");
        assert_eq!(b[0].body, "Hi Sam,\n```php\necho 1;\n```\nThanks,");
    }

    #[test]
    fn tildes_hold_backticks_and_info_lines_never_close() {
        let b = fenced_blocks("~~~draft\nA\n```\nB\n~~~");
        assert_eq!(b[0].body, "A\n```\nB");
        let b = fenced_blocks("```markdown\nA\n```php\nB\n```\nC");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].body, "A\n```php\nB");
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end() {
        let b = fenced_blocks("````markdown\nA\n```\nB");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].body, "A\n```\nB");
    }

    #[test]
    fn drafts_are_the_blocks_a_set_names_or_the_whole_reply() {
        let text = "Here it is:\n```Markdown\nHi Sam,\n```\nAnd:\n```bash\nls\n```";
        let with = set("[draft]\nfence = [\"markdown\"]\n");
        assert_eq!(drafts(&with, text), vec!["Hi Sam,".to_string()]);
        let whole = set("");
        assert_eq!(drafts(&whole, text), vec![text.to_string()]);
        assert!(drafts(&whole, "  \n").is_empty());
        assert!(drafts(&with, "no fences here").is_empty());
        let blank = set("[draft]\nfence = [\"\"]\n");
        assert_eq!(
            drafts(&blank, "a\n```\nHi Sam,\n```\n```bash\nls\n```"),
            vec!["Hi Sam,".to_string()]
        );
    }

    #[test]
    fn forbid_matches_phrases_at_word_boundaries() {
        let k = "forbid = [\"sorry for the wait\", \"don't\"]";
        assert_eq!(
            fails(k, "Hi Sam,\nSorry for the wait, Sam.").as_deref(),
            Some("Sorry for the wait, Sam.")
        );
        assert_eq!(fails(k, "So sorry for the waiting list."), None);
        assert_eq!(
            fails(k, "Don\u{2019}t worry.").as_deref(),
            Some("Don\u{2019}t worry.")
        );
        assert_eq!(fails(k, "All good."), None);
    }

    #[test]
    fn forbid_regex_reports_the_line_of_the_match() {
        let k = "forbid_regex = '\\bTODO\\b'";
        assert_eq!(
            fails(k, "a\nfix TODO here\nb").as_deref(),
            Some("fix TODO here")
        );
        assert_eq!(fails(k, "TODOS are fine"), None);
        let k = "forbid_regex = '^We '";
        assert_eq!(
            fails(k, "Hi.\nWe looked at it.").as_deref(),
            Some("We looked at it.")
        );
    }

    #[test]
    fn require_fails_when_no_phrase_appears() {
        let k = "require = [\"Thanks,\", \"Best,\"]";
        assert_eq!(fails(k, "Hi\nThanks, Sam"), None);
        assert_eq!(
            fails(k, "Hi").as_deref(),
            Some("(missing: Thanks, / Best,)")
        );
    }

    #[test]
    fn bare_urls_outside_links_and_code_fail() {
        let k = "kind = \"bare-url\"";
        assert_eq!(
            fails(k, "See https://example.com/docs today").as_deref(),
            Some("See https://example.com/docs today")
        );
        assert_eq!(
            fails(k, "See <https://example.com>").as_deref(),
            Some("See <https://example.com>")
        );
        assert_eq!(fails(k, "See [the docs](https://example.com/docs)."), None);
        assert_eq!(fails(k, "![logo](https://example.com/a.png)"), None);
        assert_eq!(fails(k, "Run `curl https://example.com` now"), None);
        assert_eq!(fails(k, "Run ``a `b` https://example.com`` now"), None);
        assert_eq!(fails(k, "Code:\n```\nhttps://example.com\n```\nDone"), None);
        assert_eq!(fails(k, "[docs]: https://example.com"), None);
        assert_eq!(fails(k, "No links."), None);
    }

    #[test]
    fn repeat_of_counts_the_captured_text_after_its_first_match() {
        let k = "repeat_of = '^Hi (\\w+),'\nmax = 0";
        assert_eq!(
            fails(k, "Hi Sam,\nThanks, Sam.").as_deref(),
            Some("Thanks, Sam.")
        );
        assert_eq!(fails(k, "Hi Sam,\nThanks."), None);
        assert_eq!(fails(k, "Hi Sam,\nSamuel helped."), None);
        assert_eq!(fails(k, "No greeting, Sam."), None);
        assert_eq!(
            fails(k, "Hi Sam, good to hear, sam.").as_deref(),
            Some("Hi Sam, good to hear, sam.")
        );
        let k = "repeat_of = '^Hi (\\w+),'\nmax = 1";
        assert_eq!(
            fails(k, "Hi Sam,\nSam, one.\nSam, two.").as_deref(),
            Some("Sam, two.")
        );
    }

    #[test]
    fn paragraphs_split_on_blank_lines_outside_fences() {
        let k = "max_paragraphs = 2";
        assert_eq!(fails(k, "a\n\nb\n\nc").as_deref(), Some("c"));
        assert_eq!(fails(k, "a\nstill a\n\nb"), None);
        let k = "max_paragraphs = 3";
        assert_eq!(fails(k, "a\n\n```\nx\n\ny\n```\n\nb"), None);
    }

    #[test]
    fn words_count_tokens_with_a_letter_or_digit() {
        let k = "max_words = 3";
        assert_eq!(
            fails(k, "one two\nthree four").as_deref(),
            Some("three four")
        );
        assert_eq!(fails(k, "one - two three"), None);
    }

    #[test]
    fn long_lines_are_cut_at_120_characters() {
        let long = "x".repeat(130);
        let c = cut(&format!("  {long}  "));
        assert_eq!(c.chars().count(), LINE_CHARS + 3);
        assert!(c.ends_with("..."));
        assert_eq!(cut("  short  "), "short");
    }

    #[test]
    fn run_reply_checks_only_drafts_and_only_reply_checks() {
        let s = set(
            "[draft]\nfence = [\"markdown\"]\n[[check]]\nid = \"stock\"\nforbid = [\"sorry for the wait\"]\nwhy = \"Open with a line from the thread.\"\n[[check]]\nid = \"push\"\non = \"tool\"\ntool = \"Bash\"\nforbid = [\"sorry\"]\nwhy = \"w\"\n",
        );
        assert!(
            run_reply(
                &s,
                "Sorry for the wait, I checked.\n```markdown\nHi Sam,\n```"
            )
            .is_empty()
        );
        let f = run_reply(&s, "Draft:\n```markdown\nHi Sam,\nSorry for the wait.\n```");
        assert_eq!(
            f,
            vec![Failure {
                set: "t".to_string(),
                check: "stock".to_string(),
                line: "Sorry for the wait.".to_string(),
                why: "Open with a line from the thread.".to_string(),
            }]
        );
    }
}
