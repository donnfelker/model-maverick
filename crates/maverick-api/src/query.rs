//! Query strings as Go's `url.ParseQuery` reads them (reference Part 5 §4): `&`-separated
//! `key=value` pairs in order, `+` as a space, and `%XX` escapes decoded. A pair with a
//! semicolon or a bad escape is dropped, as Go drops it (it records an error and reads on).

/// The decoded pairs, in query order. Empty segments are skipped.
pub(crate) fn pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.contains(';'))
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((unescape(key)?, unescape(value)?))
        })
        .collect()
}

/// Go `QueryUnescape`. Invalid UTF-8 becomes U+FFFD.
fn unescape(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                let high = hex(*bytes.get(index + 1)?)?;
                let low = hex(*bytes.get(index + 2)?)?;
                out.push(high << 4 | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn go_parse_query_rules() {
        assert_eq!(pairs(""), owned(&[]));
        assert_eq!(
            pairs("capability=factual_qa&pool=a+b&x"),
            owned(&[("capability", "factual_qa"), ("pool", "a b"), ("x", "")])
        );
        assert_eq!(pairs("a=%41%2f%2F&&b="), owned(&[("a", "A//"), ("b", "")]));
        assert_eq!(pairs("k%3D=v%3D1"), owned(&[("k=", "v=1")]));
        // Bad escapes and semicolons drop the pair only.
        assert_eq!(pairs("a=%zz&b=1&c=%4&d;e=2&f=%+1"), owned(&[("b", "1")]));
        assert_eq!(pairs("a=1&a=2"), owned(&[("a", "1"), ("a", "2")]));
        assert_eq!(pairs("a=%ff"), owned(&[("a", "\u{fffd}")]));
    }
}
