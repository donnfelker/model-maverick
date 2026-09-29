//! Parity: `parity/fixtures/measurement-io/judge_rubric.json`
//! (`internal/graders/judgerubric`).

mod common;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::block_on;
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::graders::judge_rubric::{
    DEFAULT_POOL, FamilyResolver, Judge, JudgeClient, JudgeRequest, PoolColumn, RubricItem,
    SYSTEM_PROMPT, parse_scores, pool_from_config, split_ref,
};
use maverick_domain::graders::{
    BoxError, BoxFuture, GradeError, GradeInput, GradeTask, Grader, JudgeRubric, Registry, RunMeta,
    SpecValidator,
};
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("measurement-io", "judge_rubric.json").expect("judge_rubric.json loads")
}

fn testdata_dir() -> PathBuf {
    parity_support::fixture_path(
        "measurement-io",
        "testdata/internal/graders/judgerubric/testdata",
    )
    .expect("testdata dir")
}

fn spec(raw: &str) -> JsonValue {
    gojson::decode_one(raw).expect("spec json").0
}

/// A judge that replies from a script and records every request.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Result<String, String>>>,
    requests: Mutex<Vec<JudgeRequest>>,
}

impl Scripted {
    fn new(replies: &[Result<&str, &str>]) -> Arc<Self> {
        Arc::new(Scripted {
            replies: Mutex::new(
                replies
                    .iter()
                    .map(|r| r.map(str::to_owned).map_err(str::to_owned))
                    .collect(),
            ),
            requests: Mutex::default(),
        })
    }

    fn repeating(reply: &str) -> Arc<Self> {
        Self::new(&[Ok(reply); 8])
    }

    fn requests(&self) -> Vec<JudgeRequest> {
        self.requests.lock().expect("lock").clone()
    }
}

impl JudgeClient for Scripted {
    fn complete<'a>(&'a self, request: JudgeRequest) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            self.requests.lock().expect("lock").push(request);
            let reply = self.replies.lock().expect("lock").pop_front();
            reply.expect("a scripted reply").map_err(BoxError::from)
        })
    }
}

/// Go's position-biased test judge: after `--- response format ---`, the i-th
/// `"<dim>": <0.0-1.0>` scores `1.0 - 0.4 * i`.
struct PositionBiased;

impl JudgeClient for PositionBiased {
    fn complete<'a>(&'a self, request: JudgeRequest) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            let format = request
                .prompt
                .split_once("--- response format ---")
                .map(|(_, rest)| rest)
                .unwrap_or("");
            let pattern = regex::Regex::new(r#""([a-z][a-z0-9_]*)": <0\.0-1\.0>"#).expect("regex");
            let scores: BTreeMap<String, f64> = pattern
                .captures_iter(format)
                .enumerate()
                .map(|(i, c)| (c[1].to_owned(), 1.0 - 0.4 * i as f64))
                .collect();
            let members = scores
                .into_iter()
                .map(|(k, v)| (k, JsonValue::Number(gojson::encode_f64(v).expect("finite"))))
                .collect();
            Ok(gojson::marshal(&JsonValue::Object(vec![(
                "scores".to_owned(),
                JsonValue::Object(members),
            )])))
        })
    }
}

/// Families by column id; an unknown column is an error.
struct Families(HashMap<String, String>);

impl Families {
    fn of(pairs: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Families(
            pairs
                .iter()
                .map(|(c, f)| ((*c).to_owned(), (*f).to_owned()))
                .collect(),
        ))
    }
}

impl FamilyResolver for Families {
    fn family<'a>(&'a self, column_id: &'a str) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            self.0
                .get(column_id)
                .cloned()
                .ok_or_else(|| format!("unknown column {column_id:?}").into())
        })
    }
}

fn judge(reference: &str, family: &str, model: &str, client: Arc<dyn JudgeClient>) -> Judge {
    Judge {
        reference: reference.to_owned(),
        family: family.to_owned(),
        model: model.to_owned(),
        client: Some(client),
    }
}

/// The default pool: gemini (google) first, gpt-5.5 (openai) second.
fn pool(primary: Arc<dyn JudgeClient>, secondary: Arc<dyn JudgeClient>) -> Vec<Judge> {
    vec![
        judge("gemini-3.1-pro/direct", "google", "gemini-3.1-pro", primary),
        judge("gpt-5.5/direct", "openai", "gpt-5.5", secondary),
    ]
}

fn grader(primary: Arc<dyn JudgeClient>, families: &[(&str, &str)]) -> JudgeRubric {
    JudgeRubric::new(
        pool(primary, Scripted::new(&[])),
        Some(Families::of(families)),
    )
    .expect("valid pool")
}

fn two_dim() -> Value {
    fixture()["two_dim_case"].clone()
}

fn two_dim_task() -> GradeTask {
    let task = &two_dim()["task"];
    GradeTask {
        id: task["id"].as_str().expect("id").to_owned(),
        capability: task["capability"].as_str().expect("capability").to_owned(),
        prompt: task["prompt"].as_str().expect("prompt").to_owned(),
        attachments_json: task["attachments_json"]
            .as_str()
            .expect("attachments")
            .to_owned(),
    }
}

fn run(column: &str) -> RunMeta {
    RunMeta {
        model_column_id: column.to_owned(),
        status: "ok".to_owned(),
        ..RunMeta::default()
    }
}

fn error_string(case: &str) -> String {
    fixture()["error_strings"]
        .as_array()
        .expect("errors")
        .iter()
        .find(|e| e["case"] == case)
        .unwrap_or_else(|| panic!("no case {case}"))["error"]
        .as_str()
        .expect("error")
        .to_owned()
}

fn replies_of(case: &Value) -> Vec<Result<&str, &str>> {
    case["scripted_replies_in_call_order"]
        .as_array()
        .expect("replies")
        .iter()
        .map(|r| Ok(r.as_str().expect("reply")))
        .collect()
}

fn assert_requests(got: &[JudgeRequest], want: &Value) {
    let want = want.as_array().expect("requests");
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want) {
        assert_eq!(g.model, w["Model"].as_str().expect("model"));
        assert_eq!(g.system, w["System"].as_str().expect("system"));
        assert_eq!(g.prompt, w["Prompt"].as_str().expect("prompt"));
        assert_eq!(
            f64::from(g.temperature),
            w["Temperature"].as_f64().expect("t")
        );
        assert_eq!(g.max_tokens as u64, w["MaxTokens"]);
        assert!(w["Settings"].is_null());
    }
}

/// The manifest points at `judge_rubric.json#go_tests`, which the fixture does not
/// have; Go's TestKind asserts the kind name.
///
/// parity: judgerubric.TestKind
#[test]
fn kind() {
    assert_eq!(JudgeRubric::validator().kind(), "judge_rubric");
    assert_eq!(
        fixture()["system_prompt_exact"].as_str(),
        Some(SYSTEM_PROMPT)
    );
}

/// parity: judgerubric.TestOrderSwapAveragingGolden
/// parity: judgerubric.TestJudgeCallsOnTheWire
#[test]
fn two_dim_case() {
    let case = two_dim();
    let client = Scripted::new(&replies_of(&case));
    let grader = grader(client.clone(), &[("col_judged", "anthropic")]);
    let spec = spec(case["spec_raw"].as_str().expect("spec"));
    grader.validate_spec(&spec).expect("valid spec");
    let task = two_dim_task();
    let run = run(case["judged_column"].as_str().expect("column"));
    let input = GradeInput {
        task: Some(&task),
        output: case["output"].as_str().expect("output"),
        run: Some(&run),
    };
    let result = block_on(grader.grade(&spec, input)).expect("grades");
    let tolerance = case["tolerance"].as_f64().expect("tolerance");
    let want: BTreeMap<String, f64> =
        serde_json::from_value(case["expected_dimensions"].clone()).expect("dims");
    assert_eq!(result.dimensions.len(), want.len());
    for (dim, w) in &want {
        assert!((result.dimensions[dim] - w).abs() <= tolerance, "{dim}");
    }
    assert_eq!(result.detail, case["expected_detail_exact_bytes"]);
    assert_requests(&client.requests(), &case["expected_judge_requests"]);
}

/// No judge_prompt and no reference; the openai-judged output goes to the google judge.
#[test]
fn minimal_case_openai_judged() {
    let case = fixture()["minimal_case_openai_judged"].clone();
    let client = Scripted::new(&replies_of(&case));
    let grader = grader(client.clone(), &[("col_openai", "openai")]);
    let spec = spec(case["spec_raw"].as_str().expect("spec"));
    let task = GradeTask {
        prompt: case["task_prompt"].as_str().expect("prompt").to_owned(),
        ..GradeTask::default()
    };
    let run = run("col_openai");
    let input = GradeInput {
        task: Some(&task),
        output: case["output"].as_str().expect("output"),
        run: Some(&run),
    };
    let result = block_on(grader.grade(&spec, input)).expect("grades");
    let want: BTreeMap<String, f64> =
        serde_json::from_value(case["expected_dimensions"].clone()).expect("dims");
    assert_eq!(result.dimensions, want);
    assert_requests(&client.requests(), &case["expected_judge_requests"]);
}

/// parity: judgerubric.TestFamilySelectionTable
#[test]
fn family_selection_table() {
    let table = fixture()["family_selection"].clone();
    let reply = table["reply_both_calls"].as_str().expect("reply");
    let spec = spec(two_dim()["spec_raw"].as_str().expect("spec"));
    let task = two_dim_task();
    for row in table["rows"].as_array().expect("rows") {
        let judged = row["judged"].as_str().expect("judged");
        let (primary, secondary) = (Scripted::repeating(reply), Scripted::repeating(reply));
        let grader = JudgeRubric::new(
            pool(primary.clone(), secondary.clone()),
            Some(Families::of(&[("col", judged)])),
        )
        .expect("valid pool");
        let run = run("col");
        let input = GradeInput {
            task: Some(&task),
            output: "o",
            run: Some(&run),
        };
        let result = block_on(grader.grade(&spec, input)).expect("grades");
        assert_eq!(
            primary.requests().len() as u64,
            row["calls_to_primary"],
            "{judged}"
        );
        assert_eq!(
            secondary.requests().len() as u64,
            row["calls_to_secondary"],
            "{judged}"
        );
        let family = row["judge_family"].as_str().expect("family");
        assert!(
            result.detail.contains(&format!("\"family\":\"{family}\"")),
            "{judged}"
        );
        let model = row["judge_model"].as_str().expect("model");
        assert!(
            result.detail.contains(&format!("\"model\":\"{model}\"")),
            "{judged}"
        );
    }
}

/// A `New` case: the fixture error name, the pool, and the resolver.
type PoolCase = (&'static str, Vec<Judge>, Option<Arc<dyn FamilyResolver>>);

/// parity: judgerubric.TestNewRejectsInvalidPools
#[test]
fn new_rejects_invalid_pools() {
    let resolver = || -> Option<Arc<dyn FamilyResolver>> { Some(Families::of(&[])) };
    let client = || -> Arc<dyn JudgeClient> { Scripted::new(&[]) };
    let gemini = || {
        judge(
            "gemini-3.1-pro/direct",
            "google",
            "gemini-3.1-pro",
            client(),
        )
    };
    let gpt = || judge("gpt-5.5/direct", "openai", "gpt-5.5", client());
    let broken = |edit: fn(&mut Judge)| {
        let mut judge = gpt();
        edit(&mut judge);
        vec![gemini(), judge]
    };
    let cases: Vec<PoolCase> = vec![
        (
            "New missing client",
            broken(|j| j.client = None),
            resolver(),
        ),
        (
            "New missing family",
            broken(|j| j.family.clear()),
            resolver(),
        ),
        ("New missing model", broken(|j| j.model.clear()), resolver()),
        (
            "New missing ref",
            broken(|j| j.reference.clear()),
            resolver(),
        ),
        ("New empty pool", Vec::new(), resolver()),
        ("New one member", vec![gemini()], resolver()),
        (
            "New same family",
            vec![
                gemini(),
                judge(
                    "gemini-3-flash/direct",
                    "google",
                    "gemini-3-flash",
                    client(),
                ),
            ],
            resolver(),
        ),
        ("New missing resolver", vec![gemini(), gpt()], None),
    ];
    for (case, pool, resolver) in cases {
        let error = JudgeRubric::new(pool, resolver).expect_err(case);
        assert_eq!(error, error_string(case), "{case}");
    }
    // Checks run per member in order: member 0's missing model is reported first.
    let mut first = gemini();
    first.model.clear();
    let mut second = gpt();
    second.reference.clear();
    assert_eq!(
        JudgeRubric::new(vec![first, second], resolver()).unwrap_err(),
        "judgerubric: pool[0] (gemini-3.1-pro/direct) has no model"
    );
}

/// parity: judgerubric.TestJudgeCallFailureLeavesDimensionsAbsent
#[test]
fn judge_call_failure_leaves_dimensions_absent() {
    let spec = spec(two_dim()["spec_raw"].as_str().expect("spec"));
    let task = two_dim_task();
    let run = run("col_judged");
    let input = GradeInput {
        task: Some(&task),
        output: "o",
        run: Some(&run),
    };

    let client = Scripted::new(&[Err("scripted judge outage")]);
    let mut registry = Registry::new();
    registry.must_register(Arc::new(grader(
        client.clone(),
        &[("col_judged", "anthropic")],
    )));
    let error = block_on(registry.grade("judge_rubric", &spec, input)).expect_err("fails");
    assert!(matches!(error, GradeError::Failed(_)));
    assert_eq!(error.to_string(), error_string("first call provider error"));
    // The second call is never made.
    assert_eq!(client.requests().len(), 1);

    let client = Scripted::new(&[Ok("prose")]);
    let error =
        block_on(grader(client.clone(), &[("col_judged", "anthropic")]).grade(&spec, input))
            .expect_err("fails");
    assert_eq!(error.to_string(), error_string("first call prose reply"));
    assert_eq!(client.requests().len(), 1);

    let client = Scripted::new(&[
        Ok(r#"{"scores":{"idiom":1,"readability":1}}"#),
        Err("late outage"),
    ]);
    let error =
        block_on(grader(client.clone(), &[("col_judged", "anthropic")]).grade(&spec, input))
            .expect_err("fails");
    assert_eq!(
        error.to_string(),
        "judge_rubric: judge gemini-3.1-pro/direct call 2/2: late outage"
    );
}

fn rubric(dims: &Value) -> Vec<RubricItem> {
    dims.as_array()
        .expect("rubric")
        .iter()
        .map(|d| RubricItem {
            dimension: d.as_str().expect("dim").to_owned(),
            criterion: "c".to_owned(),
        })
        .collect()
}

/// parity: judgerubric.TestStrictParseRejections
/// parity: judgerubric.TestFencedReplyAccepted
#[test]
fn parse_scores_rows() {
    let table = fixture()["parse_scores"].clone();
    let rubric = rubric(&table["rubric"]);
    let spec = spec(two_dim()["spec_raw"].as_str().expect("spec"));
    let task = two_dim_task();
    let run = run("col_judged");
    for row in table["cases"].as_array().expect("cases") {
        let reply = row["reply"].as_str().expect("reply");
        let result = parse_scores(reply, &rubric);
        if let Some(want) = row["error"].as_str() {
            assert_eq!(result.unwrap_err(), want, "{reply:?}");
        } else {
            let want: BTreeMap<String, f64> =
                serde_json::from_value(row["scores"].clone()).expect("scores");
            assert_eq!(result.expect(reply), want, "{reply:?}");
        }
        if let Some(full) = row["full_grade_error_via_registry"].as_str() {
            let mut registry = Registry::new();
            registry.must_register(Arc::new(grader(
                Scripted::new(&[Ok(reply)]),
                &[("col_judged", "anthropic")],
            )));
            let input = GradeInput {
                task: Some(&task),
                output: "o",
                run: Some(&run),
            };
            let error = block_on(registry.grade("judge_rubric", &spec, input)).expect_err(reply);
            assert_eq!(error.to_string(), full, "{reply:?}");
        }
    }
}

/// parity: judgerubric.TestGradeRequiresJudgedContext
#[test]
fn grade_requires_judged_context() {
    let spec = spec(two_dim()["spec_raw"].as_str().expect("spec"));
    let task = two_dim_task();
    let good = || {
        grader(
            Scripted::new(&[]),
            &[("col_judged", "anthropic"), ("col_empty", "")],
        )
    };
    let judged = run("col_judged");
    let grade = |grader: &JudgeRubric,
                 task: Option<&GradeTask>,
                 run: Option<&RunMeta>,
                 spec: &JsonValue| {
        let input = GradeInput {
            task,
            output: "o",
            run,
        };
        block_on(grader.grade(spec, input))
            .expect_err("fails")
            .to_string()
    };

    assert_eq!(
        grade(&good(), None, Some(&judged), &spec),
        error_string("nil task")
    );
    assert_eq!(
        grade(&good(), Some(&task), None, &spec),
        error_string("nil run")
    );
    assert_eq!(
        grade(&good(), Some(&task), Some(&run("")), &spec),
        error_string("nil run")
    );
    assert_eq!(
        grade(&good(), Some(&task), Some(&run("col_unknown")), &spec),
        error_string("resolver error")
    );
    assert_eq!(
        grade(&JudgeRubric::validator(), Some(&task), Some(&judged), &spec),
        error_string("validator instance")
    );
    assert_eq!(
        grade(
            &good(),
            Some(&task),
            Some(&judged),
            &gojson::decode_one(r#"{"rubric":[]}"#).unwrap().0
        ),
        error_string("empty rubric")
    );
    assert_eq!(
        grade(&good(), Some(&task), Some(&run("col_empty")), &spec),
        error_string("selectJudge empty family")
    );
    // "selectJudge no cross family" needs a pool New would refuse; see the unit test
    // judge_rubric::tests::select_judge_errors.
    let bad_task = GradeTask {
        attachments_json: "{broken".to_owned(),
        ..two_dim_task()
    };
    assert_eq!(
        grade(&good(), Some(&bad_task), Some(&judged), &spec),
        error_string("bad attachments")
    );
    assert!(
        grade(
            &good(),
            Some(&task),
            Some(&judged),
            &gojson::decode_one(r#"{"rubric":"x"}"#).unwrap().0
        )
        .starts_with("judge_rubric: decode spec: ")
    );

    let mut registry = Registry::new();
    registry.must_register(Arc::new(good()));
    let input = GradeInput {
        task: Some(&task),
        output: "o",
        run: Some(&judged),
    };
    assert_eq!(
        block_on(registry.grade("x", &spec, input))
            .expect_err("unknown")
            .to_string(),
        error_string("registry unknown kind x")
    );
}

/// parity: judgerubric.TestValidateSpecTable
#[test]
fn validate_spec_table() {
    let validator = JudgeRubric::validator();
    for row in fixture()["validate_spec"].as_array().expect("rows") {
        let raw = match row["spec_raw"].as_str().expect("spec_raw") {
            raw if raw.starts_with("twoDimSpec") => {
                two_dim()["spec_raw"].as_str().expect("spec").to_owned()
            }
            raw => raw.to_owned(),
        };
        let result = validator.validate_spec_json(&raw);
        if row["valid"] == true {
            result.unwrap_or_else(|e| panic!("{raw}: {e}"));
            continue;
        }
        let error = result.expect_err(&raw);
        let go_error = row["go_error"].as_str().expect("go_error");
        if let Some(at) = go_error.find("jsonschema validation failed") {
            let _ = parity_support::divergence!("D24");
            let prefix = &go_error[..at + "jsonschema validation failed".len()];
            assert!(error.starts_with(prefix), "{raw}: {error}");
        } else {
            assert_eq!(error, go_error, "{raw}");
        }
    }
}

/// parity: judgerubric.TestPoolFromConfig
#[test]
fn pool_from_config_rows() {
    let columns = [
        PoolColumn {
            model_id: "gemini-3.1-pro",
            endpoint: "direct",
            provider: "google",
        },
        PoolColumn {
            model_id: "gpt-5.5",
            endpoint: "direct",
            provider: "openai",
        },
        PoolColumn {
            model_id: "gpt-5.5",
            endpoint: "direct",
            provider: "second-match-ignored",
        },
    ];
    let mut clients: HashMap<String, Arc<dyn JudgeClient>> = HashMap::new();
    clients.insert("google".to_owned(), Scripted::new(&[]));
    clients.insert("openai".to_owned(), Scripted::new(&[]));

    let refs: Vec<&str> = DEFAULT_POOL.split(',').collect();
    let pool = pool_from_config(&refs, &columns, &clients).expect("default pool");
    let summary: Vec<(String, String, String)> = pool
        .iter()
        .map(|j| (j.reference.clone(), j.family.clone(), j.model.clone()))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "gemini-3.1-pro/direct".to_owned(),
                "google".to_owned(),
                "gemini-3.1-pro".to_owned()
            ),
            (
                "gpt-5.5/direct".to_owned(),
                "openai".to_owned(),
                "gpt-5.5".to_owned()
            ),
        ]
    );
    assert!(JudgeRubric::new(pool, Some(Families::of(&[]))).is_ok());

    for (case, refs, clients) in [
        (
            "PoolFromConfig malformed",
            vec!["gemini-3.1-pro"],
            clients.clone(),
        ),
        (
            "PoolFromConfig unknown column",
            vec!["claude-opus-5/direct"],
            clients.clone(),
        ),
        (
            "PoolFromConfig missing client",
            vec!["gemini-3.1-pro/direct"],
            HashMap::new(),
        ),
    ] {
        let error = pool_from_config(&refs, &columns, &clients).expect_err(case);
        assert_eq!(error, error_string(case), "{case}");
    }

    for (case, input) in [
        ("splitRef a/b/c", "a/b/c"),
        ("splitRef /x", "/x"),
        ("splitRef x/", "x/"),
        ("splitRef a//b", "a//b"),
    ] {
        let got = match split_ref(input) {
            Some((model, endpoint)) => format!("{model} | {endpoint} | ok"),
            None => " |  | not ok".to_owned(),
        };
        assert_eq!(got, error_string(case), "{case}");
    }
}

/// Every judge_rubric fixture through the shared harness with Go's position-biased
/// judge, then each file's exact result.
///
/// parity: judgerubric.TestGolden
#[test]
fn golden_fixtures() {
    let grader: Arc<dyn Grader> = Arc::new(
        JudgeRubric::new(
            pool(Arc::new(PositionBiased), Arc::new(PositionBiased)),
            Some(Families::of(&[("col_anthropic_fixture", "anthropic")])),
        )
        .expect("pool"),
    );
    let names = common::golden::run_dir(grader.clone(), &testdata_dir());
    let rows = fixture()["fixture_results"]
        .as_array()
        .expect("rows")
        .clone();
    assert_eq!(names.len(), rows.len());
    for row in rows {
        let file = row["file"].as_str().expect("file");
        let (fixture, spec) =
            common::golden::load_fixture(grader.kind(), &testdata_dir().join(file)).expect("loads");
        if let Some(go_error) = row["validate_error"].as_str() {
            let error = grader.validate_spec(&spec).expect_err(file);
            match go_error.find("jsonschema validation failed") {
                Some(at) => assert!(error.starts_with(&go_error[..at]), "{file}: {error}"),
                None => assert_eq!(error, go_error, "{file}"),
            }
            continue;
        }
        let (result, failures) = common::golden::grade_fixture(grader.clone(), &fixture, &spec);
        let result = result.unwrap_or_else(|e| panic!("{file}: {e}"));
        let want: BTreeMap<String, f64> =
            serde_json::from_value(row["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want, "{file}");
        assert_eq!(result.detail, row["detail_exact_bytes"], "{file}");
        assert_eq!(failures as u64, row["failure_counter"], "{file}");
    }
}
