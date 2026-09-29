//! Go RE2 (`regexp`) semantics on top of the `regex` crate.
//!
//! The syntaxes are nearly the same. The differences that change results:
//! - Go's `\d \w \s \b` (and `\D \W \S \B`) are ASCII-only; the `regex` crate's are
//!   Unicode. [`translate`] rewrites them to ASCII classes, inside and outside `[...]`.
//! - Go supports `\Q...\E` literal quoting; the `regex` crate does not.
//! - Go accepts only the flags `i`, `m`, `s`, and `U`.
//!
//! Compile errors read like Go's (`error parsing regexp: <reason>: `<pattern>``); the
//! reason text is the `regex` crate's (D36).

use regex::Regex;

const DIGIT: &str = "0-9";
const WORD: &str = "0-9A-Za-z_";
const SPACE: &str = "\\t\\n\\f\\r ";

/// Compiles a Go regular expression.
pub fn compile(pattern: &str) -> Result<Regex, String> {
    let translated = translate(pattern).map_err(|reason| go_error(&reason, pattern))?;
    Regex::new(&translated).map_err(|e| go_error(&reason(&e), pattern))
}

/// Go's anchored form `\A(?:expr)\z`, compiled. Errors show the anchored pattern, as in
/// Go.
pub fn compile_anchored(expr: &str) -> Result<Regex, String> {
    compile(&anchored(expr))
}

/// `\A(?:expr)\z`
pub fn anchored(expr: &str) -> String {
    format!("\\A(?:{expr})\\z")
}

fn go_error(reason: &str, pattern: &str) -> String {
    format!("error parsing regexp: {reason}: `{pattern}`")
}

/// The one-line reason from a `regex` crate error.
fn reason(error: &regex::Error) -> String {
    match error {
        regex::Error::Syntax(text) => text
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("error: "))
            .unwrap_or("invalid syntax")
            .to_owned(),
        regex::Error::CompiledTooBig(_) => "expression too large".to_owned(),
        _ => error.to_string(),
    }
}

/// Rewrites a Go pattern into the `regex` crate dialect.
pub fn translate(pattern: &str) -> Result<String, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut i = 0;
    // Position of the current class's `[` content start, when inside `[...]`.
    let mut class_start: Option<usize> = None;
    // Each pass consumes at least one char, so the pattern length bounds the loop. A
    // `while i < len` loop would hang under a mutated `i *= 1`, and a cargo-mutants
    // timeout fails CI.
    for _ in 0..chars.len() {
        let Some(&c) = chars.get(i) else {
            break;
        };
        match (c, class_start) {
            ('\\', _) => {
                let Some(&next) = chars.get(i + 1) else {
                    return Err("trailing backslash at end of expression".to_owned());
                };
                if next == 'Q' {
                    let rest: String = chars[i + 2..].iter().collect();
                    let (literal, after) = match rest.find("\\E") {
                        Some(end) => (&rest[..end], end + 2),
                        None => (rest.as_str(), rest.len()),
                    };
                    if class_start.is_some() {
                        // Inside a class every quoted char stands for itself.
                        for ch in literal.chars() {
                            out.push_str(&regex::escape(&ch.to_string()));
                        }
                    } else {
                        out.push_str(&regex::escape(literal));
                    }
                    i += 2 + rest[..after].chars().count();
                    continue;
                }
                match shorthand(next, class_start.is_some())? {
                    Some(replacement) => out.push_str(&replacement),
                    None => {
                        out.push('\\');
                        out.push(next);
                    }
                }
                i += 2;
            }
            ('[', None) => {
                out.push('[');
                i += 1;
                if chars.get(i) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
                // A `]` right after `[` or `[^` is a literal.
                if chars.get(i) == Some(&']') {
                    out.push_str("\\]");
                    i += 1;
                }
                class_start = Some(i);
            }
            ('[', Some(_)) => match posix_class(&chars[i..])? {
                // A POSIX class `[:name:]` or `[:^name:]`, copied as it is.
                Some(len) => {
                    out.extend(&chars[i..i + len]);
                    i += len;
                }
                // A literal `[`; the regex crate would read a nested class.
                None => {
                    out.push_str("\\[");
                    i += 1;
                }
            },
            (']', Some(_)) => {
                out.push(']');
                class_start = None;
                i += 1;
            }
            ('(', None) if chars.get(i + 1) == Some(&'?') => {
                check_flags(&chars[i + 2..])?;
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// The ASCII replacement for a Go shorthand escape (`\d` and friends), or `None` to
/// keep the escape as it is. Go has no `\b` or `\B` inside a class.
fn shorthand(escape: char, in_class: bool) -> Result<Option<String>, String> {
    let (set, negated) = match (escape, in_class) {
        ('d', _) => (DIGIT, false),
        ('D', _) => (DIGIT, true),
        ('w', _) => (WORD, false),
        ('W', _) => (WORD, true),
        ('s', _) => (SPACE, false),
        ('S', _) => (SPACE, true),
        ('b' | 'B', true) => return Err(format!("invalid escape sequence: `\\{escape}`")),
        ('b', false) => return Ok(Some("(?-u:\\b)".to_owned())),
        ('B', false) => return Ok(Some("(?-u:\\B)".to_owned())),
        _ => return Ok(None),
    };
    Ok(Some(match (in_class, negated) {
        (true, false) => set.to_owned(),
        // The regex crate allows a nested class for a negated set inside a class.
        (_, true) => format!("[^{set}]"),
        (false, false) => format!("[{set}]"),
    }))
}

/// Go's POSIX class names (the regex crate knows the same fourteen).
const POSIX_CLASSES: [&str; 14] = [
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
    "space", "upper", "word", "xdigit",
];

/// At a `[` inside a class: Go reads `[:` up to the next `:]` as a POSIX class
/// (`[:alpha:]`, `[:^alpha:]`) and rejects an unknown name. Returns the class length,
/// or `None` when the `[` is a literal.
fn posix_class(chars: &[char]) -> Result<Option<usize>, String> {
    let [_, ':', rest @ ..] = chars else {
        return Ok(None);
    };
    let Some(close) = rest.windows(2).position(|pair| pair == [':', ']']) else {
        return Ok(None);
    };
    let name: String = rest[..close].iter().collect();
    if POSIX_CLASSES.contains(&name.strip_prefix('^').unwrap_or(&name)) {
        Ok(Some(close + 4))
    } else {
        Err(format!("invalid character class range: `[:{name}:]`"))
    }
}

/// Checks the flags of a `(?flags)` or `(?flags:` group: Go accepts `i m s U` and `-`.
/// Named groups (`(?P<name>`, `(?<name>`) are left alone.
fn check_flags(after: &[char]) -> Result<(), String> {
    if matches!(after.first(), Some('P' | '<')) {
        return Ok(());
    }
    for (n, &c) in after.iter().enumerate() {
        match c {
            ')' | ':' => return Ok(()),
            'i' | 'm' | 's' | 'U' | '-' => {}
            _ => {
                let group: String = after[..=n].iter().collect();
                return Err(format!("invalid or unsupported Perl syntax: `(?{group}`"));
            }
        }
    }
    Err("missing closing )".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_match(pattern: &str, text: &str) -> bool {
        compile(pattern)
            .unwrap_or_else(|e| panic!("{pattern}: {e}"))
            .is_match(text)
    }

    #[test]
    fn shorthand_classes_are_ascii() {
        assert!(is_match(r"^\d+$", "42"));
        assert!(!is_match(r"^\d+$", "٤٢"));
        assert!(is_match(r"^\D$", "٤"));
        assert!(!is_match(r"^\w$", "é"));
        assert!(is_match(r"^\W$", "é"));
        assert!(is_match(r"^\w+$", "a_Z9"));
        assert!(is_match(r"^\s$", "\t"));
        assert!(!is_match(r"^\s$", "\u{a0}"));
        assert!(!is_match(r"^\s$", "\u{b}"));
        assert!(is_match(r"^\S$", "\u{a0}"));
        assert!(is_match(r"^[\d_]+$", "1_2"));
        assert!(!is_match(r"^[\d]$", "٤"));
        assert!(is_match(r"^[\W]$", "é"));
        assert!(is_match(r"^[a\S]$", "x"));
        assert!(is_match(r"^[\s]$", " "));
        assert!(is_match(r"^[^\d]$", "x"));
        assert!(!is_match(r"^[^\d]$", "5"));
        assert!(is_match(r"^[\D]$", "x"));
        assert!(is_match(r"^[\w]$", "q"));
    }

    #[test]
    fn class_state_ends_at_the_bracket() {
        // After `]` the shorthand is outside a class again.
        assert!(is_match(r"^[a]\d$", "a5"));
        assert!(!is_match(r"^[a]\d$", "a0-9"));
        // Inside a class `(` is literal, so `(?x` is not a flag group.
        assert!(is_match(r"^[[:alpha:](?x)]+$", "a(?x"));
        assert!(is_match(r"^[[:^digit:]]$", "a"));
        // A `[` that does not start a POSIX class is a literal.
        assert!(is_match(r"^[[:a]\d$", ":5"));
        assert!(is_match(r"^[[:a]\d$", "[5"));
        assert!(is_match(r"^[[:]]$", ":]"));
        assert!(is_match(r"^[[:]]$", "[]"));
        assert!(is_match(r"^[[:ab:x]+$", "x:"));
        assert!(is_match(r"^[[:ab]+$", "b"));
        assert!(is_match(r"^[[:word:]-]+$", "a_-"));
        // Go rejects an unknown POSIX class name.
        assert_eq!(
            compile(r"[[:AB:]]").unwrap_err(),
            "error parsing regexp: invalid character class range: `[:AB:]`: `[[:AB:]]`"
        );
        assert_eq!(
            compile(r"[[:^foo:]]").unwrap_err(),
            "error parsing regexp: invalid character class range: `[:^foo:]`: `[[:^foo:]]`"
        );
    }

    #[test]
    fn no_word_boundary_inside_a_class() {
        assert_eq!(
            compile(r"[\b]").unwrap_err(),
            "error parsing regexp: invalid escape sequence: `\\b`: `[\\b]`"
        );
        assert_eq!(
            compile(r"[\B]").unwrap_err(),
            "error parsing regexp: invalid escape sequence: `\\B`: `[\\B]`"
        );
    }

    #[test]
    fn word_boundary_is_ascii() {
        // In Go, é is not a word character, so é|c is a boundary.
        assert!(!is_match(r"\Bcaf", "écaf"));
        assert!(is_match(r"\bcaf", "écaf"));
        // In Go, é is not a word character, so there is a boundary between c and é.
        assert!(is_match(r"\bcaf\b", "café"));
        assert!(!is_match(r"\Bcaf", "café"));
        assert!(is_match(r"\bWHERE\b", "SELECT * FROM t WHERE id = 1"));
        assert!(!is_match(r"\bWHERE\b", "NOWHERE"));
    }

    #[test]
    fn quoted_literals() {
        assert!(is_match(r"^\Qa.b*\E$", "a.b*"));
        assert!(!is_match(r"^\Qa.b*\E$", "axb"));
        assert!(is_match(r"^\Q(x\E+$", "(xx"));
        assert!(is_match(r"^x\Q.*", "x.*"));
        assert!(is_match(r"^[\Q]-\E]+$", "]-]"));
        assert!(!is_match(r"^[\Q]-\E]+$", "a"));
    }

    #[test]
    fn classes_and_escapes_pass_through() {
        assert!(is_match(r"^[]a]+$", "]a]"));
        assert!(is_match(r"^[^]a]$", "b"));
        assert!(is_match(r"^[[:alpha:]]+$", "abc"));
        assert!(!is_match(r"^[[:alpha:]]$", "é"));
        assert!(is_match(r"^[a[]$", "["));
        assert!(is_match(r"^\pL+$", "élan"));
        assert!(is_match(r"^\.$", "."));
        assert!(is_match(r"(?i)^yes[.!]?$", "YES!"));
        assert!(is_match(r"(?P<n>a)(?<m>b)", "ab"));
        assert!(is_match(r"(?s:.)", "\n"));
        assert!(is_match(r"(?U)a+", "a"));
        assert!(is_match(r"(?-i)a", "a"));
    }

    #[test]
    fn go_flag_set_only() {
        assert_eq!(
            compile("(?x)a").unwrap_err(),
            "error parsing regexp: invalid or unsupported Perl syntax: `(?x`: `(?x)a`"
        );
        assert!(compile("(?im-sU:a)").is_ok());
        assert_eq!(
            compile("(?i").unwrap_err(),
            "error parsing regexp: missing closing ): `(?i`"
        );
    }

    #[test]
    fn errors_name_the_pattern() {
        assert_eq!(
            compile_anchored("(").unwrap_err(),
            "error parsing regexp: unclosed group: `\\A(?:()\\z`"
        );
        assert_eq!(
            compile("a\\").unwrap_err(),
            "error parsing regexp: trailing backslash at end of expression: `a\\`"
        );
        assert!(
            compile("a{2,1}")
                .unwrap_err()
                .starts_with("error parsing regexp: ")
        );
        assert_eq!(
            compile("a{1000}{1000}{1000}").unwrap_err(),
            "error parsing regexp: expression too large: `a{1000}{1000}{1000}`"
        );
    }

    #[test]
    fn anchoring() {
        assert_eq!(anchored("yes"), "\\A(?:yes)\\z");
        let re = compile_anchored(r"\d+").expect("compiles");
        assert!(re.is_match("42"));
        assert!(!re.is_match("The answer is 42"));
        assert!(!re.is_match("42\n"));
    }
}
