//! Conformance scenario 6 (sdk-v0 plan §2.3): the fail-open ladder and the zero-retention
//! privacy rules (G14, D17).

use std::cell::Cell;
use std::collections::BTreeSet;
use std::time::Duration;

use maverick_core::failopen::{FailOpen, FailOpenConfig, LadderError, Plan, Privacy};
use maverick_core::recommend::{
    ChainEntry, EngineFailure, RecommendError, Recommendation, RetentionMode,
};

fn entry(model: &str, retention: RetentionMode) -> ChainEntry {
    ChainEntry {
        provider: "anthropic".to_owned(),
        model: model.to_owned(),
        endpoint: "direct".to_owned(),
        column_id: format!("col_{model}"),
        retention_mode: retention,
        ..ChainEntry::default()
    }
}

fn recommendation(zero_retention: bool, chain: Vec<ChainEntry>) -> Recommendation {
    Recommendation {
        recommendation_id: "rec_1".to_owned(),
        chain,
        measured: true,
        classification_method: "declared".to_owned(),
        zero_retention,
        classification_quota_exceeded: false,
        telemetry_opt_out: false,
    }
}

fn down() -> RecommendError {
    RecommendError::Engine(EngineFailure::Transport("connection refused".to_owned()))
}

fn ladder(default_model: Option<&str>, verified: &[&str]) -> FailOpen {
    FailOpen::new(FailOpenConfig {
        default_model: default_model.map(str::to_owned),
        zero_retention_verified: verified
            .iter()
            .map(|m| (*m).to_owned())
            .collect::<BTreeSet<_>>(),
    })
}

async fn plan(
    ladder: &FailOpen,
    tenant: &str,
    outcome: Result<Recommendation, RecommendError>,
    original: Option<&str>,
) -> Result<Plan, LadderError> {
    ladder
        .plan(
            tenant,
            "structured_extraction/3",
            Privacy::Declared,
            original,
            || async { outcome },
        )
        .await
}

fn models(plan: &Plan) -> Vec<String> {
    match plan {
        Plan::Recommended(rec) => rec.chain.iter().map(|e| e.model.clone()).collect(),
        Plan::LastKnownGood(chain) => chain.iter().map(|e| e.model.clone()).collect(),
        Plan::DefaultModel(m) | Plan::OriginalModel(m) => vec![m.clone()],
    }
}

/// MM down: the cached chain; cache cold: the default model, then the caller's model.
#[tokio::test]
async fn mm_down_uses_the_cache_then_the_default() {
    let ladder = ladder(Some("claude-haiku-4-5"), &[]);
    let chain = vec![
        entry("claude-sonnet-4-6", RetentionMode::Standard),
        entry("gpt-5.5", RetentionMode::Unknown),
    ];
    let fresh = plan(&ladder, "ten_a", Ok(recommendation(false, chain)), None)
        .await
        .expect("recommended");
    assert!(matches!(fresh, Plan::Recommended(_)));

    let warm = plan(&ladder, "ten_a", Err(down()), Some("gpt-4o"))
        .await
        .expect("warm");
    assert!(matches!(warm, Plan::LastKnownGood(_)));
    assert_eq!(models(&warm), ["claude-sonnet-4-6", "gpt-5.5"]);

    // Another workload key or tenant has no cache: the default model.
    let cold = ladder
        .plan(
            "ten_b",
            "structured_extraction/3",
            Privacy::Declared,
            Some("gpt-4o"),
            || async { Err(down()) },
        )
        .await
        .expect("cold");
    assert_eq!(cold, Plan::DefaultModel("claude-haiku-4-5".to_owned()));

    // No default: the caller's concrete model; nothing at all: the failure surfaces.
    let bare = ladder_without_default();
    assert_eq!(
        plan(&bare, "ten_c", Err(down()), Some("gpt-4o")).await,
        Ok(Plan::OriginalModel("gpt-4o".to_owned()))
    );
    assert!(matches!(
        plan(&bare, "ten_c", Err(down()), None).await,
        Err(LadderError::Unavailable(EngineFailure::Transport(_)))
    ));
}

fn ladder_without_default() -> FailOpen {
    ladder(None, &[])
}

/// A last-known-good chain expires after the TTL.
#[tokio::test]
async fn the_cache_expires() {
    let ladder = FailOpen::with_ttl(
        FailOpenConfig {
            default_model: Some("claude-haiku-4-5".to_owned()),
            zero_retention_verified: BTreeSet::new(),
        },
        Duration::from_millis(50),
    );
    plan(
        &ladder,
        "ten_a",
        Ok(recommendation(
            false,
            vec![entry("claude-sonnet-4-6", RetentionMode::Standard)],
        )),
        None,
    )
    .await
    .expect("recommended");
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        plan(&ladder, "ten_a", Err(down()), None).await,
        Ok(Plan::DefaultModel("claude-haiku-4-5".to_owned()))
    );
}

/// `privacy: off`: MM is never contacted.
#[tokio::test]
async fn privacy_off_never_contacts_mm() {
    let contacted = Cell::new(0);
    let with_default = ladder(Some("claude-haiku-4-5"), &[]);
    let got = with_default
        .plan("ten_a", "w", Privacy::Off, Some("gpt-4o"), || {
            contacted.set(contacted.get() + 1);
            async { Ok(recommendation(false, Vec::new())) }
        })
        .await;
    assert_eq!(got, Ok(Plan::DefaultModel("claude-haiku-4-5".to_owned())));
    let bare = ladder_without_default();
    let got = bare
        .plan("ten_a", "w", Privacy::Off, Some("gpt-4o"), || {
            contacted.set(contacted.get() + 1);
            async { Ok(recommendation(false, Vec::new())) }
        })
        .await;
    assert_eq!(got, Ok(Plan::OriginalModel("gpt-4o".to_owned())));
    let got = bare
        .plan("ten_a", "w", Privacy::Off, None, || {
            contacted.set(contacted.get() + 1);
            async { Ok(recommendation(false, Vec::new())) }
        })
        .await;
    assert_eq!(got, Err(LadderError::NoModel));
    assert_eq!(contacted.get(), 0);
}

/// (a) The terminal privacy errors surface, never fail open, and purge the tenant's
/// cached chains (other tenants keep theirs).
#[tokio::test]
async fn terminal_privacy_errors_purge_and_surface() {
    let _ = parity_support::divergence!("D17");
    let ladder = ladder(Some("claude-haiku-4-5"), &[]);
    for tenant in ["ten_zr", "ten_other"] {
        plan(
            &ladder,
            tenant,
            Ok(recommendation(
                false,
                vec![entry("claude-sonnet-4-6", RetentionMode::Zero)],
            )),
            None,
        )
        .await
        .expect("recommended");
    }
    for terminal in [
        RecommendError::ZeroRetentionRequiresDeclared {
            status: 422,
            message: "the prompt form is refused".to_owned(),
        },
        RecommendError::NoCompliantColumns {
            status: 409,
            message: "no column in pool \"default\" holds valid zero-retention evidence".to_owned(),
        },
    ] {
        let got = plan(&ladder, "ten_zr", Err(terminal.clone()), Some("gpt-4o")).await;
        assert_eq!(got, Err(LadderError::Terminal(terminal)));
    }
    assert!(ladder.is_zero_retention("ten_zr"));
    // The purged tenant's next outage cannot use the old chain; the default model is not
    // verified zero retention, so it fails closed.
    assert_eq!(
        plan(&ladder, "ten_zr", Err(down()), Some("gpt-4o")).await,
        Err(LadderError::NoCompliantColumns)
    );
    // The other tenant still has its cache.
    let other = plan(&ladder, "ten_other", Err(down()), None)
        .await
        .expect("other");
    assert!(matches!(other, Plan::LastKnownGood(_)));
}

/// (b) A warm-cache outage for a zero-retention tenant uses only `zero`-stamped entries.
#[tokio::test]
async fn warm_outage_uses_only_zero_entries() {
    let ladder = ladder(Some("claude-haiku-4-5"), &[]);
    let chain = vec![
        entry("claude-sonnet-4-6", RetentionMode::Standard),
        entry("claude-opus-4-8", RetentionMode::Zero),
        entry("gpt-5.5", RetentionMode::Unknown),
    ];
    let fresh = plan(&ladder, "ten_zr", Ok(recommendation(true, chain)), None)
        .await
        .expect("recommended");
    assert!(matches!(fresh, Plan::Recommended(_)));
    assert!(ladder.is_zero_retention("ten_zr"));
    let warm = plan(&ladder, "ten_zr", Err(down()), None)
        .await
        .expect("warm");
    assert_eq!(models(&warm), ["claude-opus-4-8"]);

    // A cached chain with no zero entry falls to the verified-default rule.
    let only_standard = ladder_with_verified_default();
    plan(
        &only_standard,
        "ten_zr",
        Ok(recommendation(
            true,
            vec![entry("claude-sonnet-4-6", RetentionMode::Standard)],
        )),
        None,
    )
    .await
    .expect("recommended");
    assert_eq!(
        plan(&only_standard, "ten_zr", Err(down()), None).await,
        Ok(Plan::DefaultModel("claude-haiku-4-5".to_owned()))
    );
}

fn ladder_with_verified_default() -> FailOpen {
    ladder(Some("claude-haiku-4-5"), &["claude-haiku-4-5"])
}

/// (c) A cold-cache outage for a zero-retention tenant fails closed unless the default
/// (or the caller's model) is locally marked verified zero retention.
#[tokio::test]
async fn cold_outage_fails_closed_unless_verified() {
    let unverified = ladder(Some("claude-haiku-4-5"), &[]);
    plan(
        &unverified,
        "ten_zr",
        Ok(recommendation(true, Vec::new())),
        None,
    )
    .await
    .expect("recommended");
    assert_eq!(
        plan(&unverified, "ten_zr", Err(down()), Some("gpt-4o")).await,
        Err(LadderError::NoCompliantColumns)
    );

    let verified = ladder_with_verified_default();
    plan(
        &verified,
        "ten_zr",
        Ok(recommendation(true, Vec::new())),
        None,
    )
    .await
    .expect("recommended");
    assert_eq!(
        plan(&verified, "ten_zr", Err(down()), Some("gpt-4o")).await,
        Ok(Plan::DefaultModel("claude-haiku-4-5".to_owned()))
    );

    let caller_verified = ladder(None, &["gpt-4o"]);
    plan(
        &caller_verified,
        "ten_zr",
        Ok(recommendation(true, Vec::new())),
        None,
    )
    .await
    .expect("recommended");
    assert_eq!(
        plan(&caller_verified, "ten_zr", Err(down()), Some("gpt-4o")).await,
        Ok(Plan::OriginalModel("gpt-4o".to_owned()))
    );
    assert_eq!(
        plan(&caller_verified, "ten_zr", Err(down()), Some("gpt-5.5")).await,
        Err(LadderError::NoCompliantColumns)
    );
}

/// A later response without the flag clears the tenant's zero-retention state.
#[tokio::test]
async fn zero_retention_state_follows_the_engine() {
    let ladder = ladder(None, &[]);
    plan(&ladder, "ten_a", Ok(recommendation(true, Vec::new())), None)
        .await
        .expect("recommended");
    assert!(ladder.is_zero_retention("ten_a"));
    plan(
        &ladder,
        "ten_a",
        Ok(recommendation(false, Vec::new())),
        None,
    )
    .await
    .expect("recommended");
    assert!(!ladder.is_zero_retention("ten_a"));
    assert!(!ladder.is_zero_retention("ten_never_seen"));
}
