//! The G5 telemetry redaction lint (D19, plan §6, spec §14.3).
//!
//! Every `#[tracing::instrument]` must use `skip_all` and name the fields it records in
//! `fields(...)`. A bare `#[instrument]` records every argument via `Debug`, so a prompt,
//! a completion, a request body, or a credential could end up in a span.
//!
//! The check works on the token stream of each file, not on a parsed syntax tree. That way
//! it also sees attributes inside macro bodies and inside `cfg_attr`, and multi-line
//! attributes need no special handling. Comments and string literals (including doc
//! comments) are not tokens, so text such as "#[instrument]" in a doc comment is ignored.
//!
//! Rules:
//! - An attribute whose path ends in `instrument` must have `skip_all` as a top-level
//!   argument.
//! - It must not use `ret` or `err`. They record the return value or the error through
//!   `Debug` or `Display`, which is not an explicit field allowlist.
//! - `instrument` must not be renamed in a `use` item (`use tracing::instrument as x`),
//!   because the renamed attribute would escape this check.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};

/// The kind of problem found at one location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// `#[instrument]` with no arguments.
    Bare,
    /// `#[instrument(...)]` whose arguments do not include `skip_all`.
    MissingSkipAll,
    /// `#[instrument(..., ret)]`: records the return value.
    RecordsReturn,
    /// `#[instrument(..., err)]`: records the error value.
    RecordsError,
    /// `#[instrument ...]` followed by something other than a parenthesized list.
    Unrecognized,
    /// `use tracing::instrument as other`.
    Renamed,
}

impl Violation {
    pub fn message(self) -> &'static str {
        match self {
            Violation::Bare => {
                "bare #[instrument] records every argument via Debug; \
                 use #[instrument(skip_all, fields(...))]"
            }
            Violation::MissingSkipAll => {
                "#[instrument] without skip_all records arguments via Debug; \
                 add skip_all and list the recorded fields in fields(...)"
            }
            Violation::RecordsReturn => {
                "#[instrument(ret)] records the return value; \
                 record an allowlisted field or a fingerprint instead"
            }
            Violation::RecordsError => {
                "#[instrument(err)] records the error value; \
                 record an allowlisted field or an error kind instead"
            }
            Violation::Unrecognized => {
                "unrecognized #[instrument] form; use #[instrument(skip_all, fields(...))]"
            }
            Violation::Renamed => {
                "renaming tracing::instrument hides it from this lint; \
                 import it under its own name"
            }
        }
    }
}

/// One violation with its 1-based line and column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub line: usize,
    pub column: usize,
    pub violation: Violation,
}

impl Finding {
    fn at(span: Span, violation: Violation) -> Self {
        let start = span.start();
        Finding {
            line: start.line,
            column: start.column + 1,
            violation,
        }
    }
}

/// The file could not be tokenized as Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError(pub String);

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot tokenize: {}", self.0)
    }
}

/// Checks one Rust source text. Findings are in source order.
pub fn check_source(source: &str) -> Result<Vec<Finding>, LexError> {
    let stream = TokenStream::from_str(source).map_err(|e| LexError(e.to_string()))?;
    let mut findings = Vec::new();
    scan(stream, &mut findings);
    findings.sort_by_key(|f| (f.line, f.column));
    Ok(findings)
}

fn scan(stream: TokenStream, out: &mut Vec<Finding>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            TokenTree::Punct(pound) if pound.as_char() == '#' => {
                let mut j = i + 1;
                if let Some(TokenTree::Punct(bang)) = tokens.get(j) {
                    if bang.as_char() == '!' {
                        j += 1;
                    }
                }
                if let Some(TokenTree::Group(group)) = tokens.get(j) {
                    if group.delimiter() == Delimiter::Bracket {
                        check_attribute(group.stream(), pound.span(), out);
                        i = j + 1;
                        continue;
                    }
                }
            }
            TokenTree::Ident(ident) if ident == "use" => {
                let end = tokens[i + 1..]
                    .iter()
                    .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ';'))
                    .map_or(tokens.len(), |offset| i + 1 + offset);
                check_use(&tokens[i + 1..end], out);
                i = end;
                continue;
            }
            TokenTree::Group(group) => scan(group.stream(), out),
            _ => {}
        }
        i += 1;
    }
}

/// Checks the tokens inside `#[...]`. `span` is the span of the `#`.
fn check_attribute(stream: TokenStream, span: Span, out: &mut Vec<Finding>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let (last_segment, rest) = split_path(&tokens);
    match last_segment.as_deref() {
        Some("instrument") => {
            if let Some(violation) = check_instrument_args(rest) {
                out.push(Finding::at(span, violation));
            }
            if let [TokenTree::Group(args)] = rest {
                check_instrument_flags(args.stream(), span, out);
            }
        }
        Some("cfg_attr") => {
            if let [TokenTree::Group(args)] = rest {
                if args.delimiter() == Delimiter::Parenthesis {
                    // The first element is the predicate; the others are attributes.
                    for attribute in split_commas(args.stream()).into_iter().skip(1) {
                        check_attribute(attribute.into_iter().collect(), span, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Returns the violation for the argument list of an `instrument` attribute, if any.
fn check_instrument_args(rest: &[TokenTree]) -> Option<Violation> {
    match rest {
        [] => Some(Violation::Bare),
        [TokenTree::Group(args)] if args.delimiter() == Delimiter::Parenthesis => {
            let has_skip_all = split_commas(args.stream())
                .iter()
                .any(|arg| leading_ident(arg).as_deref() == Some("skip_all"));
            (!has_skip_all).then_some(Violation::MissingSkipAll)
        }
        _ => Some(Violation::Unrecognized),
    }
}

/// Flags `ret` and `err` arguments, with or without a `(Debug)` / `(Display)` mode.
fn check_instrument_flags(args: TokenStream, span: Span, out: &mut Vec<Finding>) {
    for arg in split_commas(args) {
        match leading_ident(&arg).as_deref() {
            Some("ret") => out.push(Finding::at(span, Violation::RecordsReturn)),
            Some("err") => out.push(Finding::at(span, Violation::RecordsError)),
            _ => {}
        }
    }
}

/// Flags `instrument as <name>` anywhere in a `use` tree.
fn check_use(tokens: &[TokenTree], out: &mut Vec<Finding>) {
    for (k, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(ident) if unraw(ident.to_string()) == "instrument" => {
                if let Some(TokenTree::Ident(next)) = tokens.get(k + 1) {
                    if next == "as" {
                        out.push(Finding::at(ident.span(), Violation::Renamed));
                    }
                }
            }
            TokenTree::Group(group) => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                check_use(&inner, out);
            }
            _ => {}
        }
    }
}

/// Splits `a::b::c rest...` into the last path segment (`c`) and the remaining tokens.
/// Returns `None` when the tokens do not start with a path.
fn split_path(tokens: &[TokenTree]) -> (Option<String>, &[TokenTree]) {
    let mut i = 0;
    let mut last = None;
    loop {
        // An optional `::` before each segment (a leading `::` is allowed too).
        if is_path_sep(tokens, i) {
            i += 2;
        } else if last.is_some() {
            break;
        }
        match tokens.get(i) {
            Some(TokenTree::Ident(ident)) => {
                last = Some(unraw(ident.to_string()));
                i += 1;
            }
            _ => return (None, tokens),
        }
    }
    (last, &tokens[i..])
}

fn is_path_sep(tokens: &[TokenTree], i: usize) -> bool {
    matches!(
        (tokens.get(i), tokens.get(i + 1)),
        (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)))
            if a.as_char() == ':' && b.as_char() == ':'
    )
}

/// Splits a token stream at top-level commas. Empty elements are dropped.
fn split_commas(stream: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut parts = vec![Vec::new()];
    for token in stream {
        match &token {
            TokenTree::Punct(p) if p.as_char() == ',' => parts.push(Vec::new()),
            _ => parts.last_mut().expect("parts is never empty").push(token),
        }
    }
    parts.retain(|part| !part.is_empty());
    parts
}

fn leading_ident(tokens: &[TokenTree]) -> Option<String> {
    match tokens.first() {
        Some(TokenTree::Ident(ident)) => Some(unraw(ident.to_string())),
        _ => None,
    }
}

fn unraw(name: String) -> String {
    match name.strip_prefix("r#") {
        Some(stripped) => stripped.to_owned(),
        None => name,
    }
}

/// The result of checking a set of files.
#[derive(Debug, Default)]
pub struct Report {
    pub files_checked: usize,
    pub findings: Vec<(PathBuf, Finding)>,
    pub errors: Vec<(PathBuf, String)>,
}

/// Checks every `.rs` file under each root. A root may also be a single file.
///
/// While walking, the walk skips `target`, hidden directories, and this crate's own
/// fixture directory (`instrument-lint/tests/fixtures`), which holds deliberate
/// violations. A root that is given explicitly is always checked, so the self-test can
/// point the lint at the fixtures.
pub fn check_paths(roots: &[PathBuf]) -> Report {
    let mut files = Vec::new();
    let mut report = Report::default();
    for root in roots {
        if root.is_file() {
            files.push(root.clone());
        } else if let Err(e) = collect_rs_files(root, &mut files) {
            report.errors.push((root.clone(), e.to_string()));
        }
    }
    files.sort();
    files.dedup();
    for file in files {
        report.files_checked += 1;
        match std::fs::read_to_string(&file) {
            Ok(source) => match check_source(&source) {
                Ok(findings) => {
                    for finding in findings {
                        report.findings.push((file.clone(), finding));
                    }
                }
                Err(e) => report.errors.push((file, e.to_string())),
            },
            Err(e) => report.errors.push((file, e.to_string())),
        }
    }
    report
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !skip_dir(&path) {
                collect_rs_files(&path, files)?;
            }
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn skip_dir(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "target" || name.starts_with('.') || path.ends_with("instrument-lint/tests/fixtures")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violations(source: &str) -> Vec<Violation> {
        check_source(source)
            .expect("fixture tokenizes")
            .into_iter()
            .map(|f| f.violation)
            .collect()
    }

    #[test]
    fn bare_forms_are_rejected() {
        for attr in [
            "#[instrument]",
            "#[tracing::instrument]",
            "#[::tracing::instrument]",
            "#[t::instrument]",
            "#[r#instrument]",
        ] {
            let source = format!("{attr} fn f(prompt: &str) {{}}");
            assert_eq!(violations(&source), [Violation::Bare], "{attr}");
        }
    }

    #[test]
    fn missing_skip_all_is_rejected() {
        for attr in [
            "#[instrument()]",
            "#[instrument(skip(key))]",
            "#[instrument(level = \"debug\")]",
            "#[instrument(fields(skip_all))]",
            "#[tracing::instrument(name = \"skip_all\")]",
        ] {
            let source = format!("{attr} fn f(prompt: &str, key: &str) {{}}");
            assert_eq!(violations(&source), [Violation::MissingSkipAll], "{attr}");
        }
    }

    #[test]
    fn skip_all_forms_pass() {
        for attr in [
            "#[instrument(skip_all)]",
            "#[instrument(skip_all, fields(tenant_id = %tenant))]",
            "#[tracing::instrument(level = \"debug\", skip_all, fields(n = n))]",
            "#[::tracing::instrument(name = \"recommend\", skip_all)]",
            "#[instrument(skip_all, skip(prompt))]",
        ] {
            let source = format!("{attr} fn f(tenant: &str, n: u32, prompt: &str) {{}}");
            assert_eq!(violations(&source), [], "{attr}");
        }
    }

    #[test]
    fn ret_and_err_are_rejected() {
        assert_eq!(
            violations("#[instrument(skip_all, ret)] fn f() -> String { todo!() }"),
            [Violation::RecordsReturn]
        );
        assert_eq!(
            violations("#[instrument(skip_all, err(Debug))] fn f() -> Result<(), E> { todo!() }"),
            [Violation::RecordsError]
        );
        assert_eq!(
            violations("#[instrument(ret, err)] fn f() {}"),
            [
                Violation::MissingSkipAll,
                Violation::RecordsReturn,
                Violation::RecordsError
            ]
        );
    }

    #[test]
    fn unrecognized_forms_are_rejected() {
        assert_eq!(
            violations("#[instrument = \"x\"] fn f() {}"),
            [Violation::Unrecognized]
        );
        assert_eq!(
            violations("#[instrument[skip_all]] fn f() {}"),
            [Violation::Unrecognized]
        );
    }

    #[test]
    fn cfg_attr_is_checked() {
        assert_eq!(
            violations("#[cfg_attr(feature = \"otel\", tracing::instrument)] fn f() {}"),
            [Violation::Bare]
        );
        assert_eq!(
            violations("#[cfg_attr(all(test, unix), inline, instrument(skip(x)))] fn f(x: u8) {}"),
            [Violation::MissingSkipAll]
        );
        assert_eq!(
            violations("#[cfg_attr(test, instrument(skip_all))] fn f() {}"),
            []
        );
        assert_eq!(
            violations("#[cfg_attr(a, cfg_attr(b, instrument))] fn f() {}"),
            [Violation::Bare]
        );
    }

    #[test]
    fn nested_items_and_macro_bodies_are_checked() {
        let source = r#"
            mod m {
                impl S {
                    #[instrument]
                    async fn g(&self) {}
                }
            }
            macro_rules! make {
                ($name:ident) => {
                    #[tracing::instrument(fields(x = $name))]
                    fn $name() {}
                };
            }
        "#;
        assert_eq!(
            violations(source),
            [Violation::Bare, Violation::MissingSkipAll]
        );
    }

    #[test]
    fn multi_line_attribute_reports_the_pound_position() {
        let source = "fn a() {}\n\n  #[tracing::instrument(\n    level = \"info\",\n    fields(a = 1),\n  )]\nfn f() {}\n";
        let findings = check_source(source).expect("tokenizes");
        assert_eq!(
            findings,
            [Finding {
                line: 3,
                column: 3,
                violation: Violation::MissingSkipAll
            }]
        );
    }

    #[test]
    fn comments_strings_and_other_attributes_are_ignored() {
        let source = r##"
            //! Use #[instrument] carefully.
            /// Never write #[instrument] without skip_all.
            // #[instrument]
            /* #[tracing::instrument] */
            #[derive(Debug)]
            #[doc = "#[instrument]"]
            struct S;
            const HELP: &str = "#[instrument]";
            const RAW: &str = r#"#[tracing::instrument]"#;
            #[tokio::test]
            async fn t() { let _ = fut.instrument(span).await; }
            use tracing::Instrument as _;
            fn instrument() {}
        "##;
        assert_eq!(violations(source), []);
    }

    #[test]
    fn renamed_import_is_rejected() {
        assert_eq!(
            violations("use tracing::instrument as trace_it;"),
            [Violation::Renamed]
        );
        assert_eq!(
            violations("pub use tracing::{info, instrument as t, warn};"),
            [Violation::Renamed]
        );
        assert_eq!(violations("use tracing::{info, instrument};"), []);
    }

    #[test]
    fn inner_attribute_form_is_checked() {
        assert_eq!(violations("#![instrument] fn f() {}"), [Violation::Bare]);
    }

    #[test]
    fn lex_error_is_reported() {
        assert!(check_source("fn f() { \"unterminated }").is_err());
    }
}
