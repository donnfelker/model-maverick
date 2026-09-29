//! A column's pricing snapshot and the cost of a run (spec §4.4, reference Part 2 §5.4
//! and Part 3 §5).
//!
//! Each column snapshots its list price per million tokens. A run's cost comes from the
//! snapshot at execution time and is never restated.

use crate::gojson::{self, JsonValue};

/// `catalog.Pricing`: prices in USD per million tokens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub currency: String,
    pub as_of: String,
    pub source: String,
}

const FIELDS: [&str; 5] = [
    "input_per_mtok",
    "output_per_mtok",
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
/// `pricing snapshot has a negative price`.
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
            "currency" => pricing.currency = text(field, value).map_err(error)?,
            "as_of" => pricing.as_of = text(field, value).map_err(error)?,
            _ => pricing.source = text(field, value).map_err(error)?,
        }
    }
    if pricing.input_per_mtok < 0.0 || pricing.output_per_mtok < 0.0 {
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

    #[test]
    fn cost_numeric_has_six_places() {
        assert_eq!(cost_numeric(0.0), "0.000000");
        assert_eq!(cost_numeric(118.518516), "118.518516");
    }
}
