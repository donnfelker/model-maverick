//! Parity tests for config-write validation and the G4 simulation (bead 3hk.2.16).
//!
//! Fixtures: `parity/fixtures/decision/{policy_validate.json, policy_simulate.json}`. Go source:
//! `internal/policy`. The `validate_input.json` fixture is the recommend-request validator
//! (`workload`/`mode`), which belongs to the recommend pipeline (5.4), not here. The D1
//! zero-retention simulation is Rust-only (Go ignores the flag), so those are fresh tests.

use chrono::{DateTime, Utc};
use maverick_domain::policy::{
    ColumnRetention, Pool, RetentionMode, SimulationState, StandingConstraints, WeightRow,
    simulate, validate_constraint_values, validate_pool_shapes, validate_weights,
};
use serde::Deserialize;
use serde_json::{Value, to_value};
use test_support::format_json_diff;

fn cases(file: &str) -> Vec<Value> {
    parity_support::fixture("decision", file)
        .expect("load decision fixture")
        .get("cases")
        .and_then(Value::as_array)
        .expect("cases array")
        .clone()
}

fn fixed_now() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().expect("parse a fixed clock")
}

#[derive(Deserialize)]
struct SimulateInput {
    pools: Vec<Pool>,
    constraints: StandingConstraints,
    #[serde(flatten)]
    state: SimulationState,
}

fn run_validate(case: &Value) -> Option<String> {
    let function = case.get("function").and_then(Value::as_str).unwrap_or("");
    let id = case.get("id").and_then(Value::as_str).unwrap_or("<no id>");
    let anchor = format!("fixtures/decision/policy_validate.json#{id}");
    let input = case.get("input").cloned().unwrap_or(Value::Null);
    let expected = case.get("expected").cloned().unwrap_or(Value::Null);

    let actual = match function {
        "validate_weights" => {
            let rows: Vec<WeightRow> = serde_json::from_value(input).expect("weight rows");
            to_value(validate_weights(&rows)).expect("serialize")
        }
        "validate_pool_shapes" => {
            let pools: Vec<Pool> = serde_json::from_value(input).expect("pools");
            to_value(validate_pool_shapes(&pools)).expect("serialize")
        }
        "validate_constraint_values" => {
            let constraints: StandingConstraints =
                serde_json::from_value(input).expect("constraints");
            to_value(validate_constraint_values(&constraints)).expect("serialize")
        }
        other => panic!("{anchor}: unknown function {other:?}"),
    };

    format_json_diff(&expected, &actual, Some(&anchor))
}

fn run_simulate(case: &Value) -> Option<String> {
    let id = case.get("id").and_then(Value::as_str).unwrap_or("<no id>");
    let anchor = format!("fixtures/decision/policy_simulate.json#{id}");
    let input: SimulateInput =
        serde_json::from_value(case.get("input").cloned().unwrap_or(Value::Null))
            .expect("simulate input");
    let actual = to_value(simulate(
        &input.pools,
        &input.constraints,
        &input.state,
        fixed_now(),
    ))
    .expect("serialize");
    let expected = case.get("expected").cloned().unwrap_or(Value::Null);
    format_json_diff(&expected, &actual, Some(&anchor))
}

// parity: decision/policy.TestValidateWeights
// parity: decision/policy.TestValidatePoolShapes
// parity: decision/policy.TestValidateConstraintValues
#[test]
fn config_validation_goldens() {
    let failures: Vec<String> = cases("policy_validate.json")
        .iter()
        .filter_map(run_validate)
        .collect();
    assert!(
        failures.is_empty(),
        "{} policy_validate.json case(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

// parity: decision/policy.TestSimulatePerCellSurvivorship
// parity: decision/policy.TestSimulateMissingAggregatesSurvive
// parity: decision/policy.TestSimulateAllRetired
// parity: decision/policy.TestSimulateRetiredExcludedFromSurvivors
// parity: decision/policy.TestSimulateNamesLatencyConstraint
// parity: decision/policy.TestSimulateBoundaryValuePasses
#[test]
fn g4_simulation_goldens() {
    let failures: Vec<String> = cases("policy_simulate.json")
        .iter()
        .filter_map(run_simulate)
        .collect();
    assert!(
        failures.is_empty(),
        "{} policy_simulate.json case(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn zero_retention_pool() -> (Vec<Pool>, StandingConstraints) {
    let pools = vec![Pool {
        name: "p".to_string(),
        model_column_ids: vec!["col".to_string()],
        is_default: true,
    }];
    let constraints = StandingConstraints {
        max_cost_per_run: None,
        max_latency_ms: None,
        zero_retention: true,
    };
    (pools, constraints)
}

fn state_with_retention(
    mode: RetentionMode,
    valid_until: Option<DateTime<Utc>>,
) -> SimulationState {
    let mut state = SimulationState::default();
    state
        .column_retention
        .insert("col".to_string(), ColumnRetention { mode, valid_until });
    state
}

#[test]
fn d1_valid_zero_retention_evidence_keeps_the_pool() {
    let (pools, constraints) = zero_retention_pool();
    let future = fixed_now() + chrono::Duration::days(1);
    let state = state_with_retention(RetentionMode::Zero, Some(future));
    assert!(simulate(&pools, &constraints, &state, fixed_now()).is_empty());
}

#[test]
fn d1_standard_or_unknown_or_expired_evidence_empties_the_pool() {
    let (pools, constraints) = zero_retention_pool();
    let past = fixed_now() - chrono::Duration::days(1);
    for state in [
        state_with_retention(RetentionMode::Standard, None),
        state_with_retention(RetentionMode::Unknown, None),
        state_with_retention(RetentionMode::Zero, None), // zero without evidence reads unknown
        state_with_retention(RetentionMode::Zero, Some(past)), // expired evidence reads unknown
        SimulationState::default(),                      // no evidence at all
    ] {
        let errors = simulate(&pools, &constraints, &state, fixed_now());
        assert_eq!(errors.len(), 1, "one violation");
        assert_eq!(errors[0].details["constraint"], "zero_retention");
        assert_eq!(errors[0].details["pool"], "p");
    }
}
