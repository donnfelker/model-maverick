//! A column's pricing snapshot and the cost of a run (spec §4.4, reference Part 2 §5.4
//! and Part 3 §5).
//!
//! Each column snapshots its list price per million tokens. A run's cost comes from the
//! snapshot at execution time and is never restated. The optional cache prices (D44) are
//! read only by conversation affinity (spec §7.6), never by a run's cost.

use crate::gojson::{self, JsonValue};

/// `catalog.Pricing`: prices in USD per million tokens.
///
/// D44 adds the two cache prices. They are `None` when the snapshot has no such field,
/// never 0.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
    pub currency: String,
    pub as_of: String,
    pub source: String,
}

const FIELDS: [&str; 7] = [
    "input_per_mtok",
    "output_per_mtok",
    "cache_read_per_mtok",
    "cache_write_per_mtok",
    "currency",
    "as_of",
    "source",
];

/// `ParsePricing(raw)`, decoding like Go's `json.Unmarshal` into `catalog.Pricing`.
///
/// Empty text and JSON `null` are zero pricing. Field names match case-insensitively,
/// unknown fields are ignored, a `null` field keeps its zero value, and the last
/// duplicate wins. A decode error is `parsing pricing snapshot: <json error>`, where the
/// text after the prefix is library wording (D36). A negative price is
/// `pricing snapshot has a negative price`. The cache prices (D44) parse the same way; the
/// Go engine ignored them as unknown fields.
pub fn parse_pricing(raw: &str) -> Result<Pricing, String> {
    let mut pricing = Pricing::default();
    if raw.is_empty() {
        return Ok(pricing);
    }
    let error = |e: String| format!("parsing pricing snapshot: {e}");
    let members = match gojson::unmarshal(raw).map_err(|e| error(e.0))? {
        JsonValue::Null => return Ok(pricing),
        JsonValue::Object(members) => members,
        other => {
            return Err(error(format!(
                "json: cannot unmarshal {} into Go value of type catalog.Pricing",
                other.kind_name()
            )));
        }
    };
    for (key, value) in &members {
        let Some(field) = gojson::match_field(key, &FIELDS) else {
            continue;
        };
        if *value == JsonValue::Null {
            continue;
        }
        match field {
            "input_per_mtok" => pricing.input_per_mtok = price(field, value).map_err(error)?,
            "output_per_mtok" => pricing.output_per_mtok = price(field, value).map_err(error)?,
            "cache_read_per_mtok" => {
                pricing.cache_read_per_mtok = Some(price(field, value).map_err(error)?);
            }
            "cache_write_per_mtok" => {
                pricing.cache_write_per_mtok = Some(price(field, value).map_err(error)?);
            }
            "currency" => pricing.currency = text(field, value).map_err(error)?,
            "as_of" => pricing.as_of = text(field, value).map_err(error)?,
            _ => pricing.source = text(field, value).map_err(error)?,
        }
    }
    let negative = |price: Option<f64>| price.is_some_and(|price| price < 0.0);
    if pricing.input_per_mtok < 0.0
        || pricing.output_per_mtok < 0.0
        || negative(pricing.cache_read_per_mtok)
        || negative(pricing.cache_write_per_mtok)
    {
        return Err("pricing snapshot has a negative price".to_owned());
    }
    Ok(pricing)
}

fn mismatch(field: &str, value: &JsonValue, go_type: &str) -> String {
    format!(
        "json: cannot unmarshal {} into Go struct field Pricing.{field} of type {go_type}",
        value.kind_name()
    )
}

/// A `float64` field. A literal out of the `float64` range is an error, as in Go.
fn price(field: &str, value: &JsonValue) -> Result<f64, String> {
    let JsonValue::Number(literal) = value else {
        return Err(mismatch(field, value, "float64"));
    };
    match literal.parse::<f64>() {
        Ok(x) if x.is_finite() => Ok(x),
        _ => Err(format!(
            "json: cannot unmarshal number {literal} into Go struct field Pricing.{field} of type float64"
        )),
    }
}

fn text(field: &str, value: &JsonValue) -> Result<String, String> {
    match value {
        JsonValue::String(s) => Ok(s.clone()),
        other => Err(mismatch(field, other, "string")),
    }
}

/// The `source` of a replica's local pricing snapshot (spec §14.4, D49): negotiated rates
/// set through `PUT /internal/v1/columns/{id}`. A bundle import never overwrites it.
pub const LOCAL_SOURCE: &str = "local";

/// Whether `pricing` is a replica's local snapshot.
pub fn is_local(pricing: &Pricing) -> bool {
    pricing.source == LOCAL_SOURCE
}

/// `snapshot` as a local snapshot: every member that [`parse_pricing`] reads as `source`
/// is dropped, and `"source": "local"` is appended. A value that is not an object comes
/// back unchanged, so its validation error stays the same.
pub fn mark_local(snapshot: &JsonValue) -> JsonValue {
    let JsonValue::Object(members) = snapshot else {
        return snapshot.clone();
    };
    let mut members: Vec<_> = members
        .iter()
        .filter(|(key, _)| gojson::match_field(key, &FIELDS) != Some("source"))
        .cloned()
        .collect();
    members.push((
        "source".to_owned(),
        JsonValue::String(LOCAL_SOURCE.to_owned()),
    ));
    JsonValue::Object(members)
}

/// `Cost(tokensIn, tokensOut, pricing)`: the USD cost rounded half away from zero to 6
/// decimal places, the precision of `runs.cost numeric(12,6)`.
pub fn cost(tokens_in: i64, tokens_out: i64, pricing: &Pricing) -> f64 {
    let usd = (tokens_in as f64 * pricing.input_per_mtok
        + tokens_out as f64 * pricing.output_per_mtok)
        / 1e6;
    (usd * 1e6).round() / 1e6
}

/// The decimal text a cost is persisted as: Go's `strconv.FormatFloat(cost, 'f', 6, 64)`.
pub fn cost_numeric(cost: f64) -> String {
    format!("{cost:.6}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DIVERGENCE: D49. A local snapshot keeps every price and replaces each spelling of
    /// `source`; a non-object is left for validation to refuse.
    #[test]
    fn mark_local_replaces_every_source_and_keeps_the_prices() {
        let sent = gojson::unmarshal(
            r#"{"Source": "models.dev", "input_per_mtok": 2.5, "SOURCE": null, "output_per_mtok": 10, "source_note": "x"}"#,
        )
        .expect("json");
        let marked = gojson::marshal(&mark_local(&sent));
        assert_eq!(
            marked,
            r#"{"input_per_mtok":2.5,"output_per_mtok":10,"source_note":"x","source":"local"}"#
        );
        let pricing = parse_pricing(&marked).expect("parses");
        assert!(is_local(&pricing));
        assert_eq!(
            (pricing.input_per_mtok, pricing.output_per_mtok),
            (2.5, 10.0)
        );
        assert_eq!(
            gojson::marshal(&mark_local(&JsonValue::Object(Vec::new()))),
            r#"{"source":"local"}"#
        );
        for other in ["[]", "1", "\"local\""] {
            let value = gojson::unmarshal(other).expect("json");
            assert_eq!(mark_local(&value), value, "{other}");
        }
    }

    #[test]
    fn only_the_local_source_is_local() {
        for (source, local) in [
            ("local", true),
            ("Local", false),
            ("", false),
            ("models.dev", false),
        ] {
            let pricing = Pricing {
                source: source.to_owned(),
                ..Pricing::default()
            };
            assert_eq!(is_local(&pricing), local, "{source:?}");
        }
    }

    #[test]
    fn field_names_fold_and_the_last_duplicate_wins() {
        let pricing = parse_pricing(
            r#"{"INPUT_PER_MTOK": 1, "input_per_mtok": 2, "Output_Per_Mtok": 4, "Currency": "USD", "AS_OF": "d", "source": "s", "output_per_mtok": null}"#,
        )
        .expect("parses");
        assert_eq!(
            pricing,
            Pricing {
                input_per_mtok: 2.0,
                output_per_mtok: 4.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
                currency: "USD".to_owned(),
                as_of: "d".to_owned(),
                source: "s".to_owned(),
            }
        );
    }

    #[test]
    fn decode_errors_keep_the_engine_prefix() {
        for (raw, tail) in [
            (
                "[]",
                "json: cannot unmarshal array into Go value of type catalog.Pricing",
            ),
            (
                r#"{"output_per_mtok": true}"#,
                "json: cannot unmarshal bool into Go struct field Pricing.output_per_mtok of type float64",
            ),
            (
                r#"{"input_per_mtok": 1e400}"#,
                "json: cannot unmarshal number 1e400 into Go struct field Pricing.input_per_mtok of type float64",
            ),
            (
                r#"{"currency": 1}"#,
                "json: cannot unmarshal number into Go struct field Pricing.currency of type string",
            ),
            (
                r#"{"as_of": {}}"#,
                "json: cannot unmarshal object into Go struct field Pricing.as_of of type string",
            ),
            (
                r#"{"source": []}"#,
                "json: cannot unmarshal array into Go struct field Pricing.source of type string",
            ),
        ] {
            assert_eq!(
                parse_pricing(raw),
                Err(format!("parsing pricing snapshot: {tail}")),
                "{raw}"
            );
        }
        let syntax = parse_pricing("{").expect_err("syntax error");
        assert!(syntax.starts_with("parsing pricing snapshot: "), "{syntax}");
    }

    #[test]
    fn negative_output_price_is_rejected_and_negative_zero_is_not() {
        assert_eq!(
            parse_pricing(r#"{"output_per_mtok": -0.5}"#),
            Err("pricing snapshot has a negative price".to_owned())
        );
        let zero = parse_pricing(r#"{"input_per_mtok": -0, "output_per_mtok": -0.0}"#)
            .expect("negative zero is not negative");
        assert_eq!(zero.input_per_mtok, 0.0);
    }

    /// DIVERGENCE: D44. Both cache prices, one, null, and absent. A null or absent cache
    /// price is `None`, never 0, and a zero price is `Some(0.0)`.
    #[test]
    fn cache_prices_are_optional() {
        let both = parse_pricing(
            r#"{"input_per_mtok": 3, "output_per_mtok": 15, "Cache_Read_Per_Mtok": 0.3, "cache_write_per_mtok": 3.75}"#,
        )
        .expect("parses");
        assert_eq!(both.cache_read_per_mtok, Some(0.3));
        assert_eq!(both.cache_write_per_mtok, Some(3.75));
        assert_eq!(both.input_per_mtok, 3.0);

        let read_only = parse_pricing(r#"{"cache_read_per_mtok": 0}"#).expect("parses");
        assert_eq!(read_only.cache_read_per_mtok, Some(0.0));
        assert_eq!(read_only.cache_write_per_mtok, None);

        let null = parse_pricing(r#"{"cache_read_per_mtok": null, "cache_write_per_mtok": 1}"#)
            .expect("parses");
        assert_eq!(null.cache_read_per_mtok, None);
        assert_eq!(null.cache_write_per_mtok, Some(1.0));

        let absent = parse_pricing(r#"{"input_per_mtok": 1}"#).expect("parses");
        assert_eq!(absent.cache_read_per_mtok, None);
        assert_eq!(absent.cache_write_per_mtok, None);
    }

    /// DIVERGENCE: D44. A negative cache price is a negative price, and a wrong type is a
    /// decode error naming the field. Negative zero is not negative.
    #[test]
    fn cache_prices_are_checked() {
        for raw in [
            r#"{"cache_read_per_mtok": -0.1}"#,
            r#"{"cache_write_per_mtok": -2}"#,
        ] {
            assert_eq!(
                parse_pricing(raw),
                Err("pricing snapshot has a negative price".to_owned()),
                "{raw}"
            );
        }
        assert_eq!(
            parse_pricing(r#"{"cache_write_per_mtok": "1"}"#),
            Err("parsing pricing snapshot: json: cannot unmarshal string into Go struct field Pricing.cache_write_per_mtok of type float64".to_owned())
        );
        let zero = parse_pricing(r#"{"cache_read_per_mtok": -0.0, "cache_write_per_mtok": -0}"#)
            .expect("negative zero is not negative");
        assert_eq!(zero.cache_read_per_mtok, Some(0.0));
        assert_eq!(zero.cache_write_per_mtok, Some(0.0));
    }

    #[test]
    fn cost_numeric_has_six_places() {
        assert_eq!(cost_numeric(0.0), "0.000000");
        assert_eq!(cost_numeric(118.518516), "118.518516");
    }
}
