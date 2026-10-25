//! Every job kind has a row in the two tables operators and contributors read: "Jobs and
//! schedules" in `docs/operations.md` and "The job kinds" in
//! `docs/architecture/jobs-and-scheduling.md` (bead xq7v.28). A kind added or removed
//! without its row fails here.

use std::collections::BTreeSet;

use maverick_jobs::JobKind;

const OPERATIONS: &str = include_str!("../../../docs/operations.md");
const ARCHITECTURE: &str = include_str!("../../../docs/architecture/jobs-and-scheduling.md");

/// The text under `heading` up to the next heading of the same level.
fn section<'a>(doc: &'a str, heading: &str) -> &'a str {
    let start = doc
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("the doc has no {heading:?} section"));
    let body = &doc[start + heading.len() + 2..];
    body.find("\n## ").map_or(body, |end| &body[..end])
}

/// The first cell of every table row that starts with a code span: the job names.
fn table_names(section: &str) -> BTreeSet<&str> {
    section
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|rest| rest.split_once('`'))
        .map(|(name, _)| name)
        .collect()
}

fn kinds() -> BTreeSet<&'static str> {
    JobKind::ALL.iter().map(|kind| kind.as_str()).collect()
}

#[test]
fn the_operations_jobs_table_has_one_row_per_job_kind() {
    assert_eq!(
        table_names(section(OPERATIONS, "## Jobs and schedules")),
        kinds()
    );
}

#[test]
fn the_architecture_kinds_table_has_one_row_per_job_kind() {
    assert_eq!(
        table_names(section(ARCHITECTURE, "## The job kinds")),
        kinds()
    );
}
