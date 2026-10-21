pub const MAX_GRAMS: usize = 24;

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "is", "are", "was", "were", "be", "been", "to", "of",
    "in", "on", "at", "for", "with", "about", "what", "which", "who", "whom", "when", "where",
    "why", "how", "did", "do", "does", "i", "me", "my", "you", "your", "we", "our", "it", "its",
    "that", "this", "these", "those", "have", "has", "had", "can", "could", "would", "should",
    "will", "from", "as", "by", "so", "if", "than", "then", "there", "their", "they", "them", "he",
    "she", "his", "her", "not", "no", "yes", "any", "some", "much", "many", "more", "most", "ever",
    "just", "also", "into", "over",
];

pub fn terms(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1)
    {
        let l = w.to_lowercase();
        if !STOP.contains(&l.as_str()) && !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// BM25 query: any term may match. Terms are alphanumeric, so quoting is enough.
pub fn fts_or(terms: &[String]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// Limits an FTS5 query to one column.
pub fn in_column(col: &str, m: &str) -> String {
    format!("{col} : ({m})")
}

/// Typo-tolerant query: three-letter pieces of each term of four letters or more.
pub fn trigram_or(terms: &[String]) -> Option<String> {
    let mut grams: Vec<String> = Vec::new();
    for t in terms.iter().filter(|t| t.chars().count() >= 4) {
        let cs: Vec<char> = t.chars().collect();
        for w in cs.windows(3) {
            let g: String = w.iter().collect();
            if !grams.contains(&g) && grams.len() < MAX_GRAMS {
                grams.push(g);
            }
        }
    }
    if grams.is_empty() {
        return None;
    }
    Some(
        grams
            .iter()
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_drop_stop_words_and_punctuation() {
        assert_eq!(
            terms("What did I buy at the Blue Bottle?"),
            vec!["buy", "blue", "bottle"]
        );
    }

    #[test]
    fn terms_drop_single_characters() {
        assert_eq!(
            terms("What's Emma's dog called, x? Didn't she say 7 v2"),
            vec!["emma", "dog", "called", "didn", "say", "v2"]
        );
    }

    #[test]
    fn fts_or_quotes_each_term() {
        let t = vec!["buy".to_string(), "blue".to_string()];
        assert_eq!(fts_or(&t).as_deref(), Some("\"buy\" OR \"blue\""));
        assert_eq!(fts_or(&[]), None);
    }

    #[test]
    fn in_column_wraps_the_query() {
        assert_eq!(
            in_column("text", "\"buy\" OR \"blue\""),
            "text : (\"buy\" OR \"blue\")"
        );
    }

    #[test]
    fn trigram_or_breaks_long_terms_into_grams() {
        let m = trigram_or(&["hindisght".to_string()]).unwrap();
        assert!(m.contains("\"hin\""));
        assert!(m.contains("\"ght\""));
        assert!(m.split(" OR ").count() <= MAX_GRAMS);
        assert_eq!(trigram_or(&["dog".to_string()]), None);
    }
}
