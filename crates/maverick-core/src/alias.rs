//! D45 model grammar, shared by the proxy and SDK cores (spec §8.1).
//!
//! `maverick/<route>[:<variant>]` routes over a pool. `auto` selects the default
//! pool; every other route names a pool. Variants are `quality`, `balanced`, and
//! `cost`, with `quality` as the default. Names are case sensitive and never trimmed.
//! Any string without the exact `maverick/` prefix is a direct provider model.
//! Settings are a separate request option, never part of the model grammar (D14).

/// The engine ranking mode a route selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Balance quality and cost (`:balanced`).
    Balanced,
    /// Prefer quality (the default, or `:quality`).
    Quality,
    /// Prefer cost (`:cost`).
    Cost,
}

/// A parsed Maverick route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// The named pool, or `None` for route `auto`.
    pub pool: Option<String>,
    /// The ranking mode.
    pub mode: Mode,
}

/// A Maverick route or a direct provider model (D45).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedModel<'a> {
    /// A request for a ranked chain.
    Route(Alias),
    /// An explicit provider model. Split at the first `/`, preserving the remainder.
    Direct {
        /// Provider author, when a slash is present.
        author: Option<&'a str>,
        /// The model id, including any provider variant.
        slug: &'a str,
    },
}

impl ParsedModel<'_> {
    /// Extract a route. Direct models never enter the recommendation pipeline.
    #[must_use]
    pub fn into_route(self) -> Option<Alias> {
        match self {
            Self::Route(alias) => Some(alias),
            Self::Direct { .. } => None,
        }
    }
}

/// Why a Maverick route cannot be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    /// The route, variant, or removed query suffix is malformed.
    #[error("malformed_alias")]
    MalformedAlias,
}

/// Parse the exact, case-sensitive D45 grammar. This function is total.
pub fn parse_alias(model: &str) -> Result<ParsedModel<'_>, AliasError> {
    let Some(rest) = model.strip_prefix("maverick/") else {
        let (author, slug) = match model.split_once('/') {
            Some((author, slug)) => (Some(author), slug),
            None => (None, model),
        };
        return Ok(ParsedModel::Direct { author, slug });
    };
    // D14's suffix was replaced by X-Maverick-Settings. Do not treat it as a pool.
    if rest.contains('?') {
        return Err(AliasError::MalformedAlias);
    }
    let (route, variant) = rest.split_once(':').unwrap_or((rest, "quality"));
    if route.is_empty() {
        return Err(AliasError::MalformedAlias);
    }
    let mode = match variant {
        "quality" => Mode::Quality,
        "balanced" => Mode::Balanced,
        "cost" => Mode::Cost,
        _ => return Err(AliasError::MalformedAlias),
    };
    Ok(ParsedModel::Route(Alias {
        pool: (route != "auto").then(|| route.to_owned()),
        mode,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_can_be_extracted_and_direct_models_cannot() {
        assert_eq!(
            parse_alias("maverick/auto").unwrap().into_route(),
            Some(Alias {
                pool: None,
                mode: Mode::Quality,
            })
        );
        assert_eq!(
            parse_alias("openai/gpt-6-astra").unwrap().into_route(),
            None
        );
    }

    /// DIVERGENCE D45: every hand-checked SDK v2 vector also protects the shared parser.
    #[test]
    fn alias_v2_vectors() {
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../sdk-conformance/vectors/alias.v2.json"))
                .unwrap();
        for case in doc["cases"].as_array().unwrap() {
            let got = match parse_alias(case["model"].as_str().unwrap()) {
                Ok(ParsedModel::Route(alias)) => serde_json::json!({
                    "kind": "route", "pool": alias.pool,
                    "mode": match alias.mode {
                        Mode::Quality => "quality",
                        Mode::Balanced => "balanced",
                        Mode::Cost => "cost",
                    },
                }),
                Ok(ParsedModel::Direct { author, slug }) => {
                    serde_json::json!({"kind": "direct", "author": author, "slug": slug})
                }
                Err(AliasError::MalformedAlias) => serde_json::json!({"kind": "malformed_alias"}),
            };
            assert_eq!(got, case["expect"], "{}", case["id"]);
        }
    }

    /// DIVERGENCE D45: replay the Go parser's inputs against the approved v2 results.
    // parity: proxy.TestParseAlias
    // parity: proxy.TestParseAlias/top-level_auto
    // parity: proxy.TestParseAlias/top-level_quality
    // parity: proxy.TestParseAlias/top-level_cost
    // parity: proxy.TestParseAlias/pool_with_auto
    // parity: proxy.TestParseAlias/pool_with_quality
    // parity: proxy.TestParseAlias/pool_with_cost
    // parity: proxy.TestParseAlias/concrete_model
    // parity: proxy.TestParseAlias/concrete_anthropic_model
    // parity: proxy.TestParseAlias/empty_string
    // parity: proxy.TestParseAlias/bare_prefix
    // parity: proxy.TestParseAlias/unknown_top-level_mode
    // parity: proxy.TestParseAlias/unknown_pool_mode
    // parity: proxy.TestParseAlias/empty_pool
    // parity: proxy.TestParseAlias/empty_mode_after_colon
    // parity: proxy.TestParseAlias/double_colon
    #[test]
    fn parse_alias_parity_rows() {
        // Hand-checked D45 replacements for all 23 recorded parse_alias inputs.
        let route = |pool: Option<&str>, mode| {
            Ok(ParsedModel::Route(Alias {
                pool: pool.map(str::to_owned),
                mode,
            }))
        };
        let direct = |slug| Ok(ParsedModel::Direct { author: None, slug });
        let malformed = Err(AliasError::MalformedAlias);
        let cases = [
            ("maverick/auto", route(None, Mode::Quality)),
            ("maverick/quality", route(Some("quality"), Mode::Quality)),
            ("maverick/cost", route(Some("cost"), Mode::Quality)),
            ("maverick/prod-pool:auto", malformed.clone()),
            (
                "maverick/prod-pool:quality",
                route(Some("prod-pool"), Mode::Quality),
            ),
            (
                "maverick/prod-pool:cost",
                route(Some("prod-pool"), Mode::Cost),
            ),
            ("gpt-4o", direct("gpt-4o")),
            ("claude-sonnet-4-6", direct("claude-sonnet-4-6")),
            ("", direct("")),
            ("maverick/", malformed.clone()),
            ("maverick/fast", route(Some("fast"), Mode::Quality)),
            ("maverick/prod-pool:fast", malformed.clone()),
            ("maverick/:auto", malformed.clone()),
            ("maverick/prod-pool:", malformed.clone()),
            ("maverick/prod:pool:auto", malformed.clone()),
            ("maverick/balanced", route(Some("balanced"), Mode::Quality)),
            (
                "Maverick/auto",
                Ok(ParsedModel::Direct {
                    author: Some("Maverick"),
                    slug: "auto",
                }),
            ),
            ("maverick/AUTO", route(Some("AUTO"), Mode::Quality)),
            ("maverick/ auto", route(Some(" auto"), Mode::Quality)),
            ("maverick/p p:cost", route(Some("p p"), Mode::Cost)),
            (
                "maverick/pool/x:quality",
                route(Some("pool/x"), Mode::Quality),
            ),
            ("maverick/:", malformed.clone()),
            ("maverick/auto:auto", malformed),
        ];
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../parity/fixtures/proxy/unit.json")).unwrap();
        let rows = fixture["parse_alias"].as_array().unwrap();
        assert_eq!(rows.len(), cases.len());
        for (row, (model, expected)) in rows.iter().zip(cases) {
            assert_eq!(row["model"], model);
            assert_eq!(parse_alias(model), expected, "{model:?}");
        }
    }

    /// Property (done-when): `parse_alias` is total and never panics on arbitrary input.
    /// The parser uses only total `&str` operations, so this holds by construction; the
    /// deterministic pseudo-random sweep guards against regressions. maverick-core has no
    /// `proptest` dev-dependency, so the generator is a small std-only xorshift.
    #[test]
    fn parse_never_panics_on_arbitrary_input() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next_u64 = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };

        // Alias-significant bytes plus a couple of edge bytes; all valid ASCII.
        let alphabet = b"maverick/:?=scallentgqubosto AUTO\0\x7f";
        for _ in 0..5_000 {
            let len = (next_u64() % 24) as usize;
            let mut s = String::with_capacity(len);
            for _ in 0..len {
                let byte = alphabet[(next_u64() as usize) % alphabet.len()];
                s.push(byte as char);
            }
            // Also exercise the prefix and suffix boundaries directly.
            for candidate in [
                s.clone(),
                format!("maverick/{s}"),
                format!("{s}?settings=caller"),
                format!("maverick/{s}?settings=caller"),
            ] {
                let _ = parse_alias(&candidate);
            }
        }
    }
}
