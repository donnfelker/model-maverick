//! A reporter for replay-style parity runs (API replay, proxy replay, conformance).
//!
//! Record one [`ReplayOutcome`] per replayed exchange, then print [`ReplayReport::summary`]
//! for a one-line count and call [`ReplayReport::assert_clean`] to fail the run if any
//! exchange diverged without an approved-divergence tag. Each unexplained diff prints a
//! detailed block so the failure is diagnosable without re-running.

/// The outcome of one replayed exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// The actual response matched the expected response.
    Passed,
    /// The response differed, but an approved divergence row explains it.
    Allowlisted {
        /// The divergence id, e.g. `D31`.
        divergence: String,
    },
    /// The response differed with no explanation. `detail` should carry the request and the
    /// expected and actual responses.
    Unexplained {
        /// A readable block describing the request, expected response, and actual response.
        detail: String,
    },
}

/// Accumulates replay outcomes and turns them into a summary table and pass/fail assertion.
#[derive(Debug, Default)]
pub struct ReplayReport {
    entries: Vec<(String, ReplayOutcome)>,
}

impl ReplayReport {
    /// Create an empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one outcome under the exchange `name` (for example the fixture case id).
    pub fn record(&mut self, name: impl Into<String>, outcome: ReplayOutcome) {
        self.entries.push((name.into(), outcome));
    }

    /// Record a passing exchange.
    pub fn passed(&mut self, name: impl Into<String>) {
        self.record(name, ReplayOutcome::Passed);
    }

    /// Record an exchange whose difference an approved divergence explains.
    pub fn allowlisted(&mut self, name: impl Into<String>, divergence: impl Into<String>) {
        self.record(
            name,
            ReplayOutcome::Allowlisted {
                divergence: divergence.into(),
            },
        );
    }

    /// Record an unexplained difference, with a detail block for the failure output.
    pub fn unexplained(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.record(
            name,
            ReplayOutcome::Unexplained {
                detail: detail.into(),
            },
        );
    }

    /// Total exchanges recorded.
    #[must_use]
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// Count of outcomes matching a predicate.
    fn count(&self, matches: impl Fn(&ReplayOutcome) -> bool) -> usize {
        self.entries
            .iter()
            .filter(|(_, outcome)| matches(outcome))
            .count()
    }

    /// Number of passing exchanges.
    #[must_use]
    pub fn passed_count(&self) -> usize {
        self.count(|outcome| matches!(outcome, ReplayOutcome::Passed))
    }

    /// Number of exchanges explained by an approved divergence.
    #[must_use]
    pub fn allowlisted_count(&self) -> usize {
        self.count(|outcome| matches!(outcome, ReplayOutcome::Allowlisted { .. }))
    }

    /// Number of unexplained differences.
    #[must_use]
    pub fn unexplained_count(&self) -> usize {
        self.count(|outcome| matches!(outcome, ReplayOutcome::Unexplained { .. }))
    }

    /// A one-line summary table.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "replay summary: {} total | {} passed | {} allowlisted (D-row) | {} unexplained",
            self.total(),
            self.passed_count(),
            self.allowlisted_count(),
            self.unexplained_count()
        )
    }

    /// The detail blocks for every unexplained difference.
    #[must_use]
    pub fn unexplained_details(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|(name, outcome)| match outcome {
                ReplayOutcome::Unexplained { detail } => {
                    Some(format!("--- unexplained: {name} ---\n{detail}"))
                }
                _ => None,
            })
            .collect()
    }

    /// Fail the run if any difference was unexplained, printing the summary and every
    /// unexplained detail block.
    pub fn assert_clean(&self) {
        if self.unexplained_count() == 0 {
            return;
        }
        let mut report = vec![self.summary()];
        report.extend(self.unexplained_details());
        panic!("{}", report.join("\n\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ReplayReport {
        let mut report = ReplayReport::new();
        report.passed("case-1");
        report.passed("case-2");
        report.allowlisted("case-3", "D31");
        report.unexplained("case-4", "request: GET /v1/x\nexpected: 200\nactual: 500");
        report
    }

    #[test]
    fn summary_counts_each_bucket() {
        let report = sample();
        let summary = report.summary();
        assert!(summary.contains("4 total"), "{summary}");
        assert!(summary.contains("2 passed"), "{summary}");
        assert!(summary.contains("1 allowlisted"), "{summary}");
        assert!(summary.contains("1 unexplained"), "{summary}");
        assert_eq!(report.unexplained_count(), 1);
    }

    #[test]
    fn assert_clean_passes_without_unexplained() {
        let mut report = ReplayReport::new();
        report.passed("case-1");
        report.allowlisted("case-2", "D12");
        report.assert_clean();
    }

    #[test]
    #[should_panic(expected = "unexplained: case-4")]
    fn assert_clean_panics_with_detail_blocks() {
        sample().assert_clean();
    }
}
