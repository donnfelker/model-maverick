//! Parity: `parity/fixtures/decision/rank.json`, `rank_property.json`, and
//! `rank_extras.json` (`internal/recommend` Rank, DefaultWeights, displayScore,
//! HighStakes), plus the D8 availability floor and the D9 entry fields.

use maverick_domain::policy::RetentionMode;
use maverick_domain::ranking::{
    AVAILABILITY_FLOOR, RankParams, Ranking, ScoredSurvivor, Weights, default_weights,
    display_score, high_stakes, rank,
};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Numbers compare by value: the fixtures write `90` where Rust serializes `90.0`.
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

/// Go's `Rank` output shape: drops the D8/D9 fields after checking that inputs without
/// availability, settings, or retention produce their neutral values.
fn go_shape(ranking: &Ranking) -> Value {
    let _ = parity_support::divergence!("D8");
    let _ = parity_support::divergence!("D9");
    let mut value = serde_json::to_value(ranking).expect("ranking serializes");
    for entry in value["ranked"].as_array_mut().expect("ranked") {
        let entry = entry.as_object_mut().expect("entry");
        assert_eq!(
            entry.remove("settings"),
            Some(json!({})),
            "D9 settings default"
        );
        assert_eq!(
            entry.remove("retention_mode"),
            Some(json!("unknown")),
            "D9 retention default"
        );
        let flags = entry["flags"].as_object_mut().expect("flags");
        assert_eq!(
            flags.remove("low_availability"),
            Some(json!(false)),
            "D8: no availability input never flags"
        );
    }
    normalize(value)
}

fn check_rank_case(case: &Value) {
    let id = case["id"].as_str().expect("case id");
    let input = &case["input"];
    let survivors: Vec<ScoredSurvivor> =
        serde_json::from_value(input["survivors"].clone()).expect("survivors");
    let weights: Option<Weights> =
        serde_json::from_value(input["weights"].clone()).expect("weights");
    let params: RankParams = serde_json::from_value(input["params"].clone()).expect("params");
    assert_eq!(
        params.availability_floor, AVAILABILITY_FLOOR,
        "Go params default the floor"
    );
    let mode = input["mode"].as_str().expect("mode");

    let got = rank(&survivors, mode, weights, &params);
    assert_eq!(
        go_shape(&got),
        normalize(case["expected"].clone()),
        "case {id}"
    );
}

// parity: recommend.TestRankOrdering
// parity: recommend.TestRankSingleSurvivor
// parity: recommend.TestRankNoDataExclusion
// parity: recommend.TestRankFlagPassthrough
#[test]
fn rank_matches_go_unit_cases() {
    let fixture = parity_support::fixture("decision", "rank.json").expect("fixture loads");
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 20);
    cases.iter().for_each(check_rank_case);
}

// parity: recommend.TestRankPermutationProperty
// parity: recommend.TestRankPickStableUnderJitterWhenIndistinguishable
// parity: recommend.TestRankPickStableUnderJitterWithClearWinner
#[test]
fn rank_matches_go_property_cases() {
    let fixture = parity_support::fixture("decision", "rank_property.json").expect("fixture loads");
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 2200);
    cases.iter().for_each(check_rank_case);
}

// parity: recommend.TestDefaultWeights
#[test]
fn rank_extras_match_go() {
    let fixture = parity_support::fixture("decision", "rank_extras.json").expect("fixture loads");
    let mut seen = [0usize; 3];
    for case in fixture["cases"].as_array().expect("cases") {
        let id = case["id"].as_str().expect("id");
        let input = &case["input"];
        let expected = &case["expected"];
        match case["function"].as_str().expect("function") {
            "default_weights" => {
                seen[0] += 1;
                let got = default_weights(input.as_str().expect("mode"));
                assert_eq!(
                    normalize(serde_json::to_value(got).expect("weights")),
                    normalize(expected.clone()),
                    "{id}"
                );
            }
            "display_score" => {
                seen[1] += 1;
                let got = display_score(input.as_f64().expect("quality"));
                assert_eq!(got, expected.as_f64().expect("score"), "{id}");
            }
            "high_stakes" => {
                seen[2] += 1;
                let tags: Vec<String> = serde_json::from_value(input.clone())
                    .map(|tags: Option<Vec<String>>| tags.unwrap_or_default())
                    .expect("tags");
                assert_eq!(
                    high_stakes(&tags),
                    expected.as_bool().expect("bool"),
                    "{id}"
                );
            }
            other => panic!("unknown rank_extras function {other}"),
        }
    }
    assert_eq!(seen, [5, 10, 7]);
}

// ---------------------------------------------------------------------------------
// Hand-built survivors.
// ---------------------------------------------------------------------------------

fn survivor(id: &str, quality: f64, cost: f64, family: &str) -> ScoredSurvivor {
    ScoredSurvivor {
        column_id: id.to_owned(),
        quality: Some(quality),
        cost_per_run: Some(cost),
        latency_p95: Some(1000.0),
        availability: None,
        family: family.to_owned(),
        has_data: true,
        measured: true,
        clears_difficulty: true,
        prefs_satisfied: true,
        settings: json!({}),
        retention_mode: RetentionMode::Unknown,
    }
}

fn ids(ranking: &Ranking) -> Vec<&str> {
    ranking
        .ranked
        .iter()
        .map(|entry| entry.column_id.as_str())
        .collect()
}

fn quality_mode(survivors: &[ScoredSurvivor]) -> Ranking {
    rank(survivors, "quality", None, &RankParams::default())
}

// ---------------------------------------------------------------------------------
// D8: the availability floor.
// ---------------------------------------------------------------------------------

#[test]
fn low_availability_ranks_after_every_available_column() {
    let _ = parity_support::divergence!("D8");
    let mut best = survivor("col_best", 95.0, 5.0, "anthropic");
    best.availability = Some(0.85);
    let mut ok = survivor("col_ok", 80.0, 5.0, "openai");
    ok.availability = Some(0.99);
    let unmeasured = survivor("col_new", 70.0, 5.0, "google");

    let ranking = quality_mode(&[best, ok, unmeasured]);
    assert_eq!(ids(&ranking), ["col_ok", "col_new", "col_best"]);
    let low: Vec<bool> = ranking
        .ranked
        .iter()
        .map(|entry| entry.flags.low_availability)
        .collect();
    assert_eq!(low, [false, false, true]);
    // Ranks are positions across the whole chain.
    let ranks: Vec<usize> = ranking.ranked.iter().map(|entry| entry.rank).collect();
    assert_eq!(ranks, [0, 1, 2]);
    // The low-availability partition is ordered by its own band: it has one member,
    // which leads it.
    assert!(ranking.ranked[2].flags.leader);
}

#[test]
fn availability_at_the_floor_is_not_low() {
    let _ = parity_support::divergence!("D8");
    let mut at_floor = survivor("col_a", 90.0, 5.0, "anthropic");
    at_floor.availability = Some(AVAILABILITY_FLOOR);
    let mut below = survivor("col_b", 95.0, 5.0, "openai");
    below.availability = Some(0.899_999);
    let ranking = quality_mode(&[at_floor, below]);
    assert_eq!(ids(&ranking), ["col_a", "col_b"]);
    assert!(!ranking.ranked[0].flags.low_availability);
    assert!(ranking.ranked[1].flags.low_availability);
}

#[test]
fn availability_partitions_wrap_the_preference_partitions() {
    let _ = parity_support::divergence!("D8");
    // Order: available satisfiers, available violators, low satisfiers, low violators.
    let mut low_satisfier = survivor("col_low_sat", 99.0, 1.0, "a");
    low_satisfier.availability = Some(0.5);
    let mut low_violator = survivor("col_low_vio", 98.0, 1.0, "b");
    low_violator.availability = Some(0.5);
    low_violator.prefs_satisfied = false;
    let mut violator = survivor("col_vio", 97.0, 1.0, "c");
    violator.prefs_satisfied = false;
    let satisfier = survivor("col_sat", 60.0, 1.0, "d");

    let ranking = quality_mode(&[low_violator, violator, low_satisfier, satisfier]);
    assert_eq!(
        ids(&ranking),
        ["col_sat", "col_vio", "col_low_sat", "col_low_vio"]
    );
}

#[test]
fn custom_floor_applies() {
    let _ = parity_support::divergence!("D8");
    let mut a = survivor("col_a", 95.0, 5.0, "anthropic");
    a.availability = Some(0.95);
    let b = survivor("col_b", 80.0, 5.0, "openai");
    let params = RankParams {
        availability_floor: 0.97,
        ..RankParams::default()
    };
    let ranking = rank(&[a, b], "quality", None, &params);
    assert_eq!(ids(&ranking), ["col_b", "col_a"]);
}

// ---------------------------------------------------------------------------------
// D9: entry fields.
// ---------------------------------------------------------------------------------

#[test]
fn entries_carry_settings_and_retention_mode() {
    let _ = parity_support::divergence!("D9");
    let mut thinking = survivor("col_think", 95.0, 5.0, "anthropic");
    thinking.settings = json!({"thinking": "enabled", "effort": "high"});
    thinking.retention_mode = RetentionMode::Zero;
    let plain = survivor("col_plain", 90.0, 4.0, "openai");

    let ranking = quality_mode(&[thinking, plain]);
    assert_eq!(
        ranking.ranked[0].settings,
        json!({"thinking": "enabled", "effort": "high"})
    );
    assert_eq!(ranking.ranked[0].retention_mode, RetentionMode::Zero);
    assert_eq!(ranking.ranked[1].settings, json!({}));
    assert_eq!(ranking.ranked[1].retention_mode, RetentionMode::Unknown);
    let entry = serde_json::to_value(&ranking.ranked[0]).expect("entry serializes");
    assert_eq!(entry["retention_mode"], json!("zero"));
    assert_eq!(entry["flags"]["low_availability"], json!(false));
}

// ---------------------------------------------------------------------------------
// Noise tie pass boundaries the Go fixtures do not pin.
// ---------------------------------------------------------------------------------

#[test]
fn noise_window_includes_exactly_the_noise_band() {
    // |90 - 89.25| = 0.75 exactly (both binary-exact): inside the window.
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 5.0, "anthropic"),
        survivor("col_edge", 89.25, 1.0, "openai"),
    ]);
    assert_eq!(ids(&ranking), ["col_edge", "col_head"]);
    // Just outside: the head stays.
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 5.0, "anthropic"),
        survivor("col_out", 89.0, 1.0, "openai"),
    ]);
    assert_eq!(ids(&ranking), ["col_head", "col_out"]);
}

#[test]
fn equal_cost_is_not_cheaper() {
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 5.0, "anthropic"),
        survivor("col_same_cost", 89.9, 5.0, "openai"),
    ]);
    assert_eq!(ids(&ranking), ["col_head", "col_same_cost"]);
}

#[test]
fn cheapest_ties_keep_the_first_candidate() {
    // Two cross-family candidates at the same lower cost: the first wins.
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 10.0, "anthropic"),
        survivor("col_first", 89.9, 2.0, "openai"),
        survivor("col_second", 89.8, 2.0, "google"),
    ]);
    assert_eq!(ranking.ranked[0].column_id, "col_first");

    // Two same-family siblings at the same lower cost, behind a cheaper cross-family
    // candidate: the first sibling wins.
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 10.0, "anthropic"),
        survivor("col_cross", 89.9, 1.0, "openai"),
        survivor("col_sib_first", 89.8, 3.0, "anthropic"),
        survivor("col_sib_second", 89.7, 3.0, "anthropic"),
    ]);
    assert_eq!(ranking.ranked[0].column_id, "col_sib_first");
}

#[test]
fn the_cheapest_same_family_sibling_replaces_the_head() {
    // The cheaper sibling comes second: it must win over the first sibling, and both
    // beat the cheaper cross-family candidate.
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 10.0, "anthropic"),
        survivor("col_cross", 89.9, 1.0, "openai"),
        survivor("col_sib_pricier", 89.8, 4.0, "anthropic"),
        survivor("col_sib_cheaper", 89.7, 3.0, "anthropic"),
    ]);
    assert_eq!(ranking.ranked[0].column_id, "col_sib_cheaper");

    // When the cheapest is itself same-family, it replaces the head.
    let ranking = quality_mode(&[
        survivor("col_head", 90.0, 10.0, "anthropic"),
        survivor("col_cross", 89.9, 6.0, "openai"),
        survivor("col_sib", 89.8, 2.0, "anthropic"),
    ]);
    assert_eq!(ranking.ranked[0].column_id, "col_sib");
}

#[test]
fn unknown_modes_rank_as_balanced() {
    let survivors = [
        survivor("col_a", 94.0, 8.0, "anthropic"),
        survivor("col_b", 89.0, 1.0, "openai"),
    ];
    let balanced = rank(&survivors, "balanced", None, &RankParams::default());
    let bogus = rank(&survivors, "bogus", None, &RankParams::default());
    assert_eq!(balanced, bogus);
    assert_eq!(RankParams::default().band("cost"), 5.5);
    assert_eq!(RankParams::default().band("quality"), 0.0);
    assert_eq!(RankParams::default().band(""), 1.5);
}

// ---------------------------------------------------------------------------------
// Properties.
// ---------------------------------------------------------------------------------

fn arb_survivor(i: usize) -> impl Strategy<Value = ScoredSurvivor> {
    (
        proptest::option::of(50.0f64..100.0),
        proptest::option::of(0.01f64..20.0),
        proptest::option::of(100.0f64..5_000.0),
        proptest::option::of(0.5f64..1.0),
        0usize..3,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            move |(quality, cost, latency, availability, family, has_data, prefs)| ScoredSurvivor {
                column_id: format!("col_{i}"),
                quality,
                cost_per_run: cost,
                latency_p95: latency,
                availability,
                family: ["anthropic", "openai", "google"][family].to_owned(),
                has_data,
                measured: true,
                clears_difficulty: true,
                prefs_satisfied: prefs,
                settings: json!({}),
                retention_mode: RetentionMode::Unknown,
            },
        )
}

fn arb_survivors() -> impl Strategy<Value = Vec<ScoredSurvivor>> {
    (1usize..8).prop_flat_map(|n| (0..n).map(arb_survivor).collect::<Vec<_>>())
}

proptest! {
    #[test]
    fn ranking_is_a_complete_ordered_chain(
        survivors in arb_survivors(),
        mode in prop_oneof![Just("quality"), Just("balanced"), Just("cost")],
    ) {
        let ranking = rank(&survivors, mode, None, &RankParams::default());

        // Every survivor is ranked or excluded exactly once; ranks are 0..n.
        prop_assert_eq!(ranking.ranked.len() + ranking.exclusions.len(), survivors.len());
        for (position, entry) in ranking.ranked.iter().enumerate() {
            prop_assert_eq!(entry.rank, position);
            // Default weights sum to 1 and each normalized dimension is in [0, 1].
            prop_assert!((0.0..=1.0 + 1e-12).contains(&entry.v), "v = {}", entry.v);
        }
        // G1: never empty when there was a survivor.
        prop_assert!(!ranking.ranked.is_empty());
        // D8 then preferences: the (low_availability, !prefs_satisfied) key never
        // decreases along the chain.
        let keys: Vec<(bool, bool)> = ranking
            .ranked
            .iter()
            .map(|entry| (entry.flags.low_availability, !entry.flags.prefs_satisfied))
            .collect();
        prop_assert!(keys.windows(2).all(|pair| pair[0] <= pair[1]), "{:?}", keys);
    }
}
