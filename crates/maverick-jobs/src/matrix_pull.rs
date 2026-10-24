//! D49: opt-in hourly replica pull. An unsigned index never authorizes an import.
use std::time::Duration;

use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_bundle::{BundleError, FORMAT, IndexEntry, VerifiedBundle};
use maverick_store::matrix_import::{ImportError, ImportOutcome};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

pub const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BUNDLE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone)]
pub struct MatrixPullConfig {
    location: String,
}
impl std::fmt::Debug for MatrixPullConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixPullConfig")
            .field("enabled", &!self.location.is_empty())
            .finish()
    }
}
impl MatrixPullConfig {
    pub fn new(location: &str) -> Self {
        Self {
            location: location.to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("matrix pull: location must be an HTTP(S) directory without query or fragment")]
    Location,
    #[error("matrix pull: HTTP request failed")]
    Http,
    #[error("matrix pull: response exceeds the size limit")]
    Size,
    #[error("matrix pull: invalid index")]
    Index,
    #[error("matrix pull: no bundle in a supported format")]
    Format,
    #[error("matrix pull: index and bundle identity, size, or SHA-256 differ")]
    Mismatch,
    #[error("matrix pull: unsupported database schema")]
    Schema,
    #[error("matrix pull: verification task stopped")]
    Verifier,
    #[error(transparent)]
    Bundle(#[from] BundleError),
    #[error(transparent)]
    Import(#[from] ImportError),
    #[error("matrix pull database: {0}")]
    Database(#[from] sqlx::Error),
}

impl PullError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Import(ImportError::Older) => "older_than_active",
            Self::Import(ImportError::Database(_)) | Self::Database(_) => "database_failure",
            _ => self.kind(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Location => "location",
            Self::Http => "http",
            Self::Size => "size",
            Self::Index => "index",
            Self::Format => "format",
            Self::Mismatch => "mismatch",
            Self::Schema => "schema",
            Self::Verifier => "verifier",
            Self::Bundle(_) => "bundle",
            Self::Import(_) => "import",
            Self::Database(_) => "database",
        }
    }
}

fn location(text: &str) -> Result<reqwest::Url, PullError> {
    let mut url = reqwest::Url::parse(text).map_err(|_| PullError::Location)?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(PullError::Location);
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

async fn download(
    client: &reqwest::Client,
    url: reqwest::Url,
    limit: usize,
) -> Result<Vec<u8>, PullError> {
    let mut response = client.get(url).send().await.map_err(|_| PullError::Http)?;
    if !response.status().is_success() {
        return Err(PullError::Http);
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(PullError::Size);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| PullError::Http)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(PullError::Size);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn database_schema(pool: &PgPool) -> Result<i64, PullError> {
    let mut conn = pool.acquire().await?;
    let status = maverick_store::schema::status(&mut conn).await?;
    maverick_store::schema::check(&status).map_err(|_| PullError::Schema)?;
    Ok(status.schema_version())
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if safe_redirect(attempt.url(), attempt.previous().len()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

fn safe_redirect(target: &reqwest::Url, hops: usize) -> bool {
    hops <= 3
        && target.scheme() == "https"
        && target.username().is_empty()
        && target.password().is_none()
}

pub async fn matrix_pull(
    pool: &PgPool,
    config: &MatrixPullConfig,
) -> Result<ImportOutcome, PullError> {
    pull_verified(pool, config, |bytes, schema| {
        maverick_bundle::verify(bytes, &maverick_bundle::trust::release_keys(), schema)
    })
    .await
}

async fn pull_verified(
    pool: &PgPool,
    config: &MatrixPullConfig,
    authenticate: impl FnOnce(&[u8], i64) -> Result<VerifiedBundle, BundleError> + Send + 'static,
) -> Result<ImportOutcome, PullError> {
    let base = location(&config.location)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(300))
        .redirect(redirect_policy())
        .build()
        .map_err(|_| PullError::Http)?;
    let index = download(
        &client,
        base.join("index.json").map_err(|_| PullError::Location)?,
        MAX_INDEX_BYTES,
    )
    .await?;
    let entries: Vec<IndexEntry> = serde_json::from_slice(&index).map_err(|_| PullError::Index)?;
    let entry = entries
        .into_iter()
        .filter(|entry| entry.format == FORMAT)
        .max_by(|a, b| {
            a.published_at
                .cmp(&b.published_at)
                .then(a.file.cmp(&b.file))
        })
        .ok_or(PullError::Format)?;
    // A file name from an unsigned index must not escape the configured directory.
    if entry.file != maverick_bundle::filename(entry.published_at) {
        return Err(PullError::Mismatch);
    }
    if let Some(active) = maverick_store::matrix::active_bundle(pool).await? {
        if entry.published_at < active.published_at {
            return Err(PullError::Import(ImportError::Older));
        }
        if entry.published_at == active.published_at
            && entry.matrix_version == active.matrix_version
        {
            return Ok(ImportOutcome::AlreadyActive);
        }
    }
    let size = usize::try_from(entry.size).map_err(|_| PullError::Size)?;
    if size > MAX_BUNDLE_BYTES {
        return Err(PullError::Size);
    }
    let bytes = download(
        &client,
        base.join(&entry.file).map_err(|_| PullError::Location)?,
        size,
    )
    .await?;
    if bytes.len() != size || maverick_bundle::sha256(&bytes) != entry.sha256 {
        return Err(PullError::Mismatch);
    }
    let schema = database_schema(pool).await?;
    let bundle = tokio::task::spawn_blocking(move || authenticate(&bytes, schema))
        .await
        .map_err(|_| PullError::Verifier)??;
    if bundle.manifest.published_at != entry.published_at
        || bundle.manifest.matrix_version != entry.matrix_version
        || bundle.manifest.format != entry.format
    {
        return Err(PullError::Mismatch);
    }
    // Never allow rollback from a remote or edited index. The store rechecks while
    // holding its transaction lock, including against a simultaneous manual import.
    Ok(maverick_store::matrix_import::import(pool, &bundle, false).await?)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MatrixPullJob(pub crate::HourArgs);
impl TaskHandler for MatrixPullJob {
    const IDENTIFIER: &'static str = "matrix_pull";
    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<MatrixPullConfig>() else {
            return Err("jobs: matrix_pull: missing configuration".to_owned());
        };
        crate::with_job_timeout(&ctx, crate::JobKind::MatrixPull, async {
            match matrix_pull(ctx.pg_pool(), config).await {
                Ok(outcome) => {
                    let outcome = match outcome {
                        ImportOutcome::Imported => "imported",
                        ImportOutcome::AlreadyActive => "already_active",
                    };
                    tracing::info!(outcome, "matrix pull complete");
                    Ok(())
                }
                Err(error) => {
                    tracing::warn!(
                        kind = error.kind(),
                        reason = error.reason(),
                        "matrix pull failed"
                    );
                    Err(error)
                }
            }
        })
        .await
    }
}

#[cfg(test)]
#[path = "matrix_pull_tests.rs"]
mod tests;
