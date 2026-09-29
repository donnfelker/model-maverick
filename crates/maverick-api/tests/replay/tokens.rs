//! The values that differ between the Go run and ours by construction (README §5): ids
//! the server mints, raw API keys and `last4`, prompt hashes of random tenant keys,
//! cursors, timestamps, and MCP session ids. Each recorded value maps to ours the first
//! time the two runs show it at the same place (a response pair, or a seed statement),
//! and later requests carry our value. An id the Go test seeded maps to a fresh id minted
//! when the seed runs, so seeded and server-minted ids sort in the same order as in the
//! Go run (lists ordered by id depend on it, README §5).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::{Captures, Regex};

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<id>\b(?:ten|key|col|pool|rec|out|cw|cap|cal|alr|bat)_[0-9A-HJKMNP-TV-Z]{26}\b)",
        r"|(?P<alert>\balr_[a-z_]+[0-9]{16,20}\b)",
        r"|(?P<apikey>\bmk_live_[0-9A-Za-z]{43}\b)",
        r"|(?P<hash>hmac_sha256:[0-9a-f]{64})",
        r"|(?P<ts>[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:[0-9]{2}))",
        r#"|\\?"next_cursor\\?":\\?"(?P<cursor>[A-Za-z0-9_=-]+)\\?""#,
        r#"|\\?"last4\\?":\\?"(?P<last4>[A-Za-z0-9]{4})\\?""#,
    ))
    .expect("token regex")
});

const CLASSES: [&str; 7] = ["id", "alert", "apikey", "hash", "ts", "cursor", "last4"];

/// One token found in a text: its class, value, and byte span.
struct Found {
    class: &'static str,
    value: String,
    start: usize,
    end: usize,
}

fn find(text: &str) -> Vec<Found> {
    TOKEN
        .captures_iter(text)
        .map(|caps: Captures<'_>| {
            let (class, m) = CLASSES
                .iter()
                .find_map(|class| caps.name(class).map(|m| (*class, m)))
                .expect("a class matched");
            Found {
                class,
                value: m.as_str().to_owned(),
                start: m.start(),
                end: m.end(),
            }
        })
        .collect()
}

pub fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Recorded value to ours.
#[derive(Debug, Default)]
pub struct Tokens {
    map: HashMap<String, String>,
    /// Timestamps by instant, so a value recorded in another offset still maps.
    by_instant: HashMap<DateTime<Utc>, String>,
    /// Literal ids a request carries that no run created: the same in both runs.
    fixed: HashSet<String>,
    /// Every value some recorded token maps to.
    ours: HashSet<String>,
}

impl Tokens {
    /// Maps every unmapped id in a seed statement: to an id of ours the test read back
    /// from the database (`read_ids`, a row the server created), else to a fresh id with
    /// the same prefix from the engine's own process-wide monotonic generator.
    pub fn mint_seeded(&mut self, text: &str, read_ids: &mut std::collections::VecDeque<String>) {
        for found in find(text) {
            if found.class == "id" && self.get(&found.value).is_none() {
                let prefix = &found.value[..found.value.find('_').unwrap()];
                let read = read_ids
                    .iter()
                    .position(|ours| ours.starts_with(&found.value[..prefix.len() + 1]))
                    .and_then(|at| read_ids.remove(at));
                let fresh = read.unwrap_or_else(|| maverick_domain::ids::new(prefix));
                self.ours.insert(fresh.clone());
                self.map.insert(found.value, fresh);
            }
        }
    }

    /// An id that appears first in a request (a literal such as an all-zero id): the
    /// same in both runs.
    pub fn fix(&mut self, value: &str) {
        self.fixed.insert(value.to_owned());
    }

    pub fn is_fixed(&self, value: &str) -> bool {
        self.fixed.contains(value)
    }

    /// True when `value` is already the mapped value of some recorded token.
    pub fn is_ours(&self, value: &str) -> bool {
        self.ours.contains(value)
    }

    /// True when `text` is exactly one id.
    pub fn is_id(text: &str) -> bool {
        find(text)
            .first()
            .is_some_and(|f| f.class == "id" && f.start == 0 && f.end == text.len())
    }

    pub fn get(&self, recorded: &str) -> Option<&str> {
        if let Some(ours) = self.map.get(recorded) {
            return Some(ours);
        }
        instant(recorded)
            .and_then(|at| self.by_instant.get(&at))
            .map(String::as_str)
    }

    /// Maps `recorded` to `ours`. An existing different mapping is a conflict.
    pub fn insert(&mut self, recorded: &str, ours: &str) -> Result<(), String> {
        if recorded == ours {
            return Ok(());
        }
        if self.fixed.contains(recorded) {
            return Err(format!("seeded {recorded} is {ours} in our run"));
        }
        if let Some(existing) = self.get(recorded) {
            if existing == ours {
                return Ok(());
            }
            return Err(format!("{recorded} maps to {existing}, now also {ours}"));
        }
        self.map.insert(recorded.to_owned(), ours.to_owned());
        self.ours.insert(ours.to_owned());
        if let Some(at) = instant(recorded) {
            self.by_instant.insert(at, ours.to_owned());
        }
        Ok(())
    }

    /// `text` with every mapped recorded token replaced by ours.
    pub fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for found in find(text) {
            if let Some(ours) = self.get(&found.value) {
                out.push_str(&text[at..found.start]);
                out.push_str(ours);
                at = found.end;
            }
        }
        out.push_str(&text[at..]);
        out
    }

    /// Pairs the tokens of a recorded text with ours, class by class in order, when both
    /// hold the same number of that class. Returns the conflicts found.
    pub fn pair(&mut self, recorded: &str, ours: &str) -> Vec<String> {
        let recorded = find(recorded);
        let ours = find(ours);
        let mut conflicts = Vec::new();
        for class in CLASSES {
            let r: Vec<&Found> = recorded.iter().filter(|f| f.class == class).collect();
            let o: Vec<&Found> = ours.iter().filter(|f| f.class == class).collect();
            if r.len() != o.len() {
                continue;
            }
            for (r, o) in r.iter().zip(&o) {
                if let Err(conflict) = self.insert(&r.value, &o.value) {
                    conflicts.push(conflict);
                }
            }
        }
        conflicts
    }

    /// The unmapped ids of `prefix` in a recorded text, in order.
    pub fn unmapped_ids(&self, text: &str) -> Vec<String> {
        find(text)
            .into_iter()
            .filter(|f| f.class == "id" && self.get(&f.value).is_none() && !self.is_fixed(&f.value))
            .map(|f| f.value)
            .collect()
    }

    /// The unmapped timestamps in a recorded text, in order.
    pub fn unmapped_timestamps(&self, text: &str) -> Vec<String> {
        find(text)
            .into_iter()
            .filter(|f| f.class == "ts" && self.get(&f.value).is_none())
            .map(|f| f.value)
            .collect()
    }
}

/// Go `url.QueryEscape`.
pub fn query_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-decodes a query value (`+` is a space).
pub fn query_unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A request path with every mapped token replaced: the path itself, and each query
/// value decoded, mapped, and re-encoded when it changed.
pub fn apply_path(tokens: &Tokens, path: &str) -> String {
    let (base, query) = match path.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (path, None),
    };
    let mut out = tokens.apply(base);
    if let Some(query) = query {
        out.push('?');
        let pairs: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((key, value)) => {
                    let decoded = query_unescape(value);
                    // A whole value (a cursor) maps as one token.
                    let mapped = match tokens.get(&decoded) {
                        Some(ours) => ours.to_owned(),
                        None => tokens.apply(&decoded),
                    };
                    if mapped == decoded {
                        pair.to_owned()
                    } else {
                        format!("{key}={}", query_escape(&mapped))
                    }
                }
                None => pair.to_owned(),
            })
            .collect();
        out.push_str(&pairs.join("&"));
    }
    out
}
