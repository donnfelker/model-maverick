//! `maverick upgrade` (spec §14.5, D50): the operator's one upgrade command. It downloads
//! nothing, so it works the same in a sealed network. It runs five steps and stops at the
//! first failure, naming it:
//!
//! 1. Preflight (read only): the schema state and these checks, in order: the database
//!    is not newer than this release, it is inside the 12-month upgrade window, the
//!    active matrix bundle's format, the upgrade lock (a Postgres advisory lock), and no
//!    live `serve` heartbeat while a `needs-downtime` migration is pending.
//! 2. Plan: every pending engine and job-queue migration with its class.
//! 3. Apply: the same code as `maverick migrate`, while this run holds the lock.
//! 4. Record: an `upgrades` row.
//! 5. Verify: the schema checks again with nothing pending, then one row, every column,
//!    from each table an applied migration created or altered. A failure is recorded in
//!    the row's `verify_failed`.
//!
//! `--check` stops after the plan and changes nothing. Without `--yes` the command asks in
//! a terminal and refuses when there is none. `--json` prints the report as one object.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};

use chrono::Utc;
use maverick_store::schema::{self, Class, SafetyError, Status};
use maverick_store::upgrade as store;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};

use crate::version::{BUNDLE_FORMATS, VERSION};

/// The command's options.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub check: bool,
    pub yes: bool,
    pub json: bool,
    /// Stdin is a terminal, so the command may ask before it applies.
    pub interactive: bool,
}

/// One preflight check.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

/// What a run did, rendered as text or JSON at the end.
#[derive(Debug, Default)]
struct Report {
    database_schema: i64,
    last_upgrade: Option<store::Upgrade>,
    checks: Vec<Check>,
    plan: Vec<(String, Class)>,
    applied: Vec<String>,
    verify: Option<Result<(), String>>,
    result: &'static str,
    error: Option<String>,
}

/// Runs the command and writes its report to `out`. Returns the exit code: 0 when the
/// database is upgraded, already current, or (with `--check`) would upgrade; 1 otherwise.
pub async fn run(
    database_url: &str,
    operator: &str,
    options: Options,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
    out: &mut dyn Write,
) -> i32 {
    let mut report = Report::default();
    let code = upgrade(database_url, operator, options, input, prompt, &mut report).await;
    let _ = if options.json {
        writeln!(out, "{}", report_json(&report))
    } else {
        out.write_all(report_text(&report).as_bytes())
    };
    code
}

async fn upgrade(
    database_url: &str,
    operator: &str,
    options: Options,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
    report: &mut Report,
) -> i32 {
    let mut conn = match PgConnection::connect(database_url).await {
        Ok(conn) => conn,
        Err(error) => return failed(report, format!("connecting to the database: {error}")),
    };
    let code = upgrade_on(
        &mut conn,
        database_url,
        operator,
        options,
        input,
        prompt,
        report,
    )
    .await;
    // The upgrade lock is a session lock; closing the connection releases it too.
    let _ = store::release_upgrade_lock(&mut conn).await;
    let _ = conn.close().await;
    code
}

async fn upgrade_on(
    conn: &mut PgConnection,
    database_url: &str,
    operator: &str,
    options: Options,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
    report: &mut Report,
) -> i32 {
    // 1. Preflight.
    let status = match schema::status(conn).await {
        Ok(status) => status,
        Err(error) => return failed(report, format!("reading the schema status: {error}")),
    };
    report.database_schema = status.schema_version();
    report.last_upgrade = match optional(store::last_upgrade(&mut *conn).await) {
        Ok(last) => last.flatten(),
        Err(error) => return failed(report, format!("reading the upgrade history: {error}")),
    };
    let declared = match schema::declared() {
        Ok(declared) => declared,
        Err(error) => return failed(report, error),
    };
    // 2. Plan (computed first: the serve check needs the classes).
    report.plan = status
        .pending
        .iter()
        .map(|version| {
            let class = declared
                .iter()
                .find(|d| d.version == *version)
                .map_or(Class::NeedsDowntime, |d| d.class);
            (schema::migration_name(*version), class)
        })
        .chain(
            status
                .queue_pending
                .iter()
                .map(|n| (schema::queue_migration_name(*n), schema::queue_class(*n))),
        )
        .collect();
    if !preflight(conn, &status, report).await {
        report.result = "refused";
        return 1;
    }
    if options.check {
        report.result = "checked";
        return 0;
    }
    let release_changed = report
        .last_upgrade
        .as_ref()
        .is_none_or(|last| last.to_release != VERSION);
    if report.plan.is_empty() && !release_changed {
        report.result = "up_to_date";
        return 0;
    }
    if !report.plan.is_empty() && !options.yes {
        if !options.interactive {
            return refused(
                report,
                "not applied: run with --yes to apply without a terminal".to_owned(),
            );
        }
        let _ = write!(
            prompt,
            "apply {} migration(s) to the database? [y/N] ",
            report.plan.len()
        );
        let _ = prompt.flush();
        let mut answer = String::new();
        let _ = input.read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return refused(report, "not applied: the operator declined".to_owned());
        }
    }

    // 3. Apply.
    let started_at = Utc::now();
    let applied = match maverick_store::migrate::migrate(database_url).await {
        Ok(applied) => applied,
        Err(error) => return failed(report, format!("applying migrations: {error}")),
    };
    report.applied = applied
        .schema
        .iter()
        .cloned()
        .chain(
            applied
                .queue
                .iter()
                .map(|n| schema::queue_migration_name(i64::from(*n))),
        )
        .collect();

    // 4. Record.
    let id = store::new_upgrade_id();
    let from = report
        .last_upgrade
        .as_ref()
        .map(|last| last.to_release.clone());
    if let Err(error) = store::start_upgrade(
        &mut *conn,
        &id,
        from.as_deref(),
        VERSION,
        operator,
        started_at,
    )
    .await
    {
        return failed(report, format!("recording the upgrade: {error}"));
    }

    // 5. Verify.
    let applied_versions: Vec<i64> = status.pending.clone();
    let verified = verify(conn, &applied_versions, &declared).await;
    let failure = verified.as_ref().err().cloned();
    if let Err(error) = store::finish_upgrade(
        &mut *conn,
        &id,
        &report.applied,
        Utc::now(),
        failure.as_deref(),
    )
    .await
    {
        return failed(report, format!("recording the upgrade: {error}"));
    }
    report.verify = Some(verified);
    if let Some(failure) = failure {
        report.result = "verify_failed";
        report.error = Some(failure);
        return 1;
    }
    report.result = "upgraded";
    0
}

/// The preflight checks, in order. Stops at the first failure; true when all pass.
async fn preflight(conn: &mut PgConnection, status: &Status, report: &mut Report) -> bool {
    let newer = schema::check_newer(status);
    if !push(report, "newer database", newer.map(|()| String::new())) {
        return false;
    }
    let window = schema::check_window(status, schema::RELEASES, schema::this_release_date());
    if !push(report, "upgrade window", window.map(|()| String::new())) {
        return false;
    }
    let bundle = match active_bundle_format(conn).await {
        Ok(None) => "no active bundle".to_owned(),
        Ok(Some(format)) if BUNDLE_FORMATS.contains(&format.as_str()) => format,
        Ok(Some(format)) => format!(
            "the active bundle is {format}, which this release does not read; import a bundle in a format it reads ({}) after the upgrade",
            formats()
        ),
        Err(error) => {
            push_failure(
                report,
                "bundle format",
                format!("reading the active bundle: {error}"),
            );
            return false;
        }
    };
    push(report, "bundle format", Ok(bundle));
    match store::try_upgrade_lock(conn).await {
        Ok(true) => {
            push(report, "upgrade lock", Ok(String::new()));
        }
        Ok(false) => {
            push_failure(
                report,
                "upgrade lock",
                "another maverick upgrade holds the upgrade lock".to_owned(),
            );
            return false;
        }
        Err(error) => {
            push_failure(report, "upgrade lock", error.to_string());
            return false;
        }
    }
    let live = match optional(store::live_heartbeats(&mut *conn, Utc::now()).await) {
        Ok(live) => live.unwrap_or_default(),
        Err(error) => {
            push_failure(
                report,
                "serve processes",
                format!("reading heartbeats: {error}"),
            );
            return false;
        }
    };
    let downtime: Vec<&str> = report
        .plan
        .iter()
        .filter(|(_, class)| *class == Class::NeedsDowntime)
        .map(|(name, _)| name.as_str())
        .collect();
    let running: Vec<String> = live
        .iter()
        .map(|beat| format!("{} ({})", beat.host_label, beat.release))
        .collect();
    if !downtime.is_empty() && !live.is_empty() {
        push_failure(
            report,
            "serve processes",
            format!(
                "{} needs downtime, and serve still runs on {}; stop every serve process first",
                downtime.join(", "),
                running.join(", ")
            ),
        );
        return false;
    }
    let detail = if running.is_empty() {
        "none running".to_owned()
    } else {
        format!(
            "running: {}; every pending migration is online-safe",
            running.join(", ")
        )
    };
    push(report, "serve processes", Ok(detail));
    true
}

/// The bundle formats this release reads, for messages.
fn formats() -> String {
    if BUNDLE_FORMATS.is_empty() {
        "none yet".to_owned()
    } else {
        BUNDLE_FORMATS.join(", ")
    }
}

/// The active bundle's format, when a bundle is active. Before 0011 there is no table.
async fn active_bundle_format(conn: &mut PgConnection) -> sqlx::Result<Option<String>> {
    let exists: bool = sqlx::query_scalar("select to_regclass('matrix_imports') is not null")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(None);
    }
    sqlx::query_scalar("select format from matrix_imports where active")
        .fetch_optional(conn)
        .await
}

/// Step 5: nothing pending, the schema checks, then one row from each touched table.
async fn verify(
    conn: &mut PgConnection,
    applied: &[i64],
    declared: &[schema::Declared],
) -> Result<(), String> {
    let status = schema::status(conn)
        .await
        .map_err(|error| format!("check schema status: {error}"))?;
    if !status.pending.is_empty() || !status.queue_pending.is_empty() {
        return Err(format!(
            "check pending: {} engine and {} job-queue migrations are still pending",
            status.pending.len(),
            status.queue_pending.len()
        ));
    }
    schema::check(&status).map_err(|error: SafetyError| format!("check schema: {error}"))?;
    let mut tables = BTreeSet::new();
    for version in applied {
        if let Some(declared) = declared.iter().find(|d| d.version == *version) {
            tables.extend(declared.tables.iter().cloned());
        }
    }
    for table in &tables {
        store::read_one_row(conn, table)
            .await
            .map_err(|error| format!("table {table}: {error}"))?;
    }
    Ok(())
}

/// A query result where a missing table (before migration 0012) reads as `None`.
fn optional<T>(result: sqlx::Result<T>) -> sqlx::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if maverick_store::sqlstate(&error).as_deref() == Some("42P01") => Ok(None),
        Err(error) => Err(error),
    }
}

fn push(report: &mut Report, name: &'static str, outcome: Result<String, SafetyError>) -> bool {
    match outcome {
        Ok(detail) => {
            report.checks.push(Check {
                name,
                ok: true,
                detail,
            });
            true
        }
        Err(error) => {
            push_failure(report, name, error.to_string());
            false
        }
    }
}

fn push_failure(report: &mut Report, name: &'static str, detail: String) {
    report.error = Some(format!("{name}: {detail}"));
    report.checks.push(Check {
        name,
        ok: false,
        detail,
    });
}

fn failed(report: &mut Report, error: String) -> i32 {
    report.result = "failed";
    report.error = Some(error);
    1
}

fn refused(report: &mut Report, error: String) -> i32 {
    report.result = "refused";
    report.error = Some(error);
    1
}

fn report_text(report: &Report) -> String {
    let mut out = format!(
        "maverick upgrade: release {VERSION}; database schema {}, this release's schema {}\n",
        report.database_schema,
        schema::binary_schema_version()
    );
    match &report.last_upgrade {
        Some(last) => out.push_str(&format!(
            "last upgrade: to {} at {}\n",
            last.to_release,
            last.started_at.to_rfc3339()
        )),
        None => out.push_str("last upgrade: none recorded\n"),
    }
    if !report.checks.is_empty() {
        out.push_str("preflight:\n");
        for check in &report.checks {
            let mark = if check.ok { "ok" } else { "FAILED" };
            if check.detail.is_empty() {
                out.push_str(&format!("  {mark} {}\n", check.name));
            } else {
                out.push_str(&format!("  {mark} {}: {}\n", check.name, check.detail));
            }
        }
    }
    out.push_str("plan:\n");
    if report.plan.is_empty() {
        out.push_str("  nothing to apply\n");
    }
    for (name, class) in &report.plan {
        out.push_str(&format!("  {name} ({})\n", class.as_str()));
    }
    if !report.applied.is_empty() {
        out.push_str(&format!("applied: {}\n", report.applied.join(", ")));
    }
    match &report.verify {
        Some(Ok(())) => out.push_str("verify: ok\n"),
        Some(Err(failure)) => out.push_str(&format!("verify: FAILED {failure}\n")),
        None => {}
    }
    out.push_str(&format!("result: {}\n", report.result));
    if let Some(error) = &report.error {
        out.push_str(&format!("error: {error}\n"));
    }
    out
}

fn report_json(report: &Report) -> Value {
    json!({
        "release": VERSION,
        "database_schema": report.database_schema,
        "release_schema": schema::binary_schema_version(),
        "last_upgrade": report.last_upgrade.as_ref().map(|last| json!({
            "id": last.id,
            "from_release": last.from_release,
            "to_release": last.to_release,
            "started_at": last.started_at.to_rfc3339(),
            "finished_at": last.finished_at.map(|at| at.to_rfc3339()),
            "verify_failed": last.verify_failed,
        })),
        "checks": report.checks.iter().map(|check| json!({
            "name": check.name,
            "ok": check.ok,
            "detail": check.detail,
        })).collect::<Vec<_>>(),
        "plan": report.plan.iter().map(|(name, class)| json!({
            "migration": name,
            "class": class.as_str(),
        })).collect::<Vec<_>>(),
        "applied": report.applied,
        "verify": match &report.verify {
            None => Value::Null,
            Some(Ok(())) => json!({"ok": true}),
            Some(Err(failure)) => json!({"ok": false, "failed": failure}),
        },
        "result": report.result,
        "error": report.error,
    })
}
