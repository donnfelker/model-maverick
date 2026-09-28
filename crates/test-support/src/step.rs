//! Numbered step logging for multi-step tests (store tests, replay, e2e, acceptance).
//!
//! [`step!`] logs a numbered step with the elapsed time since [`init_test_tracing`] ran, so
//! a failing test shows the last step that completed. The step counter is thread-local and
//! reset by `init_test_tracing`, which keeps numbering correct even when the test harness
//! reuses a thread for later tests.
//!
//! [`init_test_tracing`]: crate::init_test_tracing

use std::cell::RefCell;
use std::time::Instant;

struct StepState {
    start: Instant,
    counter: u32,
}

thread_local! {
    static STEP_STATE: RefCell<Option<StepState>> = const { RefCell::new(None) };
}

/// Reset the step counter and start the elapsed clock for the current thread.
pub(crate) fn reset_steps() {
    STEP_STATE.with(|slot| {
        *slot.borrow_mut() = Some(StepState {
            start: Instant::now(),
            counter: 0,
        });
    });
}

/// Advance the step counter and return `(step_number, elapsed_ms)`. Public only so the
/// [`step!`] macro can call it; not part of the stable surface.
#[doc(hidden)]
#[must_use]
pub fn next_step() -> (u32, f64) {
    STEP_STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow.get_or_insert_with(|| StepState {
            start: Instant::now(),
            counter: 0,
        });
        state.counter += 1;
        let elapsed_ms = state.start.elapsed().as_secs_f64() * 1000.0;
        (state.counter, elapsed_ms)
    })
}

/// Emit one step event. Public only so the [`step!`] macro can call it without requiring
/// the caller to depend on `tracing`.
#[doc(hidden)]
pub fn log_step(step_number: u32, elapsed_ms: f64, message: &str) {
    tracing::info!(step = step_number, elapsed_ms = elapsed_ms, "{message}");
}

/// Log a numbered step with its elapsed time. Use it in multi-step tests so a failure
/// points at the last completed step.
///
/// ```
/// # fn main() {
/// let _capture = test_support::init_test_tracing();
/// test_support::step!("load config");
/// test_support::step!("insert {} rows", 3);
/// # }
/// ```
#[macro_export]
macro_rules! step {
    ($($arg:tt)+) => {{
        let (step_number, elapsed_ms) = $crate::next_step();
        $crate::log_step(step_number, elapsed_ms, &format!($($arg)+));
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_steps_from_one_after_reset() {
        reset_steps();
        let (first, _) = next_step();
        let (second, _) = next_step();
        assert_eq!(first, 1);
        assert_eq!(second, 2);

        reset_steps();
        let (again, _) = next_step();
        assert_eq!(again, 1, "reset restarts numbering");
    }

    #[test]
    fn elapsed_is_non_negative() {
        reset_steps();
        let (_, elapsed_ms) = next_step();
        assert!(elapsed_ms >= 0.0);
    }
}
