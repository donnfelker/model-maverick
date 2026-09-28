//! What a test database contains before the test starts.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};

/// Error type that a schema step returns.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

type StepFuture = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>;
type Step = Arc<dyn Fn(String) -> StepFuture + Send + Sync>;

/// The migrations that every test database starts with.
///
/// A schema is applied once per server to a template database. The template name is a
/// hash of the schema key, so a changed migration builds a new template and an unchanged
/// one is reused, also across test binaries that share a server.
#[derive(Clone)]
pub struct Schema {
    key: String,
    steps: Vec<Step>,
}

impl Schema {
    /// An empty database.
    pub fn empty() -> Self {
        Self {
            key: String::new(),
            steps: Vec::new(),
        }
    }

    /// Runs one SQL script. The key is the script text.
    pub fn sql(sql: &'static str) -> Self {
        Self::custom(format!("sql:{sql}"), move |url| async move {
            let mut conn = PgConnection::connect(&url).await?;
            sqlx::raw_sql(sql).execute(&mut conn).await?;
            conn.close().await?;
            Ok(())
        })
    }

    /// Runs a sqlx migrator. The key is the version and the checksum of every migration.
    pub fn sqlx(migrator: &'static Migrator) -> Self {
        let mut key = String::from("sqlx:");
        for migration in migrator.iter() {
            let _ = write!(key, "{}:", migration.version);
            push_hex(&mut key, &migration.checksum);
            key.push(';');
        }
        Self::custom(key, move |url| async move {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await?;
            migrator.run(&pool).await?;
            pool.close().await;
            Ok(())
        })
    }

    /// Runs `apply` with the connection URL of the template database. `apply` must close
    /// every connection that it opens before it returns.
    ///
    /// `key` identifies the result. Change it whenever `apply` would build a different
    /// database, or a stale template is reused.
    pub fn custom<F, Fut>(key: impl Into<String>, apply: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        let key = key.into();
        let step: Step = Arc::new(move |url| Box::pin(apply(url)));
        Self {
            key: format!("{}:{key}", key.len()),
            steps: vec![step],
        }
    }

    /// Applies `next` after `self`. Use it to add the job queue schema after the engine
    /// migrations.
    #[must_use]
    pub fn then(mut self, next: Schema) -> Self {
        self.key.push_str(&next.key);
        self.steps.extend(next.steps);
        self
    }

    /// The template database name: `mvtpl_` and 16 hex digits of the key hash.
    pub(crate) fn template_name(&self) -> String {
        let digest = Sha256::digest(self.key.as_bytes());
        let mut name = String::from("mvtpl_");
        push_hex(&mut name, &digest[..8]);
        name
    }

    pub(crate) async fn apply(&self, url: &str) -> Result<(), BoxError> {
        for step in &self.steps {
            step(url.to_owned()).await?;
        }
        Ok(())
    }
}

fn push_hex(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
}

#[cfg(test)]
mod tests {
    use super::Schema;

    #[test]
    fn template_name_is_a_stable_hash_of_the_key() {
        let name = Schema::sql("CREATE TABLE t (id int)").template_name();
        assert_eq!(name, Schema::sql("CREATE TABLE t (id int)").template_name());
        assert_eq!(name.len(), "mvtpl_".len() + 16);
        assert!(name.starts_with("mvtpl_"));
        assert_ne!(
            name,
            Schema::sql("CREATE TABLE t (id bigint)").template_name()
        );
        assert_ne!(name, Schema::empty().template_name());
    }

    #[test]
    fn chained_keys_do_not_collide() {
        let ab = Schema::sql("a").then(Schema::sql("b"));
        let a_b = Schema::sql("ab");
        assert_ne!(ab.template_name(), a_b.template_name());
        let ba = Schema::sql("b").then(Schema::sql("a"));
        assert_ne!(ab.template_name(), ba.template_name());
    }
}
