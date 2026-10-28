//! Names without a model: capitalized words mid-sentence, mixed-case words, and
//! words that mix letters and digits (ids, versions). Stored as lowercase parts so
//! they line up with question terms.

pub fn extract_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, raw) in words.iter().enumerate() {
        let trimmed = raw.trim_matches(|c: char| !c.is_alphanumeric());
        let w = trimmed
            .strip_suffix("'s")
            .or_else(|| trimmed.strip_suffix("\u{2019}s"))
            .unwrap_or(trimmed);
        if w.chars().count() < 2 || w.contains('\'') || w.contains('\u{2019}') {
            continue;
        }
        let starts_sentence = i == 0 || words[i - 1].ends_with(['.', '!', '?', ':', '"']);
        let first_upper = w.chars().next().is_some_and(char::is_uppercase);
        let inner_upper = w.chars().skip(1).any(char::is_uppercase);
        let letters_and_digits =
            w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(char::is_alphabetic);
        if (first_upper && !starts_sentence) || inner_upper || letters_and_digits {
            for part in w
                .split(|c: char| !c.is_alphanumeric())
                .filter(|p| p.chars().count() >= 2)
            {
                let key = part.to_lowercase();
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitalized_words_mid_sentence() {
        assert_eq!(
            extract_names("I met Emma at Blue Bottle."),
            vec!["emma", "blue", "bottle"]
        );
    }

    #[test]
    fn sentence_start_is_not_a_name_but_possessives_are() {
        assert_eq!(extract_names("What is Emma's dog called?"), vec!["emma"]);
    }

    #[test]
    fn ids_and_versions_split_into_parts() {
        assert_eq!(
            extract_names("see ticket ACME-226 and the v2 release"),
            vec!["acme", "226", "v2"]
        );
    }
}
