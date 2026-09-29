//! The `rule_scan` grader (spec §5.3, reference Part 2 §10).
//!
//! An ordered list of rules over the raw output. Each rule has one kind:
//! `must_include` / `must_not_include` (literal or regex, optional case folding),
//! `max_length_chars`, `max_length_words`, `line_count`, or `language`.
//! `constraint_compliance` is the passed weight over the total weight; a failed
//! `critical` rule forces it to 0.

use std::collections::BTreeMap;

use regex::Regex;
use serde::Deserialize;
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_script::{Script, UnicodeScript};

use super::envelope::GraderKind;
use super::goregex;
use super::registry::{BoxError, BoxFuture, GradeInput, GradeResult, Grader};
use super::spec::{SpecValidator, check_schema};
use crate::gojson::{self, JsonValue};
use crate::gostrings;

/// The emitted dimension.
pub const DIMENSION: &str = "constraint_compliance";

/// The rule_scan grader.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleScan;

#[derive(Debug, Deserialize)]
struct Spec {
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    must_include: Option<String>,
    must_not_include: Option<String>,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    max_length_chars: Option<i64>,
    max_length_words: Option<i64>,
    line_count: Option<LineCount>,
    language: Option<String>,
    weight: Option<f64>,
    #[serde(default)]
    critical: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LineCount {
    Exact(i64),
    Bounds { min: Option<i64>, max: Option<i64> },
}

/// A text pattern: a compiled regex, or a literal (lowercased when case-insensitive).
enum Pattern {
    Regex(Regex),
    Literal { text: String, fold: bool },
}

impl Pattern {
    fn is_found(&self, output: &str) -> bool {
        match self {
            Pattern::Regex(re) => re.is_match(output),
            Pattern::Literal { text, fold: true } => {
                gostrings::to_lower(output).contains(text.as_str())
            }
            Pattern::Literal { text, fold: false } => output.contains(text.as_str()),
        }
    }
}

/// A compiled rule check.
enum Check {
    MustInclude(Pattern),
    MustNotInclude(Pattern),
    MaxLengthChars(i64),
    MaxLengthWords(i64),
    LineCount { min: Option<i64>, max: Option<i64> },
    Language(String),
}

impl Check {
    fn kind(&self) -> &'static str {
        match self {
            Check::MustInclude(_) => "must_include",
            Check::MustNotInclude(_) => "must_not_include",
            Check::MaxLengthChars(_) => "max_length_chars",
            Check::MaxLengthWords(_) => "max_length_words",
            Check::LineCount { .. } => "line_count",
            Check::Language(_) => "language",
        }
    }

    fn passes(&self, output: &str) -> bool {
        match self {
            Check::MustInclude(pattern) => pattern.is_found(output),
            Check::MustNotInclude(pattern) => !pattern.is_found(output),
            Check::MaxLengthChars(limit) => count(output.chars().count()) <= *limit,
            Check::MaxLengthWords(limit) => count(output.split_whitespace().count()) <= *limit,
            Check::LineCount { min, max } => {
                let lines = count_lines(output);
                min.is_none_or(|min| lines >= min) && max.is_none_or(|max| lines <= max)
            }
            Check::Language(code) => language_matches(code, output),
        }
    }
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `compileRule`: the rule's check, by Go's kind precedence (must_include,
/// must_not_include, max_length_chars, max_length_words, line_count, language).
fn compile_rule(rule: &RawRule) -> Result<Check, String> {
    let pattern = |text: &str| -> Result<Pattern, String> {
        if rule.regex {
            let expr = if rule.case_insensitive {
                format!("(?i){text}")
            } else {
                text.to_owned()
            };
            Ok(Pattern::Regex(goregex::compile(&expr)?))
        } else if rule.case_insensitive {
            Ok(Pattern::Literal {
                text: gostrings::to_lower(text),
                fold: true,
            })
        } else {
            Ok(Pattern::Literal {
                text: text.to_owned(),
                fold: false,
            })
        }
    };
    if let Some(text) = &rule.must_include {
        return Ok(Check::MustInclude(pattern(text)?));
    }
    if let Some(text) = &rule.must_not_include {
        return Ok(Check::MustNotInclude(pattern(text)?));
    }
    if let Some(limit) = rule.max_length_chars {
        return Ok(Check::MaxLengthChars(limit));
    }
    if let Some(limit) = rule.max_length_words {
        return Ok(Check::MaxLengthWords(limit));
    }
    if let Some(line_count) = &rule.line_count {
        return match *line_count {
            LineCount::Exact(n) => Ok(Check::LineCount {
                min: Some(n),
                max: Some(n),
            }),
            LineCount::Bounds {
                min: None,
                max: None,
            } => Err("line_count: needs min or max".to_owned()),
            LineCount::Bounds {
                min: Some(min),
                max: Some(max),
            } if min > max => Err(format!("line_count: min {min} > max {max}")),
            LineCount::Bounds { min, max } => Ok(Check::LineCount { min, max }),
        };
    }
    if let Some(code) = &rule.language {
        return Ok(Check::Language(code.clone()));
    }
    Err("rule has no kind".to_owned())
}

impl SpecValidator for RuleScan {
    fn kind(&self) -> &str {
        GraderKind::RuleScan.as_str()
    }

    /// Schema, then the typed decode (so `max_length_chars: 1.0` fails, QUIRK Q-R1),
    /// then `compileRule` per rule.
    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        let value = check_schema(GraderKind::RuleScan, spec)?;
        let spec: Spec =
            serde_json::from_value(value).map_err(|e| format!("rule_scan spec: {e}"))?;
        for (i, rule) in spec.rules.iter().enumerate() {
            compile_rule(rule).map_err(|e| format!("rule_scan spec: rules[{i}]: {e}"))?;
        }
        Ok(())
    }
}

impl Grader for RuleScan {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move { grade(spec, input.output).map_err(BoxError::from) })
    }
}

fn grade(spec: &JsonValue, output: &str) -> Result<GradeResult, String> {
    let spec: Spec = serde_json::from_value(spec.to_serde()?)
        .map_err(|e| format!("rule_scan: decode spec: {e}"))?;
    if spec.rules.is_empty() {
        return Err("rule_scan: spec has no rules".to_owned());
    }
    let mut records = Vec::with_capacity(spec.rules.len());
    let mut passed_weight = 0.0;
    let mut total_weight = 0.0;
    let mut critical_failed = false;
    for (i, rule) in spec.rules.iter().enumerate() {
        let check = compile_rule(rule).map_err(|e| format!("rule_scan: rules[{i}]: {e}"))?;
        let pass = check.passes(output);
        let weight = rule.weight.unwrap_or(1.0);
        total_weight += weight;
        if pass {
            passed_weight += weight;
        } else if rule.critical {
            critical_failed = true;
        }
        let mut record = vec![
            ("index".to_owned(), JsonValue::Number(i.to_string())),
            (
                "kind".to_owned(),
                JsonValue::String(check.kind().to_owned()),
            ),
            ("pass".to_owned(), JsonValue::Bool(pass)),
            (
                "weight".to_owned(),
                JsonValue::Number(gojson::encode_f64(weight)?),
            ),
        ];
        if rule.critical {
            record.push(("critical".to_owned(), JsonValue::Bool(true)));
        }
        records.push(JsonValue::Object(record));
    }
    let score = if critical_failed {
        0.0
    } else {
        passed_weight / total_weight
    };
    let detail = JsonValue::Object(vec![
        ("rules".to_owned(), JsonValue::Array(records)),
        (
            "score".to_owned(),
            JsonValue::Number(gojson::encode_f64(score)?),
        ),
        (
            "critical_failed".to_owned(),
            JsonValue::Bool(critical_failed),
        ),
    ]);
    Ok(GradeResult {
        dimensions: BTreeMap::from([(DIMENSION.to_owned(), score)]),
        detail: gojson::marshal(&detail),
    })
}

/// `countLines`: strip trailing `\r` and `\n`; whitespace-only is 0 lines; otherwise
/// the number of `\n` plus one. A lone `\r` is not a line break; interior blank lines
/// count.
pub fn count_lines(output: &str) -> i64 {
    let body = output.trim_end_matches(['\r', '\n']);
    if body.trim().is_empty() {
        0
    } else {
        count(body.matches('\n').count()) + 1
    }
}

/// The scripts that decide the non-Latin languages.
const SCRIPT_LANGUAGES: [(&str, Script); 7] = [
    ("ko", Script::Hangul),
    ("ru", Script::Cyrillic),
    ("ar", Script::Arabic),
    ("he", Script::Hebrew),
    ("el", Script::Greek),
    ("hi", Script::Devanagari),
    ("th", Script::Thai),
];

/// The stopwords that decide the Latin-script languages (exact lists).
const STOPWORDS: [(&str, &str); 7] = [
    (
        "en",
        "the and is of to in that it with for was are this not on as be at by an",
    ),
    (
        "es",
        "el la los las es de que y en un una por con para no se su al lo como más",
    ),
    (
        "fr",
        "le les est et des du que qui dans pour avec une ne pas sur ce il vous je nous",
    ),
    (
        "de",
        "der die das und ist nicht ein eine mit für auf den dem zu sich wird sind im auch",
    ),
    (
        "it",
        "il lo gli è che di non per con uno sono del della più anche come si nel alla",
    ),
    (
        "pt",
        "o os as é que não um uma para com do da em se por mais como você mas",
    ),
    (
        "nl",
        "het een en van is niet met voor aan op dat zijn te ook naar bij uit",
    ),
];

/// The runes trimmed from each token before the stopword lookup.
const TOKEN_TRIM: [char; 16] = [
    '.', ',', ';', ':', '!', '?', '"', '\'', '(', ')', '[', ']', '«', '»', '\u{2014}', '\u{2013}',
];

/// Go `unicode.IsLetter`: general category L (Lu, Ll, Lt, Lm, Lo).
fn is_letter(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

/// The language detector (reference Part 2 §10, exact).
///
/// Letters are counted by script. A script language (ko, ru, ar, he, el, hi, th)
/// passes when at least half the letters are in its script. `zh` needs half Han and no
/// kana; `ja` needs half Han plus kana and some kana. A Latin language needs half the
/// letters Latin, then strictly more stopword hits than every other Latin language.
pub fn language_matches(code: &str, output: &str) -> bool {
    let mut total = 0_usize;
    let mut latin = 0_usize;
    let mut han = 0_usize;
    let mut kana = 0_usize;
    let mut script_counts = [0_usize; SCRIPT_LANGUAGES.len()];
    for c in output.chars().filter(|&c| is_letter(c)) {
        total += 1;
        match c.script() {
            Script::Latin => latin += 1,
            Script::Han => han += 1,
            Script::Hiragana | Script::Katakana => kana += 1,
            script => {
                if let Some(n) = SCRIPT_LANGUAGES.iter().position(|(_, s)| *s == script) {
                    script_counts[n] += 1;
                }
            }
        }
    }
    if total == 0 {
        return false;
    }
    if let Some(n) = SCRIPT_LANGUAGES.iter().position(|(lang, _)| *lang == code) {
        return 2 * script_counts[n] >= total;
    }
    match code {
        "zh" => 2 * han >= total && kana == 0,
        "ja" => 2 * (han + kana) >= total && kana > 0,
        _ => {
            if 2 * latin < total {
                return false;
            }
            let hits = stopword_hits(output);
            let own = hits.get(code).copied().unwrap_or(0);
            // Go also requires own > 0; beating every other language's count (each
            // >= 0) already implies it, for known and unknown codes alike.
            hits.iter().all(|(lang, &n)| *lang == code || own > n)
        }
    }
}

/// Stopword hits per Latin language over the lowercased, trimmed tokens. A token counts
/// once for every language whose list contains it.
fn stopword_hits(output: &str) -> BTreeMap<&'static str, usize> {
    let mut hits: BTreeMap<&'static str, usize> =
        STOPWORDS.iter().map(|(lang, _)| (*lang, 0)).collect();
    let lowered = gostrings::to_lower(output);
    for token in lowered.split_whitespace() {
        let token = token.trim_matches(TOKEN_TRIM.as_slice());
        if token.is_empty() {
            continue;
        }
        for (lang, words) in STOPWORDS {
            if words.split(' ').any(|w| w == token) {
                *hits.get_mut(lang).expect("every language has a count") += 1;
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> JsonValue {
        gojson::decode_one(json).expect("json").0
    }

    fn rule(json: &str) -> RawRule {
        serde_json::from_str(json).expect("rule")
    }

    #[test]
    fn line_count_rules() {
        assert!(matches!(
            compile_rule(&rule(r#"{"line_count":3}"#)),
            Ok(Check::LineCount {
                min: Some(3),
                max: Some(3)
            })
        ));
        assert_eq!(
            compile_rule(&rule(r#"{"line_count":{}}"#)).err(),
            Some("line_count: needs min or max".to_owned())
        );
        assert_eq!(
            compile_rule(&rule(r#"{"line_count":{"min":5,"max":2}}"#)).err(),
            Some("line_count: min 5 > max 2".to_owned())
        );
        assert!(compile_rule(&rule(r#"{"line_count":{"min":2,"max":2}}"#)).is_ok());
        assert_eq!(
            compile_rule(&rule(r#"{"weight":1}"#)).err(),
            Some("rule has no kind".to_owned())
        );
        let check = compile_rule(&rule(r#"{"line_count":{"min":2}}"#)).unwrap();
        assert!(!check.passes("a"));
        assert!(check.passes("a\nb\nc"));
        let check = compile_rule(&rule(r#"{"line_count":{"max":2}}"#)).unwrap();
        assert!(check.passes("a\nb"));
        assert!(!check.passes("a\nb\nc"));
    }

    #[test]
    fn kind_precedence() {
        let check = compile_rule(&rule(
            r#"{"must_include":"a","must_not_include":"b","max_length_chars":1,"max_length_words":1,"line_count":1,"language":"en"}"#,
        ))
        .unwrap();
        assert_eq!(check.kind(), "must_include");
        for (json, kind) in [
            (
                r#"{"must_not_include":"b","max_length_chars":1}"#,
                "must_not_include",
            ),
            (
                r#"{"max_length_chars":1,"max_length_words":1}"#,
                "max_length_chars",
            ),
            (
                r#"{"max_length_words":1,"line_count":1}"#,
                "max_length_words",
            ),
            (r#"{"line_count":1,"language":"en"}"#, "line_count"),
            (r#"{"language":"en"}"#, "language"),
        ] {
            assert_eq!(compile_rule(&rule(json)).unwrap().kind(), kind, "{json}");
        }
    }

    #[test]
    fn patterns() {
        let pass = |json: &str, output: &str| compile_rule(&rule(json)).unwrap().passes(output);
        assert!(pass(r#"{"must_include":"Where"}"#, "Where"));
        assert!(!pass(r#"{"must_include":"Where"}"#, "WHERE"));
        assert!(pass(
            r#"{"must_include":"Where","case_insensitive":true}"#,
            "WHERE"
        ));
        assert!(pass(r#"{"must_include":"w.e","regex":true}"#, "a wxe b"));
        assert!(!pass(r#"{"must_include":"W.E","regex":true}"#, "wxe"));
        assert!(pass(
            r#"{"must_include":"W.E","regex":true,"case_insensitive":true}"#,
            "wxe"
        ));
        assert!(pass(
            r#"{"must_not_include":"as an ai","case_insensitive":true}"#,
            "fine"
        ));
        assert!(!pass(
            r#"{"must_not_include":"as an ai","case_insensitive":true}"#,
            "As An AI"
        ));
        assert!(pass(r#"{"max_length_words":2}"#, " a \u{3000} b "));
        assert!(!pass(r#"{"max_length_words":1}"#, "a b"));
        assert!(pass(r#"{"max_length_chars":2}"#, "éé"));
    }

    #[test]
    fn grade_errors_and_weights() {
        assert_eq!(
            grade(&spec(r#"{"rules":[]}"#), "x").unwrap_err(),
            "rule_scan: spec has no rules"
        );
        assert!(
            grade(&spec(r#"{"rules":1}"#), "x")
                .unwrap_err()
                .starts_with("rule_scan: decode spec: ")
        );
        assert_eq!(
            grade(
                &spec(r#"{"rules":[{"must_include":"x"},{"must_include":"(","regex":true}]}"#),
                "x"
            )
            .unwrap_err(),
            "rule_scan: rules[1]: error parsing regexp: unclosed group: `(`"
        );
        let result = grade(
            &spec(r#"{"rules":[{"must_include":"x","weight":0.25},{"must_include":"y","weight":0.5,"critical":false}]}"#),
            "y",
        )
        .unwrap();
        assert_eq!(result.dimensions[DIMENSION], 0.5 / 0.75);
        assert_eq!(
            result.detail,
            r#"{"rules":[{"index":0,"kind":"must_include","pass":false,"weight":0.25},{"index":1,"kind":"must_include","pass":true,"weight":0.5}],"score":0.6666666666666666,"critical_failed":false}"#
        );
    }

    #[test]
    fn letters_are_general_category_l() {
        assert!(is_letter('a'));
        assert!(is_letter('É'));
        assert!(is_letter('ǅ'));
        assert!(is_letter('ʰ'));
        assert!(is_letter('字'));
        assert!(!is_letter('1'));
        assert!(!is_letter('\u{301}'));
        assert!(!is_letter('\u{93e}'));
        assert!(!is_letter('Ⅻ'));
    }

    #[test]
    fn language_edges() {
        // Script languages need at least half.
        assert!(language_matches("ru", "Да ok"));
        assert!(!language_matches("ru", "Д okk"));
        // Kana decides between zh and ja.
        assert!(language_matches("ja", "漢字かな"));
        assert!(!language_matches("zh", "漢字かな"));
        assert!(language_matches("zh", "漢字ab"));
        assert!(!language_matches("zh", "漢abc"));
        assert!(!language_matches("ja", "漢字"));
        assert!(!language_matches("ja", "かabc"));
        // A Latin language needs at least half Latin letters.
        assert!(language_matches("en", "the Да"));
        assert!(!language_matches("en", "the ДаДа"));
        // Tokens are trimmed of the exact punctuation set; em and en dashes too.
        assert!(language_matches("en", "—the– «is»"));
        // An unknown code never matches.
        assert!(!language_matches("xx", "the and is"));
    }

    #[test]
    fn language_thresholds_are_exactly_half() {
        // Exactly half the letters in the script passes; one fewer fails.
        assert!(language_matches("ru", "Даа abc"));
        assert!(!language_matches("ru", "Д ab"));
        assert!(language_matches("zh", "漢字語abc"));
        assert!(!language_matches("zh", "漢ab"));
        assert!(language_matches("ja", "漢かなabc"));
        assert!(!language_matches("ja", "かabc"));
        assert!(language_matches("en", "the Даа"));
        assert!(!language_matches("en", "the Дааа"));
    }
}
