//! Parity: `parity/fixtures/decision/policy_evaluate.json`, `policy_property_g1.json`,
//! and `policy_reasons.json` (`internal/policy` Evaluate and Reasons), plus the D1
//! zero-retention rules that Go did not have.

use chrono::{DateTime, Duration, TimeZone, Utc};
use maverick_domain::policy::{
    CatalogColumn, CellAggregate, CellAggregates, ColumnStatus, ExclusionReason, PolicyErrorCode,
    PolicyRequest, Pool, RequestConstraints, RetentionMode, StandingConstraints, TenantConfig,
    evaluate, resolve_pool,
};
use proptest::prelude::*;
use serde_json::{Value, json};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
        .single()
        .expect("valid time")
}

/// Numbers compare by value: the fixtures write `2000` where Rust serializes `2000.0`.
fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) => Value::from(n.as_f64().expect("finite number")),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

/// Runs one fixture case through `evaluate` and compares the JSON it would store in
/// the decision trace (or the error it would answer) with Go's.
fn check_case(case: &Value) -> Option<usize> {
    let id = case["id"].as_str().expect("case id");
    let input = &case["input"];
    let cfg: TenantConfig =
        serde_json::from_value(input["tenant_config"].clone()).expect("tenant_config");
    let catalog: Vec<CatalogColumn> =
        serde_json::from_value(input["catalog"].clone()).expect("catalog");
    let aggregates: CellAggregates =
        serde_json::from_value(input["cell_aggregates"].clone()).expect("cell_aggregates");
    let request: PolicyRequest = serde_json::from_value(input["request"].clone()).expect("request");

    let expected = &case["expected"];
    match evaluate(&cfg, &catalog, &aggregates, &request, now()) {
        Ok(decision) => {
            assert!(
                expected["error"].is_null(),
                "case {id}: got a decision, want error {}",
                expected["error"]
            );
            let got = serde_json::to_value(&decision).expect("decision serializes");
            assert_eq!(
                normalize(got),
                normalize(expected["decision"].clone()),
                "case {id}"
            );
            Some(decision.survivors.len())
        }
        Err(err) => {
            let got = serde_json::to_value(&err).expect("error serializes");
            assert_eq!(
                normalize(got),
                normalize(expected["error"].clone()),
                "case {id}"
            );
            None
        }
    }
}

// parity: policy.TestEvaluatePoolResolution
// parity: policy.TestEvaluateRetiredExclusion
// parity: policy.TestEvaluateAllowlistNarrowing
// parity: policy.TestEvaluateAllowlistErrors
// parity: policy.TestEvaluateStandingConstraints
// parity: policy.TestEvaluateDriftFallback
// parity: policy.TestEvaluateDriftFallbackTie
// parity: policy.TestEvaluateDriftFallbackRelativeExcess
// parity: policy.TestEvaluatePreferences
// parity: policy.TestEvaluateDriftFallbackThenPreferences
// parity: policy.TestEvaluateAllRetiredPoolAlertsInsteadOfSilence
// parity: policy.TestEvaluatePoolMemberMissingFromCatalogIsIgnored
// parity: policy.TestEvaluateDeterminism
#[test]
fn evaluate_matches_go_unit_cases() {
    let fixture =
        parity_support::fixture("decision", "policy_evaluate.json").expect("fixture loads");
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 31);
    for case in cases {
        check_case(case);
    }
}

// parity: policy.TestEvaluateSurvivorsNeverEmptyProperty
#[test]
fn evaluate_matches_go_g1_property_cases() {
    let fixture =
        parity_support::fixture("decision", "policy_property_g1.json").expect("fixture loads");
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 600);
    for case in cases {
        let survivors = check_case(case).expect("G1 cases never error");
        assert!(survivors > 0, "G1: case {} left no survivors", case["id"]);
    }
}

// parity: policy.TestReasonsEnum
#[test]
fn reasons_enum_is_go_order_plus_zero_retention() {
    let _ = parity_support::divergence!("D1");
    let fixture =
        parity_support::fixture("decision", "policy_reasons.json").expect("fixture loads");
    let mut expected: Vec<String> = fixture["cases"][0]["expected"]
        .as_array()
        .expect("reason list")
        .iter()
        .map(|reason| reason.as_str().expect("reason").to_owned())
        .collect();
    // D1 adds `standing_constraint:zero_retention` after the other standing constraints.
    let at = expected
        .iter()
        .position(|reason| reason == "standing_constraint:max_latency_ms")
        .expect("latency reason")
        + 1;
    expected.insert(at, "standing_constraint:zero_retention".to_owned());

    let got: Vec<&str> = ExclusionReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(got, expected);
    for reason in ExclusionReason::ALL {
        assert_eq!(
            serde_json::to_value(reason).expect("serializes"),
            json!(reason.as_str()),
            "serde spelling of {reason:?}"
        );
    }
}

// ---------------------------------------------------------------------------------
// D1: zero retention (new in the Rust port).
// ---------------------------------------------------------------------------------

fn column(id: &str, mode: RetentionMode, valid_until: Option<DateTime<Utc>>) -> CatalogColumn {
    CatalogColumn {
        id: id.to_owned(),
        model_id: format!("model-{id}"),
        status: ColumnStatus::Active,
        retention_mode: mode,
        retention_valid_until: valid_until,
    }
}

fn tenant(ids: &[&str], constraints: StandingConstraints) -> TenantConfig {
    TenantConfig {
        tenant_id: "ten_zr".to_owned(),
        pools: vec![Pool {
            name: "hipaa".to_owned(),
            model_column_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            is_default: true,
        }],
        constraints,
    }
}

fn zero_retention() -> StandingConstraints {
    StandingConstraints {
        zero_retention: true,
        ..StandingConstraints::default()
    }
}

fn request() -> PolicyRequest {
    PolicyRequest {
        pool: String::new(),
        allowed_models: None,
        capability: "structured_extraction".to_owned(),
        difficulty_tier: 3,
        constraints: RequestConstraints::default(),
    }
}

fn later() -> Option<DateTime<Utc>> {
    Some(now() + Duration::days(30))
}

fn earlier() -> Option<DateTime<Utc>> {
    Some(now() - Duration::days(1))
}

#[test]
fn zero_retention_keeps_only_columns_with_valid_zero_evidence() {
    let _ = parity_support::divergence!("D1");
    let catalog = vec![
        column("col_std", RetentionMode::Standard, later()),
        column("col_zero", RetentionMode::Zero, later()),
        column("col_unknown", RetentionMode::Unknown, None),
        column("col_expired", RetentionMode::Zero, earlier()),
        column("col_zero2", RetentionMode::Zero, later()),
    ];
    let ids: Vec<&str> = catalog.iter().map(|c| c.id.as_str()).collect();
    let cfg = tenant(&ids, zero_retention());

    let decision = evaluate(&cfg, &catalog, &CellAggregates::new(), &request(), now())
        .expect("compliant columns remain");
    let survivors: Vec<&str> = decision
        .survivors
        .iter()
        .map(|s| s.column_id.as_str())
        .collect();
    assert_eq!(survivors, ["col_zero", "col_zero2"]);
    assert!(decision.constraints_met);
    assert!(decision.drift_alert.is_none());

    let excluded: Vec<(&str, ExclusionReason, &str)> = decision
        .exclusions
        .iter()
        .map(|e| (e.column_id.as_str(), e.reason, e.detail.as_str()))
        .collect();
    assert_eq!(
        excluded,
        [
            (
                "col_std",
                ExclusionReason::StandingZeroRetention,
                "column retention_mode is standard, not zero"
            ),
            (
                "col_unknown",
                ExclusionReason::StandingZeroRetention,
                "column retention_mode is unknown, not zero"
            ),
            // Expired evidence reads as unknown, never as stale compliance.
            (
                "col_expired",
                ExclusionReason::StandingZeroRetention,
                "column retention_mode is unknown, not zero"
            ),
        ]
    );
}

#[test]
fn retention_expiry_yields_no_compliant_columns() {
    let _ = parity_support::divergence!("D1");
    let catalog = vec![
        column("col_a", RetentionMode::Zero, earlier()),
        column("col_b", RetentionMode::Standard, later()),
    ];
    let cfg = tenant(&["col_a", "col_b"], zero_retention());

    let err = evaluate(&cfg, &catalog, &CellAggregates::new(), &request(), now())
        .expect_err("no column holds valid evidence");
    assert_eq!(err.code, PolicyErrorCode::NoCompliantColumns);
    assert_eq!(err.code.as_str(), "no_compliant_columns");
    assert_eq!(err.code.http_status(), 409);
    assert_eq!(
        err.message,
        "no column in pool \"hipaa\" holds valid zero-retention evidence"
    );
    assert_eq!(err.details, json!({"pool": "hipaa"}));

    let alert = err.alert.expect("critical alert names tenant and pool");
    assert_eq!(
        serde_json::to_value(&alert).expect("alert serializes"),
        json!({
            "kind": "pool_would_be_empty",
            "severity": "critical",
            "subject": {
                "tenant_id": "ten_zr",
                "pool": "hipaa",
                "capability": "structured_extraction",
                "difficulty_tier": 3,
                "retained": null,
                "constraint": "zero_retention"
            },
            "message": "zero retention emptied pool \"hipaa\" for structured_extraction tier 3 \
                        at request time; no column holds valid zero-retention evidence"
        })
    );
}

#[test]
fn unknown_and_evidence_free_zero_never_satisfy_zero_retention() {
    let _ = parity_support::divergence!("D1");
    for mode in [RetentionMode::Unknown, RetentionMode::Standard] {
        let catalog = vec![column("col_a", mode, later())];
        let cfg = tenant(&["col_a"], zero_retention());
        let err = evaluate(&cfg, &catalog, &CellAggregates::new(), &request(), now())
            .expect_err("only zero complies");
        assert_eq!(err.code, PolicyErrorCode::NoCompliantColumns, "{mode:?}");
    }
    // `zero` with no `retention_valid_until` has no evidence window: fail closed.
    let catalog = vec![column("col_a", RetentionMode::Zero, None)];
    let cfg = tenant(&["col_a"], zero_retention());
    let err = evaluate(&cfg, &catalog, &CellAggregates::new(), &request(), now())
        .expect_err("no evidence window");
    assert_eq!(err.code, PolicyErrorCode::NoCompliantColumns);
}

#[test]
fn evidence_expires_exactly_at_valid_until() {
    let _ = parity_support::divergence!("D1");
    let at = now();
    assert_eq!(
        RetentionMode::Zero.effective(Some(at), at),
        RetentionMode::Unknown,
        "at valid_until the evidence has expired"
    );
    assert_eq!(
        RetentionMode::Zero.effective(Some(at + Duration::seconds(1)), at),
        RetentionMode::Zero
    );
    assert_eq!(
        RetentionMode::Standard.effective(Some(at - Duration::seconds(1)), at),
        RetentionMode::Unknown,
        "expired evidence reads as unknown for every mode"
    );
    assert_eq!(
        RetentionMode::Standard.effective(None, at),
        RetentionMode::Standard
    );
    assert_eq!(
        RetentionMode::Unknown.effective(None, at),
        RetentionMode::Unknown
    );
    assert_eq!(
        RetentionMode::Zero.effective(None, at),
        RetentionMode::Unknown
    );
}

#[test]
fn drift_fallback_draws_only_from_zero_compliant_columns() {
    let _ = parity_support::divergence!("D1");
    // The standard column is the only one inside the cost cap, but it is not
    // zero-compliant. The G1 fallback must keep the least-violating zero column.
    let catalog = vec![
        column("col_cheap_std", RetentionMode::Standard, later()),
        column("col_zero_a", RetentionMode::Zero, later()),
        column("col_zero_b", RetentionMode::Zero, later()),
    ];
    let cfg = tenant(
        &["col_cheap_std", "col_zero_a", "col_zero_b"],
        StandingConstraints {
            max_cost_per_run: Some(0.01),
            max_latency_ms: None,
            zero_retention: true,
        },
    );
    let aggregates: CellAggregates = [
        ("col_cheap_std", 0.001),
        ("col_zero_a", 0.03),
        ("col_zero_b", 0.02),
    ]
    .into_iter()
    .map(|(id, cost)| {
        (
            id.to_owned(),
            CellAggregate {
                cost_per_run_usd: Some(cost),
                latency_p95_ms: None,
            },
        )
    })
    .collect();

    let decision =
        evaluate(&cfg, &catalog, &aggregates, &request(), now()).expect("fallback retains");
    let survivors: Vec<&str> = decision
        .survivors
        .iter()
        .map(|s| s.column_id.as_str())
        .collect();
    assert_eq!(survivors, ["col_zero_b"]);
    assert!(!decision.constraints_met);
    let reasons: Vec<(&str, ExclusionReason)> = decision
        .exclusions
        .iter()
        .map(|e| (e.column_id.as_str(), e.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            ("col_cheap_std", ExclusionReason::StandingZeroRetention),
            ("col_zero_a", ExclusionReason::StandingMaxCostPerRun),
        ]
    );
    let alert = decision.drift_alert.expect("drift alert");
    assert_eq!(alert.subject.retained, Some(vec!["col_zero_b".to_owned()]));
    assert_eq!(alert.subject.constraint, None);
}

#[test]
fn zero_retention_off_ignores_retention_like_go() {
    let catalog = vec![
        column("col_a", RetentionMode::Standard, earlier()),
        column("col_b", RetentionMode::Unknown, None),
    ];
    let cfg = tenant(&["col_a", "col_b"], StandingConstraints::default());
    let decision = evaluate(&cfg, &catalog, &CellAggregates::new(), &request(), now())
        .expect("no retention filter");
    assert_eq!(decision.survivors.len(), 2);
    assert!(decision.exclusions.is_empty());
}

#[test]
fn allowlist_narrowed_to_non_compliant_columns_is_refused() {
    let _ = parity_support::divergence!("D1");
    let catalog = vec![
        column("col_std", RetentionMode::Standard, later()),
        column("col_zero", RetentionMode::Zero, later()),
    ];
    let cfg = tenant(&["col_std", "col_zero"], zero_retention());
    let mut req = request();
    req.allowed_models = Some(vec!["col_std".to_owned()]);
    let err = evaluate(&cfg, &catalog, &CellAggregates::new(), &req, now())
        .expect_err("the allowlist leaves only a standard column");
    assert_eq!(err.code, PolicyErrorCode::NoCompliantColumns);
}

// ---------------------------------------------------------------------------------
// Go behavior the fixtures do not reach.
// ---------------------------------------------------------------------------------

#[test]
fn retired_match_outside_the_pool_gets_the_plain_message() {
    // QUIRK: the "matches only retired columns" wording needs a retired match inside
    // the pool.
    let mut retired = column("col_old", RetentionMode::Unknown, None);
    retired.status = ColumnStatus::Retired;
    let catalog = vec![column("col_a", RetentionMode::Unknown, None), retired];
    let cfg = tenant(&["col_a"], StandingConstraints::default());
    let mut req = request();
    req.allowed_models = Some(vec!["col_old".to_owned()]);
    let err = evaluate(&cfg, &catalog, &CellAggregates::new(), &req, now())
        .expect_err("entry is not in the pool");
    assert_eq!(err.code, PolicyErrorCode::ModelNotInPool);
    assert_eq!(err.code.http_status(), 422);
    assert_eq!(
        err.message,
        "allowed_models entry \"col_old\" is not in pool \"hipaa\""
    );
}

#[test]
fn non_positive_limit_ranks_the_fallback_by_raw_excess() {
    // Unreachable through validated writes, but Go's rule: a limit <= 0 adds the raw
    // overshoot. A relative excess would divide by zero and tie both columns.
    let catalog = vec![
        column("col_a", RetentionMode::Unknown, None),
        column("col_b", RetentionMode::Unknown, None),
    ];
    let cfg = tenant(
        &["col_a", "col_b"],
        StandingConstraints {
            max_cost_per_run: Some(0.0),
            max_latency_ms: None,
            zero_retention: false,
        },
    );
    let aggregates: CellAggregates = [("col_a", 0.03), ("col_b", 0.02)]
        .into_iter()
        .map(|(id, cost)| {
            (
                id.to_owned(),
                CellAggregate {
                    cost_per_run_usd: Some(cost),
                    latency_p95_ms: None,
                },
            )
        })
        .collect();
    let decision = evaluate(&cfg, &catalog, &aggregates, &request(), now()).expect("fallback");
    let survivors: Vec<&str> = decision
        .survivors
        .iter()
        .map(|s| s.column_id.as_str())
        .collect();
    assert_eq!(survivors, ["col_b"]);
}

#[test]
fn resolve_pool_by_name_and_default() {
    let cfg = TenantConfig {
        tenant_id: "ten".to_owned(),
        pools: vec![
            Pool {
                name: "a".to_owned(),
                model_column_ids: vec!["x".to_owned()],
                is_default: false,
            },
            Pool {
                name: "b".to_owned(),
                model_column_ids: vec!["y".to_owned()],
                is_default: true,
            },
        ],
        constraints: StandingConstraints::default(),
    };
    assert_eq!(resolve_pool(&cfg, "").expect("default").name, "b");
    assert_eq!(resolve_pool(&cfg, "a").expect("named").name, "a");
    let err = resolve_pool(&cfg, "c").expect_err("unknown");
    assert_eq!(err.code, PolicyErrorCode::UnknownPool);
    assert_eq!(
        err.to_string(),
        "pool \"c\" is not configured for this tenant"
    );
}

// ---------------------------------------------------------------------------------
// Properties: G1 (always a model) and G2 (a pool is a hard boundary).
// ---------------------------------------------------------------------------------

fn arb_status() -> impl Strategy<Value = ColumnStatus> {
    prop_oneof![
        Just(ColumnStatus::ColdStart),
        Just(ColumnStatus::Active),
        Just(ColumnStatus::Deprecated),
        Just(ColumnStatus::Retired),
    ]
}

fn arb_aggregate() -> impl Strategy<Value = Option<CellAggregate>> {
    proptest::option::of(
        (
            proptest::option::of(0.0001f64..0.1),
            proptest::option::of(100.0f64..10_000.0),
        )
            .prop_map(|(cost, latency)| CellAggregate {
                cost_per_run_usd: cost,
                latency_p95_ms: latency,
            }),
    )
}

proptest! {
    #[test]
    fn survivors_stay_in_pool_and_every_column_is_traced(
        columns in proptest::collection::vec((arb_status(), arb_aggregate(), any::<bool>()), 1..8),
        max_cost in proptest::option::of(0.001f64..0.05),
        max_latency in proptest::option::of(200i32..5_000),
        pref_cost in proptest::option::of(0.001f64..0.05),
        pref_latency in proptest::option::of(200i32..5_000),
    ) {
        let catalog: Vec<CatalogColumn> = columns
            .iter()
            .enumerate()
            .map(|(i, (status, _, _))| CatalogColumn {
                id: format!("col_{i:02}"),
                model_id: format!("model-{i}"),
                status: *status,
                retention_mode: RetentionMode::Unknown,
                retention_valid_until: None,
            })
            .collect();
        let pool_ids: Vec<String> = columns
            .iter()
            .enumerate()
            .filter(|(_, (_, _, in_pool))| *in_pool)
            .map(|(i, _)| format!("col_{i:02}"))
            .collect();
        prop_assume!(!pool_ids.is_empty());
        let aggregates: CellAggregates = columns
            .iter()
            .enumerate()
            .filter_map(|(i, (_, agg, _))| agg.map(|agg| (format!("col_{i:02}"), agg)))
            .collect();
        let cfg = TenantConfig {
            tenant_id: "ten_prop".to_owned(),
            pools: vec![Pool { name: "p".to_owned(), model_column_ids: pool_ids.clone(), is_default: true }],
            constraints: StandingConstraints { max_cost_per_run: max_cost, max_latency_ms: max_latency, zero_retention: false },
        };
        let mut req = request();
        req.constraints = RequestConstraints { max_cost_per_run: pref_cost, max_latency_ms: pref_latency };

        let decision = evaluate(&cfg, &catalog, &aggregates, &req, now()).expect("no allowlist, no D1");

        // G2: survivors never leave the pool.
        for survivor in &decision.survivors {
            prop_assert!(pool_ids.contains(&survivor.column_id));
        }
        // G1: a pool with a usable column always yields a survivor.
        let usable = catalog
            .iter()
            .any(|c| c.status != ColumnStatus::Retired && pool_ids.contains(&c.id));
        prop_assert_eq!(!decision.survivors.is_empty(), usable);
        // The trace is complete: each catalog column is a survivor or has one reason.
        for column in &catalog {
            let as_survivor = decision.survivors.iter().filter(|s| s.column_id == column.id).count();
            let as_exclusion = decision.exclusions.iter().filter(|e| e.column_id == column.id).count();
            prop_assert_eq!(as_survivor + as_exclusion, 1, "column {}", column.id);
        }
        // Satisfiers precede preference violators.
        let first_violator = decision.survivors.iter().position(|s| !s.prefs_satisfied);
        if let Some(at) = first_violator {
            prop_assert!(decision.survivors[at..].iter().all(|s| !s.prefs_satisfied));
        }
    }
}
