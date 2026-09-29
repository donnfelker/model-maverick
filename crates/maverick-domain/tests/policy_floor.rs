//! `pool_floor` (reference Part 4 §1.5 `poolFloor`): the fewest usable columns any cell leaves,
//! the reducing filter, and the cell. The G4 simulation fails a pool at 0 survivors (covered by
//! the `policy_simulate.json` replay); the nightly drift check (bead 7.9) alerts at 1 or fewer,
//! so the non-zero floors here are what it reads.

use chrono::{DateTime, Duration, Utc};
use maverick_domain::policy::{
    CellEwma, ColumnRetention, ColumnStatus, PoolFloor, RetentionMode, SimCell, SimulationState,
    StandingConstraints, pool_floor,
};

fn now() -> DateTime<Utc> {
    "2031-09-10T06:10:00Z".parse().expect("clock")
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn cell(capability: &str, tier: i32) -> SimCell {
    SimCell {
        capability: capability.to_owned(),
        difficulty_tier: tier,
    }
}

fn ewma(column: &str, capability: &str, tier: i32, value: f64) -> CellEwma {
    CellEwma {
        column_id: column.to_owned(),
        capability: capability.to_owned(),
        difficulty_tier: tier,
        ewma: value,
    }
}

fn state(cells: &[SimCell], cost: Vec<CellEwma>, latency: Vec<CellEwma>) -> SimulationState {
    SimulationState {
        cells_sorted: cells.to_vec(),
        cost_ewmas: cost,
        latency_ewmas: latency,
        ..SimulationState::default()
    }
}

fn cost_limit(limit: f64) -> StandingConstraints {
    StandingConstraints {
        max_cost_per_run: Some(limit),
        ..StandingConstraints::default()
    }
}

fn floor(survivors: usize, constraint: &'static str, cell: Option<SimCell>) -> PoolFloor {
    PoolFloor {
        survivors,
        constraint,
        cell,
    }
}

#[test]
fn retired_columns_are_not_usable() {
    let mut st = state(&[], vec![], vec![]);
    st.column_status.insert("a".into(), ColumnStatus::Retired);
    st.column_status.insert("b".into(), ColumnStatus::Retired);
    let none = StandingConstraints::default();
    assert_eq!(
        pool_floor(&ids(&["a", "b"]), &none, &st, now()),
        floor(0, "status_retired", None)
    );
    // An active column and one with no status row are both usable.
    st.column_status.insert("c".into(), ColumnStatus::Active);
    assert_eq!(
        pool_floor(&ids(&["a", "c", "d"]), &none, &st, now()),
        floor(2, "", None)
    );
}

#[test]
fn zero_retention_keeps_only_unexpired_zero_evidence() {
    let mut st = state(&[], vec![], vec![]);
    let zero_until = |days| ColumnRetention {
        mode: RetentionMode::Zero,
        valid_until: Some(now() + Duration::days(days)),
    };
    st.column_retention.insert("valid".into(), zero_until(30));
    st.column_retention.insert("expired".into(), zero_until(-1));
    st.column_retention.insert(
        "standard".into(),
        ColumnRetention {
            mode: RetentionMode::Standard,
            valid_until: Some(now() + Duration::days(30)),
        },
    );
    let zr = StandingConstraints {
        zero_retention: true,
        ..StandingConstraints::default()
    };
    assert_eq!(
        pool_floor(&ids(&["expired", "standard", "none"]), &zr, &st, now()),
        floor(0, "zero_retention", None)
    );
    assert_eq!(
        pool_floor(&ids(&["valid", "expired", "none"]), &zr, &st, now()),
        floor(1, "", None)
    );
    // Without the constraint, evidence does not matter.
    assert_eq!(
        pool_floor(
            &ids(&["expired", "standard", "none"]),
            &StandingConstraints::default(),
            &st,
            now()
        ),
        floor(3, "", None)
    );
}

#[test]
fn no_constraints_leave_every_usable_column() {
    let st = state(
        &[cell("coding", 1)],
        vec![ewma("a", "coding", 1, 9.0)],
        vec![ewma("a", "coding", 1, 9_000.0)],
    );
    assert_eq!(
        pool_floor(
            &ids(&["a", "b"]),
            &StandingConstraints::default(),
            &st,
            now()
        ),
        floor(2, "", None)
    );
}

#[test]
fn cost_reduces_and_equal_or_missing_ewmas_pass() {
    let st = state(
        &[cell("coding", 3)],
        vec![
            ewma("cheap", "coding", 3, 0.2),
            ewma("at_limit", "coding", 3, 0.5),
            ewma("dear", "coding", 3, 0.51),
        ],
        vec![],
    );
    // `unmeasured` has no EWMA, so it passes.
    assert_eq!(
        pool_floor(
            &ids(&["cheap", "at_limit", "dear", "unmeasured"]),
            &cost_limit(0.5),
            &st,
            now()
        ),
        floor(3, "max_cost_per_run", Some(cell("coding", 3)))
    );
}

#[test]
fn latency_is_the_reducer_only_when_it_removes_a_column() {
    let st = state(
        &[cell("coding", 1)],
        vec![ewma("a", "coding", 1, 1.0), ewma("b", "coding", 1, 0.1)],
        vec![
            ewma("b", "coding", 1, 900.0),
            ewma("c", "coding", 1, 100.0),
            ewma("d", "coding", 1, 500.0),
        ],
    );
    let both = |latency| StandingConstraints {
        max_cost_per_run: Some(0.5),
        max_latency_ms: Some(latency),
        zero_retention: false,
    };
    // Cost removes a; latency 1000 removes nothing: cost is the reducer.
    assert_eq!(
        pool_floor(&ids(&["a", "b", "c", "d"]), &both(1_000), &st, now()),
        floor(3, "max_cost_per_run", Some(cell("coding", 1)))
    );
    // Latency 500 (equal passes) then removes b as well: latency is the reducer.
    assert_eq!(
        pool_floor(&ids(&["a", "b", "c", "d"]), &both(500), &st, now()),
        floor(2, "max_latency_ms", Some(cell("coding", 1)))
    );
    // Cost removes a, latency the rest: latency is reported (QUIRK).
    assert_eq!(
        pool_floor(&ids(&["a", "b"]), &both(500), &st, now()),
        floor(0, "max_latency_ms", Some(cell("coding", 1)))
    );
    // Latency alone.
    let latency_only = StandingConstraints {
        max_latency_ms: Some(200),
        ..StandingConstraints::default()
    };
    assert_eq!(
        pool_floor(&ids(&["b", "c", "d"]), &latency_only, &st, now()),
        floor(1, "max_latency_ms", Some(cell("coding", 1)))
    );
}

#[test]
fn cost_emptying_a_cell_stays_the_reducer() {
    let st = state(
        &[cell("coding", 1)],
        vec![ewma("a", "coding", 1, 1.0), ewma("b", "coding", 1, 2.0)],
        vec![ewma("a", "coding", 1, 900.0)],
    );
    let both = StandingConstraints {
        max_cost_per_run: Some(0.5),
        max_latency_ms: Some(100),
        zero_retention: false,
    };
    assert_eq!(
        pool_floor(&ids(&["a", "b"]), &both, &st, now()),
        floor(0, "max_cost_per_run", Some(cell("coding", 1)))
    );
}

#[test]
fn the_first_cell_reaching_the_minimum_is_reported() {
    let cells = [cell("coding", 1), cell("coding", 2), cell("writing", 1)];
    // coding/1 and coding/2 both leave 2 of 3; writing/1 leaves 1.
    let st = state(
        &cells,
        vec![
            ewma("a", "coding", 1, 1.0),
            ewma("b", "coding", 2, 1.0),
            ewma("a", "writing", 1, 1.0),
            ewma("c", "writing", 1, 1.0),
        ],
        vec![],
    );
    assert_eq!(
        pool_floor(&ids(&["a", "b", "c"]), &cost_limit(0.5), &st, now()),
        floor(1, "max_cost_per_run", Some(cell("writing", 1)))
    );
    // Without the writing cell, the tie between coding/1 and coding/2 keeps coding/1.
    let st = state(&cells[..2], st.cost_ewmas.clone(), vec![]);
    assert_eq!(
        pool_floor(&ids(&["a", "b", "c"]), &cost_limit(0.5), &st, now()),
        floor(2, "max_cost_per_run", Some(cell("coding", 1)))
    );
}

#[test]
fn an_empty_cell_is_not_replaced_by_a_later_one() {
    let cells = [cell("coding", 1), cell("coding", 2)];
    let st = state(
        &cells,
        vec![
            ewma("a", "coding", 1, 1.0),
            ewma("b", "coding", 1, 1.0),
            ewma("a", "coding", 2, 1.0),
            ewma("b", "coding", 2, 1.0),
        ],
        vec![],
    );
    assert_eq!(
        pool_floor(&ids(&["a", "b"]), &cost_limit(0.5), &st, now()),
        floor(0, "max_cost_per_run", Some(cell("coding", 1)))
    );
}

#[test]
fn an_ewma_applies_only_to_its_own_column_capability_and_tier() {
    let cells = [cell("coding", 1)];
    let expensive_elsewhere = vec![
        ewma("a", "coding", 2, 9.0),
        ewma("a", "writing", 1, 9.0),
        ewma("other", "coding", 1, 9.0),
    ];
    let st = state(&cells, expensive_elsewhere.clone(), expensive_elsewhere);
    let both = StandingConstraints {
        max_cost_per_run: Some(0.5),
        max_latency_ms: Some(1),
        zero_retention: false,
    };
    assert_eq!(
        pool_floor(&ids(&["a"]), &both, &st, now()),
        floor(1, "", None)
    );
}
