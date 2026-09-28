//! End-to-end tests for the test-support tools across the crate boundary: the `step!`
//! macro, the tracing capture, and the secret-leak drop guard.

use test_support::{MAVERICK_TEST_CANARY, init_test_tracing, init_test_tracing_with_secrets, step};

#[test]
fn step_macro_logs_numbered_steps_with_timing() {
    let capture = init_test_tracing();
    step!("load config");
    step!("insert {} rows", 3);

    let steps: Vec<_> = capture
        .captured_json()
        .into_iter()
        .filter(|event| event.get("step").is_some())
        .collect();

    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["step"], 1);
    assert_eq!(steps[0]["msg"], "load config");
    assert_eq!(steps[1]["step"], 2);
    assert_eq!(steps[1]["msg"], "insert 3 rows");
    assert!(steps[0].get("elapsed_ms").is_some(), "elapsed_ms present");
}

#[test]
fn clean_test_records_no_violations() {
    let capture = init_test_tracing();
    tracing::info!(status = 200, "request");
    assert!(capture.secret_violations().is_empty());
}

#[test]
fn drop_guard_fails_the_test_when_a_secret_leaks() {
    // The drop guard panics when a registered secret reached the logs and the test did not
    // acknowledge it. Catch that panic here and suppress the default hook so the expected
    // failure output does not clutter a passing run.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| {
        let _capture = init_test_tracing_with_secrets(&["master-key-xyz"]);
        tracing::info!(key = "master-key-xyz", "leaked a master key");
        // No take_violations(): dropping `_capture` must panic and fail the test.
    });
    std::panic::set_hook(previous);

    assert!(
        result.is_err(),
        "leaking a registered secret must panic on drop"
    );
}

#[test]
fn canary_in_a_prompt_echo_is_caught() {
    let capture = init_test_tracing();
    // Simulate an accidental prompt echo carrying the acceptance canary (spec §14.3).
    tracing::info!("classifier prompt: {}", MAVERICK_TEST_CANARY);
    let violations = capture.take_violations();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].secret, MAVERICK_TEST_CANARY);
}
