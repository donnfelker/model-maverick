//! Conversation affinity in the ranking (spec §7.6, D44). Expected values are computed
//! by hand in the comments. The turn-cost cases use the numbers of
//! `plans/conversation-affinity-plan.md` §1: a 120,000-token history, a $0.50 cache
//! read, and a $12.50 cache write per million tokens.

use std::collections::BTreeMap;

use maverick_domain::affinity::{
    AffinityContext, AffinityDecision, AffinityReason, AffinityStatus, BonusWithheld,
    INCUMBENT_BONUS, ServedEntry, TurnCostSkipped, TurnPrices, rank_with_affinity, served_column,
};
use maverick_domain::policy::{Exclusion, ExclusionReason, RetentionMode};
use maverick_domain::pricing::Pricing;
use maverick_domain::ranking::{RankParams, Ranking, ScoredSurvivor, Weights, rank};
use serde_json::{Value, json};

fn survivor(id: &str, quality: f64, cost: f64) -> ScoredSurvivor {
    ScoredSurvivor {
        column_id: id.to_owned(),
        quality: Some(quality),
        cost_per_run: Some(cost),
        latency_p95: Some(1000.0),
        availability: None,
        family: format!("family-{id}"),
        has_data: true,
        measured: true,
        clears_difficulty: true,
        prefs_satisfied: true,
        settings: json!({}),
        retention_mode: RetentionMode::Unknown,
    }
}

fn prices(input: f64, read: Option<f64>, write: Option<f64>) -> TurnPrices {
    TurnPrices {
        input_per_mtok: Some(input),
        cache_read_per_mtok: read,
        cache_write_per_mtok: write,
    }
}

fn context(previous: Option<&str>, cost_aware: bool) -> AffinityContext {
    AffinityContext {
        previous_column_id: previous.map(str::to_owned),
        cache_warm: false,
        context_tokens: None,
        cost_aware,
        incumbent_bonus: INCUMBENT_BONUS,
    }
}

fn run(
    survivors: &[ScoredSurvivor],
    prices: &BTreeMap<String, TurnPrices>,
    exclusions: &[Exclusion],
    mode: &str,
    affinity: &AffinityContext,
) -> (Vec<String>, AffinityDecision) {
    let _ = parity_support::divergence!("D44");
    let out = rank_with_affinity(
        survivors,
        prices,
        exclusions,
        mode,
        None,
        &RankParams::default(),
        affinity,
    );
    let ids = out
        .ranking
        .ranked
        .iter()
        .map(|entry| entry.column_id.clone())
        .collect();
    (ids, out.decision)
}

fn close(got: Option<f64>, want: f64) {
    let got = got.expect("a turn cost");
    assert!((got - want).abs() < 1e-12, "{got} != {want}");
}

// ---------------------------------------------------------------------------------
// Cost-aware off is today's ranking.
// ---------------------------------------------------------------------------------

fn fixture_cases(name: &str) -> Vec<Value> {
    let fixture = parity_support::fixture("decision", name).expect("fixture loads");
    fixture["cases"].as_array().expect("cases").clone()
}

fn check_unchanged(case: &Value) {
    let id = case["id"].as_str().expect("case id");
    let input = &case["input"];
    let survivors: Vec<ScoredSurvivor> =
        serde_json::from_value(input["survivors"].clone()).expect("survivors");
    let weights: Option<Weights> =
        serde_json::from_value(input["weights"].clone()).expect("weights");
    let params: RankParams = serde_json::from_value(input["params"].clone()).expect("params");
    let mode = input["mode"].as_str().expect("mode");
    let today: Ranking = rank(&survivors, mode, weights, &params);
    let no_prices = BTreeMap::new();

    let previous = survivors.last().map(|s| s.column_id.clone());
    for affinity in [
        // Off: the order never changes, whatever the row says.
        AffinityContext {
            cache_warm: true,
            context_tokens: Some(120_000),
            ..context(previous.as_deref(), false)
        },
        // On with no turn cost and a zero bonus changes nothing either.
        AffinityContext {
            incumbent_bonus: 0.0,
            ..context(previous.as_deref(), true)
        },
    ] {
        let out = rank_with_affinity(
            &survivors,
            &no_prices,
            &[],
            mode,
            weights,
            &params,
            &affinity,
        );
        assert_eq!(out.ranking, today, "case {id}");
        let Some(previous) = previous.as_deref() else {
            assert_eq!(out.decision.status, AffinityStatus::New, "case {id}");
            continue;
        };
        let expected = match today.ranked.iter().position(|e| e.column_id == previous) {
            Some(0) => (AffinityStatus::Kept, None),
            Some(_) if !affinity.cost_aware => {
                (AffinityStatus::Switched, Some(AffinityReason::BestModel))
            }
            Some(_) => (AffinityStatus::Switched, out.decision.reason),
            None => (
                AffinityStatus::Broken,
                Some(AffinityReason::Excluded(ExclusionReason::NoData)),
            ),
        };
        assert_eq!(
            (out.decision.status, out.decision.reason),
            expected,
            "case {id}"
        );
    }
}

/// DIVERGENCE: D44. Every Go ranking fixture keeps its order with affinity off, and with
/// affinity on when there is neither a turn cost nor a bonus.
#[test]
fn affinity_without_adjustments_keeps_every_fixture_order() {
    for name in ["rank.json", "rank_property.json"] {
        fixture_cases(name).iter().for_each(check_unchanged);
    }
}

// ---------------------------------------------------------------------------------
// The incumbent bonus (step 2) and escalation (step 3), in quality mode: band 0.
// ---------------------------------------------------------------------------------

/// DIVERGENCE: D44. P (90) plus the 1.5 bonus is 91.5, ahead of T (91): kept. Without
/// cost-aware affinity, T leads alone and P is 1 point behind, outside the 0.75 noise
/// band: switched, best_model.
#[test]
fn a_rival_inside_the_bonus_keeps_the_pick() {
    let survivors = [survivor("col_p", 90.0, 0.05), survivor("col_t", 91.0, 0.05)];
    let none = BTreeMap::new();

    let (ids, on) = run(
        &survivors,
        &none,
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_p", "col_t"]);
    assert_eq!((on.status, on.reason), (AffinityStatus::Kept, None));
    assert_eq!(on.bonus, 1.5);
    assert_eq!(on.bonus_withheld, None);
    assert_eq!(on.turn_cost_skipped, Some(TurnCostSkipped::NoContextTokens));
    assert_eq!(on.ranking_costs, None);

    let (ids, off) = run(
        &survivors,
        &none,
        &[],
        "quality",
        &context(Some("col_p"), false),
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        (off.status, off.reason),
        (AffinityStatus::Switched, Some(AffinityReason::BestModel))
    );
    assert_eq!(off.bonus, 0.0);
    assert_eq!(off.turn_cost_skipped, Some(TurnCostSkipped::CostAwareOff));
}

/// DIVERGENCE: D44. T (92) beats P plus the bonus (91.5): switched, outranked.
#[test]
fn a_rival_beyond_the_bonus_takes_the_pick() {
    let survivors = [survivor("col_p", 90.0, 0.05), survivor("col_t", 92.0, 0.05)];
    let (ids, decision) = run(
        &survivors,
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        (decision.status, decision.reason),
        (AffinityStatus::Switched, Some(AffinityReason::Outranked))
    );
    assert_eq!(decision.bonus, 1.5);
}

/// DIVERGENCE: D44. P does not clear the tier and T does: the bonus is withheld, and T
/// takes the pick. When neither clears, the bonus applies again.
#[test]
fn escalation_withholds_the_bonus() {
    let mut p = survivor("col_p", 90.0, 0.05);
    p.clears_difficulty = false;
    let t = survivor("col_t", 91.0, 0.05);
    let (ids, decision) = run(
        &[p.clone(), t.clone()],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        (decision.status, decision.reason),
        (AffinityStatus::Switched, Some(AffinityReason::Escalation))
    );
    assert_eq!(decision.bonus, 0.0);
    assert_eq!(decision.bonus_withheld, Some(BonusWithheld::Escalation));

    let mut t = t;
    t.clears_difficulty = false;
    let (ids, decision) = run(
        &[p, t],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_p", "col_t"]);
    assert_eq!(decision.status, AffinityStatus::Kept);
    assert_eq!(decision.bonus, 1.5);
    assert_eq!(decision.bonus_withheld, None);
}

/// DIVERGENCE: D44. A pinned column with no quality gets no bonus.
#[test]
fn an_unmeasured_pinned_column_gets_no_bonus() {
    let mut p = survivor("col_p", 90.0, 0.05);
    p.quality = None;
    let (_, decision) = run(
        &[p, survivor("col_t", 91.0, 0.05)],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(decision.bonus, 0.0);
    assert_eq!(decision.bonus_withheld, None);
}

// ---------------------------------------------------------------------------------
// Partitions still win (step 4), and the reason order.
// ---------------------------------------------------------------------------------

/// DIVERGENCE: D44. P (92) violates a request preference and T (90) does not: T ranks
/// first however good P is.
#[test]
fn a_preference_violator_loses() {
    let mut p = survivor("col_p", 92.0, 0.05);
    p.prefs_satisfied = false;
    let (ids, decision) = run(
        &[p, survivor("col_t", 90.0, 0.05)],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(decision.reason, Some(AffinityReason::Preference));
}

/// DIVERGENCE: D44. P is under the availability floor: low_availability, which comes
/// before preference when both hold. When the pick is under the floor too, neither
/// partition explains the switch: outranked.
#[test]
fn low_availability_loses_and_comes_first() {
    let mut p = survivor("col_p", 92.0, 0.05);
    p.availability = Some(0.5);
    p.prefs_satisfied = false;
    let mut t = survivor("col_t", 90.0, 0.05);
    t.availability = Some(0.99);
    let (ids, decision) = run(
        &[p.clone(), t.clone()],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(decision.reason, Some(AffinityReason::LowAvailability));

    // Both under the floor, both satisfying: T (93) beats P (90) plus the bonus.
    let mut p = survivor("col_p", 90.0, 0.05);
    p.availability = Some(0.5);
    let mut t = survivor("col_t", 93.0, 0.05);
    t.availability = Some(0.5);
    let (ids, decision) = run(
        &[p, t],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(decision.reason, Some(AffinityReason::Outranked));
}

// ---------------------------------------------------------------------------------
// The turn cost (step 1), in cost mode: band 5.5, weights 0.25/0.6/0.15.
// ---------------------------------------------------------------------------------

fn cost_pair() -> [ScoredSurvivor; 2] {
    // P costs more per benchmark run (0.09) than T (0.04), and T is 1 point better.
    [survivor("col_p", 90.0, 0.09), survivor("col_t", 91.0, 0.04)]
}

fn turn(warm: bool) -> AffinityContext {
    AffinityContext {
        cache_warm: warm,
        context_tokens: Some(120_000),
        incumbent_bonus: 0.0,
        ..context(Some("col_p"), true)
    }
}

/// DIVERGENCE: D44. Without the turn cost, T is better and cheaper: V_T = 0.25 + 0.6 =
/// 0.85 and V_P = 0. With a warm cache, P pays its cache-read price: C'_P = 0.09 +
/// 120000 x 0.50 / 1e6 = 0.15. T pays its cache-write price, not its read price: C'_T =
/// 0.04 + 120000 x 12.50 / 1e6 = 1.54. Now V_P = 0.6 and V_T = 0.25, so P is kept. A
/// zero bonus still applies the turn cost.
#[test]
fn the_turn_cost_flips_a_close_cost_mode_ranking() {
    let survivors = cost_pair();
    let table = BTreeMap::from([
        ("col_p".to_owned(), prices(5.0, Some(0.5), Some(6.25))),
        ("col_t".to_owned(), prices(10.0, Some(1.25), Some(12.5))),
    ]);

    let (ids, _) = run(
        &survivors,
        &table,
        &[],
        "cost",
        &context(Some("col_p"), false),
    );
    assert_eq!(ids, ["col_t", "col_p"]);

    let (ids, decision) = run(&survivors, &table, &[], "cost", &turn(true));
    assert_eq!(ids, ["col_p", "col_t"]);
    assert_eq!(decision.status, AffinityStatus::Kept);
    assert!(decision.cache_warm);
    assert_eq!(decision.bonus, 0.0);
    assert_eq!(decision.turn_cost_skipped, None);
    let costs = decision.ranking_costs.expect("turn costs");
    assert_eq!(costs.len(), 2);
    close(costs["col_p"], 0.15);
    close(costs["col_t"], 1.54);
}

/// DIVERGENCE: D44. With a cold cache, P pays its cache-write price: C'_P = 0.09 +
/// 120000 x 12.50 / 1e6 = 1.59, more than C'_T = 1.54, so T takes the pick. Warm, P pays
/// 0.09 + 120000 x 1.00 / 1e6 = 0.21 and is kept.
#[test]
fn a_cold_cache_charges_the_pinned_column_the_write_price() {
    let survivors = cost_pair();
    let table = BTreeMap::from([
        ("col_p".to_owned(), prices(10.0, Some(1.0), Some(12.5))),
        ("col_t".to_owned(), prices(10.0, None, Some(12.5))),
    ]);

    let (ids, warm) = run(&survivors, &table, &[], "cost", &turn(true));
    assert_eq!(ids, ["col_p", "col_t"]);
    close(warm.ranking_costs.as_ref().expect("costs")["col_p"], 0.21);

    let (ids, cold) = run(&survivors, &table, &[], "cost", &turn(false));
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        (cold.status, cold.reason),
        (AffinityStatus::Switched, Some(AffinityReason::Outranked))
    );
    assert!(!cold.cache_warm);
    let costs = cold.ranking_costs.expect("costs");
    close(costs["col_p"], 1.59);
    close(costs["col_t"], 1.54);
}

/// DIVERGENCE: D44. Without a cache-write price, a column pays its input price: C'_T =
/// 0.04 + 120000 x 3.00 / 1e6 = 0.40. A warm P without a cache-read price pays its write
/// price: 0.09 + 120000 x 6.25 / 1e6 = 0.84.
#[test]
fn the_input_price_is_the_last_resort() {
    let table = BTreeMap::from([
        ("col_p".to_owned(), prices(5.0, None, Some(6.25))),
        ("col_t".to_owned(), prices(3.0, None, None)),
    ]);
    let (_, decision) = run(&cost_pair(), &table, &[], "cost", &turn(true));
    let costs = decision.ranking_costs.expect("costs");
    close(costs["col_p"], 0.84);
    close(costs["col_t"], 0.40);
}

/// DIVERGENCE: D44. A survivor without an input price skips the turn cost for everyone;
/// so does a request without `context_tokens`.
#[test]
fn a_missing_input_price_or_token_count_skips_the_turn_cost() {
    let survivors = cost_pair();
    let partial = BTreeMap::from([("col_p".to_owned(), prices(5.0, Some(0.5), None))]);
    let (ids, decision) = run(&survivors, &partial, &[], "cost", &turn(true));
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        decision.turn_cost_skipped,
        Some(TurnCostSkipped::MissingInputPrice)
    );
    assert_eq!(decision.ranking_costs, None);

    let zero = BTreeMap::from([
        ("col_p".to_owned(), prices(5.0, Some(0.5), None)),
        (
            "col_t".to_owned(),
            TurnPrices::from_pricing(&Pricing::default()),
        ),
    ]);
    let (_, decision) = run(&survivors, &zero, &[], "cost", &turn(true));
    assert_eq!(
        decision.turn_cost_skipped,
        Some(TurnCostSkipped::MissingInputPrice)
    );

    let full = BTreeMap::from([
        ("col_p".to_owned(), prices(5.0, Some(0.5), None)),
        ("col_t".to_owned(), prices(10.0, None, Some(12.5))),
    ]);
    let no_tokens = AffinityContext {
        context_tokens: None,
        ..turn(true)
    };
    let (ids, decision) = run(&survivors, &full, &[], "cost", &no_tokens);
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        decision.turn_cost_skipped,
        Some(TurnCostSkipped::NoContextTokens)
    );
    assert_eq!(decision.ranking_costs, None);
}

/// DIVERGENCE: D44. A `no_data` survivor is not ranked, so its missing price does not
/// skip the turn cost, and it gets no turn cost entry.
#[test]
fn a_no_data_survivor_does_not_block_the_turn_cost() {
    let mut x = survivor("col_x", 95.0, 0.01);
    x.has_data = false;
    let [p, t] = cost_pair();
    let table = BTreeMap::from([
        ("col_p".to_owned(), prices(5.0, Some(0.5), None)),
        ("col_t".to_owned(), prices(10.0, None, Some(12.5))),
    ]);
    let (ids, decision) = run(&[p, t, x], &table, &[], "cost", &turn(true));
    assert_eq!(ids, ["col_p", "col_t"]);
    let costs = decision.ranking_costs.expect("costs");
    assert_eq!(
        costs.keys().map(String::as_str).collect::<Vec<_>>(),
        ["col_p", "col_t"]
    );
}

/// DIVERGENCE: D44. When no survivor has data, every one is ranked unmeasured, so every
/// one gets a turn cost entry. Without a cost EWMA the entry has no cost.
#[test]
fn unmeasured_survivors_still_get_turn_cost_entries() {
    let unmeasured = |id: &str| ScoredSurvivor {
        quality: None,
        cost_per_run: None,
        has_data: false,
        measured: false,
        ..survivor(id, 0.0, 0.0)
    };
    let table = BTreeMap::from([
        ("col_p".to_owned(), prices(5.0, None, None)),
        ("col_t".to_owned(), prices(5.0, None, None)),
    ]);
    let (ids, decision) = run(
        &[unmeasured("col_p"), unmeasured("col_t")],
        &table,
        &[],
        "cost",
        &turn(false),
    );
    assert_eq!(ids, ["col_p", "col_t"]);
    assert_eq!(decision.status, AffinityStatus::Kept);
    assert_eq!(
        decision.ranking_costs,
        Some(BTreeMap::from([
            ("col_p".to_owned(), None),
            ("col_t".to_owned(), None)
        ]))
    );
}

/// DIVERGENCE: D44. An input price of 0 is no price: an empty snapshot parses to zero.
#[test]
fn turn_prices_read_the_snapshot() {
    let pricing = Pricing {
        input_per_mtok: 3.0,
        cache_read_per_mtok: Some(0.3),
        cache_write_per_mtok: Some(3.75),
        ..Pricing::default()
    };
    assert_eq!(
        TurnPrices::from_pricing(&pricing),
        prices(3.0, Some(0.3), Some(3.75))
    );
    assert_eq!(
        TurnPrices::from_pricing(&Pricing::default()),
        TurnPrices::default()
    );
}

// ---------------------------------------------------------------------------------
// New and broken.
// ---------------------------------------------------------------------------------

/// DIVERGENCE: D44. No live row: new, no reason, and the cache is never warm.
#[test]
fn no_row_is_new() {
    let warm = AffinityContext {
        cache_warm: true,
        ..context(None, true)
    };
    let (ids, decision) = run(
        &[survivor("col_p", 90.0, 0.05), survivor("col_t", 91.0, 0.05)],
        &BTreeMap::new(),
        &[],
        "quality",
        &warm,
    );
    assert_eq!(ids, ["col_t", "col_p"]);
    assert_eq!(
        (decision.status, decision.reason),
        (AffinityStatus::New, None)
    );
    assert!(!decision.cache_warm);
    assert_eq!(decision.previous_column_id, None);
    assert_eq!(decision.bonus, 0.0);
}

/// DIVERGENCE: D44. A pinned column that did not survive is broken with its exclusion
/// reason: `no_data` from the ranking, a §7.1 reason, or `not_in_pool` when no step
/// names it.
#[test]
fn a_pinned_column_that_did_not_survive_is_broken() {
    let survivors = [survivor("col_t", 91.0, 0.05)];
    for reason in [
        ExclusionReason::NotInAllowlist,
        ExclusionReason::StandingZeroRetention,
        ExclusionReason::StatusRetired,
    ] {
        let excluded = [Exclusion {
            column_id: "col_p".to_owned(),
            reason,
            detail: String::new(),
        }];
        let (ids, decision) = run(
            &survivors,
            &BTreeMap::new(),
            &excluded,
            "quality",
            &context(Some("col_p"), true),
        );
        assert_eq!(ids, ["col_t"]);
        assert_eq!(
            (decision.status, decision.reason),
            (
                AffinityStatus::Broken,
                Some(AffinityReason::Excluded(reason))
            )
        );
        assert_eq!(decision.previous_column_id.as_deref(), Some("col_p"));
    }

    let (_, decision) = run(
        &survivors,
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), false),
    );
    assert_eq!(
        decision.reason,
        Some(AffinityReason::Excluded(ExclusionReason::NotInPool))
    );

    let mut p = survivor("col_p", 99.0, 0.05);
    p.has_data = false;
    let (ids, decision) = run(
        &[p, survivor("col_t", 91.0, 0.05)],
        &BTreeMap::new(),
        &[],
        "quality",
        &context(Some("col_p"), true),
    );
    assert_eq!(ids, ["col_t"]);
    assert_eq!(
        decision.reason,
        Some(AffinityReason::Excluded(ExclusionReason::NoData))
    );
    assert_eq!(decision.bonus, 0.0);
}

// ---------------------------------------------------------------------------------
// Wire spellings.
// ---------------------------------------------------------------------------------

/// DIVERGENCE: D44. The closed reason set and the decision's JSON keys.
#[test]
fn reasons_and_decisions_serialize_as_the_spec_spells_them() {
    let fixed = [
        (AffinityReason::BestModel, "best_model"),
        (AffinityReason::LowAvailability, "low_availability"),
        (AffinityReason::Preference, "preference"),
        (AffinityReason::Escalation, "escalation"),
        (AffinityReason::Outranked, "outranked"),
    ];
    for (reason, text) in fixed {
        assert_eq!(reason.as_str(), text);
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(text));
    }
    for reason in ExclusionReason::ALL {
        assert_eq!(AffinityReason::Excluded(reason).as_str(), reason.as_str());
    }

    let decision = AffinityDecision {
        previous_column_id: Some("col_p".to_owned()),
        status: AffinityStatus::Switched,
        reason: Some(AffinityReason::Escalation),
        cache_warm: false,
        context_tokens: Some(7),
        cost_aware: true,
        bonus: 0.0,
        bonus_withheld: Some(BonusWithheld::Escalation),
        turn_cost_skipped: Some(TurnCostSkipped::MissingInputPrice),
        ranking_costs: None,
    };
    assert_eq!(
        serde_json::to_string(&decision).unwrap(),
        r#"{"previous_column_id":"col_p","status":"switched","reason":"escalation","cache_warm":false,"context_tokens":7,"cost_aware":true,"bonus":0.0,"bonus_withheld":"escalation","turn_cost_skipped":"missing_input_price","ranking_costs":null}"#
    );
    for (status, text) in [
        (AffinityStatus::New, "new"),
        (AffinityStatus::Kept, "kept"),
        (AffinityStatus::Broken, "broken"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(text));
    }
    for (skipped, text) in [
        (TurnCostSkipped::CostAwareOff, "cost_aware_off"),
        (TurnCostSkipped::NoContextTokens, "no_context_tokens"),
    ] {
        assert_eq!(serde_json::to_value(skipped).unwrap(), json!(text));
    }
}

// ---------------------------------------------------------------------------------
// The telemetry follow rule (spec §9.10).
// ---------------------------------------------------------------------------------

fn chain() -> Vec<ServedEntry> {
    serde_json::from_value(json!([
        {"provider": "anthropic", "model": "claude-a", "endpoint": "direct", "column_id": "col_0",
         "execution_aliases": {"openrouter": "anthropic/claude-a"}, "settings": {}},
        {"model": "gpt-b", "endpoint": "direct", "column_id": "col_1", "execution_aliases": {}},
        {"model": "claude-a", "endpoint": "vertex", "column_id": "col_2",
         "execution_aliases": {"openrouter": "anthropic/claude-a-vertex", "odd": 5}},
        {"model": "gpt-b", "endpoint": "direct", "column_id": "col_3"}
    ]))
    .expect("chain")
}

/// DIVERGENCE: D44. A later entry that served the turn wins; the first of two that share
/// model and endpoint wins.
#[test]
fn a_later_entry_that_served_the_turn_is_followed() {
    let chain = chain();
    assert_eq!(served_column(&chain, "gpt-b", "direct"), Some("col_1"));
    assert_eq!(served_column(&chain, "claude-a", "vertex"), Some("col_2"));
}

/// DIVERGENCE: D44. An OpenRouter item names the alias, not the model.
#[test]
fn an_execution_alias_matches_its_entry() {
    let chain = chain();
    assert_eq!(
        served_column(&chain, "anthropic/claude-a-vertex", "openrouter"),
        Some("col_2")
    );
    // The alias name must match the endpoint, and its value the model.
    assert_eq!(
        served_column(&chain, "anthropic/claude-a-vertex", "direct"),
        None
    );
    assert_eq!(served_column(&chain, "claude-a", "openrouter"), None);
    // A non-string alias value never matches.
    assert_eq!(served_column(&chain, "5", "odd"), None);
}

/// DIVERGENCE: D44. When the pick served the turn (directly or through its alias), or no
/// entry matches, the row stays.
#[test]
fn the_pick_or_an_unknown_model_changes_nothing() {
    let chain = chain();
    assert_eq!(served_column(&chain, "claude-a", "direct"), None);
    assert_eq!(
        served_column(&chain, "anthropic/claude-a", "openrouter"),
        None
    );
    assert_eq!(served_column(&chain, "gpt-b", "vertex"), None);
    assert_eq!(served_column(&chain, "unknown", "direct"), None);
    assert_eq!(served_column(&[], "gpt-b", "direct"), None);
}
