//! D49: publish only the explicit public projection after a completed rebuild.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, NaiveDate, Utc};
use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_bundle::{BundleError, FORMAT, IndexEntry, Manifest, SchemaRange, SigningKeys};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MatrixExportArgs {
    pub night: String,
    pub matrix_version: String,
    pub published_at: DateTime<Utc>,
    pub rebuild_id: String,
}

/// Signing file settings never enter diagnostics.
#[derive(Clone)]
pub struct MatrixExportConfig {
    pub directory: PathBuf,
    pub signing_files: String,
}

impl std::fmt::Debug for MatrixExportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixExportConfig")
            .field("directory", &self.directory)
            .field("signing_files", &"<redacted>")
            .finish()
    }
}

impl MatrixExportConfig {
    pub fn from_config(config: &maverick_config::Config) -> Self {
        Self {
            directory: config.matrix_export_dir.clone().into(),
            signing_files: config.matrix_signing_keys.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("matrix export: invalid night")]
    Night,
    #[error("matrix export database: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Bundle(#[from] BundleError),
    #[error("matrix export publisher stopped")]
    Publisher,
}

pub async fn matrix_export(
    pool: &sqlx::PgPool,
    args: &MatrixExportArgs,
    config: &MatrixExportConfig,
) -> Result<IndexEntry, ExportError> {
    let night =
        NaiveDate::parse_from_str(&args.night, "%Y-%m-%d").map_err(|_| ExportError::Night)?;
    let (transaction, matrix) =
        maverick_store::matrix::export_snapshot(pool, &args.matrix_version, night).await?;
    let manifest = Manifest {
        format: FORMAT.to_owned(),
        matrix_version: args.matrix_version.clone(),
        published_at: args.published_at,
        origin_release: env!("CARGO_PKG_VERSION").to_owned(),
        // mmbundle1 uses the additive schema introduced in 0011. Later additive
        // migrations remain compatible; a breaking format needs a new format id.
        schema: SchemaRange {
            min: 11,
            max: i64::MAX,
        },
        files: BTreeMap::new(),
    };
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let result = (|| {
            let keys = SigningKeys::from_files(&config.signing_files)?;
            let bytes = maverick_bundle::build(manifest, &matrix, &keys)?;
            maverick_bundle::publish(&config.directory, &bytes)
        })();
        drop(transaction);
        result
    })
    .await
    .map_err(|_| ExportError::Publisher)?
    .map_err(Into::into)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MatrixExportJob(pub MatrixExportArgs);

impl TaskHandler for MatrixExportJob {
    const IDENTIFIER: &'static str = "matrix_export";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<MatrixExportConfig>() else {
            return Err("jobs: matrix_export: missing publisher configuration".to_owned());
        };
        crate::with_job_timeout(&ctx, crate::JobKind::MatrixExport, async {
            matrix_export(ctx.pg_pool(), &self.0, config)
                .await
                .map(|_| ())
        })
        .await
    }
}
