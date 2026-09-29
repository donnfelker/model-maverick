//! The `judge_rubric` grader (spec §5.5, reference Part 2 §11).
//!
//! A judge model from a different family than the graded column scores each rubric
//! dimension in `[0, 1]`. Every output is judged twice, the second time with the rubric
//! reversed; a dimension's score is the mean of the two, and the absolute difference is
//! recorded as the order-swap disagreement. Judges see the task prompt and the reference
//! material; benchmarked models never see the reference (G7).
//!
//! The grader calls the judge through [`JudgeClient`] and resolves the graded column's
//! family through [`FamilyResolver`]; the runner and jobs wire real providers and the
//! catalog to these traits, and tests use scripted fakes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;

use super::envelope::GraderKind;
use super::registry::{BoxError, BoxFuture, GradeInput, GradeResult, Grader};
use super::spec::{SpecValidator, check_schema};
use crate::gofmt;
use crate::gojson::{self, JsonValue};
use crate::tasks;

/// The judge's maximum reply length, in tokens.
pub const JUDGE_MAX_TOKENS: usize = 1024;

/// The default `MAVERICK_JUDGE_POOL`.
pub const DEFAULT_POOL: &str = "gemini-3.1-pro/direct,gpt-5.5/direct";

/// The judge system prompt (exact).
pub const SYSTEM_PROMPT: &str = "You are an impartial grading judge. Score the output under evaluation against each rubric dimension on a scale from 0.0 to 1.0, anchoring your judgment to the reference material when it is provided. Respond with only a JSON object of the form {\"scores\": {\"<dimension>\": <number>}} covering every rubric dimension exactly once. No prose, no markdown.";

/// A plain-text completion request to a judge: temperature 0, no settings, so every
/// provider receives an ordinary completion.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeRequest {
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub temperature: f32,
    pub max_tokens: usize,
}

/// Calls a judge model and returns its reply text.
pub trait JudgeClient: Send + Sync {
    fn complete<'a>(&'a self, request: JudgeRequest) -> BoxFuture<'a, Result<String, BoxError>>;
}

/// Resolves a model column to its family (its provider).
pub trait FamilyResolver: Send + Sync {
    fn family<'a>(&'a self, column_id: &'a str) -> BoxFuture<'a, Result<String, BoxError>>;
}

/// One judge pool member.
#[derive(Clone)]
pub struct Judge {
    /// `model/endpoint`, as configured.
    pub reference: String,
    pub family: String,
    pub model: String,
    pub client: Option<Arc<dyn JudgeClient>>,
}

impl std::fmt::Debug for Judge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Judge")
            .field("reference", &self.reference)
            .field("family", &self.family)
            .field("model", &self.model)
            .field("client", &self.client.is_some())
            .finish()
    }
}

/// The judge_rubric spec.
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeRubricSpec {
    #[serde(default)]
    pub rubric: Vec<RubricItem>,
    #[serde(default)]
    pub judge_prompt: String,
    #[serde(default)]
    pub reference: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RubricItem {
    #[serde(default)]
    pub dimension: String,
    #[serde(default)]
    pub criterion: String,
}

/// The judge_rubric grader. [`JudgeRubric::validator`] is the validation-only instance
/// the corpus loader uses; [`JudgeRubric::new`] wires a judge pool for grading.
pub struct JudgeRubric {
    pool: Vec<Judge>,
    resolver: Option<Arc<dyn FamilyResolver>>,
}

impl std::fmt::Debug for JudgeRubric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeRubric")
            .field("pool", &self.pool)
            .field("grading", &self.resolver.is_some())
            .finish()
    }
}

impl JudgeRubric {
    /// A validation-only instance (`NewValidator()`): it validates specs, and grading
    /// fails.
    pub fn validator() -> Self {
        JudgeRubric {
            pool: Vec::new(),
            resolver: None,
        }
    }

    /// `New(pool, resolve)`: exactly two judges from two families, each with a ref,
    /// family, model, and client, plus a family resolver.
    pub fn new(
        pool: Vec<Judge>,
        resolver: Option<Arc<dyn FamilyResolver>>,
    ) -> Result<Self, String> {
        if pool.len() != 2 {
            return Err(format!(
                "judgerubric: pool has {} members; spec 5.5 requires 2",
                pool.len()
            ));
        }
        for (i, judge) in pool.iter().enumerate() {
            if judge.reference.is_empty() {
                return Err(format!("judgerubric: pool[{i}] has no ref"));
            }
            let missing = if judge.family.is_empty() {
                "family"
            } else if judge.model.is_empty() {
                "model"
            } else if judge.client.is_none() {
                "provider client"
            } else {
                continue;
            };
            return Err(format!(
                "judgerubric: pool[{i}] ({}) has no {missing}",
                judge.reference
            ));
        }
        if pool[0].family == pool[1].family {
            return Err(format!(
                "judgerubric: pool members {} and {} share family {}; spec 5.5 requires two families",
                pool[0].reference,
                pool[1].reference,
                gofmt::quote(&pool[0].family)
            ));
        }
        let Some(resolver) = resolver else {
            return Err("judgerubric: a judged-family resolver is required".to_owned());
        };
        Ok(JudgeRubric {
            pool,
            resolver: Some(resolver),
        })
    }

    /// `selectJudge`: the first pool member whose family differs from the judged one.
    fn select_judge(&self, judged_family: &str) -> Result<&Judge, String> {
        if judged_family.is_empty() {
            return Err("judge_rubric: judged column resolved to an empty family".to_owned());
        }
        self.pool
            .iter()
            .find(|judge| judge.family != judged_family)
            .ok_or_else(|| {
                format!(
                    "judge_rubric: no cross-family judge for family {} in the pool",
                    gofmt::quote(judged_family)
                )
            })
    }
}

/// A catalog column, as `PoolFromConfig` reads it.
#[derive(Debug, Clone, Copy)]
pub struct PoolColumn<'a> {
    pub model_id: &'a str,
    pub endpoint: &'a str,
    pub provider: &'a str,
}

/// `splitRef`: a pool ref splits at its last `/` into model and endpoint; both must be
/// non-empty.
pub fn split_ref(reference: &str) -> Option<(&str, &str)> {
    let (model, endpoint) = reference.rsplit_once('/')?;
    (!model.is_empty() && !endpoint.is_empty()).then_some((model, endpoint))
}

/// `PoolFromConfig(refs, columns, clients)`: each ref resolves to the first column
/// (catalog order) with that model and endpoint, and the client for its provider. The
/// judge's family is the column's provider.
pub fn pool_from_config(
    refs: &[&str],
    columns: &[PoolColumn<'_>],
    clients: &HashMap<String, Arc<dyn JudgeClient>>,
) -> Result<Vec<Judge>, String> {
    refs.iter()
        .map(|reference| {
            let (model, endpoint) = split_ref(reference).ok_or_else(|| {
                format!(
                    "judgerubric: pool ref {} is not \"model/endpoint\"",
                    gofmt::quote(reference)
                )
            })?;
            let column = columns
                .iter()
                .find(|c| c.model_id == model && c.endpoint == endpoint)
                .ok_or_else(|| {
                    format!(
                        "judgerubric: no model column matches pool ref {}",
                        gofmt::quote(reference)
                    )
                })?;
            let client = clients.get(column.provider).ok_or_else(|| {
                format!(
                    "judgerubric: no provider client configured for {} (pool ref {})",
                    gofmt::quote(column.provider),
                    gofmt::quote(reference)
                )
            })?;
            Ok(Judge {
                reference: (*reference).to_owned(),
                family: column.provider.to_owned(),
                model: column.model_id.to_owned(),
                client: Some(client.clone()),
            })
        })
        .collect()
}

impl SpecValidator for JudgeRubric {
    fn kind(&self) -> &str {
        GraderKind::JudgeRubric.as_str()
    }

    /// Schema, then in rubric order a duplicate dimension (reported at the second
    /// occurrence) or a whitespace-only criterion.
    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        let value = check_schema(GraderKind::JudgeRubric, spec)?;
        let spec: JudgeRubricSpec =
            serde_json::from_value(value).map_err(|e| format!("judge_rubric spec: {e}"))?;
        let mut seen = HashSet::new();
        for (i, item) in spec.rubric.iter().enumerate() {
            if !seen.insert(item.dimension.as_str()) {
                return Err(format!(
                    "judge_rubric spec: rubric[{i}]: duplicate dimension {}",
                    gofmt::quote(&item.dimension)
                ));
            }
            // Rust's `trim` uses the same whitespace set as Go's `strings.TrimSpace`.
            if item.criterion.trim().is_empty() {
                return Err(format!(
                    "judge_rubric spec: rubric[{i}] ({}): criterion is blank",
                    item.dimension
                ));
            }
        }
        Ok(())
    }
}

impl Grader for JudgeRubric {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move { self.grade_run(spec, input).await.map_err(BoxError::from) })
    }
}

impl JudgeRubric {
    async fn grade_run(
        &self,
        spec: &JsonValue,
        input: GradeInput<'_>,
    ) -> Result<GradeResult, String> {
        let Some(resolver) = &self.resolver else {
            return Err("judge_rubric: validation-only instance cannot grade (wire New with a judge pool and family resolver)".to_owned());
        };
        let spec: JudgeRubricSpec = serde_json::from_value(spec.to_serde()?)
            .map_err(|e| format!("judge_rubric: decode spec: {e}"))?;
        if spec.rubric.is_empty() {
            return Err("judge_rubric: spec has no rubric items".to_owned());
        }
        let Some(task) = input.task else {
            return Err(
                "judge_rubric: grading requires the task (reference-anchored inputs, spec 5.5)"
                    .to_owned(),
            );
        };
        let column = match input.run {
            Some(run) if !run.model_column_id.is_empty() => run.model_column_id.as_str(),
            _ => return Err("judge_rubric: grading requires the judged model column".to_owned()),
        };
        let judged_family = resolver
            .family(column)
            .await
            .map_err(|e| format!("judge_rubric: resolving family of column {column}: {e}"))?;
        let judge = self.select_judge(&judged_family)?;
        let client = judge
            .client
            .as_ref()
            .expect("New requires a client per judge");
        let attachments = tasks::parse_attachments(&task.attachments_json)
            .map_err(|e| format!("judge_rubric: assembling task prompt: {e}"))?;
        let task_prompt = tasks::prompt_text(&task.prompt, &attachments);

        let forward: Vec<&RubricItem> = spec.rubric.iter().collect();
        let reversed: Vec<&RubricItem> = spec.rubric.iter().rev().collect();
        let mut calls = Vec::with_capacity(2);
        for (n, order) in [(1, &forward), (2, &reversed)] {
            let request = JudgeRequest {
                model: judge.model.clone(),
                system: SYSTEM_PROMPT.to_owned(),
                prompt: judge_prompt(&spec, &task_prompt, order, input.output),
                temperature: 0.0,
                max_tokens: JUDGE_MAX_TOKENS,
            };
            let at = |e: String| format!("judge_rubric: judge {} call {n}/2: {e}", judge.reference);
            let reply = client
                .complete(request)
                .await
                .map_err(|e| at(e.to_string()))?;
            let scores = parse_scores(&reply, &spec.rubric).map_err(at)?;
            calls.push((order, scores));
        }

        let (first, second) = (&calls[0].1, &calls[1].1);
        let mut dimensions = BTreeMap::new();
        let mut disagreement = BTreeMap::new();
        for item in &spec.rubric {
            let (a, b) = (first[&item.dimension], second[&item.dimension]);
            dimensions.insert(item.dimension.clone(), (a + b) / 2.0);
            disagreement.insert(item.dimension.clone(), (a - b).abs());
        }

        let order_swap = calls
            .iter()
            .map(|(order, scores)| {
                Ok(JsonValue::Object(vec![
                    (
                        "rubric_order".to_owned(),
                        JsonValue::Array(
                            order
                                .iter()
                                .map(|item| JsonValue::String(item.dimension.clone()))
                                .collect(),
                        ),
                    ),
                    ("scores".to_owned(), float_map(scores)?),
                ]))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let detail = JsonValue::Object(vec![
            (
                "judge".to_owned(),
                JsonValue::Object(vec![
                    ("ref".to_owned(), JsonValue::String(judge.reference.clone())),
                    ("family".to_owned(), JsonValue::String(judge.family.clone())),
                    ("model".to_owned(), JsonValue::String(judge.model.clone())),
                ]),
            ),
            ("judged_family".to_owned(), JsonValue::String(judged_family)),
            ("order_swap".to_owned(), JsonValue::Array(order_swap)),
            ("disagreement".to_owned(), float_map(&disagreement)?),
        ]);
        Ok(GradeResult {
            dimensions,
            detail: gojson::marshal(&detail),
        })
    }
}

/// A sorted map of floats in Go's JSON form.
fn float_map(map: &BTreeMap<String, f64>) -> Result<JsonValue, String> {
    Ok(JsonValue::Object(
        map.iter()
            .map(|(k, &v)| Ok((k.clone(), JsonValue::Number(gojson::encode_f64(v)?))))
            .collect::<Result<_, String>>()?,
    ))
}

/// The judge prompt for one call, with the rubric in that call's order (exact bytes).
fn judge_prompt(
    spec: &JudgeRubricSpec,
    task_prompt: &str,
    order: &[&RubricItem],
    output: &str,
) -> String {
    let mut prompt = String::new();
    if !spec.judge_prompt.is_empty() {
        prompt.push_str(&spec.judge_prompt);
        prompt.push_str("\n\n");
    }
    prompt.push_str("--- task prompt ---\n");
    prompt.push_str(task_prompt);
    prompt.push_str("\n\n--- rubric ---\n");
    for (i, item) in order.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. {}: {}\n",
            i + 1,
            item.dimension,
            item.criterion
        ));
    }
    if !spec.reference.is_empty() {
        prompt.push_str("\n--- reference material ---\n");
        prompt.push_str(&spec.reference);
        prompt.push('\n');
    }
    prompt.push_str("\n--- output under evaluation ---\n");
    prompt.push_str(output);
    prompt.push_str("\n\n--- response format ---\n{\"scores\": {");
    for (i, item) in order.iter().enumerate() {
        if i > 0 {
            prompt.push_str(", ");
        }
        prompt.push_str(&gofmt::quote(&item.dimension));
        prompt.push_str(": <0.0-1.0>");
    }
    prompt.push_str("}}");
    prompt
}

/// `stripFence`: a reply wrapped in ```json ... ``` or ``` ... ``` is unwrapped; with no
/// closing fence the body is left as it is (and fails to parse).
fn strip_fence(body: &str) -> &str {
    let Some(rest) = body
        .strip_prefix("```json")
        .or_else(|| body.strip_prefix("```"))
    else {
        return body;
    };
    match rest.trim().strip_suffix("```") {
        Some(inner) => inner.trim(),
        None => body,
    }
}

/// `parseScores(reply, rubric)`: strict. One JSON object with only a `scores` field
/// (matched case-insensitively; a repeated `scores` merges, as Go decodes into the same
/// map), numbers in `[0, 1]` for exactly the rubric's dimensions; `null` scores are 0.
pub fn parse_scores(reply: &str, rubric: &[RubricItem]) -> Result<BTreeMap<String, f64>, String> {
    let body = strip_fence(reply.trim());
    let parse = |e: String| format!("parsing judge reply: {e}");
    let (value, rest) = gojson::decode_one(body).map_err(|e| parse(e.0))?;

    let mut scores: Option<BTreeMap<String, f64>> = None;
    let mut first_error: Option<String> = None;
    let mut save = |e: String| {
        first_error.get_or_insert(e);
    };
    match value {
        JsonValue::Null => {}
        JsonValue::Object(members) => {
            for (key, value) in members {
                if gojson::match_field(&key, &["scores"]).is_none() {
                    save(format!("json: unknown field {}", gofmt::quote(&key)));
                    continue;
                }
                match value {
                    JsonValue::Null => scores = None,
                    JsonValue::Object(entries) => {
                        let map = scores.get_or_insert_with(BTreeMap::new);
                        for (dim, score) in entries {
                            let score = match score {
                                JsonValue::Null => 0.0,
                                JsonValue::Number(literal) => match literal.parse::<f64>() {
                                    Ok(v) if v.is_finite() => v,
                                    _ => {
                                        save(format!(
                                            "json: cannot unmarshal number {literal} into Go struct field .scores of type float64"
                                        ));
                                        0.0
                                    }
                                },
                                other => {
                                    save(format!(
                                        "json: cannot unmarshal {} into Go struct field .scores of type float64",
                                        other.kind_name()
                                    ));
                                    0.0
                                }
                            };
                            map.insert(dim, score);
                        }
                    }
                    other => save(format!(
                        "json: cannot unmarshal {} into Go struct field .scores of type map[string]float64",
                        other.kind_name()
                    )),
                }
            }
        }
        other => save(format!(
            "json: cannot unmarshal {} into Go value of type struct {{ Scores map[string]float64 \"json:\\\"scores\\\"\" }}",
            other.kind_name()
        )),
    }
    if let Some(error) = first_error {
        return Err(parse(error));
    }
    if !gojson::is_blank(rest) {
        return Err(parse("trailing data after the scores object".to_owned()));
    }
    let scores = match scores {
        Some(scores) if !scores.is_empty() => scores,
        _ => return Err("judge reply has no scores".to_owned()),
    };
    for item in rubric {
        let Some(&score) = scores.get(&item.dimension) else {
            return Err(format!(
                "judge reply is missing dimension {}",
                gofmt::quote(&item.dimension)
            ));
        };
        if !(0.0..=1.0).contains(&score) {
            return Err(format!(
                "judge reply scores {} = {}, outside [0, 1]",
                gofmt::quote(&item.dimension),
                gofmt::fmt_v_f64(score)
            ));
        }
    }
    let dims: HashSet<&str> = rubric.iter().map(|item| item.dimension.as_str()).collect();
    let extras: Vec<&str> = scores
        .keys()
        .map(String::as_str)
        .filter(|d| !dims.contains(d))
        .collect();
    if !extras.is_empty() {
        return Err(format!(
            "judge reply scores dimensions [{}] that are not in the rubric",
            extras.join(" ")
        ));
    }
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rubric(dims: &[&str]) -> Vec<RubricItem> {
        dims.iter()
            .map(|d| RubricItem {
                dimension: (*d).to_owned(),
                criterion: "c".to_owned(),
            })
            .collect()
    }

    #[test]
    fn fences() {
        assert_eq!(strip_fence("```json\n{}\n```"), "{}");
        assert_eq!(strip_fence("```\n{}\n```"), "{}");
        assert_eq!(strip_fence("```json\n{}"), "```json\n{}");
        assert_eq!(strip_fence("{}"), "{}");
        assert_eq!(strip_fence("```JSON\n{}\n```"), "JSON\n{}");
    }

    #[test]
    fn parse_scores_edges() {
        let r = rubric(&["a"]);
        assert_eq!(
            parse_scores(r#"{"scores":{"a":1e400}}"#, &r).unwrap_err(),
            "parsing judge reply: json: cannot unmarshal number 1e400 into Go struct field .scores of type float64"
        );
        assert_eq!(
            parse_scores(r#"{"scores":[1]}"#, &r).unwrap_err(),
            "parsing judge reply: json: cannot unmarshal array into Go struct field .scores of type map[string]float64"
        );
        assert_eq!(
            parse_scores(r#"{"scores":{"a":0.5},"scores":null}"#, &r).unwrap_err(),
            "judge reply has no scores"
        );
        assert_eq!(
            parse_scores(r#"{"scores":{"a":0.5},"scores":{"b":0.1}}"#, &r).unwrap_err(),
            "judge reply scores dimensions [b] that are not in the rubric"
        );
        assert_eq!(
            parse_scores(r#"{"x":1,"scores":{"a":"s"}}"#, &r).unwrap_err(),
            "parsing judge reply: json: unknown field \"x\""
        );
        assert_eq!(
            parse_scores(r#""s""#, &r).unwrap_err(),
            "parsing judge reply: json: cannot unmarshal string into Go value of type struct { Scores map[string]float64 \"json:\\\"scores\\\"\" }"
        );
        assert_eq!(
            parse_scores(
                r#"{"scores":{"a":0,"b":1,"c":2}}"#,
                &rubric(&["a", "b", "c"])
            )
            .unwrap_err(),
            "judge reply scores \"c\" = 2, outside [0, 1]"
        );
        assert_eq!(
            parse_scores(r#"{"scores":{"z":1,"y":0,"a":1}}"#, &r).unwrap_err(),
            "judge reply scores dimensions [y z] that are not in the rubric"
        );
    }

    /// Go builds these pools by hand inside the package; `new` would refuse them.
    #[test]
    fn select_judge_errors() {
        let member = |family: &str| Judge {
            reference: format!("{family}/direct"),
            family: family.to_owned(),
            model: family.to_owned(),
            client: None,
        };
        let grader = JudgeRubric {
            pool: vec![member("google"), member("google")],
            resolver: None,
        };
        assert_eq!(
            grader.select_judge("google").unwrap_err(),
            "judge_rubric: no cross-family judge for family \"google\" in the pool"
        );
        assert_eq!(
            grader.select_judge("").unwrap_err(),
            "judge_rubric: judged column resolved to an empty family"
        );
        assert_eq!(grader.select_judge("openai").unwrap().family, "google");
        let mixed = JudgeRubric {
            pool: vec![member("google"), member("openai")],
            resolver: None,
        };
        assert_eq!(mixed.select_judge("google").unwrap().family, "openai");
        assert_eq!(mixed.select_judge("xai").unwrap().family, "google");
    }

    #[test]
    fn debug_shows_the_pool_without_clients() {
        assert_eq!(
            format!("{:?}", JudgeRubric::validator()),
            "JudgeRubric { pool: [], grading: false }"
        );
        let judge = Judge {
            reference: "m/direct".to_owned(),
            family: "google".to_owned(),
            model: "m".to_owned(),
            client: None,
        };
        assert_eq!(
            format!("{judge:?}"),
            "Judge { reference: \"m/direct\", family: \"google\", model: \"m\", client: false }"
        );
    }

    #[test]
    fn split_ref_rules() {
        assert_eq!(split_ref("a/b/c"), Some(("a/b", "c")));
        assert_eq!(split_ref("a//b"), Some(("a/", "b")));
        assert_eq!(split_ref("/x"), None);
        assert_eq!(split_ref("x/"), None);
        assert_eq!(split_ref("x"), None);
    }
}
