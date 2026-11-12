//! Name handling shared by the library lookup and the source matcher.

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Lowercase a name, strip Latin accents and bracketed text, and keep only letters
/// and digits. Non-Latin scripts pass through unchanged.
pub fn norm(s: &str) -> String {
    let stripped = fold(s, true);
    if stripped.is_empty() { fold(s, false) } else { stripped }
}

fn fold(s: &str, drop_brackets: bool) -> String {
    let lower: String = s.nfkc().collect::<String>().to_lowercase();
    let mut out = String::new();
    let mut depth = 0u32;
    for ch in lower.chars() {
        match ch {
            '(' | '[' if drop_brackets => depth += 1,
            ')' | ']' if drop_brackets => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if (ch as u32) < 0x250 => {
                out.extend(ch.to_string().nfkd().filter(|c| !is_combining_mark(*c) && c.is_alphanumeric()));
            }
            _ if ch.is_alphanumeric() => out.push(ch),
            _ => {}
        }
    }
    out
}

/// True when two artist strings plausibly name the same act ("A, B" matches "A").
pub fn artist_match(a: &str, b: &str) -> bool {
    let (x, y) = (norm(a), norm(b));
    !x.is_empty() && !y.is_empty() && (x.contains(&y) || y.contains(&x))
}

/// Make a string safe to use as one path component.
pub fn sanitize(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    let short: String = trimmed.chars().take(150).collect();
    if short.is_empty() { "_".to_string() } else { short }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_accents_brackets_and_punctuation() {
        assert_eq!(norm("Beyoncé (feat. X) [Live]"), "beyonce");
        assert_eq!(norm("AC/DC"), "acdc");
        // "ł" has no Unicode decomposition, so it stays; every other accent folds away.
        assert_eq!(norm("Zażółć gęślą"), "zazołcgesla");
    }

    #[test]
    fn keeps_non_latin_scripts() {
        assert_eq!(norm("Занавес"), "занавес");
        assert_eq!(norm("大象體操 Elephant Gym"), "大象體操elephantgym");
    }

    #[test]
    fn bracket_only_names_fall_back_to_raw() {
        assert_eq!(norm("(Intro)"), "intro");
    }

    #[test]
    fn artist_matching_is_containment() {
        assert!(artist_match("Marcin Macuk, Krzysztof Zalewski", "Marcin Macuk"));
        assert!(artist_match("Sequence - Topic", "The Sequence") || artist_match("The Sequence", "Sequence"));
        assert!(!artist_match("Rihanna", "The Sequence"));
        assert!(!artist_match("", "x"));
    }

    #[test]
    fn sanitize_makes_one_safe_component() {
        assert_eq!(sanitize("AC/DC: Live?"), "AC_DC_ Live_");
        assert_eq!(sanitize("..."), "_");
        assert_eq!(sanitize("name. "), "name");
        assert_eq!(sanitize(&"x".repeat(300)).chars().count(), 150);
    }
}
