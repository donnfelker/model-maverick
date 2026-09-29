//! A subset of Go's `flag` package with `ContinueOnError`, as the Go engine's commands
//! used it: bool and string flags, Go's parsing rules and error texts, and the exact
//! `PrintDefaults` layout.
//!
//! Parsing follows `FlagSet.parseOne`: `-f` and `--f` are the same; `-f=v`, `-f v`
//! (non-bool only); a bool takes no separate value but accepts `-f=false`; `--` ends
//! the flags; the first argument that is not a flag stops parsing (later arguments
//! are left positional). Errors print `<message>` and the usage to stderr.

use std::collections::HashMap;
use std::io::Write;

use maverick_domain::gofmt::quote;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    String,
}

#[derive(Debug, Clone)]
struct Flag {
    name: &'static str,
    usage: &'static str,
    kind: Kind,
    default: &'static str,
}

/// Why parsing stopped. Both mean exit code 2 for the Go commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// `-h` or `-help`: the usage was printed.
    Help,
    /// Any other error; the message and the usage were printed.
    Invalid(String),
}

/// A named set of flags (Go `flag.NewFlagSet(name, flag.ContinueOnError)`).
#[derive(Debug, Clone)]
pub struct FlagSet {
    name: String,
    flags: Vec<Flag>,
}

/// The parsed values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    bools: HashMap<&'static str, bool>,
    strings: HashMap<&'static str, String>,
    args: Vec<String>,
}

impl Parsed {
    /// A bool flag's value (its default when not set).
    pub fn bool(&self, name: &str) -> bool {
        self.bools.get(name).copied().unwrap_or(false)
    }

    /// A string flag's value (its default when not set).
    pub fn string(&self, name: &str) -> &str {
        self.strings.get(name).map_or("", String::as_str)
    }

    /// The arguments left after the flags.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

impl FlagSet {
    pub fn new(name: impl Into<String>) -> Self {
        FlagSet {
            name: name.into(),
            flags: Vec::new(),
        }
    }

    /// Defines a bool flag (default false).
    pub fn bool(mut self, name: &'static str, usage: &'static str) -> Self {
        self.flags.push(Flag {
            name,
            usage,
            kind: Kind::Bool,
            default: "false",
        });
        self
    }

    /// Defines a string flag.
    pub fn string(
        mut self,
        name: &'static str,
        default: &'static str,
        usage: &'static str,
    ) -> Self {
        self.flags.push(Flag {
            name,
            usage,
            kind: Kind::String,
            default,
        });
        self
    }

    /// Parses `args`, writing any error and the usage to `stderr`.
    pub fn parse(&self, args: &[String], stderr: &mut dyn Write) -> Result<Parsed, ParseError> {
        let mut parsed = Parsed {
            bools: HashMap::new(),
            strings: HashMap::new(),
            args: Vec::new(),
        };
        for flag in &self.flags {
            match flag.kind {
                Kind::Bool => {
                    parsed.bools.insert(flag.name, false);
                }
                Kind::String => {
                    parsed.strings.insert(flag.name, flag.default.to_owned());
                }
            }
        }

        let mut rest = args;
        while let Some(arg) = rest.first() {
            let bytes = arg.as_bytes();
            if bytes.len() < 2 || bytes[0] != b'-' {
                break;
            }
            let mut minuses = 1;
            if bytes[1] == b'-' {
                minuses = 2;
                if bytes.len() == 2 {
                    // "--" ends the flags.
                    rest = &rest[1..];
                    break;
                }
            }
            let mut name = &arg[minuses..];
            if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
                return Err(self.fail(stderr, format!("bad flag syntax: {arg}")));
            }
            rest = &rest[1..];
            // The value after `=`, which cannot be the first character of the name.
            let mut value = None;
            if let Some(at) = name[1..].find('=') {
                value = Some(&name[at + 2..]);
                name = &name[..at + 1];
            }

            let Some(flag) = self.flags.iter().find(|flag| flag.name == name) else {
                if name == "help" || name == "h" {
                    self.usage(stderr);
                    return Err(ParseError::Help);
                }
                return Err(self.fail(stderr, format!("flag provided but not defined: -{name}")));
            };
            match flag.kind {
                Kind::Bool => {
                    let text = value.unwrap_or("true");
                    let Some(b) = parse_bool(text) else {
                        return Err(self.fail(
                            stderr,
                            format!(
                                "invalid boolean value {} for -{name}: parse error",
                                quote(text)
                            ),
                        ));
                    };
                    parsed.bools.insert(flag.name, b);
                }
                Kind::String => {
                    let text = match value {
                        Some(text) => text.to_owned(),
                        None => match rest.first() {
                            Some(next) => {
                                rest = &rest[1..];
                                next.clone()
                            }
                            None => {
                                return Err(
                                    self.fail(stderr, format!("flag needs an argument: -{name}"))
                                );
                            }
                        },
                    };
                    parsed.strings.insert(flag.name, text);
                }
            }
        }
        parsed.args = rest.to_vec();
        Ok(parsed)
    }

    /// Go `failf`: the message on its own line, then the usage.
    fn fail(&self, stderr: &mut dyn Write, message: String) -> ParseError {
        let _ = writeln!(stderr, "{message}");
        self.usage(stderr);
        ParseError::Invalid(message)
    }

    /// Go's default usage: `Usage of <name>:` and `PrintDefaults`.
    pub fn usage(&self, out: &mut dyn Write) {
        let _ = writeln!(out, "Usage of {}:", self.name);
        self.print_defaults(out);
    }

    /// Go `PrintDefaults`: flags sorted by name; `  -name <type>`, then the usage on
    /// the same line after a tab when the head is at most four bytes long, else on the
    /// next line after four spaces and a tab; non-zero defaults are appended.
    pub fn print_defaults(&self, out: &mut dyn Write) {
        let mut flags: Vec<&Flag> = self.flags.iter().collect();
        flags.sort_by_key(|flag| flag.name);
        for flag in flags {
            let (type_name, usage) = unquote_usage(flag);
            let mut line = format!("  -{}", flag.name);
            if !type_name.is_empty() {
                line.push(' ');
                line.push_str(&type_name);
            }
            if line.len() <= 4 {
                line.push('\t');
            } else {
                line.push_str("\n    \t");
            }
            line.push_str(&usage.replace('\n', "\n    \t"));
            let zero = match flag.kind {
                Kind::Bool => flag.default == "false",
                Kind::String => flag.default.is_empty(),
            };
            if !zero {
                match flag.kind {
                    Kind::String => line.push_str(&format!(" (default {})", quote(flag.default))),
                    Kind::Bool => line.push_str(&format!(" (default {})", flag.default)),
                }
            }
            let _ = writeln!(out, "{line}");
        }
    }
}

/// Go `UnquoteUsage`: a back-quoted word in the usage names the value and loses its
/// quotes; otherwise a string flag's value is named `string` and a bool has no name.
fn unquote_usage(flag: &Flag) -> (String, String) {
    let usage = flag.usage;
    if let Some(start) = usage.find('`') {
        if let Some(len) = usage[start + 1..].find('`') {
            let end = start + 1 + len;
            let name = usage[start + 1..end].to_owned();
            let unquoted = format!("{}{}{}", &usage[..start], name, &usage[end + 1..]);
            return (name, unquoted);
        }
    }
    let name = match flag.kind {
        Kind::Bool => "",
        Kind::String => "string",
    };
    (name.to_owned(), usage.to_owned())
}

/// Go `strconv.ParseBool`.
fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> FlagSet {
        FlagSet::new("corpus load")
            .string("version", "", "corpus `version` to load")
            .string("dir", "", "directory of task files")
            .string("seed", "scripts/demo/seed.sql", "seed file")
            .bool("strict", "fail on warnings")
    }

    fn parse(args: &[&str]) -> (Result<Parsed, ParseError>, String) {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let mut err = Vec::new();
        let result = set().parse(&args, &mut err);
        (result, String::from_utf8(err).expect("utf8"))
    }

    #[test]
    fn value_forms_and_prefixes() {
        let (parsed, err) = parse(&["-version", "v1", "--dir=d", "--strict", "extra", "-x"]);
        let parsed = parsed.expect("parses");
        assert_eq!(err, "");
        assert_eq!(parsed.string("version"), "v1");
        assert_eq!(parsed.string("dir"), "d");
        assert!(parsed.bool("strict"));
        assert_eq!(parsed.string("seed"), "scripts/demo/seed.sql");
        assert_eq!(parsed.args(), ["extra", "-x"]);
    }

    #[test]
    fn bool_values_last_wins_and_terminator() {
        let (parsed, _) = parse(&["--strict", "-strict=false", "--", "--dir", "d"]);
        let parsed = parsed.expect("parses");
        assert!(!parsed.bool("strict"));
        assert_eq!(parsed.string("dir"), "");
        assert_eq!(parsed.args(), ["--dir", "d"]);
        // A value may contain '='.
        let (parsed, _) = parse(&["--dir=a=b"]);
        assert_eq!(parsed.expect("parses").string("dir"), "a=b");
    }

    #[test]
    fn errors_print_the_message_and_usage() {
        let (result, err) = parse(&["--nope"]);
        assert_eq!(
            result,
            Err(ParseError::Invalid(
                "flag provided but not defined: -nope".into()
            ))
        );
        assert!(err.starts_with("flag provided but not defined: -nope\nUsage of corpus load:\n"));

        let (result, err) = parse(&["--dir"]);
        assert_eq!(
            result,
            Err(ParseError::Invalid("flag needs an argument: -dir".into()))
        );
        assert!(err.starts_with("flag needs an argument: -dir\n"));

        let (result, _) = parse(&["-strict=maybe"]);
        assert_eq!(
            result,
            Err(ParseError::Invalid(
                "invalid boolean value \"maybe\" for -strict: parse error".into()
            ))
        );

        for bad in ["---dir", "-=x", "--=x"] {
            let (result, _) = parse(&[bad]);
            assert_eq!(
                result,
                Err(ParseError::Invalid(format!("bad flag syntax: {bad}")))
            );
        }
    }

    #[test]
    fn help_prints_usage_only() {
        for help in ["-h", "--help", "-help"] {
            let (result, err) = parse(&[help]);
            assert_eq!(result, Err(ParseError::Help));
            assert!(err.starts_with("Usage of corpus load:\n"), "{err}");
        }
    }

    #[test]
    fn print_defaults_matches_go_layout() {
        let mut out = Vec::new();
        set().print_defaults(&mut out);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "  -dir string\n    \tdirectory of task files\n  -seed string\n    \tseed file \
             (default \"scripts/demo/seed.sql\")\n  -strict\n    \tfail on warnings\n  -version \
             version\n    \tcorpus version to load\n"
        );
        // A one-letter bool keeps its usage on the same line.
        let mut out = Vec::new();
        FlagSet::new("x")
            .bool("v", "verbose")
            .print_defaults(&mut out);
        assert_eq!(String::from_utf8(out).expect("utf8"), "  -v\tverbose\n");
    }

    #[test]
    fn parse_bool_accepts_go_spellings() {
        for t in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(t), Some(true), "{t}");
        }
        for f in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(f), Some(false), "{f}");
        }
        assert_eq!(parse_bool("yes"), None);
    }
}
