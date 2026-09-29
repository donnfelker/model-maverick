//! Go `strings` behavior that engine results depend on.

/// Go `strings.ToLower`: the simple (one-rune) lowercase mapping of each rune. Rust's
/// `str::to_lowercase` differs in two ways: U+0130 (İ) becomes two chars, and a final
/// capital sigma becomes `ς`. Go gives `i` and `σ`.
pub fn to_lower(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{130}' => 'i',
            c => c.to_lowercase().next().unwrap_or(c),
        })
        .collect()
}

/// Go `strings.Join(strings.Fields(s), " ")`: collapse runs of Unicode whitespace to one
/// space and trim both ends. (Rust's whitespace set equals Go's `unicode.IsSpace`.)
pub fn collapse_fields(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_lower_is_per_rune_simple() {
        assert_eq!(to_lower("İSTANBUL"), "istanbul");
        assert_eq!(to_lower("ΟΔΟΣ"), "οδοσ");
        assert_eq!(to_lower("ß"), "ß");
        assert_eq!(to_lower("Ǆ"), "ǆ");
        assert_eq!(to_lower("ABC xyz"), "abc xyz");
    }

    #[test]
    fn collapse_fields_uses_unicode_space() {
        assert_eq!(collapse_fields("  a \t\n b\u{3000}c  "), "a b c");
        assert_eq!(collapse_fields("x\u{200b}y"), "x\u{200b}y");
        assert_eq!(collapse_fields(""), "");
    }
}
