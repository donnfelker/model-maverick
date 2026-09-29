//! Decision-trace candidates (bead 2.27): `tracing_trace_shape.json` exact bytes, the
//! survivor and exclusion builders, ordering, and the D8 fields.

use maverick_domain::policy::{Exclusion, ExclusionReason, RetentionMode};
use maverick_domain::ranking::{RankFlags, RankedEntry};
use maverick_domain::trace::{
    CandidateStatus, ColumnInfo, SurvivorMetrics, TraceCandidate, candidates, candidates_json,
};

fn column(provider: &str, model: &str, endpoint: &str) -> ColumnInfo {
    ColumnInfo {
        provider: provider.to_owned(),
        model: model.to_owned(),
        endpoint: endpoint.to_owned(),
    }
}

/// The four candidates the Go shape test builds, set field by field.
fn fixture_candidates() -> Vec<TraceCandidate> {
    vec![
        TraceCandidate {
            column_id: "col_a".to_owned(),
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-4-6".to_owned(),
            endpoint: "direct".to_owned(),
            status: CandidateStatus::Survivor,
            rank: Some(0),
            quality: Some(88.0),
            pass_rate: Some(0.9),
            cost_per_run_usd: Some(0.004),
            latency_p95_ms: Some(1200.0),
            v: Some(0.62),
            leader: Some(true),
            measured: Some(true),
            clears_difficulty: Some(true),
            prefs_satisfied: Some(false),
            ..TraceCandidate::default()
        },
        TraceCandidate {
            column_id: "col_b".to_owned(),
            provider: "openai".to_owned(),
            model: "gpt-5.5".to_owned(),
            endpoint: "direct".to_owned(),
            status: CandidateStatus::Survivor,
            rank: Some(0),
            ..TraceCandidate::default()
        },
        TraceCandidate {
            column_id: "col_c".to_owned(),
            provider: "google".to_owned(),
            model: "gemini-3.1-pro".to_owned(),
            endpoint: "vertex".to_owned(),
            status: CandidateStatus::Excluded,
            exclusion_reason: "no_data".to_owned(),
            detail: "no aggregate row for the workload cell".to_owned(),
            ..TraceCandidate::default()
        },
        TraceCandidate {
            status: CandidateStatus::Excluded,
            exclusion_reason: "not_in_pool".to_owned(),
            ..TraceCandidate::default()
        },
    ]
}

/// `tracing_trace_shape.json`: the exact candidates JSON bytes.
#[test]
fn trace_shape_is_byte_exact() {
    let fixture = parity_support::fixture("decision", "tracing_trace_shape.json").expect("fixture");
    let want = fixture["candidates_json_exact"]
        .as_str()
        .expect("candidates_json_exact");
    let got = candidates_json(&fixture_candidates()).expect("marshal");
    assert_eq!(got, want);
}

fn ranked(column_id: &str, rank: usize, v: f64, flags: RankFlags) -> RankedEntry {
    RankedEntry {
        column_id: column_id.to_owned(),
        rank,
        v,
        flags,
        settings: serde_json::json!({}),
        retention_mode: RetentionMode::default(),
    }
}

#[test]
fn survivor_builder_rounds_quality_and_records_every_flag() {
    let flags = RankFlags {
        leader: false,
        measured: true,
        clears_difficulty: false,
        prefs_satisfied: true,
        low_availability: false,
    };
    let metrics = SurvivorMetrics {
        quality: Some(87.95),
        pass_rate: Some(0.9),
        cost_per_run_usd: Some(0.004),
        latency_p95_ms: None,
        availability: None,
    };
    let candidate = TraceCandidate::survivor(
        &ranked("col_a", 2, 0.5, flags),
        &column("anthropic", "claude", "direct"),
        &metrics,
    );
    assert_eq!(
        candidate.quality,
        Some(88.0),
        "display score, rounded to 0.1"
    );
    assert_eq!(
        candidates_json(&[candidate]).expect("marshal"),
        "[{\"column_id\":\"col_a\",\"provider\":\"anthropic\",\"model\":\"claude\",\"endpoint\":\"direct\",\
         \"status\":\"survivor\",\"rank\":2,\"quality\":88,\"pass_rate\":0.9,\"cost_per_run_usd\":0.004,\
         \"v\":0.5,\"leader\":false,\"measured\":true,\"clears_difficulty\":false,\"prefs_satisfied\":true,\
         \"low_availability\":false}]"
    );
}

/// DIVERGENCE: D8. Survivors also record `availability` and `low_availability`, after
/// every Go field, so the Go-shaped prefix compares field by field.
#[test]
fn d8_fields_follow_every_go_field() {
    let d8 = parity_support::divergence!("D8");
    assert_eq!(d8.id(), "D8");
    let flags = RankFlags {
        leader: true,
        measured: true,
        clears_difficulty: true,
        prefs_satisfied: true,
        low_availability: true,
    };
    let metrics = SurvivorMetrics {
        quality: None,
        availability: Some(0.85),
        ..SurvivorMetrics::default()
    };
    let json = candidates_json(&[TraceCandidate::survivor(
        &ranked("col_z", 0, 1.0, flags),
        &column("xai", "grok", "direct"),
        &metrics,
    )])
    .expect("marshal");
    assert!(
        json.ends_with(
            "\"prefs_satisfied\":true,\"availability\":0.85,\"low_availability\":true}]"
        ),
        "{json}"
    );
    assert!(
        !json.contains("\"quality\""),
        "quality omitted when the cell has none: {json}"
    );
}

#[test]
fn exclusion_builder_writes_reason_and_detail() {
    let with_detail = Exclusion {
        column_id: "col_x".to_owned(),
        reason: ExclusionReason::StandingZeroRetention,
        detail: "column retention_mode is unknown, not zero".to_owned(),
    };
    let without_detail = Exclusion {
        column_id: "col_y".to_owned(),
        reason: ExclusionReason::NotInPool,
        detail: String::new(),
    };
    let json = candidates_json(&[
        TraceCandidate::exclusion(&with_detail, &column("openai", "gpt", "direct")),
        TraceCandidate::exclusion(&without_detail, &ColumnInfo::default()),
    ])
    .expect("marshal");
    assert_eq!(
        json,
        "[{\"column_id\":\"col_x\",\"provider\":\"openai\",\"model\":\"gpt\",\"endpoint\":\"direct\",\
         \"status\":\"excluded\",\"exclusion_reason\":\"standing_constraint:zero_retention\",\
         \"detail\":\"column retention_mode is unknown, not zero\"},\
         {\"column_id\":\"col_y\",\"provider\":\"\",\"model\":\"\",\"endpoint\":\"\",\"status\":\"excluded\",\
         \"exclusion_reason\":\"not_in_pool\"}]"
    );
}

#[test]
fn candidates_put_survivors_in_rank_order_then_exclusions_in_given_order() {
    let survivor = |id: &str, rank: usize| TraceCandidate {
        column_id: id.to_owned(),
        rank: Some(rank),
        ..TraceCandidate::default()
    };
    let excluded = |id: &str| TraceCandidate {
        column_id: id.to_owned(),
        status: CandidateStatus::Excluded,
        ..TraceCandidate::default()
    };
    let ordered = candidates(
        [survivor("s2", 2), survivor("s0", 0), survivor("s1", 1)],
        [excluded("policy"), excluded("no_data")],
    );
    let ids: Vec<&str> = ordered
        .iter()
        .map(|candidate| candidate.column_id.as_str())
        .collect();
    assert_eq!(ids, ["s0", "s1", "s2", "policy", "no_data"]);
    assert!(candidates(Vec::new(), Vec::new()).is_empty());
    assert_eq!(candidates_json(&[]).expect("marshal"), "[]");
}

#[test]
fn go_number_format_and_non_finite_values() {
    let candidate = |v: f64| TraceCandidate {
        v: Some(v),
        latency_p95_ms: Some(1e21),
        ..TraceCandidate::default()
    };
    let json = candidates_json(&[candidate(0.000_001)]).expect("marshal");
    assert!(json.contains("\"latency_p95_ms\":1e+21"), "{json}");
    assert!(json.contains("\"v\":0.000001"), "{json}");
    assert!(
        candidates_json(&[candidate(f64::NAN)]).is_err(),
        "Go refuses NaN"
    );
    assert!(
        candidates_json(&[candidate(f64::INFINITY)]).is_err(),
        "Go refuses +Inf"
    );
}
