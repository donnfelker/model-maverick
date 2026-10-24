//! Schema safety (spec §14.5, D50): what a database has applied against what this binary
//! ships, the checks every command runs before it touches a schema, and each migration's
//! class.
//!
//! - [`status`] reads the engine migrations and graphile_worker's migrations: applied,
//!   pending, and unknown (applied, but not in this binary).
//! - [`check_newer`] refuses a database with migrations this binary does not know. Rollback
//!   is a restore from backup (D37), so an older binary must never run on it.
//! - [`check_window`] refuses a database older than the 12-month upgrade window. The window
//!   is data: [`RELEASES`] records each release's newest migration.
//! - [`declared`] gives each migration's class (online-safe or needs-downtime) and the
//!   tables it creates or alters. From 0012 on the file declares them in its header.
//!   0001 to 0011 predate that rule and are never edited, because a changed file changes
//!   its sqlx checksum, so they are declared in [`LEGACY`].

use std::collections::BTreeSet;

use chrono::{Months, NaiveDate, Utc};
use sqlx::{AssertSqlSafe, PgConnection};

use crate::MIGRATOR;
use crate::health::GRAPHILE_WORKER_SCHEMA;

/// How a migration affects the previous release's running `serve` processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The previous release keeps working against the new schema: new tables, new
    /// columns with defaults or nullable, new indexes, widened checks.
    OnlineSafe,
    /// The previous release can fail against the new schema, so every `serve` stops first.
    NeedsDowntime,
}

impl Class {
    /// The header value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnlineSafe => "online-safe",
            Self::NeedsDowntime => "needs-downtime",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "online-safe" => Some(Self::OnlineSafe),
            "needs-downtime" => Some(Self::NeedsDowntime),
            _ => None,
        }
    }
}

/// One engine migration's class and the tables it creates or alters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub version: i64,
    pub class: Class,
    pub tables: Vec<String>,
}

/// The newest migration declared in [`LEGACY`]. Every later one declares its header.
pub const LEGACY_LAST: i64 = 11;

/// Migrations 0001 to 0011: class and touched tables. They were applied before the header
/// rule and are never edited (D37).
///
/// 0005 is `needs-downtime`: it adds `debug_captures.capture_key_version int not null` with
/// no default, so a release before it cannot insert a capture, and the migration itself
/// needs an empty table. Every other one only adds.
pub const LEGACY: &[(i64, Class, &[&str])] = &[
    (
        1,
        Class::OnlineSafe,
        &[
            "tenants",
            "api_keys",
            "model_columns",
            "corpus_versions",
            "tasks",
            "runs",
            "scores",
            "aggregates",
            "pools",
            "tenant_weights",
            "tenant_constraints",
            "recommendations",
            "outcomes",
            "predictions",
            "calibration",
            "capture_windows",
            "debug_captures",
            "capture_access_log",
            "alerts",
            "classification_cache",
        ],
    ),
    (2, Class::OnlineSafe, &["batches"]),
    (3, Class::OnlineSafe, &["model_columns"]),
    (4, Class::OnlineSafe, &["model_columns"]),
    (5, Class::NeedsDowntime, &["debug_captures"]),
    (6, Class::OnlineSafe, &["outcomes"]),
    (7, Class::OnlineSafe, &["tenants"]),
    (8, Class::OnlineSafe, &["classifier_usage"]),
    (9, Class::OnlineSafe, &["tenant_constraints"]),
    (
        10,
        Class::OnlineSafe,
        &[
            "conversation_affinity",
            "tenant_constraints",
            "recommendations",
        ],
    ),
    (11, Class::OnlineSafe, &["matrix_imports"]),
];

/// Reads a migration file's header: the leading comment lines must include
/// `-- class: <class>` and `-- tables: <name>, <name>`.
pub fn parse_header(version: i64, sql: &str) -> Result<Declared, String> {
    let mut class = None;
    let mut tables = None;
    for line in sql.lines() {
        let Some(comment) = line.trim().strip_prefix("--") else {
            break;
        };
        let comment = comment.trim();
        if let Some(value) = comment.strip_prefix("class:") {
            let value = value.trim();
            class = Some(Class::parse(value).ok_or_else(|| {
                format!("migration {version:04}: unknown class {value:?} (online-safe or needs-downtime)")
            })?);
        } else if let Some(value) = comment.strip_prefix("tables:") {
            let names: Vec<String> = value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            if names.is_empty() {
                return Err(format!(
                    "migration {version:04}: `-- tables:` names no table"
                ));
            }
            tables = Some(names);
        }
    }
    Ok(Declared {
        version,
        class: class
            .ok_or_else(|| format!("migration {version:04}: no `-- class:` header line"))?,
        tables: tables
            .ok_or_else(|| format!("migration {version:04}: no `-- tables:` header line"))?,
    })
}

/// Every engine migration's class and tables, in version order. An error names the first
/// migration that declares nothing, declares badly, or is declared twice.
pub fn declared() -> Result<Vec<Declared>, String> {
    let versions: BTreeSet<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    if let Some((version, _, _)) = LEGACY.iter().find(|(v, _, _)| !versions.contains(v)) {
        return Err(format!(
            "LEGACY lists migration {version:04}, which does not exist"
        ));
    }
    MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|migration| {
            if migration.version <= LEGACY_LAST {
                let (_, class, tables) = LEGACY
                    .iter()
                    .find(|(v, _, _)| *v == migration.version)
                    .ok_or_else(|| {
                        format!("migration {:04} is missing from LEGACY", migration.version)
                    })?;
                Ok(Declared {
                    version: migration.version,
                    class: *class,
                    tables: tables.iter().map(|t| (*t).to_owned()).collect(),
                })
            } else {
                parse_header(migration.version, migration.sql.as_str())
            }
        })
        .collect()
}

/// What a database has applied, against what this binary ships.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Engine migration versions applied, in order.
    pub applied: Vec<i64>,
    /// Engine migrations this binary ships that are not applied, in order.
    pub pending: Vec<i64>,
    /// Applied engine versions this binary does not know: the database is newer.
    pub unknown: Vec<i64>,
    /// Engine versions whose last run failed part way (sqlx's dirty flag).
    pub dirty: Vec<i64>,
    /// graphile_worker migrations applied, pending, and unknown.
    pub queue_applied: Vec<i64>,
    pub queue_pending: Vec<i64>,
    pub queue_unknown: Vec<i64>,
}

impl Status {
    /// The database's engine schema version: its newest applied migration, 0 when none.
    #[must_use]
    pub fn schema_version(&self) -> i64 {
        self.applied.last().copied().unwrap_or(0)
    }
}

/// The newest engine migration this binary ships.
#[must_use]
pub fn binary_schema_version() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

/// The graphile_worker migrations this binary's queue library ships.
fn queue_known() -> Vec<i64> {
    graphile_worker_migrations::sql::GRAPHILE_WORKER_MIGRATIONS
        .iter()
        .map(|migration| i64::from(migration.migration_number()))
        .collect()
}

/// Reads the schema status. A fresh database has applied nothing.
pub async fn status(conn: &mut PgConnection) -> sqlx::Result<Status> {
    let table = MIGRATOR.table_name.as_ref();
    let mut applied = Vec::new();
    let mut dirty = Vec::new();
    if exists(conn, table).await? {
        let rows: Vec<(i64, bool)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT version, success FROM {table} ORDER BY version"
        )))
        .fetch_all(&mut *conn)
        .await?;
        for (version, success) in rows {
            if success {
                applied.push(version);
            } else {
                dirty.push(version);
            }
        }
    }
    let queue_table = format!("{GRAPHILE_WORKER_SCHEMA}.migrations");
    let queue_applied: Vec<i64> = if exists(conn, &queue_table).await? {
        sqlx::query_scalar::<_, i32>(AssertSqlSafe(format!(
            "SELECT id FROM {queue_table} ORDER BY id"
        )))
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(i64::from)
        .collect()
    } else {
        Vec::new()
    };
    let known: Vec<i64> = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| m.version)
        .collect();
    let queue_known = queue_known();
    Ok(Status {
        pending: missing(&known, &applied),
        unknown: missing(&applied, &known),
        queue_pending: missing(&queue_known, &queue_applied),
        queue_unknown: missing(&queue_applied, &queue_known),
        applied,
        dirty,
        queue_applied,
    })
}

/// The versions in `from` that are not in `other`, in order.
fn missing(from: &[i64], other: &[i64]) -> Vec<i64> {
    from.iter()
        .filter(|v| !other.contains(v))
        .copied()
        .collect()
}

async fn exists(conn: &mut PgConnection, table: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(conn)
        .await
}

/// Why a command refuses to touch a schema. The message says what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SafetyError {
    /// The database has engine migrations this binary does not know.
    #[error(
        "the database schema is {database}, newer than this release's {binary} (unknown migrations: {unknown}); run a release that knows them, or restore a backup"
    )]
    Newer {
        database: i64,
        binary: i64,
        unknown: String,
    },
    /// The job queue has migrations this binary's queue library does not know.
    #[error(
        "the job queue schema has migrations this release does not know ({unknown}); run a release that knows them, or restore a backup"
    )]
    QueueNewer { unknown: String },
    /// The database is older than the 12-month upgrade window.
    #[error(
        "the database schema is {database}, from release {release} ({date}), older than this release's 12-month upgrade window; upgrade to {intermediate} first"
    )]
    OutsideWindow {
        database: i64,
        release: String,
        date: NaiveDate,
        intermediate: String,
    },
}

fn list(versions: &[i64]) -> String {
    versions
        .iter()
        .map(|v| format!("{v:04}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuses a database newer than this binary, engine or job queue.
pub fn check_newer(status: &Status) -> Result<(), SafetyError> {
    if !status.unknown.is_empty() {
        return Err(SafetyError::Newer {
            database: status.schema_version(),
            binary: binary_schema_version(),
            unknown: list(&status.unknown),
        });
    }
    if !status.queue_unknown.is_empty() {
        return Err(SafetyError::QueueNewer {
            unknown: list(&status.queue_unknown),
        });
    }
    Ok(())
}

/// One published release: its name, its release date, and its newest engine migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub name: &'static str,
    /// `YYYY-MM-DD`.
    pub date: &'static str,
    pub newest_migration: i64,
}

impl Release {
    /// The release date. The table is checked by a test, so a bad date is a bug here.
    #[must_use]
    pub fn day(&self) -> NaiveDate {
        NaiveDate::parse_from_str(self.date, "%Y-%m-%d").expect("RELEASES dates are YYYY-MM-DD")
    }
}

/// Every published release, oldest first. The release pipeline appends one row per
/// release; no release exists yet. Rows are never removed while a release can still be
/// upgraded from.
pub const RELEASES: &[Release] = &[];

/// This build's release date, set at build time through `MAVERICK_RELEASE_DATE` (the same
/// variable `maverick --version` prints). A development build has none and uses today.
#[must_use]
pub fn this_release_date() -> NaiveDate {
    option_env!("MAVERICK_RELEASE_DATE")
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
        .unwrap_or_else(|| Utc::now().date_naive())
}

/// Refuses a database older than the 12-month upgrade window of the release dated
/// `today`. The database's release is the newest release whose newest migration it has
/// applied; a schema older than every release counts as the first release's. A fresh
/// database, or an empty release table, is always inside.
pub fn check_window(
    status: &Status,
    releases: &[Release],
    today: NaiveDate,
) -> Result<(), SafetyError> {
    let schema = status.schema_version();
    let Some(first) = releases.first() else {
        return Ok(());
    };
    if schema == 0 {
        return Ok(());
    }
    let start = today
        .checked_sub_months(Months::new(12))
        .unwrap_or(NaiveDate::MIN);
    let from = releases
        .iter()
        .rev()
        .find(|release| release.newest_migration <= schema);
    let (name, date) = match from {
        Some(release) => (release.name.to_owned(), release.day()),
        None => ("before the first release".to_owned(), first.day()),
    };
    if date >= start {
        return Ok(());
    }
    // The newest later release still inside the database's own window upgrades it
    // directly. The first release also upgrades a schema that predates it.
    let reach = date
        .checked_add_months(Months::new(12))
        .unwrap_or(NaiveDate::MAX);
    let later = |release: &&Release| match from {
        Some(from) => release.day() > from.day(),
        None => true,
    };
    let intermediate = releases
        .iter()
        .rev()
        .filter(later)
        .find(|release| release.day() <= reach)
        .or_else(|| releases.iter().find(later))
        .map_or_else(|| name.clone(), |release| release.name.to_owned());
    Err(SafetyError::OutsideWindow {
        database: schema,
        release: name,
        date,
        intermediate,
    })
}

/// The checks every schema command runs first: not newer, and inside the window.
pub fn check(status: &Status) -> Result<(), SafetyError> {
    check_newer(status)?;
    check_window(status, RELEASES, this_release_date())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn at(schema: i64) -> Status {
        Status {
            applied: (1..=schema).collect(),
            ..Status::default()
        }
    }

    const TABLE: &[Release] = &[
        Release {
            name: "1.0.0",
            date: "2027-01-10",
            newest_migration: 12,
        },
        Release {
            name: "1.4.0",
            date: "2027-09-01",
            newest_migration: 15,
        },
        Release {
            name: "1.8.0",
            date: "2028-05-01",
            newest_migration: 19,
        },
        Release {
            name: "2.0.0",
            date: "2029-03-01",
            newest_migration: 22,
        },
    ];

    #[test]
    fn every_migration_declares_its_class_and_tables() {
        let declared = declared().expect("every migration is declared");
        let versions: Vec<i64> = declared.iter().map(|d| d.version).collect();
        let expected: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(versions, expected);
        for migration in &declared {
            assert!(!migration.tables.is_empty(), "{:04}", migration.version);
        }
        // Only 0005 needs downtime among the legacy migrations.
        let downtime: Vec<i64> = declared
            .iter()
            .filter(|d| d.version <= LEGACY_LAST && d.class == Class::NeedsDowntime)
            .map(|d| d.version)
            .collect();
        assert_eq!(downtime, [5]);
    }

    #[test]
    fn migrations_after_the_legacy_table_carry_a_header() {
        for migration in MIGRATOR.iter().filter(|m| m.version > LEGACY_LAST) {
            parse_header(migration.version, migration.sql.as_str())
                .unwrap_or_else(|e| panic!("{e}"));
        }
        assert!(LEGACY.iter().all(|(v, _, _)| *v <= LEGACY_LAST));
    }

    #[test]
    fn headers_parse_and_refuse_what_they_must() {
        let sql =
            "-- What this does.\n-- class: needs-downtime\n-- tables: a, b\ncreate table a ();\n";
        assert_eq!(
            parse_header(12, sql),
            Ok(Declared {
                version: 12,
                class: Class::NeedsDowntime,
                tables: vec!["a".to_owned(), "b".to_owned()],
            })
        );
        assert_eq!(
            parse_header(12, "-- tables: a\ncreate table a ();"),
            Err("migration 0012: no `-- class:` header line".to_owned())
        );
        assert_eq!(
            parse_header(12, "-- class: online-safe\ncreate table a ();"),
            Err("migration 0012: no `-- tables:` header line".to_owned())
        );
        assert_eq!(
            parse_header(12, "-- class: maybe\n-- tables: a\n"),
            Err(
                "migration 0012: unknown class \"maybe\" (online-safe or needs-downtime)"
                    .to_owned()
            )
        );
        assert_eq!(
            parse_header(12, "-- class: online-safe\n-- tables: , \n"),
            Err("migration 0012: `-- tables:` names no table".to_owned())
        );
        // The header is the leading comment block only.
        assert!(
            parse_header(
                12,
                "create table a ();\n-- class: online-safe\n-- tables: a\n"
            )
            .is_err()
        );
        assert_eq!(Class::OnlineSafe.as_str(), "online-safe");
        assert_eq!(Class::NeedsDowntime.as_str(), "needs-downtime");
    }

    #[test]
    fn the_release_table_is_ordered_and_names_real_migrations() {
        let known: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        for pair in RELEASES.windows(2) {
            assert!(pair[0].day() < pair[1].day(), "{pair:?}");
            assert!(
                pair[0].newest_migration <= pair[1].newest_migration,
                "{pair:?}"
            );
        }
        for release in RELEASES {
            let _ = release.day();
            assert!(known.contains(&release.newest_migration), "{release:?}");
        }
    }

    #[test]
    fn newer_databases_are_refused_with_both_versions() {
        let mut status = at(3);
        status.applied.push(9001);
        status.unknown = vec![9001];
        let error = check_newer(&status).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "the database schema is 9001, newer than this release's {} (unknown migrations: 9001); run a release that knows them, or restore a backup",
                binary_schema_version()
            )
        );
        let queue = Status {
            queue_unknown: vec![30, 31],
            ..at(3)
        };
        assert_eq!(
            check_newer(&queue).unwrap_err().to_string(),
            "the job queue schema has migrations this release does not know (0030, 0031); run a release that knows them, or restore a backup"
        );
        assert_eq!(check_newer(&at(3)), Ok(()));
    }

    #[test]
    fn the_window_is_twelve_months_of_release_data() {
        // Empty table, or a fresh database: always inside.
        assert_eq!(check_window(&at(5), &[], day("2040-01-01")), Ok(()));
        assert_eq!(
            check_window(&Status::default(), TABLE, day("2040-01-01")),
            Ok(())
        );
        // 1.8.0 (2028-05-01) runs on a 1.4.0 database (2027-09-01): inside.
        assert_eq!(check_window(&at(15), TABLE, day("2028-05-01")), Ok(()));
        // Exactly 12 months is inside; one day more is not.
        assert_eq!(check_window(&at(15), TABLE, day("2028-09-01")), Ok(()));
        assert_eq!(
            check_window(&at(15), TABLE, day("2028-09-02")),
            Err(SafetyError::OutsideWindow {
                database: 15,
                release: "1.4.0".to_owned(),
                date: day("2027-09-01"),
                intermediate: "1.8.0".to_owned(),
            })
        );
        // 2.0.0 (2029-03-01) on a 1.0.0 database (2027-01-10): 1.4.0 and 1.8.0 are later;
        // 1.8.0 is past 1.0.0's own window (2028-01-10), so 1.4.0 is the step.
        assert_eq!(
            check_window(&at(12), TABLE, day("2029-03-01")),
            Err(SafetyError::OutsideWindow {
                database: 12,
                release: "1.0.0".to_owned(),
                date: day("2027-01-10"),
                intermediate: "1.4.0".to_owned(),
            })
        );
        // A schema between releases belongs to the older one.
        assert_eq!(
            check_window(&at(17), TABLE, day("2029-03-01")),
            Err(SafetyError::OutsideWindow {
                database: 17,
                release: "1.4.0".to_owned(),
                date: day("2027-09-01"),
                intermediate: "1.8.0".to_owned(),
            })
        );
        // A schema older than every release counts as the first release's: 1.0.0 and
        // anything inside its window upgrade it, and later releases name the step.
        assert_eq!(check_window(&at(5), TABLE, day("2027-02-01")), Ok(()));
        assert_eq!(check_window(&at(5), TABLE, day("2028-01-10")), Ok(()));
        assert_eq!(
            check_window(&at(5), TABLE, day("2029-03-01")),
            Err(SafetyError::OutsideWindow {
                database: 5,
                release: "before the first release".to_owned(),
                date: day("2027-01-10"),
                intermediate: "1.4.0".to_owned(),
            })
        );
        let message = check_window(&at(12), TABLE, day("2029-03-01"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            message,
            "the database schema is 12, from release 1.0.0 (2027-01-10), older than this release's 12-month upgrade window; upgrade to 1.4.0 first"
        );
    }
}
