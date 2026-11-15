//! Spots a typed prompt that may correct Claude's last reply, so the prompt
//! hook can ask Claude to turn the mistake into a rule set that waits for the
//! user's yes (see `chat::Wait::Ask`).
//!
//! Plain patterns on normalized text, no model: lowercase words joined by
//! single spaces, with typos mapped to a short list of words the patterns
//! use (swapped neighbors, one wrong, extra or missing letter, scrambled
//! letters). A word in the system dictionary is never treated as a typo.
//! A false flag costs one short note that Claude ignores, and a miss costs
//! one rule that isn't proposed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};
use serde_json::Value;

/// Words a typo can map to.
const LEXICON: &str = "
and the what that this with have from not now are was were they their there them then than think
fuck fucking fucked fuckin shit stupid bullshit crap idiot angry depressed depression annoyed upset
bogus slop robotic generic corporate inspired garbage
wrong incorrect correct inaccurate inaccuracy mistake mistakes false promise promised promises accurate
again already always every usual same still keep keeps times many next time
banned follow followed following apply applied applying using load loaded loading read break breaks breaking
rules rule skills skill voice writing corrections correction flagged strict
asked asking ask told said says tell who why what how where when which reason reasons
you your you're yourself didn't don't isn't doesn't aren't wasn't haven't shouldn't can't won't should
check checked checking verify verified test tested testing research analyse analyze analysis
assume assuming assumption assumptions imagination source truth know
command commands path link give send share
fixed fix fixing nothing working works work changed solved resolved done good
waste wasted wasting whole tokens token
remove removed added adding add mention mentioned include included need needed unnecessary dare
never ever please sounds sound human long short
draft drafts reply understand without instead before
email chat like just make will would could about because other
";

/// A word used as an instruction ahead of time, not a complaint about
/// what was written. Matches become a space before the patterns run.
const IGNORE: &[&str] = &[
    r"\b(not|no|less|avoid|without|never|zero|any) (an? )?(ai )?(slop|bogus|robotic|generic|corporate|garbage)\b",
    r"\bright (or|r) wrong\b",
];

/// Each pattern's label and regex, run on normalized text.
const PATTERNS: &[(&str, &str)] = &[
    (
        "swear",
        r"\b(fuck\w*|shit\w*|stupid|bullshit|crap|wtf|idiot\w*)\b",
    ),
    (
        "mood",
        r"\b(angry|depress\w*|frustr\w*|annoyed|upset|tired of)\b",
    ),
    (
        "slop",
        r"\b(ai slop|slop|bogus|robotic|generic|corporate|ai inspired|sounds? (like )?ai|not human\w*|garbage)\b",
    ),
    ("long", r"\b(too?|so|very|super) (long|lengthy|wordy)\b"),
    (
        "wrong",
        r"\b(wrong|incorrect|inaccura\w*|false promise\w*|is it accurate|mistakes?|not (good|correct|accurate|right|true|clear|meaningful\w*))\b",
    ),
    ("banned", r"\b(banned|strict rule)\b"),
    (
        "rule",
        r"\b(didn'?t|dint|don'?t|not|never|aren'?t|isn'?t|did you|are you|why|should|you should) (\w+ ){0,3}(follow\w*|apply\w*|using|use|load\w*|read)\b( \w+){0,4} (rules?|skills?|voice|corrections?)\b",
    ),
    ("breakrule", r"\bbreak\w* (\w+ ){0,3}(rules?|voice)\b"),
    (
        "overreach",
        r"\b(did i ask|do i ask|i (didn'?t|did not|never) ask\w*|who (asked|told|says|said|is we))\b",
    ),
    (
        "why",
        r"\bwhy (did |do |are |were |would |is |didn'?t |don'?t )?(you|you're|we|we're|it|this|that)\b",
    ),
    (
        "whynot",
        r"\bwhy not (check|use|ask|search|test|read|look|run|follow|load|verify)\w*\b",
    ),
    ("reason", r"\bany reasons?\b"),
    (
        "repeat",
        r"\b(how many times?|many times|every (time|session)|you always|as usual|same (mistakes?|thing|issue|problem) again|(you|we|it) (\w+ )?again|again (you|we|same|the same)|(you|you're|we|it|why|still) (are |is )?keeps? (on )?\w+ing|still (not|no|the same|same|broken|there|showing|wrong|missing|bogus|slop))\b",
    ),
    (
        "verify",
        r"\b(assum\w*|imagination|(didn'?t|did not|not) (you )?(check\w*|verif\w*|test\w*|research\w*|analy\w*)|did you (check|verify|test|read|analy\w*|look|confirm|run)|who says|how (do |did )?(you|we) know|how (did |do )?you decide|source of truth|research yourself|study yourself)\b",
    ),
    (
        "path",
        r"\b(with (the )?path|command with|give me (the )?commands?|(didn'?t|did not|not) (give|send|share)\b( \w+){0,8} (commands?|link))\b",
    ),
    (
        "notdone",
        r"\b(not (fixed|done|working|solved|resolved|changed)|nothing (changed|fixed)|didn'?t (fix|run|work|change)|doesn'?t work|you said (it'?s )?(done|fixed))\b",
    ),
    (
        "waste",
        r"\b(wast\w* (\w+ ){0,3}(time|tokens?|day|hours?|minutes?)|(time|tokens?|day|hours?|minutes?) (\w+ ){0,2}wast\w*|whole day)\b",
    ),
    (
        "ask",
        r"\b(why (are you |you )?ask\w* me|don'?t ask me|if i have to do it|why am i (using|paying)|your job|my (step|job)\?*)\b",
    ),
    (
        "dont",
        r"\b(don'?t|never) (ever )?(dare|add|use|say|share|mention|make|send|recommend|ask|write|put|change|do)\b",
    ),
    (
        "should",
        r"\b(you|we) (shouldn'?t|should not|should have|should've)\b",
    ),
    ("nexttime", r"\bnext time\b"),
    (
        "remove",
        r"\b(remove (this|that|it|the|these|those)|no need (to|for)|not needed|unnecessary)\b",
    ),
    (
        "stalled",
        r"\bwhere is (the|my) (draft|reply|command|pr|link)\b",
    ),
];

/// "I don't think" followed by a word, as a disagreement. Regex here has no
/// lookahead, so the words that make it a plain opinion are left out after
/// the match.
const DISAGREE: &str = r"\bi don'?t think (\w+)";
const NOT_DISAGREE: &[&str] = &["they", "there", "he", "she", "so"];

/// Blocks the app or a paste puts in a prompt: none of it is the user's
/// own words.
const BLOCK_TAGS: &[&str] = &[
    "system-reminder",
    "pasted_content",
    "tools-used-before-summary",
    "command-name",
    "command-message",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "ide_selection",
    "ide_opened_file",
    "bash-input",
    "bash-stdout",
    "bash-stderr",
    "task-notification",
    "side-sessions-event",
    "ci-monitor-event",
];

/// Where systems keep a word list: macOS, then Debian and Ubuntu with a
/// words package.
const WORD_LISTS: [&str; 3] = [
    "/usr/share/dict/words",
    "/usr/share/dict/american-english",
    "/usr/share/dict/british-english",
];

fn system_words() -> Option<String> {
    WORD_LISTS.iter().find_map(|p| fs::read_to_string(p).ok())
}

struct Lexicon {
    words: BTreeSet<&'static str>,
    /// Each word without apostrophes, so "shouldnt" can reach "shouldn't",
    /// by that length, in sorted order.
    by_len: BTreeMap<usize, Vec<(String, &'static str)>>,
}

static LEX: LazyLock<Lexicon> = LazyLock::new(|| {
    let words: BTreeSet<&'static str> = LEXICON.split_whitespace().collect();
    let mut by_len: BTreeMap<usize, Vec<(String, &'static str)>> = BTreeMap::new();
    for w in &words {
        let bare = w.replace('\'', "");
        by_len.entry(bare.len()).or_default().push((bare, w));
    }
    Lexicon { words, by_len }
});

/// Normalized text holds only `a-z`, apostrophes and spaces, so the
/// patterns skip Unicode classes, which keeps them small.
fn ascii(p: &str) -> Regex {
    RegexBuilder::new(p)
        .unicode(false)
        .build()
        .expect("correction pattern")
}

static IGNORE_RE: LazyLock<Vec<Regex>> =
    LazyLock::new(|| IGNORE.iter().map(|p| ascii(p)).collect());
static PATTERN_RE: LazyLock<Vec<(&'static str, Regex)>> =
    LazyLock::new(|| PATTERNS.iter().map(|(l, p)| (*l, ascii(p))).collect());
static DISAGREE_RE: LazyLock<Regex> = LazyLock::new(|| ascii(DISAGREE));
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z][a-z']*").expect("word"));
/// "youa re" and "yaou re" style splits.
static SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-z]{2,5}) re\b").expect("split"));
static BLOCK_RE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    BLOCK_TAGS
        .iter()
        // The app closes pastes with the id too: </pasted_content id="36e4">
        .map(|t| Regex::new(&format!(r"(?s)<{t}\b[^>]*>.*?</{t}\b[^>]*>")).expect("block"))
        .collect()
});
static ARGS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</?command-args>").expect("args"));
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?```").expect("fence"));

/// One insert, delete, wrong letter or swap of neighbors at most.
fn within_one(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > 1 {
        return false;
    }
    if la == lb {
        let diff: Vec<usize> = (0..la).filter(|&i| a[i] != b[i]).collect();
        return match diff.as_slice() {
            [_] => true,
            [i, j] => *j == i + 1 && a[*i] == b[*j] && a[*j] == b[*i],
            _ => false,
        };
    }
    let (short, long) = if la < lb { (a, b) } else { (b, a) };
    let (mut i, mut j) = (0, 0);
    let mut skipped = false;
    while i < short.len() && j < long.len() {
        if short[i] == long[j] {
            i += 1;
            j += 1;
        } else if skipped {
            return false;
        } else {
            skipped = true;
            j += 1;
        }
    }
    true
}

/// 0 same, 1 swapped neighbors, 2 one wrong letter, 3 one letter extra or
/// missing, None when further apart.
fn slip_cost(a: &[u8], b: &[u8]) -> Option<u8> {
    if a == b {
        return Some(0);
    }
    if !within_one(a, b) {
        return None;
    }
    if a.len() != b.len() {
        return Some(3);
    }
    let wrong = a.iter().zip(b).filter(|(x, y)| x != y).count();
    Some(if wrong == 1 { 2 } else { 1 })
}

fn sorted(s: &[u8]) -> Vec<u8> {
    let mut v = s.to_vec();
    v.sort_unstable();
    v
}

/// The lexicon word `tok` may be a typo of, before the dictionary has its
/// say. None for a lexicon word or one with no near word.
fn near_word(tok: &str) -> Option<&'static str> {
    let bare = tok.replace('\'', "");
    if LEX.words.contains(tok) || bare.len() < 3 {
        return None;
    }
    let b = bare.as_bytes();
    let mut best: Option<(u8, &'static str)> = None;
    for n in [b.len(), b.len() - 1, b.len() + 1] {
        for (cand, w) in LEX.by_len.get(&n).into_iter().flatten() {
            let Some(cost) = slip_cost(b, cand.as_bytes()) else {
                continue;
            };
            // Three-letter typos only count as swapped neighbors ("teh"),
            // never a new letter.
            if cand.len() < 3 || (b.len() == 3 && cost > 1) {
                continue;
            }
            if best.is_none_or(|(c, _)| cost < c) {
                best = Some((cost, w));
            }
        }
    }
    if let Some((_, w)) = best {
        return Some(w);
    }
    if b.len() < 5 {
        return None;
    }
    // Scrambled letters, like "udnerstand".
    let key = sorted(b);
    LEX.by_len
        .get(&b.len())
        .into_iter()
        .flatten()
        .find(|(cand, _)| cand.as_bytes()[0] == b[0] && sorted(cand.as_bytes()) == key)
        .map(|(_, w)| *w)
}

/// Drops from `near` every word the word list has, read once and only when
/// some word may be a typo. With no word list it drops them all: a real
/// word one letter off a lexicon word, like "ship" for "skip", would
/// otherwise read as a typo.
fn keep_typos(near: &mut HashMap<&str, &'static str>, words: impl FnOnce() -> Option<String>) {
    if near.is_empty() {
        return;
    }
    let Some(dict) = words() else {
        near.clear();
        return;
    };
    // One search for every word at once, as a whole line, is far quicker
    // than reading the file line by line. A word list writes a word in
    // lowercase, with a capital first or in capitals, so those are the
    // forms searched; a case-insensitive search is ten times slower.
    let mut alts = Vec::new();
    for t in near.keys() {
        let mut first = t.to_string();
        first[..1].make_ascii_uppercase();
        alts.extend([t.to_string(), first, t.to_ascii_uppercase()].map(|w| regex::escape(&w)));
    }
    let Ok(re) = Regex::new(&format!(r"(?m)^(?:{})\r?$", alts.join("|"))) else {
        return;
    };
    for m in re.find_iter(&dict) {
        near.remove(m.as_str().trim_end().to_ascii_lowercase().as_str());
        if near.is_empty() {
            return;
        }
    }
}

/// The text as the patterns read it: lowercase words joined by single
/// spaces, with typos mapped to lexicon words.
pub fn normalize(s: &str) -> String {
    normalize_with(s, system_words)
}

fn normalize_with(s: &str, words: impl FnOnce() -> Option<String>) -> String {
    let lower = s.to_lowercase().replace('\u{2019}', "'");
    let glued = SPLIT_RE.replace_all(&lower, "${1}re");
    let toks: Vec<&str> = WORD.find_iter(&glued).map(|m| m.as_str()).collect();
    let mut near: HashMap<&str, &'static str> = HashMap::new();
    for t in &toks {
        if !near.contains_key(t)
            && let Some(w) = near_word(t)
        {
            near.insert(t, w);
        }
    }
    keep_typos(&mut near, words);
    toks.iter()
        .map(|t| near.get(t).copied().unwrap_or(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The user's own words in a prompt: without blocks the app put in, pasted
/// text, fenced code and quoted lines.
pub fn own_words(text: &str) -> String {
    let mut s = text.to_string();
    if s.contains('<') {
        for re in BLOCK_RE.iter() {
            s = re.replace_all(&s, " ").into_owned();
        }
        s = ARGS_RE.replace_all(&s, " ").into_owned();
    }
    s = FENCE_RE.replace_all(&s, " ").into_owned();
    let kept: Vec<&str> = s
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect();
    kept.join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The labels of the patterns a typed prompt matches. Empty when it reads
/// as anything but a correction.
pub fn flagged(prompt: &str) -> Vec<&'static str> {
    let mut norm = normalize(&own_words(prompt));
    if norm.is_empty() {
        return Vec::new();
    }
    for re in IGNORE_RE.iter() {
        norm = re.replace_all(&norm, " ").into_owned();
    }
    let mut out: Vec<&'static str> = PATTERN_RE
        .iter()
        .filter(|(_, re)| re.is_match(&norm))
        .map(|(l, _)| *l)
        .collect();
    let disagrees = DISAGREE_RE
        .captures_iter(&norm)
        .any(|c| !NOT_DISAGREE.contains(&&c[1]));
    if disagrees {
        out.push("disagree");
    }
    out
}

/// Whether Claude has replied in this session yet. A prompt before the
/// first reply is a brief, not a correction. A record that can't be read
/// has no reply yet: the app writes it once the session starts.
pub fn answered(transcript: &Path) -> bool {
    let Ok(f) = File::open(transcript) else {
        return false;
    };
    let mut r = BufReader::new(f);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(n) if n > 0 => {}
            _ => return false,
        }
        let line = String::from_utf8_lossy(&buf);
        if !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("assistant")
            && v.get("isSidechain").and_then(Value::as_bool) != Some(true)
        {
            return true;
        }
    }
}

/// What the prompt hook tells Claude about a prompt that may be a
/// correction. `cmd` runs dotonce.
pub fn note(cmd: &str) -> String {
    format!(
        "This prompt may be a correction. Fix what was asked first.\n\
         Then propose a rule only if all three hold: a pattern in the text or a tool call can \
         catch the mistake (a phrase, a regex, a required line, a file path), no rule set that's \
         on already catches it, and it's likely to happen again. Scope it to the folder or skill \
         where it happened. Patterns are Rust regex, with no lookahead: put exceptions in \
         [draft] skip or in the scope. Grep the loaded instruction files for the key term and put \
         any line that says the opposite in --note. Write FILE outside the dotonce folder, then \
         run: {cmd} rules propose FILE --ask --note \"...\".\n\
         Don't mention it in your reply. A fact goes to {cmd} save instead. If this wasn't a \
         correction, ignore this note.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(s: &str) -> Vec<&'static str> {
        flagged(s)
    }

    #[test]
    fn every_pattern_compiles() {
        assert_eq!(PATTERN_RE.len(), PATTERNS.len());
        assert_eq!(IGNORE_RE.len(), IGNORE.len());
        let _ = &*DISAGREE_RE;
        let _ = &*BLOCK_RE;
    }

    #[test]
    fn corrections_are_flagged() {
        for (p, label) in [
            ("That's wrong, the order total is 40 not 50.", "wrong"),
            ("Why did you change the acme config?", "why"),
            ("You keep adding a summary at the end.", "repeat"),
            ("Did you check the docs on example.com first?", "verify"),
            ("Don't add emoji to the reply.", "dont"),
            ("It's still broken after the change.", "repeat"),
            ("Give me the commands with the path.", "path"),
            ("You didn't follow the writing skills here.", "rule"),
            ("Too long, cut it in half.", "long"),
            ("Remove this line from the draft.", "remove"),
            ("Next time ask before you push.", "nexttime"),
            ("I don't think that is how acme works.", "disagree"),
            ("Where is the draft?", "stalled"),
            ("It's not fixed, the page still errors.", "notdone"),
        ] {
            assert!(
                hits(p).contains(&label),
                "{p:?} should hit {label}: {:?}",
                hits(p)
            );
        }
    }

    #[test]
    fn plain_requests_pass() {
        for p in [
            "Add a settings page to the acme plugin.",
            "Can you write a test for the checkout total on example.com?",
            "Thanks, that works. Now the README.",
            "Please draft a reply to the acme vendor about the refund.",
            "Run the build and show me the output.",
            "I don't think so, but let's see what the logs say.",
            "I don't think they changed it.",
        ] {
            assert!(hits(p).is_empty(), "{p:?} hit {:?}", hits(p));
        }
    }

    /// A small word list, so the tests read the same on every system.
    fn words() -> Option<String> {
        Some("the\nrules\nship\nShip\nYou\n".to_string())
    }

    #[test]
    fn typos_map_to_the_word_they_miss() {
        assert_eq!(normalize_with("teh rules", words), "the rules");
        assert_eq!(
            normalize_with("you shoudlnt chekc", words),
            "you shouldn't check"
        );
        assert_eq!(normalize_with("udnerstand", words), "understand");
        // "youa re" style splits glue back on.
        assert_eq!(normalize_with("youa re", words), "you're");
        assert_eq!(
            normalize_with("why did yuo do that", words),
            "why did you do that"
        );
    }

    #[test]
    fn a_word_in_the_word_list_or_with_no_list_is_not_a_typo() {
        // A list without "ship" leaves it one letter off "skip".
        assert_ne!(normalize_with("ship it", || Some(String::new())), "ship it");
        assert_eq!(normalize_with("ship it", words), "ship it");
        assert_eq!(normalize_with("teh ship", || None), "teh ship");
    }

    #[test]
    fn a_word_used_ahead_of_time_is_not_a_complaint() {
        assert!(hits("Write it with no AI slop please.").is_empty());
        assert!(hits("Right or wrong, ship the acme fix.").is_empty());
        assert!(hits("This reads like AI slop.").contains(&"slop"));
    }

    #[test]
    fn pasted_text_quotes_and_code_are_not_the_users_words() {
        let p = "Check this.\n<pasted_content id=\"ab12\">why did you break it, wrong wrong</pasted_content id=\"ab12\">\n> that's wrong\n```\nwrong\n```";
        assert_eq!(own_words(p), "Check this.");
        assert!(hits(p).is_empty());
    }

    #[test]
    fn slip_costs_rank_swaps_first() {
        assert_eq!(slip_cost(b"teh", b"the"), Some(1));
        assert_eq!(slip_cost(b"thw", b"the"), Some(2));
        assert_eq!(slip_cost(b"thee", b"the"), Some(3));
        assert_eq!(slip_cost(b"tthee", b"the"), None);
    }

    #[test]
    fn a_prompt_before_any_reply_is_not_answered() {
        let dir = std::env::temp_dir().join(format!("dotonce-corr-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        assert!(!answered(&t));
        fs::write(&t, "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n").unwrap();
        assert!(!answered(&t));
        fs::write(
            &t,
            "{\"type\":\"user\"}\n{\"type\":\"assistant\",\"isSidechain\":true}\n",
        )
        .unwrap();
        assert!(!answered(&t));
        fs::write(&t, "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n").unwrap();
        assert!(answered(&t));
        let _ = fs::remove_dir_all(&dir);
    }
}
