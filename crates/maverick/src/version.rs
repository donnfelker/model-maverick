//! `maverick --version` (spec §14.5, D50): the release, the build, the schema this binary
//! migrates to, and the matrix bundle formats it imports.

/// The engine release (the workspace version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The release date (`YYYY-MM-DD`), set by the release pipeline at build time through
/// `MAVERICK_RELEASE_DATE`; `dev` for a development build.
pub const RELEASE_DATE: &str = match option_env!("MAVERICK_RELEASE_DATE") {
    Some(date) => date,
    None => "dev",
};

/// The git commit, set by the release pipeline at build time through
/// `MAVERICK_RELEASE_COMMIT`; `unknown` for a development build.
pub const COMMIT: &str = match option_env!("MAVERICK_RELEASE_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

/// The matrix bundle formats this release imports (spec §14.4). Bundle import (bead
/// xq7v.10) adds `mmbundle1`.
pub const BUNDLE_FORMATS: &[&str] = &[];

/// The newest engine migration this binary ships: the schema version a database needs
/// before `serve` runs on it.
pub fn schema_version() -> i64 {
    maverick_store::MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// The `--version` text, one `name: value` line per field after the first.
pub fn text() -> String {
    let formats = if BUNDLE_FORMATS.is_empty() {
        "none".to_owned()
    } else {
        BUNDLE_FORMATS.join(", ")
    };
    format!(
        "maverick {VERSION}\nrelease date: {RELEASE_DATE}\ncommit: {COMMIT}\n\
         minimum schema: {}\nbundle formats: {formats}\n",
        schema_version()
    )
}
