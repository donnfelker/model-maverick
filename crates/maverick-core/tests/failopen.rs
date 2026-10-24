//! Conformance scenario 6 (sdk-v0 plan §2.3): the fail-open ladder and the zero-retention
//! privacy rules (G14, D17).

use std::cell::Cell;
use std::collections::BTreeSet;
use std::time::Duration;

use maverick_core::failopen::{FailOpen, FailOpenConfig, Fallback, LadderError, Plan, Privacy};
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

fn with_id(id: &str, zero_retention: bool, chain: Vec<ChainEntry>) -> Recommendation {
    Recommendation {
        recommendation_id: id.to_owned(),
        ..recommendation(zero_retention, chain)
    }
}

fn fallback_models(fallback: Option<Fallback>) -> Option<(String, Vec<String>)> {
    fallback.map(|fallback| match fallback {
        Fallback::LastKnownGood(rec) => (
            rec.recommendation_id,
            rec.chain.iter().map(|e| e.model.clone()).collect(),
        ),
        Fallback::DefaultModel(model) => ("default".to_owned(), vec![model]),
    })
}

/// D46: the proxy's steps. A recorded recommendation is the key's last good one, with
/// its id; another key gets the default it passes; no default means nothing usable.
#[tokio::test]
async fn the_proxy_falls_back_to_the_last_good_recommendation_then_its_default() {
    let ladder = ladder(None, &[]);
    let chain = vec![
        entry("claude-sonnet-4-6", RetentionMode::Standard),
        entry("claude-opus-4-8", RetentionMode::Unknown),
    ];
    ladder.record("ten_a", "auto:quality", &with_id("rec_9", false, chain));
    assert_eq!(
        fallback_models(ladder.fall_back("ten_a", "auto:quality", Some("claude-haiku-4-5"))),
        Some((
            "rec_9".to_owned(),
            vec!["claude-sonnet-4-6".to_owned(), "claude-opus-4-8".to_owned()]
        ))
    );
    assert_eq!(
        ladder.fall_back("ten_a", "auto:cost", Some("claude-haiku-4-5")),
        Some(Fallback::DefaultModel("claude-haiku-4-5".to_owned()))
    );
    assert_eq!(ladder.fall_back("ten_b", "auto:quality", None), None);
    // An empty chain is never a last good list.
    ladder.record(
        "ten_c",
        "auto:quality",
        &with_id("rec_e", false, Vec::new()),
    );
    assert_eq!(ladder.fall_back("ten_c", "auto:quality", None), None);
}

/// D46: a terminal privacy error purges the tenant and marks it zero retention, so only
/// a verified default is allowed afterwards; a zero-retention tenant's last good list
/// keeps only its `zero` entries.
#[tokio::test]
async fn the_proxy_fall_back_follows_the_zero_retention_rule() {
    let chain = vec![
        entry("claude-sonnet-4-6", RetentionMode::Standard),
        entry("claude-opus-4-8", RetentionMode::Zero),
    ];
    let unverified = ladder(None, &[]);
    unverified.record(
        "ten_zr",
        "auto:quality",
        &with_id("rec_z", true, chain.clone()),
    );
    assert_eq!(
        fallback_models(unverified.fall_back("ten_zr", "auto:quality", Some("claude-haiku-4-5"))),
        Some(("rec_z".to_owned(), vec!["claude-opus-4-8".to_owned()]))
    );
    unverified.record(
        "ten_zr",
        "auto:cost",
        &with_id("rec_s", true, chain[..1].to_vec()),
    );
    assert_eq!(
        unverified.fall_back("ten_zr", "auto:cost", Some("claude-haiku-4-5")),
        None
    );
    unverified.refuse("ten_zr");
    assert!(unverified.is_zero_retention("ten_zr"));
    assert_eq!(
        unverified.fall_back("ten_zr", "auto:quality", Some("claude-haiku-4-5")),
        None
    );

    let verified = ladder(None, &["claude-haiku-4-5"]);
    verified.record("ten_ok", "auto:quality", &with_id("rec_o", false, chain));
    verified.refuse("ten_zr");
    assert_eq!(
        verified.fall_back("ten_zr", "auto:quality", Some("claude-haiku-4-5")),
        Some(Fallback::DefaultModel("claude-haiku-4-5".to_owned()))
    );
    assert_eq!(
        verified.fall_back("ten_zr", "auto:quality", Some("gpt-5.5")),
        None
    );
    // Another tenant keeps its list.
    assert!(matches!(
        verified.fall_back("ten_ok", "auto:quality", None),
        Some(Fallback::LastKnownGood(_))
    ));
}

/// D46: the store is bounded; storing past the bound evicts the least recently used list,
/// and a read counts as a use. Zero means unbounded.
#[tokio::test]
async fn the_store_evicts_the_least_recently_used_list() {
    let chain = vec![entry("claude-sonnet-4-6", RetentionMode::Standard)];
    let bounded = ladder(None, &[]).with_capacity(2);
    bounded.record("ten_a", "k1", &with_id("rec_1", false, chain.clone()));
    bounded.record("ten_a", "k2", &with_id("rec_2", false, chain.clone()));
    assert!(bounded.fall_back("ten_a", "k1", None).is_some());
    bounded.record("ten_a", "k3", &with_id("rec_3", false, chain.clone()));
    assert!(bounded.fall_back("ten_a", "k2", None).is_none());
    assert!(bounded.fall_back("ten_a", "k1", None).is_some());
    assert!(bounded.fall_back("ten_a", "k3", None).is_some());
    // Replacing a stored key evicts nothing.
    bounded.record("ten_a", "k3", &with_id("rec_3b", false, chain.clone()));
    assert!(bounded.fall_back("ten_a", "k1", None).is_some());
    assert_eq!(
        fallback_models(bounded.fall_back("ten_a", "k3", None)).map(|(id, _)| id),
        Some("rec_3b".to_owned())
    );

    let unbounded = ladder(None, &[]).with_capacity(0);
    for key in ["k1", "k2", "k3"] {
        unbounded.record("ten_a", key, &with_id(key, false, chain.clone()));
    }
    for key in ["k1", "k2", "k3"] {
        assert!(unbounded.fall_back("ten_a", key, None).is_some(), "{key}");
    }
}

/// D46: the proxy's last good list expires after the ladder's max age.
#[tokio::test]
async fn the_proxy_last_good_list_expires() {
    let ladder = FailOpen::with_ttl(FailOpenConfig::default(), Duration::from_millis(50));
    ladder.record(
        "ten_a",
        "auto:quality",
        &with_id(
            "rec_1",
            false,
            vec![entry("claude-sonnet-4-6", RetentionMode::Standard)],
        ),
    );
    assert!(ladder.fall_back("ten_a", "auto:quality", None).is_some());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(ladder.fall_back("ten_a", "auto:quality", None), None);
}
