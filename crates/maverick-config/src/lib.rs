#![recursion_limit = "256"]

//! Typed startup configuration with the engine's exact environment contract.
//!
//! The loader takes an injected lookup so callers and tests do not depend on the
//! process environment. Secret fields have redacted `Debug` implementations.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

mod compat;

pub use compat::{
    CompatWarning, RENAME_WINDOW_DAYS, RENAMED, Rename, compatibility_warnings, known_settings,
};

const POSITIVE_INT: &str = "must be a positive integer";
const NON_NEGATIVE_INT: &str = "must be a non-negative integer";
const FINITE_NUMBER: &str = "must be a finite number";
const NON_NEGATIVE_NUMBER: &str = "must not be negative";
const LISTEN_SHAPE: &str = "must be a listen address like \":8080\" or \"host:port\"";
const PORT_RANGE: &str = "port must be a number between 0 and 65535";
const TOKEN_LIST: &str = "must be a comma-separated list of \"label:secret\" pairs";
const COLUMN_LIST: &str =
    "must be a comma-separated list of column identifiers with no empty entries";
const CRON: &str = "must be a 5-field cron expression like \"0 3 * * *\"";
const DURATION: &str = "must be a positive duration like \"120s\"";
const BOOLEAN: &str = "must be true or false";
const HTTP_URL: &str = "must be an http:// or https:// URL";
const MODELS_DEV_PRICE_MAP: &str = "must be a JSON array of mapping objects with non-empty column_provider, column_model_id, column_endpoint, models_dev_provider, and models_dev_model_id";
const DEFAULT_MODELS_DEV_PRICE_URL: &str = "https://models.dev/api.json";

/// Deployment role (spec §14.4, D49). Startup requires an explicit choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Origin,
    Replica,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Replica => "replica",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalToken {
    pub label: String,
    pub secret: String,
}

impl fmt::Debug for InternalToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InternalToken")
            .field("label", &self.label)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRingEntry {
    pub version: u32,
    pub key: [u8; 32],
}

impl fmt::Debug for KeyRingEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyRingEntry")
            .field("version", &self.version)
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLimit {
    pub max_concurrency: u32,
    pub rpm_limit: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLimits {
    pub anthropic: ProviderLimit,
    pub openai: ProviderLimit,
    pub google: ProviderLimit,
    pub xai: ProviderLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsDevPriceMapEntry {
    pub column_provider: String,
    pub column_model_id: String,
    pub column_endpoint: String,
    pub models_dev_provider: String,
    pub models_dev_model_id: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub role: Role,
    pub matrix_export_dir: String,
    pub matrix_signing_keys: String,
    pub database_url: String,
    pub public_addr: String,
    pub internal_addr: String,
    pub internal_tokens: Vec<InternalToken>,
    pub capture_master_key: [u8; 32],
    pub capture_master_key_hex: String,
    pub anthropic_api_key: String,
    pub openai_api_key: String,
    pub google_api_key: String,
    pub xai_api_key: String,
    /// Provider API base URLs. D23: the engine reads no ambient SDK variables, so these are
    /// the only way to move a provider (a gateway, or the e2e fake provider). Empty is the
    /// provider's default.
    pub anthropic_base_url: String,
    pub openai_base_url: String,
    pub google_base_url: String,
    pub xai_base_url: String,
    pub classifier_column: String,
    pub classifier_zero_retention_column: Option<String>,
    pub classifier_deadline: Duration,
    pub classifier_daily_quota: u32,
    pub classifier_daily_spend_cap_usd: f64,
    pub judge_pool: Vec<String>,
    pub runner_repeats: u32,
    pub runner_timeout: Duration,
    pub provider_limits: ProviderLimits,
    pub batch_job_timeout: Duration,
    pub batch_lease_ttl: Duration,
    pub batch_heartbeat_interval: Duration,
    pub analytics_job_timeout: Duration,
    pub spend_nightly_cap_usd: f64,
    pub spend_weekly_cap_usd: f64,
    pub scoring_alpha: f64,
    pub scoring_lambda_var: f64,
    pub scoring_lambda_amb: f64,
    pub scoring_min_nights: u32,
    pub ranking_band_quality: f64,
    pub ranking_band_balanced: f64,
    pub ranking_band_cost: f64,
    pub ranking_noise_band: f64,
    pub ranking_availability_floor: f64,
    /// `MAVERICK_AFFINITY_ENABLED` (spec §7.6, D44, Rust-only). False accepts and
    /// validates `affinity` but reads and writes no row.
    pub affinity_enabled: bool,
    /// `MAVERICK_AFFINITY_TTL`: a conversation's row lives this long after its last use.
    pub affinity_ttl: Duration,
    /// `MAVERICK_AFFINITY_CACHE_WARM_SECONDS`: the warm window of the pinned column's
    /// provider cache.
    pub affinity_cache_warm_seconds: u32,
    /// `MAVERICK_AFFINITY_INCUMBENT_BONUS`, in quality points.
    pub affinity_incumbent_bonus: f64,
    /// `MAVERICK_TENANT_NAME` (spec §14.4, D49): the name of the one tenant a replica
    /// creates at bootstrap. Read only then.
    pub tenant_name: String,
    /// `MAVERICK_BOOTSTRAP_KEY_FILE`: where a replica's bootstrap writes its API key (mode
    /// 0600). Empty: the key is only printed.
    pub bootstrap_key_file: String,
    pub ceiling_buffer: f64,
    pub catalog_cold_start_nights: u32,
    pub ratelimit_rpm: u32,
    pub history_retention_days: u32,
    pub capture_max_window_minutes: u32,
    pub capture_retention_hours: u32,
    pub capture_master_key_version: u32,
    pub capture_previous_master_key: Option<KeyRingEntry>,
    pub smtp_url: String,
    pub alerts_webhook_url: String,
    pub alerts_webhook_secret: String,
    pub cron_prediction_lock: String,
    pub cron_nightly: String,
    pub cron_weekly: String,
    pub cron_latency_probes: String,
    pub cron_price_sync: String,
    pub cron_catalog_sync: String,
    /// `MAVERICK_CRON_MODEL_DISCOVERY` (spec §4.6, §11; D51, Rust-only).
    pub cron_model_discovery: String,
    pub cron_capture_expiry: String,
    /// `MAVERICK_CRON_AFFINITY_PRUNE` (spec §11; D44, Rust-only).
    pub cron_affinity_prune: String,
    pub cron_history_prune: String,
    pub models_dev_price_url: String,
    pub models_dev_price_map: Vec<ModelsDevPriceMapEntry>,
    pub otel_exporter_endpoint: String,
    pub otel_service_name: String,
}

/// A URL for Debug output (G5): the userinfo password and any `password=` query value
/// become `<redacted>`. `postgres://`, `smtp://`, and OTLP URLs can carry credentials.
fn redact_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _)) => format!("{user}:<redacted>@{host}"),
            None => format!("{userinfo}@{host}"),
        },
        None => authority.to_owned(),
    };
    let tail = match tail.split_once('?') {
        Some((path, query)) => {
            let query: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((key, _)) if key.eq_ignore_ascii_case("password") => {
                        format!("{key}=<redacted>")
                    }
                    _ => pair.to_owned(),
                })
                .collect();
            format!("{path}?{}", query.join("&"))
        }
        None => tail.to_owned(),
    };
    format!("{scheme}://{authority}{tail}")
}

/// Only the scheme and host of a URL, for Debug output (G5). Webhook URLs often carry
/// their secret in the path or the query.
fn origin_only(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return if url.is_empty() {
            String::new()
        } else {
            "<redacted>".to_owned()
        };
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..authority_end]
        .rsplit_once('@')
        .map_or(&rest[..authority_end], |(_, h)| h);
    if authority_end == rest.len() {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}/<redacted>")
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("role", &self.role)
            .field("matrix_export_dir", &self.matrix_export_dir)
            .field("matrix_signing_keys", &"<redacted>")
            .field("database_url", &redact_credentials(&self.database_url))
            .field("public_addr", &self.public_addr)
            .field("internal_addr", &self.internal_addr)
            .field("internal_tokens", &self.internal_tokens)
            .field("capture_master_key", &"<redacted>")
            .field("capture_master_key_hex", &"<redacted>")
            .field("anthropic_api_key", &"<redacted>")
            .field("openai_api_key", &"<redacted>")
            .field("google_api_key", &"<redacted>")
            .field("xai_api_key", &"<redacted>")
            .field(
                "anthropic_base_url",
                &redact_credentials(&self.anthropic_base_url),
            )
            .field(
                "openai_base_url",
                &redact_credentials(&self.openai_base_url),
            )
            .field(
                "google_base_url",
                &redact_credentials(&self.google_base_url),
            )
            .field("xai_base_url", &redact_credentials(&self.xai_base_url))
            .field("classifier_column", &self.classifier_column)
            .field(
                "classifier_zero_retention_column",
                &self.classifier_zero_retention_column,
            )
            .field("classifier_deadline", &self.classifier_deadline)
            .field("classifier_daily_quota", &self.classifier_daily_quota)
            .field(
                "classifier_daily_spend_cap_usd",
                &self.classifier_daily_spend_cap_usd,
            )
            .field("judge_pool", &self.judge_pool)
            .field("runner_repeats", &self.runner_repeats)
            .field("runner_timeout", &self.runner_timeout)
            .field("provider_limits", &self.provider_limits)
            .field("batch_job_timeout", &self.batch_job_timeout)
            .field("batch_lease_ttl", &self.batch_lease_ttl)
            .field("batch_heartbeat_interval", &self.batch_heartbeat_interval)
            .field("analytics_job_timeout", &self.analytics_job_timeout)
            .field("spend_nightly_cap_usd", &self.spend_nightly_cap_usd)
            .field("spend_weekly_cap_usd", &self.spend_weekly_cap_usd)
            .field("scoring_alpha", &self.scoring_alpha)
            .field("scoring_lambda_var", &self.scoring_lambda_var)
            .field("scoring_lambda_amb", &self.scoring_lambda_amb)
            .field("scoring_min_nights", &self.scoring_min_nights)
            .field("ranking_band_quality", &self.ranking_band_quality)
            .field("ranking_band_balanced", &self.ranking_band_balanced)
            .field("ranking_band_cost", &self.ranking_band_cost)
            .field("ranking_noise_band", &self.ranking_noise_band)
            .field(
                "ranking_availability_floor",
                &self.ranking_availability_floor,
            )
            .field("affinity_enabled", &self.affinity_enabled)
            .field("affinity_ttl", &self.affinity_ttl)
            .field(
                "affinity_cache_warm_seconds",
                &self.affinity_cache_warm_seconds,
            )
            .field("affinity_incumbent_bonus", &self.affinity_incumbent_bonus)
            .field("tenant_name", &self.tenant_name)
            .field("bootstrap_key_file", &self.bootstrap_key_file)
            .field("ceiling_buffer", &self.ceiling_buffer)
            .field("catalog_cold_start_nights", &self.catalog_cold_start_nights)
            .field("ratelimit_rpm", &self.ratelimit_rpm)
            .field("history_retention_days", &self.history_retention_days)
            .field(
                "capture_max_window_minutes",
                &self.capture_max_window_minutes,
            )
            .field("capture_retention_hours", &self.capture_retention_hours)
            .field(
                "capture_master_key_version",
                &self.capture_master_key_version,
            )
            .field(
                "capture_previous_master_key",
                &self.capture_previous_master_key,
            )
            .field("smtp_url", &redact_credentials(&self.smtp_url))
            .field("alerts_webhook_url", &origin_only(&self.alerts_webhook_url))
            .field("alerts_webhook_secret", &"<redacted>")
            .field("cron_prediction_lock", &self.cron_prediction_lock)
            .field("cron_nightly", &self.cron_nightly)
            .field("cron_weekly", &self.cron_weekly)
            .field("cron_latency_probes", &self.cron_latency_probes)
            .field("cron_price_sync", &self.cron_price_sync)
            .field("cron_catalog_sync", &self.cron_catalog_sync)
            .field("cron_model_discovery", &self.cron_model_discovery)
            .field("cron_capture_expiry", &self.cron_capture_expiry)
            .field("cron_affinity_prune", &self.cron_affinity_prune)
            .field("cron_history_prune", &self.cron_history_prune)
            .field("models_dev_price_url", &self.models_dev_price_url)
            .field("models_dev_price_map", &self.models_dev_price_map)
            .field(
                "otel_exporter_endpoint",
                &redact_credentials(&self.otel_exporter_endpoint),
            )
            .field("otel_service_name", &self.otel_service_name)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
    pub var: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    issues: Vec<ConfigIssue>,
}

impl ConfigError {
    pub fn issues(&self) -> &[ConfigIssue] {
        &self.issues
    }

    pub fn into_issues(self) -> Vec<ConfigIssue> {
        self.issues
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid configuration: ")?;
        for (index, issue) in self.issues.iter().enumerate() {
            if index > 0 {
                write!(formatter, "; ")?;
            }
            write!(formatter, "{}: {}", issue.var, issue.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        load_env()
    }

    pub fn load_with<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        load_with(lookup)
    }
}

pub fn load_env() -> Result<Config, ConfigError> {
    load_with(|name| std::env::var(name).ok())
}

pub fn load_with<F>(lookup: F) -> Result<Config, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    // D50: a renamed setting still reads under its old name for 12 months.
    let lookup = compat::with_renames(lookup, compat::RENAMED);
    let mut issues = Vec::new();
    let role = match lookup("MAVERICK_ROLE").as_deref() {
        Some("origin") => Role::Origin,
        Some("replica") => Role::Replica,
        _ => {
            issue(&mut issues, "MAVERICK_ROLE", "must be origin or replica");
            Role::Replica // Used only to collect further issues; no Config is returned.
        }
    };
    let matrix_export_dir = if role == Role::Origin {
        optional(&lookup, "MAVERICK_MATRIX_EXPORT_DIR", "")
    } else {
        String::new()
    };
    let matrix_signing_keys = if !matrix_export_dir.is_empty() {
        required(&lookup, "MAVERICK_MATRIX_SIGNING_KEYS", &mut issues)
    } else {
        String::new()
    };
    let database_url = required(&lookup, "MAVERICK_DATABASE_URL", &mut issues);
    let public_addr = listen_addr(&lookup, "MAVERICK_PUBLIC_ADDR", ":8080", &mut issues);
    let internal_addr = listen_addr(&lookup, "MAVERICK_INTERNAL_ADDR", ":8081", &mut issues);
    let internal_tokens = internal_tokens(&lookup, &mut issues);
    let (capture_master_key, capture_master_key_hex) = master_key(&lookup, &mut issues);
    let anthropic_api_key = optional(&lookup, "MAVERICK_ANTHROPIC_API_KEY", "");
    let openai_api_key = optional(&lookup, "MAVERICK_OPENAI_API_KEY", "");
    let google_api_key = optional(&lookup, "MAVERICK_GOOGLE_API_KEY", "");
    let xai_api_key = optional(&lookup, "MAVERICK_XAI_API_KEY", "");
    let anthropic_base_url = http_url(&lookup, "MAVERICK_ANTHROPIC_BASE_URL", &mut issues);
    let openai_base_url = http_url(&lookup, "MAVERICK_OPENAI_BASE_URL", &mut issues);
    let google_base_url = http_url(&lookup, "MAVERICK_GOOGLE_BASE_URL", &mut issues);
    let xai_base_url = http_url(&lookup, "MAVERICK_XAI_BASE_URL", &mut issues);
    let classifier_column = optional(
        &lookup,
        "MAVERICK_CLASSIFIER_COLUMN",
        "claude-haiku-4-5/direct",
    );
    let judge_pool = if role == Role::Origin {
        column_list(
            &lookup,
            "MAVERICK_JUDGE_POOL",
            &["gemini-3.1-pro/direct", "gpt-5.5/direct"],
            &mut issues,
        )
    } else {
        Vec::new()
    };
    let runner_repeats = positive_int(&lookup, "MAVERICK_RUNNER_REPEATS", 3, &mut issues);
    let runner_timeout = duration(
        &lookup,
        "MAVERICK_RUNNER_TIMEOUT",
        Duration::from_secs(120),
        Duration::from_secs(600),
        &mut issues,
    );

    let anthropic = provider_limit(&lookup, "ANTHROPIC", &mut issues);
    let openai = provider_limit(&lookup, "OPENAI", &mut issues);
    let google = provider_limit(&lookup, "GOOGLE", &mut issues);
    let xai = provider_limit(&lookup, "XAI", &mut issues);
    let batch_job_timeout = duration(
        &lookup,
        "MAVERICK_BATCH_JOB_TIMEOUT",
        Duration::from_secs(6 * 60 * 60),
        Duration::from_secs(24 * 60 * 60),
        &mut issues,
    );
    let spend_nightly_cap_usd = if role == Role::Origin {
        non_negative_float(
            &lookup,
            "MAVERICK_SPEND_NIGHTLY_CAP_USD",
            100.0,
            &mut issues,
        )
    } else {
        0.0
    };
    let spend_weekly_cap_usd = if role == Role::Origin {
        non_negative_float(&lookup, "MAVERICK_SPEND_WEEKLY_CAP_USD", 500.0, &mut issues)
    } else {
        0.0
    };
    let scoring_alpha = unit_interval(&lookup, "MAVERICK_SCORING_ALPHA", 0.3, &mut issues);
    let scoring_lambda_var =
        non_negative_float(&lookup, "MAVERICK_SCORING_LAMBDA_VAR", 0.5, &mut issues);
    let scoring_lambda_amb =
        non_negative_float(&lookup, "MAVERICK_SCORING_LAMBDA_AMB", 0.25, &mut issues);
    let scoring_min_nights = positive_int(&lookup, "MAVERICK_SCORING_MIN_NIGHTS", 3, &mut issues);
    let ranking_band_quality =
        non_negative_float(&lookup, "MAVERICK_RANKING_BAND_QUALITY", 0.0, &mut issues);
    let ranking_band_balanced =
        non_negative_float(&lookup, "MAVERICK_RANKING_BAND_BALANCED", 1.5, &mut issues);
    let ranking_band_cost =
        non_negative_float(&lookup, "MAVERICK_RANKING_BAND_COST", 5.5, &mut issues);
    let ranking_noise_band =
        non_negative_float(&lookup, "MAVERICK_RANKING_NOISE_BAND", 0.75, &mut issues);
    let ceiling_buffer = non_negative_float(&lookup, "MAVERICK_CEILING_BUFFER", 0.05, &mut issues);
    let catalog_cold_start_nights = positive_int(
        &lookup,
        "MAVERICK_CATALOG_COLD_START_NIGHTS",
        5,
        &mut issues,
    );
    let ratelimit_rpm = positive_int(&lookup, "MAVERICK_RATELIMIT_RPM", 300, &mut issues);
    let history_retention_days =
        positive_int(&lookup, "MAVERICK_HISTORY_RETENTION_DAYS", 365, &mut issues);
    let capture_max_window_minutes = positive_int(
        &lookup,
        "MAVERICK_CAPTURE_MAX_WINDOW_MINUTES",
        1440,
        &mut issues,
    );
    let capture_retention_hours =
        positive_int(&lookup, "MAVERICK_CAPTURE_RETENTION_HOURS", 72, &mut issues);
    let smtp_url = optional(&lookup, "MAVERICK_SMTP_URL", "");
    let alerts_webhook_url = optional(&lookup, "MAVERICK_ALERTS_WEBHOOK_URL", "");
    let alerts_webhook_secret = optional(&lookup, "MAVERICK_ALERTS_WEBHOOK_SECRET", "");
    let cron_prediction_lock = cron(
        &lookup,
        "MAVERICK_CRON_PREDICTION_LOCK",
        "50 2 * * *",
        &mut issues,
    );
    let cron_nightly = cron(&lookup, "MAVERICK_CRON_NIGHTLY", "0 3 * * *", &mut issues);
    let cron_weekly = cron(&lookup, "MAVERICK_CRON_WEEKLY", "0 4 * * 0", &mut issues);
    let cron_latency_probes = cron(
        &lookup,
        "MAVERICK_CRON_LATENCY_PROBES",
        "0 * * * *",
        &mut issues,
    );
    let cron_price_sync =
        if role == Role::Replica && present(&lookup, "MAVERICK_CRON_PRICE_SYNC").is_none() {
            String::new()
        } else {
            cron(
                &lookup,
                "MAVERICK_CRON_PRICE_SYNC",
                "0 6 * * *",
                &mut issues,
            )
        };
    let cron_catalog_sync = cron(
        &lookup,
        "MAVERICK_CRON_CATALOG_SYNC",
        "10 6 * * *",
        &mut issues,
    );
    let cron_model_discovery = cron(
        &lookup,
        "MAVERICK_CRON_MODEL_DISCOVERY",
        "20 6 * * *",
        &mut issues,
    );
    let cron_capture_expiry = cron(
        &lookup,
        "MAVERICK_CRON_CAPTURE_EXPIRY",
        "0 * * * *",
        &mut issues,
    );
    let cron_affinity_prune = cron(
        &lookup,
        "MAVERICK_CRON_AFFINITY_PRUNE",
        "5 * * * *",
        &mut issues,
    );
    let cron_history_prune = cron(
        &lookup,
        "MAVERICK_CRON_HISTORY_PRUNE",
        "0 5 * * *",
        &mut issues,
    );

    let classifier_zero_retention_column =
        optional_opt(&lookup, "MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN");
    let classifier_deadline = duration(
        &lookup,
        "MAVERICK_CLASSIFIER_DEADLINE",
        Duration::from_millis(1500),
        Duration::from_secs(60),
        &mut issues,
    );
    let classifier_daily_quota = non_negative_int(
        &lookup,
        "MAVERICK_CLASSIFIER_DAILY_QUOTA",
        1000,
        &mut issues,
    );
    let classifier_daily_spend_cap_usd = non_negative_float(
        &lookup,
        "MAVERICK_CLASSIFIER_DAILY_SPEND_CAP_USD",
        5.0,
        &mut issues,
    );
    let capture_master_key_version = positive_int(
        &lookup,
        "MAVERICK_CAPTURE_MASTER_KEY_VERSION",
        1,
        &mut issues,
    );
    let capture_previous_master_key =
        previous_key(&lookup, "MAVERICK_CAPTURE_PREVIOUS_MASTER_KEY", &mut issues);
    let ranking_availability_floor = unit_interval(
        &lookup,
        "MAVERICK_RANKING_AVAILABILITY_FLOOR",
        0.9,
        &mut issues,
    );
    let affinity_enabled = boolean(&lookup, "MAVERICK_AFFINITY_ENABLED", true, &mut issues);
    let affinity_ttl = duration(
        &lookup,
        "MAVERICK_AFFINITY_TTL",
        Duration::from_secs(24 * 60 * 60),
        Duration::from_secs(30 * 24 * 60 * 60),
        &mut issues,
    );
    let affinity_cache_warm_seconds = non_negative_int(
        &lookup,
        "MAVERICK_AFFINITY_CACHE_WARM_SECONDS",
        300,
        &mut issues,
    );
    let affinity_incumbent_bonus = non_negative_float(
        &lookup,
        "MAVERICK_AFFINITY_INCUMBENT_BONUS",
        1.5,
        &mut issues,
    );
    let tenant_name = optional(&lookup, "MAVERICK_TENANT_NAME", "default");
    let bootstrap_key_file = optional(&lookup, "MAVERICK_BOOTSTRAP_KEY_FILE", "");
    let batch_lease_ttl = duration(
        &lookup,
        "MAVERICK_BATCH_LEASE_TTL",
        Duration::from_secs(60),
        Duration::from_secs(60 * 60),
        &mut issues,
    );
    let batch_heartbeat_interval = duration(
        &lookup,
        "MAVERICK_BATCH_HEARTBEAT_INTERVAL",
        Duration::from_secs(15),
        Duration::from_secs(60 * 60),
        &mut issues,
    );
    let analytics_job_timeout = duration(
        &lookup,
        "MAVERICK_ANALYTICS_JOB_TIMEOUT",
        Duration::from_secs(15 * 60),
        Duration::from_secs(24 * 60 * 60),
        &mut issues,
    );
    let models_dev_price_url = optional(
        &lookup,
        "MAVERICK_MODELS_DEV_PRICE_URL",
        DEFAULT_MODELS_DEV_PRICE_URL,
    );
    let models_dev_price_map = models_dev_price_map(&lookup, &mut issues);
    let otel_exporter_endpoint = optional(&lookup, "MAVERICK_OTEL_EXPORTER_ENDPOINT", "");
    let otel_service_name = optional(&lookup, "MAVERICK_OTEL_SERVICE_NAME", "maverick");

    if issues.is_empty() {
        Ok(Config {
            role,
            matrix_export_dir,
            matrix_signing_keys,
            database_url,
            public_addr,
            internal_addr,
            internal_tokens,
            capture_master_key,
            capture_master_key_hex,
            anthropic_api_key,
            openai_api_key,
            google_api_key,
            xai_api_key,
            anthropic_base_url,
            openai_base_url,
            google_base_url,
            xai_base_url,
            classifier_column,
            classifier_zero_retention_column,
            classifier_deadline,
            classifier_daily_quota,
            classifier_daily_spend_cap_usd,
            judge_pool,
            runner_repeats,
            runner_timeout,
            provider_limits: ProviderLimits {
                anthropic,
                openai,
                google,
                xai,
            },
            batch_job_timeout,
            batch_lease_ttl,
            batch_heartbeat_interval,
            analytics_job_timeout,
            spend_nightly_cap_usd,
            spend_weekly_cap_usd,
            scoring_alpha,
            scoring_lambda_var,
            scoring_lambda_amb,
            scoring_min_nights,
            ranking_band_quality,
            ranking_band_balanced,
            ranking_band_cost,
            ranking_noise_band,
            ranking_availability_floor,
            affinity_enabled,
            affinity_ttl,
            affinity_cache_warm_seconds,
            affinity_incumbent_bonus,
            tenant_name,
            bootstrap_key_file,
            ceiling_buffer,
            catalog_cold_start_nights,
            ratelimit_rpm,
            history_retention_days,
            capture_max_window_minutes,
            capture_retention_hours,
            capture_master_key_version,
            capture_previous_master_key,
            smtp_url,
            alerts_webhook_url,
            alerts_webhook_secret,
            cron_prediction_lock,
            cron_nightly,
            cron_weekly,
            cron_latency_probes,
            cron_price_sync,
            cron_catalog_sync,
            cron_model_discovery,
            cron_capture_expiry,
            cron_affinity_prune,
            cron_history_prune,
            models_dev_price_url,
            models_dev_price_map,
            otel_exporter_endpoint,
            otel_service_name,
        })
    } else {
        Err(ConfigError { issues })
    }
}

fn present<F>(lookup: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(name).filter(|value| !value.is_empty())
}

fn optional<F>(lookup: &F, name: &str, default: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    present(lookup, name).unwrap_or_else(|| default.to_string())
}

fn optional_opt<F>(lookup: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    present(lookup, name)
}

fn required<F>(lookup: &F, name: &str, issues: &mut Vec<ConfigIssue>) -> String
where
    F: Fn(&str) -> Option<String>,
{
    match present(lookup, name) {
        Some(value) => value,
        None => {
            issue(issues, name, "required but not set");
            String::new()
        }
    }
}

/// An optional `http://` or `https://` URL; unset or empty is `""`.
fn http_url<F>(lookup: &F, name: &str, issues: &mut Vec<ConfigIssue>) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return String::new();
    };
    let host = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"));
    if host.is_some_and(|host| !host.is_empty()) {
        value
    } else {
        issue(issues, name, HTTP_URL);
        String::new()
    }
}

fn issue(issues: &mut Vec<ConfigIssue>, var: &str, reason: &str) {
    issues.push(ConfigIssue {
        var: var.to_string(),
        reason: reason.to_string(),
    });
}

fn listen_addr<F>(lookup: &F, name: &str, default: &str, issues: &mut Vec<ConfigIssue>) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let value = optional(lookup, name, default);
    let port = if let Some(rest) = value.strip_prefix('[') {
        let Some(end) = rest.find("]:") else {
            issue(issues, name, LISTEN_SHAPE);
            return value;
        };
        &rest[end + 2..]
    } else {
        let Some((host, port)) = value.rsplit_once(':') else {
            issue(issues, name, LISTEN_SHAPE);
            return value;
        };
        if host.contains(':') {
            issue(issues, name, LISTEN_SHAPE);
            return value;
        }
        port
    };
    if port
        .parse::<i64>()
        .map_or(true, |port| !(0..=65535).contains(&port))
    {
        issue(issues, name, PORT_RANGE);
    }
    value
}

fn internal_tokens<F>(lookup: &F, issues: &mut Vec<ConfigIssue>) -> Vec<InternalToken>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, "MAVERICK_INTERNAL_TOKENS") else {
        issue(issues, "MAVERICK_INTERNAL_TOKENS", "required but not set");
        return Vec::new();
    };
    let mut parsed = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        let Some((label, secret)) = entry.split_once(':') else {
            issue(issues, "MAVERICK_INTERNAL_TOKENS", TOKEN_LIST);
            return Vec::new();
        };
        if label.is_empty() || secret.is_empty() {
            issue(issues, "MAVERICK_INTERNAL_TOKENS", TOKEN_LIST);
            return Vec::new();
        }
        parsed.push(InternalToken {
            label: label.to_string(),
            secret: secret.to_string(),
        });
    }
    parsed
}

fn master_key<F>(lookup: &F, issues: &mut Vec<ConfigIssue>) -> ([u8; 32], String)
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, "MAVERICK_CAPTURE_MASTER_KEY") else {
        issue(
            issues,
            "MAVERICK_CAPTURE_MASTER_KEY",
            "required but not set",
        );
        return ([0; 32], String::new());
    };
    let Some(bytes) = decode_hex(&value) else {
        issue(
            issues,
            "MAVERICK_CAPTURE_MASTER_KEY",
            "must be 64 hex characters encoding a 32-byte key",
        );
        return ([0; 32], String::new());
    };
    if bytes.len() != 32 {
        issue(
            issues,
            "MAVERICK_CAPTURE_MASTER_KEY",
            "must be 64 hex characters encoding a 32-byte key",
        );
        return ([0; 32], String::new());
    }
    let mut key = [0_u8; 32];
    key.copy_from_slice(&bytes);
    (key, bytes_to_hex(&key))
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut output = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        output.push((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?);
    }
    Some(output)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn bytes_to_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn column_list<F>(
    lookup: &F,
    name: &str,
    default: &[&str],
    issues: &mut Vec<ConfigIssue>,
) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return default.iter().map(|entry| (*entry).to_string()).collect();
    };
    let entries = value
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if entries.iter().any(String::is_empty) {
        issue(issues, name, COLUMN_LIST);
        Vec::new()
    } else {
        entries
    }
}

fn provider_limit<F>(lookup: &F, provider: &str, issues: &mut Vec<ConfigIssue>) -> ProviderLimit
where
    F: Fn(&str) -> Option<String>,
{
    let max_name = format!("MAVERICK_PROVIDERS_{provider}_MAX_CONCURRENCY");
    let rpm_name = format!("MAVERICK_PROVIDERS_{provider}_RPM_LIMIT");
    ProviderLimit {
        max_concurrency: positive_int(lookup, &max_name, 8, issues),
        rpm_limit: non_negative_int(lookup, &rpm_name, 0, issues),
    }
}

fn positive_int<F>(lookup: &F, name: &str, default: u32, issues: &mut Vec<ConfigIssue>) -> u32
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return default;
    };
    match value.parse::<i64>() {
        Ok(parsed) if parsed > 0 && parsed <= i64::from(u32::MAX) => parsed as u32,
        _ => {
            issue(issues, name, POSITIVE_INT);
            default
        }
    }
}

fn non_negative_int<F>(lookup: &F, name: &str, default: u32, issues: &mut Vec<ConfigIssue>) -> u32
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return default;
    };
    match value.parse::<i64>() {
        Ok(parsed) if parsed >= 0 && parsed <= i64::from(u32::MAX) => parsed as u32,
        _ => {
            issue(issues, name, NON_NEGATIVE_INT);
            default
        }
    }
}

/// Go `strconv.ParseBool`: `1`, `t`, `T`, `TRUE`, `true`, `True`, and the false forms.
fn boolean<F>(lookup: &F, name: &str, default: bool, issues: &mut Vec<ConfigIssue>) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return default;
    };
    match value.as_str() {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
        "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
        _ => {
            issue(issues, name, BOOLEAN);
            default
        }
    }
}

fn float<F>(lookup: &F, name: &str, default: f64, issues: &mut Vec<ConfigIssue>) -> f64
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return default;
    };
    match parse_float(&value) {
        Some(value) if value.is_finite() => value,
        _ => {
            issue(issues, name, FINITE_NUMBER);
            default
        }
    }
}

fn parse_float(value: &str) -> Option<f64> {
    if let Ok(parsed) = value.parse::<f64>() {
        return Some(parsed);
    }
    let (sign, unsigned) = if let Some(value) = value.strip_prefix('-') {
        (-1.0, value)
    } else if let Some(value) = value.strip_prefix('+') {
        (1.0, value)
    } else {
        (1.0, value)
    };
    let unsigned = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))?;
    let (mantissa, exponent) = unsigned
        .split_once('p')
        .or_else(|| unsigned.split_once('P'))?;
    let exponent = exponent.parse::<i32>().ok()?;
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let mut digits = 0.0;
    for digit in whole.bytes().chain(fraction.bytes()) {
        digits = digits * 16.0 + f64::from(hex_digit(digit)?);
    }
    let scale = 16_f64.powi(-(fraction.len() as i32));
    Some(sign * digits * scale * 2_f64.powi(exponent))
}

fn non_negative_float<F>(lookup: &F, name: &str, default: f64, issues: &mut Vec<ConfigIssue>) -> f64
where
    F: Fn(&str) -> Option<String>,
{
    let value = float(lookup, name, default, issues);
    let parsed = present(lookup, name).and_then(|raw| parse_float(&raw));
    if parsed.is_some_and(|raw| raw.is_finite()) && value < 0.0 {
        issue(issues, name, NON_NEGATIVE_NUMBER);
        default
    } else {
        value
    }
}

fn unit_interval<F>(lookup: &F, name: &str, default: f64, issues: &mut Vec<ConfigIssue>) -> f64
where
    F: Fn(&str) -> Option<String>,
{
    let value = float(lookup, name, default, issues);
    let parsed = present(lookup, name).and_then(|raw| parse_float(&raw));
    if parsed.is_some_and(|raw| raw.is_finite()) && !(value > 0.0 && value <= 1.0) {
        issue(issues, name, "must be a number in (0, 1]");
        default
    } else {
        value
    }
}

fn duration<F>(
    lookup: &F,
    name: &str,
    default: Duration,
    max: Duration,
    issues: &mut Vec<ConfigIssue>,
) -> Duration
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = present(lookup, name) else {
        return default;
    };
    let Some(parsed) = parse_duration(&value) else {
        issue(issues, name, DURATION);
        return default;
    };
    if parsed.is_zero() {
        issue(issues, name, DURATION);
        return default;
    }
    if parsed > max {
        let reason = if max == Duration::from_secs(600) {
            "must not exceed 10m0s"
        } else if max == Duration::from_secs(24 * 60 * 60) {
            "must not exceed 24h0m0s"
        } else {
            "must not exceed configured maximum"
        };
        issue(issues, name, reason);
        return default;
    }
    parsed
}

fn parse_duration(value: &str) -> Option<Duration> {
    if value == "0" {
        return Some(Duration::ZERO);
    }
    let mut rest = value;
    let negative = rest.starts_with('-');
    if negative || rest.starts_with('+') {
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return None;
    }
    let mut total_seconds = 0.0;
    while !rest.is_empty() {
        let number_end = rest
            .char_indices()
            .find(|(_, character)| !character.is_ascii_digit() && *character != '.')
            .map_or(rest.len(), |(index, _)| index);
        if number_end == 0 {
            return None;
        }
        let number = rest[..number_end].parse::<f64>().ok()?;
        rest = &rest[number_end..];
        let multiplier = if rest.starts_with("ns") {
            rest = &rest[2..];
            1e-9
        } else if rest.starts_with("us") {
            rest = &rest[2..];
            1e-6
        } else if rest.starts_with("µs") || rest.starts_with("μs") {
            rest = &rest["µs".len()..];
            1e-6
        } else if rest.starts_with("ms") {
            rest = &rest[2..];
            1e-3
        } else if rest.starts_with('s') {
            rest = &rest[1..];
            1.0
        } else if rest.starts_with('m') {
            rest = &rest[1..];
            60.0
        } else if rest.starts_with('h') {
            rest = &rest[1..];
            3600.0
        } else {
            return None;
        };
        total_seconds += number * multiplier;
    }
    if negative {
        total_seconds = -total_seconds;
    }
    if !total_seconds.is_finite() || total_seconds <= 0.0 {
        return Some(Duration::ZERO);
    }
    let nanos = total_seconds * 1_000_000_000.0;
    if nanos > u64::MAX as f64 {
        return None;
    }
    Some(Duration::from_nanos(nanos as u64))
}

fn cron<F>(lookup: &F, name: &str, default: &str, issues: &mut Vec<ConfigIssue>) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let value = optional(lookup, name, default);
    if value.split_whitespace().count() != 5 {
        issue(issues, name, CRON);
    }
    value
}

fn models_dev_price_map<F>(lookup: &F, issues: &mut Vec<ConfigIssue>) -> Vec<ModelsDevPriceMapEntry>
where
    F: Fn(&str) -> Option<String>,
{
    let name = "MAVERICK_MODELS_DEV_PRICE_MAP";
    let Some(value) = present(lookup, name) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<ModelsDevPriceMapEntry>>(&value) else {
        issue(issues, name, MODELS_DEV_PRICE_MAP);
        return Vec::new();
    };
    let valid_text = |value: &str| !value.is_empty() && value.trim() == value;
    if entries.iter().any(|entry| {
        !valid_text(&entry.column_provider)
            || !valid_text(&entry.column_model_id)
            || !valid_text(&entry.column_endpoint)
            || !valid_text(&entry.models_dev_provider)
            || !valid_text(&entry.models_dev_model_id)
    }) {
        issue(issues, name, MODELS_DEV_PRICE_MAP);
        return Vec::new();
    }
    let mut seen = BTreeSet::new();
    if entries.iter().any(|entry| {
        !seen.insert((
            entry.column_provider.as_str(),
            entry.column_model_id.as_str(),
            entry.column_endpoint.as_str(),
        ))
    }) {
        issue(
            issues,
            name,
            "must not map the same catalog column more than once",
        );
        return Vec::new();
    }
    entries
}

fn previous_key<F>(lookup: &F, name: &str, issues: &mut Vec<ConfigIssue>) -> Option<KeyRingEntry>
where
    F: Fn(&str) -> Option<String>,
{
    let value = present(lookup, name)?;
    let Some((version, key)) = value.split_once(':') else {
        issue(issues, name, "must be in the form \"version:hex\"");
        return None;
    };
    let Ok(version) = version.parse::<u32>() else {
        issue(issues, name, "must be in the form \"version:hex\"");
        return None;
    };
    let Some(key) = decode_hex(key) else {
        issue(issues, name, "must be in the form \"version:hex\"");
        return None;
    };
    if version == 0 || key.len() != 32 {
        issue(issues, name, "must be in the form \"version:hex\"");
        return None;
    }
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&key);
    Some(KeyRingEntry {
        version,
        key: bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // DIVERGENCE: D49. Recorded Go scenarios describe the hosted origin.
    fn load_origin(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        super::load_with(|name| {
            if name == "MAVERICK_ROLE" {
                Some("origin".to_owned())
            } else {
                lookup(name)
            }
        })
    }

    #[test]
    fn export_requires_runtime_keys_only_on_an_enabled_origin() {
        let load = |role: &str, export: Option<&str>, keys: Option<&str>| {
            super::load_with(|name| match name {
                "MAVERICK_ROLE" => Some(role.to_owned()),
                "MAVERICK_DATABASE_URL" => Some("postgres://db".to_owned()),
                "MAVERICK_INTERNAL_TOKENS" => Some("ops:s".to_owned()),
                "MAVERICK_CAPTURE_MASTER_KEY" => Some("ab".repeat(32)),
                "MAVERICK_MATRIX_EXPORT_DIR" => export.map(str::to_owned),
                "MAVERICK_MATRIX_SIGNING_KEYS" => keys.map(str::to_owned),
                _ => None,
            })
        };
        assert!(
            load("origin", None, None)
                .unwrap()
                .matrix_export_dir
                .is_empty()
        );
        let error = load("origin", Some("/exports"), None).unwrap_err();
        assert_eq!(error.issues().len(), 1);
        assert_eq!(error.issues()[0].var, "MAVERICK_MATRIX_SIGNING_KEYS");
        let origin = load("origin", Some("/exports"), Some("kid:/private/key-canary")).unwrap();
        assert_eq!(origin.matrix_signing_keys, "kid:/private/key-canary");
        assert!(!format!("{origin:?}").contains("key-canary"));
        let replica = load("replica", Some("/ignored"), None).unwrap();
        assert!(replica.matrix_export_dir.is_empty());
        assert!(replica.matrix_signing_keys.is_empty());
    }

    /// DIVERGENCE: D49. No startup silently chooses a role.
    #[test]
    fn role_is_required_and_names_both_choices() {
        for role in [None, Some(""), Some("Origin"), Some("other")] {
            let error = super::load_with(|name| match name {
                "MAVERICK_ROLE" => role.map(str::to_owned),
                "MAVERICK_DATABASE_URL" => Some("postgres://db".to_owned()),
                "MAVERICK_INTERNAL_TOKENS" => Some("ops:s".to_owned()),
                "MAVERICK_CAPTURE_MASTER_KEY" => Some("ab".repeat(32)),
                _ => None,
            })
            .unwrap_err();
            assert_eq!(
                error.issues(),
                &[ConfigIssue {
                    var: "MAVERICK_ROLE".to_owned(),
                    reason: "must be origin or replica".to_owned(),
                }]
            );
        }
    }

    /// DIVERGENCE: D49. Replica startup needs no corpus settings or price source.
    #[test]
    fn replica_ignores_corpus_settings_and_opts_in_to_prices() {
        let load = |cron: Option<&str>| {
            super::load_with(|name| match name {
                "MAVERICK_ROLE" => Some("replica".to_owned()),
                "MAVERICK_DATABASE_URL" => Some("postgres://db".to_owned()),
                "MAVERICK_INTERNAL_TOKENS" => Some("ops:s".to_owned()),
                "MAVERICK_CAPTURE_MASTER_KEY" => Some("ab".repeat(32)),
                "MAVERICK_JUDGE_POOL" => Some("not,a,valid,pool,".to_owned()),
                "MAVERICK_SPEND_NIGHTLY_CAP_USD" | "MAVERICK_SPEND_WEEKLY_CAP_USD" => {
                    Some("unused".to_owned())
                }
                "MAVERICK_CRON_PRICE_SYNC" => cron.map(str::to_owned),
                _ => None,
            })
        };
        let config = load(None).unwrap();
        assert_eq!(config.role, Role::Replica);
        assert!(config.judge_pool.is_empty());
        assert_eq!(
            (config.spend_nightly_cap_usd, config.spend_weekly_cap_usd),
            (0.0, 0.0)
        );
        assert!(config.cron_price_sync.is_empty());
        assert!(load(Some("")).unwrap().cron_price_sync.is_empty());
        assert_eq!(
            load(Some("5 6 * * *")).unwrap().cron_price_sync,
            "5 6 * * *"
        );
        assert!(
            load(Some("bad"))
                .unwrap_err()
                .issues()
                .iter()
                .any(|i| i.var == "MAVERICK_CRON_PRICE_SYNC")
        );
    }
    #[test]
    fn debug_output_never_prints_credentials() {
        let vars = [
            (
                "MAVERICK_DATABASE_URL",
                "postgres://maverick:dbhunter2@db:5432/maverick?sslmode=disable&password=qhunter2",
            ),
            ("MAVERICK_INTERNAL_TOKENS", "ops:tokhunter2"),
            ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32).leak()),
            (
                "MAVERICK_SMTP_URL",
                "smtp://alerts:smtphunter2@mail.example.com:587/?from=a@x&to=b@x",
            ),
            (
                "MAVERICK_ALERTS_WEBHOOK_URL",
                "https://hooks.example.com/services/T0/B0/hookhunter2",
            ),
            ("MAVERICK_ALERTS_WEBHOOK_SECRET", "sechunter2"),
            (
                "MAVERICK_OTEL_EXPORTER_ENDPOINT",
                "https://otel:otelhunter2@collector:4318",
            ),
            ("MAVERICK_ANTHROPIC_API_KEY", "keyhunter2"),
            (
                "MAVERICK_OPENAI_BASE_URL",
                "https://gw:gwhunter2@gateway.example.com/v1",
            ),
        ];
        let config = load_origin(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
        .expect("valid config");
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("postgres://maverick:<redacted>@db:5432/maverick?sslmode=disable&password=<redacted>"), "{debug}");
        assert!(
            debug.contains("smtp://alerts:<redacted>@mail.example.com:587/?from=a@x&to=b@x"),
            "{debug}"
        );
        assert!(
            debug.contains("https://hooks.example.com/<redacted>"),
            "{debug}"
        );
        assert!(
            debug.contains("https://otel:<redacted>@collector:4318"),
            "{debug}"
        );
        assert!(
            debug.contains("https://gw:<redacted>@gateway.example.com/v1"),
            "{debug}"
        );
    }

    #[test]
    fn provider_base_urls_are_optional_http_urls() {
        let base = [
            ("MAVERICK_DATABASE_URL", "postgres://db"),
            ("MAVERICK_INTERNAL_TOKENS", "ops:alpha"),
            ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32).leak()),
        ];
        let load = |extra: &[(&'static str, &'static str)]| {
            load_origin(|name| {
                base.iter()
                    .chain(extra)
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        };
        let config = load(&[]).expect("defaults");
        assert_eq!(
            [
                config.anthropic_base_url.as_str(),
                config.openai_base_url.as_str(),
                config.google_base_url.as_str(),
                config.xai_base_url.as_str(),
            ],
            ["", "", "", ""],
            "empty is the provider default"
        );
        let config = load(&[
            ("MAVERICK_ANTHROPIC_BASE_URL", "http://127.0.0.1:9000"),
            ("MAVERICK_OPENAI_BASE_URL", "https://gateway.example.com/v1"),
            ("MAVERICK_GOOGLE_BASE_URL", "http://fake:1/v1beta"),
            ("MAVERICK_XAI_BASE_URL", ""),
        ])
        .expect("urls");
        assert_eq!(config.anthropic_base_url, "http://127.0.0.1:9000");
        assert_eq!(config.openai_base_url, "https://gateway.example.com/v1");
        assert_eq!(config.google_base_url, "http://fake:1/v1beta");
        assert_eq!(config.xai_base_url, "");
        for bad in ["ftp://x", "localhost:9000", "http://", "https://"] {
            let error = load(&[("MAVERICK_XAI_BASE_URL", bad)]).expect_err(bad);
            assert!(
                error.to_string().contains("MAVERICK_XAI_BASE_URL")
                    && error.to_string().contains(HTTP_URL),
                "{bad}: {error}"
            );
        }
    }

    /// D49: the replica bootstrap's tenant name defaults to `default`, and its key file
    /// is unset; both read as given.
    #[test]
    fn bootstrap_settings_parse_and_default() {
        let load = |extra: &[(&'static str, &'static str)]| {
            let extra = extra.to_vec();
            load_with(move |name| {
                extra
                    .iter()
                    .chain(&[
                        ("MAVERICK_ROLE", "replica"),
                        ("MAVERICK_DATABASE_URL", "postgres://db"),
                        ("MAVERICK_INTERNAL_TOKENS", "ops:alpha"),
                        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32).leak()),
                    ])
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
            .expect("loads")
        };
        let config = load(&[]);
        assert_eq!(config.tenant_name, "default");
        assert_eq!(config.bootstrap_key_file, "");
        let config = load(&[
            ("MAVERICK_TENANT_NAME", ""),
            ("MAVERICK_BOOTSTRAP_KEY_FILE", ""),
        ]);
        assert_eq!(config.tenant_name, "default");
        let config = load(&[
            ("MAVERICK_TENANT_NAME", "Acme Corp"),
            ("MAVERICK_BOOTSTRAP_KEY_FILE", "/run/secrets/maverick-key"),
        ]);
        assert_eq!(config.tenant_name, "Acme Corp");
        assert_eq!(config.bootstrap_key_file, "/run/secrets/maverick-key");
    }

    /// D44: the four affinity settings, their defaults, and their startup errors.
    #[test]
    fn affinity_settings_parse_and_default() {
        let base = [
            ("MAVERICK_ROLE", "origin"),
            ("MAVERICK_DATABASE_URL", "postgres://db"),
            ("MAVERICK_INTERNAL_TOKENS", "ops:alpha"),
            ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32).leak()),
        ];
        let load = |extra: &[(&'static str, &'static str)]| {
            load_with(|name| {
                extra
                    .iter()
                    .chain(base.iter())
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        };
        let config = load(&[]).expect("defaults");
        assert!(config.affinity_enabled);
        assert_eq!(config.affinity_ttl, Duration::from_secs(24 * 60 * 60));
        assert_eq!(config.affinity_cache_warm_seconds, 300);
        assert_eq!(config.affinity_incumbent_bonus, 1.5);

        let config = load(&[
            ("MAVERICK_AFFINITY_ENABLED", "false"),
            ("MAVERICK_AFFINITY_TTL", "90m"),
            ("MAVERICK_AFFINITY_CACHE_WARM_SECONDS", "0"),
            ("MAVERICK_AFFINITY_INCUMBENT_BONUS", "0"),
        ])
        .expect("set");
        assert!(!config.affinity_enabled);
        assert_eq!(config.affinity_ttl, Duration::from_secs(90 * 60));
        assert_eq!(config.affinity_cache_warm_seconds, 0);
        assert_eq!(config.affinity_incumbent_bonus, 0.0);
        for (value, enabled) in [("1", true), ("T", true), ("TRUE", true), ("True", true)]
            .into_iter()
            .chain([
                ("0", false),
                ("f", false),
                ("FALSE", false),
                ("False", false),
            ])
        {
            let config = load(&[("MAVERICK_AFFINITY_ENABLED", value)]).expect(value);
            assert_eq!(config.affinity_enabled, enabled, "{value}");
        }

        for (var, value, reason) in [
            ("MAVERICK_AFFINITY_ENABLED", "yes", BOOLEAN),
            ("MAVERICK_AFFINITY_TTL", "0s", DURATION),
            (
                "MAVERICK_AFFINITY_TTL",
                "721h",
                "must not exceed configured maximum",
            ),
            (
                "MAVERICK_AFFINITY_CACHE_WARM_SECONDS",
                "-1",
                NON_NEGATIVE_INT,
            ),
            (
                "MAVERICK_AFFINITY_INCUMBENT_BONUS",
                "-0.5",
                NON_NEGATIVE_NUMBER,
            ),
        ] {
            let error = load(&[(var, value)]).expect_err(value);
            assert!(
                error
                    .issues()
                    .iter()
                    .any(|issue| issue.var == var && issue.reason == reason),
                "{var}={value}: {error}"
            );
        }
        let config = load(&[("MAVERICK_AFFINITY_TTL", "720h")]).expect("the maximum");
        assert_eq!(config.affinity_ttl, Duration::from_secs(720 * 60 * 60));
    }

    #[test]
    fn redaction_keeps_urls_without_credentials() {
        assert_eq!(redact_credentials(""), "");
        assert_eq!(
            redact_credentials("http://collector:4318"),
            "http://collector:4318"
        );
        assert_eq!(redact_credentials("postgres://u@h/db"), "postgres://u@h/db");
        assert_eq!(origin_only(""), "");
        assert_eq!(
            origin_only("https://h.example.com"),
            "https://h.example.com"
        );
        assert_eq!(origin_only("not a url"), "<redacted>");
    }
    use parity_support::fixture;
    use serde_json::{Map, Value, json};

    /// parity: config.TestLoadDefaults
    /// parity: config.TestLoadOverrides
    /// parity: config.TestLoadRequiredMissing
    /// parity: config.TestLoadEmptyRequiredIsMissing
    /// parity: config.TestLoadMalformedValues/public_addr_without_port
    /// parity: config.TestLoadMalformedValues/public_addr_non-numeric_port
    /// parity: config.TestLoadMalformedValues/internal_addr_port_out_of_range
    /// parity: config.TestLoadMalformedValues/tokens_missing_separator
    /// parity: config.TestLoadMalformedValues/tokens_empty_label
    /// parity: config.TestLoadMalformedValues/tokens_empty_secret
    /// parity: config.TestLoadMalformedValues/master_key_not_hex
    /// parity: config.TestLoadMalformedValues/master_key_too_short
    /// parity: config.TestLoadMalformedValues/judge_pool_empty_entry
    /// parity: config.TestLoadMalformedValues/nightly_cap_not_a_number
    /// parity: config.TestLoadMalformedValues/nightly_cap_negative
    /// parity: config.TestLoadMalformedValues/alpha_not_a_number
    /// parity: config.TestLoadMalformedValues/alpha_above_one
    /// parity: config.TestLoadMalformedValues/alpha_zero
    /// parity: config.TestLoadMalformedValues/lambda_var_not_a_number
    /// parity: config.TestLoadMalformedValues/noise_band_NaN
    /// parity: config.TestLoadMalformedValues/rpm_non-numeric
    /// parity: config.TestLoadMalformedValues/rpm_zero
    /// parity: config.TestLoadMalformedValues/rpm_negative
    /// parity: config.TestLoadMalformedValues/retention_days_float
    /// parity: config.TestLoadMalformedValues/capture_max_window_zero
    /// parity: config.TestLoadMalformedValues/capture_retention_hours_negative
    /// parity: config.TestLoadMalformedValues/capture_expiry_cron_free_text
    /// parity: config.TestLoadMalformedValues/history_prune_cron_too_few_fields
    /// parity: config.TestLoadMalformedValues/min_nights_non-numeric
    /// parity: config.TestLoadMalformedValues/cold_start_nights_zero
    /// parity: config.TestLoadMalformedValues/cron_too_few_fields
    /// parity: config.TestLoadMalformedValues/cron_free_text
    /// parity: config.TestLoadMalformedValues/runner_repeats_zero
    /// parity: config.TestLoadMalformedValues/runner_timeout_not_a_duration
    /// parity: config.TestLoadMalformedValues/runner_timeout_zero
    /// parity: config.TestLoadMalformedValues/runner_timeout_above_cap
    /// parity: config.TestLoadMalformedValues/batch_job_timeout_not_a_duration
    /// parity: config.TestLoadMalformedValues/batch_job_timeout_zero
    /// parity: config.TestLoadMalformedValues/batch_job_timeout_negative
    /// parity: config.TestLoadMalformedValues/batch_job_timeout_above_cap
    /// parity: config.TestLoadMalformedValues/provider_max_concurrency_zero
    /// parity: config.TestLoadMalformedValues/provider_rpm_negative
    /// parity: config.TestLoadMalformedValues/provider_rpm_non-numeric
    /// parity: config.TestLoadReportsAllIssuesAtOnce
    /// parity: config.TestLoadMalformedValues
    #[test]
    fn config_fixture_matches() {
        let fixture = fixture("store", "config_load.json").expect("config fixture");
        for case in fixture["cases"].as_array().expect("cases") {
            let mut env = Map::new();
            for (key, value) in case["env"].as_object().expect("env") {
                env.insert(key.clone(), value.clone());
            }
            let result =
                load_origin(|name| env.get(name).and_then(Value::as_str).map(str::to_string));
            match (case["ok"].as_bool().expect("ok"), result) {
                (true, Ok(config)) => assert_eq!(parity_view(&config), case["config"]),
                (false, Err(error)) => {
                    let expected = case["issues"].as_array().expect("issues");
                    let actual = error
                        .issues()
                        .iter()
                        .map(|issue| json!({"var": issue.var, "reason": issue.reason}))
                        .collect::<Vec<_>>();
                    assert_eq!(actual, *expected, "case {}", case["id"]);
                    assert_eq!(error.to_string(), case["error_text"]);
                }
                (true, Err(error)) => panic!("{} unexpectedly failed: {error}", case["id"]),
                (false, Ok(_)) => panic!("{} unexpectedly succeeded", case["id"]),
            }
        }
    }

    #[test]
    fn rust_only_config_defaults_and_key_ring() {
        let env = Map::from_iter([
            ("MAVERICK_DATABASE_URL".to_string(), json!("postgres://db")),
            (
                "MAVERICK_INTERNAL_TOKENS".to_string(),
                json!("rails:secret"),
            ),
            (
                "MAVERICK_CAPTURE_MASTER_KEY".to_string(),
                json!("ab".repeat(32)),
            ),
            (
                "MAVERICK_CAPTURE_PREVIOUS_MASTER_KEY".to_string(),
                json!(format!("2:{}", "cd".repeat(32))),
            ),
            ("MAVERICK_CLASSIFIER_DEADLINE".to_string(), json!("1.5s")),
            (
                "MAVERICK_RANKING_AVAILABILITY_FLOOR".to_string(),
                json!("0.95"),
            ),
        ]);
        let config = load_origin(|name| env.get(name).and_then(Value::as_str).map(str::to_string))
            .expect("rust config");
        assert_eq!(config.classifier_deadline, Duration::from_millis(1500));
        assert_eq!(config.ranking_availability_floor, 0.95);
        assert_eq!(config.capture_master_key_version, 1);
        assert_eq!(
            config.capture_previous_master_key.as_ref().unwrap().version,
            2
        );
        assert_eq!(config.classifier_daily_quota, 1000);
        assert_eq!(config.classifier_daily_spend_cap_usd, 5.0);
        assert_eq!(config.models_dev_price_url, DEFAULT_MODELS_DEV_PRICE_URL);
        assert!(config.models_dev_price_map.is_empty());
    }

    #[test]
    fn models_dev_price_map_is_explicit_json() {
        let map = serde_json::to_string(&json!([
            {
                "column_provider": "anthropic",
                "column_model_id": "claude-opus",
                "column_endpoint": "direct",
                "models_dev_provider": "anthropic",
                "models_dev_model_id": "claude-4-opus"
            },
            {
                "column_provider": "anthropic",
                "column_model_id": "claude-opus",
                "column_endpoint": "openrouter",
                "models_dev_provider": "openrouter",
                "models_dev_model_id": "anthropic/claude-4-opus"
            }
        ]))
        .expect("json");
        let env = Map::from_iter([
            ("MAVERICK_DATABASE_URL".to_string(), json!("postgres://db")),
            (
                "MAVERICK_INTERNAL_TOKENS".to_string(),
                json!("rails:secret"),
            ),
            (
                "MAVERICK_CAPTURE_MASTER_KEY".to_string(),
                json!("ab".repeat(32)),
            ),
            (
                "MAVERICK_MODELS_DEV_PRICE_URL".to_string(),
                json!("https://example.test/models.dev.json"),
            ),
            ("MAVERICK_MODELS_DEV_PRICE_MAP".to_string(), json!(map)),
        ]);

        let config = load_origin(|name| env.get(name).and_then(Value::as_str).map(str::to_string))
            .expect("rust config");

        assert_eq!(
            config.models_dev_price_url,
            "https://example.test/models.dev.json"
        );
        assert_eq!(
            config.models_dev_price_map,
            vec![
                ModelsDevPriceMapEntry {
                    column_provider: "anthropic".to_owned(),
                    column_model_id: "claude-opus".to_owned(),
                    column_endpoint: "direct".to_owned(),
                    models_dev_provider: "anthropic".to_owned(),
                    models_dev_model_id: "claude-4-opus".to_owned(),
                },
                ModelsDevPriceMapEntry {
                    column_provider: "anthropic".to_owned(),
                    column_model_id: "claude-opus".to_owned(),
                    column_endpoint: "openrouter".to_owned(),
                    models_dev_provider: "openrouter".to_owned(),
                    models_dev_model_id: "anthropic/claude-4-opus".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn models_dev_price_map_rejects_malformed_entries() {
        for (value, reason) in [
            ("{}", MODELS_DEV_PRICE_MAP),
            (
                r#"[{"column_provider":"","column_model_id":"m","column_endpoint":"direct","models_dev_provider":"p","models_dev_model_id":"m"}]"#,
                MODELS_DEV_PRICE_MAP,
            ),
            (
                r#"[
                    {"column_provider":"p","column_model_id":"m","column_endpoint":"direct","models_dev_provider":"p","models_dev_model_id":"m1"},
                    {"column_provider":"p","column_model_id":"m","column_endpoint":"direct","models_dev_provider":"p","models_dev_model_id":"m2"}
                ]"#,
                "must not map the same catalog column more than once",
            ),
        ] {
            let env = Map::from_iter([
                ("MAVERICK_DATABASE_URL".to_string(), json!("postgres://db")),
                (
                    "MAVERICK_INTERNAL_TOKENS".to_string(),
                    json!("rails:secret"),
                ),
                (
                    "MAVERICK_CAPTURE_MASTER_KEY".to_string(),
                    json!("ab".repeat(32)),
                ),
                ("MAVERICK_MODELS_DEV_PRICE_MAP".to_string(), json!(value)),
            ]);

            let error =
                load_origin(|name| env.get(name).and_then(Value::as_str).map(str::to_string))
                    .expect_err("invalid config");
            assert!(
                error
                    .issues()
                    .iter()
                    .any(|issue| issue.var == "MAVERICK_MODELS_DEV_PRICE_MAP"
                        && issue.reason == reason),
                "{error}"
            );
        }
    }

    #[test]
    fn parser_edges_cover_ipv6_and_go_hex_floats() {
        let mut issues = Vec::new();
        let address = listen_addr(
            &|name| (name == "MAVERICK_PUBLIC_ADDR").then(|| "[::1]:1".to_string()),
            "MAVERICK_PUBLIC_ADDR",
            ":8080",
            &mut issues,
        );
        assert_eq!(address, "[::1]:1");
        assert!(issues.is_empty());

        assert_eq!(parse_float("0x1p4"), Some(16.0));
        assert_eq!(parse_float("0x1.8p-2"), Some(0.375));
        assert_eq!(parse_float("-0x1.8p2"), Some(-6.0));
    }

    fn parity_view(config: &Config) -> Value {
        json!({
            "alerts_webhook_secret": config.alerts_webhook_secret,
            "alerts_webhook_url": config.alerts_webhook_url,
            "anthropic_api_key": config.anthropic_api_key,
            "batch_job_timeout_seconds": json_number(config.batch_job_timeout.as_secs_f64()),
            "capture_master_key_hex": config.capture_master_key_hex,
            "capture_max_window_minutes": config.capture_max_window_minutes,
            "capture_retention_hours": config.capture_retention_hours,
            "catalog_cold_start_nights": config.catalog_cold_start_nights,
            "ceiling_buffer": json_number(config.ceiling_buffer),
            "classifier_column": config.classifier_column,
            "cron_capture_expiry": config.cron_capture_expiry,
            "cron_catalog_sync": config.cron_catalog_sync,
            "cron_history_prune": config.cron_history_prune,
            "cron_latency_probes": config.cron_latency_probes,
            "cron_nightly": config.cron_nightly,
            "cron_prediction_lock": config.cron_prediction_lock,
            "cron_price_sync": config.cron_price_sync,
            "cron_weekly": config.cron_weekly,
            "database_url": config.database_url,
            "google_api_key": config.google_api_key,
            "history_retention_days": config.history_retention_days,
            "internal_addr": config.internal_addr,
            "internal_tokens": config.internal_tokens,
            "judge_pool": config.judge_pool,
            "openai_api_key": config.openai_api_key,
            "provider_limits": {
                "anthropic": config.provider_limits.anthropic,
                "google": config.provider_limits.google,
                "openai": config.provider_limits.openai,
                "xai": config.provider_limits.xai,
            },
            "public_addr": config.public_addr,
            "ranking_band_balanced": json_number(config.ranking_band_balanced),
            "ranking_band_cost": json_number(config.ranking_band_cost),
            "ranking_band_quality": json_number(config.ranking_band_quality),
            "ranking_noise_band": json_number(config.ranking_noise_band),
            "ratelimit_rpm": config.ratelimit_rpm,
            "runner_repeats": config.runner_repeats,
            "runner_timeout_seconds": json_number(config.runner_timeout.as_secs_f64()),
            "scoring_alpha": json_number(config.scoring_alpha),
            "scoring_lambda_amb": json_number(config.scoring_lambda_amb),
            "scoring_lambda_var": json_number(config.scoring_lambda_var),
            "scoring_min_nights": config.scoring_min_nights,
            "smtp_url": config.smtp_url,
            "spend_nightly_cap_usd": json_number(config.spend_nightly_cap_usd),
            "spend_weekly_cap_usd": json_number(config.spend_weekly_cap_usd),
            "xai_api_key": config.xai_api_key,
        })
    }

    fn json_number(value: f64) -> Value {
        if value == 0.0 && value.is_sign_negative() {
            json!(value)
        } else if value.fract() == 0.0 {
            json!(value as i64)
        } else {
            json!(value)
        }
    }
}
