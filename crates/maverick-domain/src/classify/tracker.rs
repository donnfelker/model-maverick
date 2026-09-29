//! The `classifier_degraded` tracker (reference Part 4 §3.6): process-local, one per
//! resolver. Sustained classifier failure raises one critical alert per episode.

use chrono::{DateTime, TimeDelta, Utc};

use crate::gojson::{self, JsonValue};

/// The alert kind.
pub const ALERT_KIND: &str = "classifier_degraded";
/// The alert severity.
pub const ALERT_SEVERITY: &str = "critical";
/// The trailing window, in seconds (15 minutes).
pub const DEGRADED_WINDOW_SECONDS: i64 = 900;
/// Degraded above this failure rate (strict).
pub const DEGRADED_FAILURE_RATE: f64 = 0.05;
/// Degraded only with at least this many outcomes in the window.
pub const DEGRADED_MIN_SAMPLES: usize = 20;
/// The window as Go's `time.Duration` prints it.
const WINDOW_TEXT: &str = "15m0s";

/// The window after one recorded outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub samples: usize,
    pub rate: f64,
    /// True when this outcome raises the alert.
    pub fire: bool,
}

/// The classify outcomes of the last 15 minutes and whether the current episode has
/// already alerted.
#[derive(Debug, Clone, Default)]
pub struct DegradedTracker {
    events: Vec<(DateTime<Utc>, bool)>,
    raised: bool,
}

impl DegradedTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one outcome at `now`. Outcomes strictly before `now - 15 min` leave the
    /// window (one exactly at the cutoff stays). Degraded is `samples >= 20` and a
    /// failure rate above 5%. The alert fires once per degraded episode and re-arms when
    /// the window recovers.
    pub fn record(&mut self, now: DateTime<Utc>, failed: bool) -> Observation {
        self.events.push((now, failed));
        let cutoff = now - TimeDelta::seconds(DEGRADED_WINDOW_SECONDS);
        self.events.retain(|(at, _)| *at >= cutoff);
        let samples = self.events.len();
        let failures = self.events.iter().filter(|(_, failed)| *failed).count();
        let rate = failures as f64 / samples as f64;
        let degraded = samples >= DEGRADED_MIN_SAMPLES && rate > DEGRADED_FAILURE_RATE;
        let fire = degraded && !self.raised;
        self.raised = degraded;
        Observation {
            samples,
            rate,
            fire,
        }
    }
}

/// The alert message (exact Go template).
pub fn degraded_alert_message(classifier_version: &str, rate: f64) -> String {
    format!(
        "classifier {classifier_version} failed {:.1}% of calls over the last {WINDOW_TEXT} (threshold {:.0}%); prompt-path requests are receiving the fallback classification",
        rate * 100.0,
        DEGRADED_FAILURE_RATE * 100.0
    )
}

/// The alert subject as Go's `json.Marshal` of the map writes it (keys sorted). Fails
/// only for a non-finite rate.
pub fn degraded_alert_subject(
    classifier_version: &str,
    rate: f64,
    samples: usize,
) -> Result<String, String> {
    let members = vec![
        (
            "classifier_version".to_owned(),
            JsonValue::String(classifier_version.to_owned()),
        ),
        (
            "failure_rate".to_owned(),
            JsonValue::Number(gojson::encode_f64(rate)?),
        ),
        ("samples".to_owned(), JsonValue::Number(samples.to_string())),
        (
            "window_seconds".to_owned(),
            JsonValue::Number(DEGRADED_WINDOW_SECONDS.to_string()),
        ),
    ];
    Ok(gojson::marshal(&JsonValue::Object(members)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn twenty_samples_are_enough() {
        let now = Utc
            .with_ymd_and_hms(2031, 1, 1, 0, 0, 0)
            .single()
            .expect("time");
        let mut tracker = DegradedTracker::new();
        for _ in 0..18 {
            assert!(!tracker.record(now, false).fire);
        }
        assert!(!tracker.record(now, true).fire);
        let twentieth = tracker.record(now, true);
        assert_eq!(twentieth.samples, 20);
        assert_eq!(twentieth.rate, 0.1);
        assert!(twentieth.fire);
    }
}
