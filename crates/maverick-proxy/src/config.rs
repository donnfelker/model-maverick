//! Proxy configuration, read once at startup (reference part 6 §2; D12, D13, D38).
//!
//! Every optional variable is used only when it is present AND non-empty; otherwise the
//! default applies. URLs are not parsed at startup.
//!
//! Rust adds a deployment mode, fixed per deployment by [`MODE_VAR`]:
//!
//! - `self-hosted` (the default, Go behavior): one configured `MAVERICK_API_KEY` for every
//!   engine call, and the provider env keys are loaded as a credential fallback.
//! - `hosted` (MM's shared instance): the tenant's key arrives per request in
//!   `X-Maverick-API-Key` (D13), so `MAVERICK_API_KEY` is not read; and the provider env
//!   keys are never read, not even looked up (D12).
//!
//! The fail-open ladder (D46) reads [`LKG_MAX_AGE_VAR`], the per-surface default models,
//! and whether those defaults are verified zero retention.

use std::fmt;
use std::time::Duration;

/// The deployment mode variable: `self-hosted` (default) or `hosted`.
pub const MODE_VAR: &str = "MAVERICK_PROXY_MODE";

/// The body cap variable (D38).
pub const MAX_BODY_BYTES_VAR: &str = "MAVERICK_PROXY_MAX_BODY_BYTES";

/// The default body cap: 32 MiB (D38).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// The per-tenant rate limit variable, requests per minute (D28).
pub const RATELIMIT_RPM_VAR: &str = "MAVERICK_PROXY_RATELIMIT_RPM";

/// The default per-tenant rate limit: 600 requests per minute (D28; owner decision
/// 2026-10-24, until v1.1 per-tenant limits). A routed request also counts against the
/// engine's own per-key limit.
pub const DEFAULT_RATELIMIT_RPM: u32 = 600;

/// The last good list's maximum age, a Go duration such as `24h` (D46).
pub const LKG_MAX_AGE_VAR: &str = "MAVERICK_PROXY_LKG_MAX_AGE";

/// The default maximum age of a last good list: 24 h (D46).
pub const DEFAULT_LKG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The Messages surface's fail-open default model, unset by default (D46).
pub const DEFAULT_MODEL_MESSAGES_VAR: &str = "MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES";

/// The chat-completions surface's fail-open default model, unset by default (D46).
pub const DEFAULT_MODEL_CHAT_VAR: &str = "MAVERICK_PROXY_DEFAULT_MODEL_CHAT";

/// `true` marks both default models as verified zero retention, so a zero-retention tenant
/// may fall open to them (D46). Go `strconv.ParseBool` values; default `false`.
pub const DEFAULT_MODEL_ZERO_RETENTION_VAR: &str = "MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION";

/// The error when self-hosted mode has no key (exact Go text).
pub const MISSING_API_KEY: &str = "MAVERICK_API_KEY: required but not set";

/// How this proxy instance is deployed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentMode {
    /// One configured tenant key; provider env keys may be used (Go behavior).
    SelfHosted,
    /// MM's shared instance: per-request tenant keys, no provider env keys.
    Hosted,
}

impl DeploymentMode {
    /// The value of [`MODE_VAR`] for this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfHosted => "self-hosted",
            Self::Hosted => "hosted",
        }
    }
}

/// The loaded configuration. `Debug` output never shows a key.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// [`MODE_VAR`].
    pub mode: DeploymentMode,
    /// `MAVERICK_ENGINE_URL`, one trailing `/` stripped.
    pub engine_url: String,
    /// `MAVERICK_API_KEY`. Empty in hosted mode, where it is not read.
    pub api_key: String,
    /// `MAVERICK_PROXY_ADDR`, Go form (`:8090` listens on every interface).
    pub addr: String,
    /// `MAVERICK_PROXY_TELEMETRY`: false iff the value is `off` in any case.
    pub telemetry: bool,
    /// `MAVERICK_PROXY_OTLP_ENDPOINT`, one trailing `/` stripped; empty means no OTel.
    pub otlp_endpoint: String,
    /// `MAVERICK_PROXY_OTLP_SERVICE_NAME`.
    pub otlp_service_name: String,
    /// `MAVERICK_UPSTREAM_OPENAI`, one trailing `/` stripped.
    pub upstream_openai: String,
    /// `MAVERICK_UPSTREAM_ANTHROPIC`, one trailing `/` stripped.
    pub upstream_anthropic: String,
    /// `MAVERICK_UPSTREAM_GOOGLE`, one trailing `/` stripped.
    pub upstream_google: String,
    /// `MAVERICK_UPSTREAM_XAI`, one trailing `/` stripped.
    pub upstream_xai: String,
    /// `MAVERICK_UPSTREAM_OPENROUTER`, one trailing `/` stripped: the OpenAI-compatible
    /// base of OpenRouter execution (hosted mode with an `sk-or-` key; D15).
    pub upstream_openrouter: String,
    /// `ANTHROPIC_API_KEY`. Always empty in hosted mode.
    pub anthropic_api_key: String,
    /// `OPENAI_API_KEY`. Always empty in hosted mode.
    pub openai_api_key: String,
    /// `GEMINI_API_KEY`. Always empty in hosted mode.
    pub gemini_api_key: String,
    /// `XAI_API_KEY`. Always empty in hosted mode.
    pub xai_api_key: String,
    /// [`MAX_BODY_BYTES_VAR`] (D38).
    pub max_body_bytes: u64,
    /// [`RATELIMIT_RPM_VAR`] (D28).
    pub ratelimit_rpm: u32,
    /// [`LKG_MAX_AGE_VAR`] (D46).
    pub lkg_max_age: Duration,
    /// [`DEFAULT_MODEL_MESSAGES_VAR`] (D46).
    pub default_model_messages: Option<String>,
    /// [`DEFAULT_MODEL_CHAT_VAR`] (D46).
    pub default_model_chat: Option<String>,
    /// [`DEFAULT_MODEL_ZERO_RETENTION_VAR`] (D46).
    pub default_model_zero_retention: bool,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = |value: &str| {
            if value.is_empty() {
                "<unset>"
            } else {
                "<redacted>"
            }
        };
        f.debug_struct("Config")
            .field("mode", &self.mode)
            .field("engine_url", &self.engine_url)
            .field("api_key", &key(&self.api_key))
            .field("addr", &self.addr)
            .field("telemetry", &self.telemetry)
            .field("otlp_endpoint", &self.otlp_endpoint)
            .field("otlp_service_name", &self.otlp_service_name)
            .field("upstream_openai", &self.upstream_openai)
            .field("upstream_anthropic", &self.upstream_anthropic)
            .field("upstream_google", &self.upstream_google)
            .field("upstream_xai", &self.upstream_xai)
            .field("upstream_openrouter", &self.upstream_openrouter)
            .field("anthropic_api_key", &key(&self.anthropic_api_key))
            .field("openai_api_key", &key(&self.openai_api_key))
            .field("gemini_api_key", &key(&self.gemini_api_key))
            .field("xai_api_key", &key(&self.xai_api_key))
            .field("max_body_bytes", &self.max_body_bytes)
            .field("ratelimit_rpm", &self.ratelimit_rpm)
            .field("lkg_max_age", &self.lkg_max_age)
            .field("default_model_messages", &self.default_model_messages)
            .field("default_model_chat", &self.default_model_chat)
            .field(
                "default_model_zero_retention",
                &self.default_model_zero_retention,
            )
            .finish()
    }
}

/// A configuration the proxy cannot start with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Self-hosted mode needs `MAVERICK_API_KEY`.
    #[error("MAVERICK_API_KEY: required but not set")]
    MissingApiKey,
    /// [`MODE_VAR`] is neither `hosted` nor `self-hosted`.
    #[error("MAVERICK_PROXY_MODE: must be \"hosted\" or \"self-hosted\", got {0:?}")]
    InvalidMode(String),
    /// [`MAX_BODY_BYTES_VAR`] is not a positive decimal integer.
    #[error("MAVERICK_PROXY_MAX_BODY_BYTES: must be a positive integer, got {0:?}")]
    InvalidMaxBodyBytes(String),
    /// [`RATELIMIT_RPM_VAR`] is not a positive decimal integer that fits in 32 bits.
    #[error("MAVERICK_PROXY_RATELIMIT_RPM: must be a positive integer, got {0:?}")]
    InvalidRateLimitRpm(String),
    /// [`LKG_MAX_AGE_VAR`] is not a positive Go duration.
    #[error("MAVERICK_PROXY_LKG_MAX_AGE: must be a positive duration such as 24h, got {0:?}")]
    InvalidLkgMaxAge(String),
    /// A default model variable names a `maverick/*` route: the ladder calls it directly.
    #[error("{var}: must be a provider model id, not a maverick/* route, got {value:?}")]
    InvalidDefaultModel {
        /// The variable.
        var: &'static str,
        /// Its value.
        value: String,
    },
    /// [`DEFAULT_MODEL_ZERO_RETENTION_VAR`] is not a Go boolean.
    #[error("MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION: must be true or false, got {0:?}")]
    InvalidDefaultModelZeroRetention(String),
}

/// Load the configuration through `lookup` (the process environment in production).
pub fn load_config(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let get = |name: &str| lookup(name).filter(|value| !value.is_empty());
    let or = |name: &str, default: &str| get(name).unwrap_or_else(|| default.to_owned());
    let url = |name: &str, default: &str| strip_one_slash(or(name, default));

    let mode = match get(MODE_VAR).as_deref() {
        None | Some("self-hosted") => DeploymentMode::SelfHosted,
        Some("hosted") => DeploymentMode::Hosted,
        Some(other) => return Err(ConfigError::InvalidMode(other.to_owned())),
    };
    let api_key = match mode {
        DeploymentMode::SelfHosted => get("MAVERICK_API_KEY").ok_or(ConfigError::MissingApiKey)?,
        DeploymentMode::Hosted => String::new(),
    };
    // D12: a hosted instance never reads a provider key from its environment.
    let provider_key = |name: &str| match mode {
        DeploymentMode::SelfHosted => get(name).unwrap_or_default(),
        DeploymentMode::Hosted => String::new(),
    };
    let max_body_bytes = match get(MAX_BODY_BYTES_VAR) {
        None => DEFAULT_MAX_BODY_BYTES,
        Some(value) => parse_positive(&value).ok_or(ConfigError::InvalidMaxBodyBytes(value))?,
    };
    let ratelimit_rpm = match get(RATELIMIT_RPM_VAR) {
        None => DEFAULT_RATELIMIT_RPM,
        Some(value) => parse_positive(&value)
            .and_then(|rpm| u32::try_from(rpm).ok())
            .ok_or(ConfigError::InvalidRateLimitRpm(value))?,
    };
    let lkg_max_age = match get(LKG_MAX_AGE_VAR) {
        None => DEFAULT_LKG_MAX_AGE,
        Some(value) => parse_duration(&value).ok_or(ConfigError::InvalidLkgMaxAge(value))?,
    };
    let default_model = |var: &'static str| match get(var) {
        Some(value) if value.starts_with("maverick/") => {
            Err(ConfigError::InvalidDefaultModel { var, value })
        }
        other => Ok(other),
    };
    let default_model_messages = default_model(DEFAULT_MODEL_MESSAGES_VAR)?;
    let default_model_chat = default_model(DEFAULT_MODEL_CHAT_VAR)?;
    let default_model_zero_retention = match get(DEFAULT_MODEL_ZERO_RETENTION_VAR) {
        None => false,
        Some(value) => {
            parse_bool(&value).ok_or(ConfigError::InvalidDefaultModelZeroRetention(value))?
        }
    };

    Ok(Config {
        mode,
        engine_url: url("MAVERICK_ENGINE_URL", "http://localhost:8080"),
        api_key,
        addr: or("MAVERICK_PROXY_ADDR", ":8090"),
        telemetry: !get("MAVERICK_PROXY_TELEMETRY")
            .is_some_and(|value| value.eq_ignore_ascii_case("off")),
        otlp_endpoint: url("MAVERICK_PROXY_OTLP_ENDPOINT", ""),
        otlp_service_name: or("MAVERICK_PROXY_OTLP_SERVICE_NAME", "maverick-proxy"),
        upstream_openai: url("MAVERICK_UPSTREAM_OPENAI", "https://api.openai.com"),
        upstream_anthropic: url("MAVERICK_UPSTREAM_ANTHROPIC", "https://api.anthropic.com"),
        upstream_google: url(
            "MAVERICK_UPSTREAM_GOOGLE",
            "https://generativelanguage.googleapis.com/v1beta/openai",
        ),
        upstream_xai: url("MAVERICK_UPSTREAM_XAI", "https://api.x.ai"),
        upstream_openrouter: url(
            "MAVERICK_UPSTREAM_OPENROUTER",
            "https://openrouter.ai/api/v1",
        ),
        anthropic_api_key: provider_key("ANTHROPIC_API_KEY"),
        openai_api_key: provider_key("OPENAI_API_KEY"),
        gemini_api_key: provider_key("GEMINI_API_KEY"),
        xai_api_key: provider_key("XAI_API_KEY"),
        max_body_bytes,
        ratelimit_rpm,
        lkg_max_age,
        default_model_messages,
        default_model_chat,
        default_model_zero_retention,
    })
}

/// Strip ONE trailing `/` (Go: `http://o//` becomes `http://o/`).
fn strip_one_slash(value: String) -> String {
    match value.strip_suffix('/') {
        Some(stripped) => stripped.to_owned(),
        None => value,
    }
}

/// ASCII digits only (no sign), greater than zero.
fn parse_positive(value: &str) -> Option<u64> {
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|parsed| *parsed > 0)
}

/// A positive Go duration (`time.ParseDuration`): decimal numbers, each with a unit of
/// `ns`, `us` (or `µs`), `ms`, `s`, `m`, or `h`, as in `24h` or `1h30m`. No sign, and the
/// total must be more than zero.
fn parse_duration(value: &str) -> Option<Duration> {
    let mut rest = value;
    let mut seconds = 0.0_f64;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, scale) = [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("\u{b5}s", 1e-6),
            ("\u{3bc}s", 1e-6),
            ("ms", 1e-3),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        rest = &rest[unit.len()..];
        seconds += number * scale;
    }
    (seconds.is_finite() && seconds > 0.0)
        .then(|| Duration::try_from_secs_f64(seconds).ok())
        .flatten()
}

/// Go `strconv.ParseBool`.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// The socket address for a Go-style listen address: `:8090` means every IPv4 interface.
#[must_use]
pub fn listen_addr(addr: &str) -> String {
    if addr.starts_with(':') {
        format!("0.0.0.0{addr}")
    } else {
        addr.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_and_number_helpers() {
        assert_eq!(strip_one_slash("http://o//".to_owned()), "http://o/");
        assert_eq!(strip_one_slash("http://o".to_owned()), "http://o");
        assert_eq!(strip_one_slash(String::new()), "");
        assert_eq!(parse_positive("33554432"), Some(33_554_432));
        for bad in [
            "0",
            "+5",
            "-5",
            "5 ",
            " 5",
            "1e6",
            "x",
            "99999999999999999999",
        ] {
            assert_eq!(parse_positive(bad), None, "{bad:?}");
        }
        assert_eq!(listen_addr(":8090"), "0.0.0.0:8090");
        assert_eq!(listen_addr("127.0.0.1:1"), "127.0.0.1:1");
    }

    #[test]
    fn rate_limit_setting() {
        let load = |rpm: Option<&str>| {
            load_config(|name| match name {
                "MAVERICK_API_KEY" => Some("mk_live_x".to_owned()),
                RATELIMIT_RPM_VAR => rpm.map(str::to_owned),
                _ => None,
            })
        };
        assert_eq!(
            load(None).map(|c| c.ratelimit_rpm),
            Ok(DEFAULT_RATELIMIT_RPM)
        );
        assert_eq!(DEFAULT_RATELIMIT_RPM, 600);
        assert_eq!(load(Some("")).map(|c| c.ratelimit_rpm), Ok(600));
        assert_eq!(load(Some("1")).map(|c| c.ratelimit_rpm), Ok(1));
        assert_eq!(
            load(Some("4294967295")).map(|c| c.ratelimit_rpm),
            Ok(u32::MAX)
        );
        for bad in ["0", "-1", "4294967296", "60/min", " 60"] {
            assert_eq!(
                load(Some(bad)).map(|c| c.ratelimit_rpm),
                Err(ConfigError::InvalidRateLimitRpm(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn duration_and_bool_helpers() {
        assert_eq!(parse_duration("24h"), Some(DEFAULT_LKG_MAX_AGE));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("1us"), Some(Duration::from_micros(1)));
        assert_eq!(parse_duration("1\u{b5}s"), Some(Duration::from_micros(1)));
        assert_eq!(parse_duration("3ns"), Some(Duration::from_nanos(3)));
        for bad in [
            "", "0", "0s", "24", "h", "-1h", "+1h", "1d", "1h ", " 1h", "1.2.3s",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
        for (text, want) in [("true", true), ("1", true), ("T", true), ("False", false)] {
            assert_eq!(parse_bool(text), Some(want), "{text:?}");
        }
        for bad in ["yes", "on", "tRUE", ""] {
            assert_eq!(parse_bool(bad), None, "{bad:?}");
        }
    }

    /// D46: the fail-open settings, their defaults, and their startup errors.
    #[test]
    fn fail_open_settings() {
        let load = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            load_config(move |name| {
                if name == "MAVERICK_API_KEY" {
                    return Some("mk_live_x".to_owned());
                }
                vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
            })
        };
        let defaults = load(&[]).expect("defaults");
        assert_eq!(defaults.lkg_max_age, DEFAULT_LKG_MAX_AGE);
        assert_eq!(defaults.default_model_messages, None);
        assert_eq!(defaults.default_model_chat, None);
        assert!(!defaults.default_model_zero_retention);

        let set = load(&[
            (LKG_MAX_AGE_VAR, "90m"),
            (DEFAULT_MODEL_MESSAGES_VAR, "claude-haiku-4-5"),
            (DEFAULT_MODEL_CHAT_VAR, "openai/gpt-5.5"),
            (DEFAULT_MODEL_ZERO_RETENTION_VAR, "true"),
        ])
        .expect("set");
        assert_eq!(set.lkg_max_age, Duration::from_secs(5400));
        assert_eq!(
            set.default_model_messages.as_deref(),
            Some("claude-haiku-4-5")
        );
        assert_eq!(set.default_model_chat.as_deref(), Some("openai/gpt-5.5"));
        assert!(set.default_model_zero_retention);

        assert_eq!(
            load(&[(LKG_MAX_AGE_VAR, "0s")]),
            Err(ConfigError::InvalidLkgMaxAge("0s".to_owned()))
        );
        assert_eq!(
            load(&[(DEFAULT_MODEL_CHAT_VAR, "maverick/auto")]),
            Err(ConfigError::InvalidDefaultModel {
                var: DEFAULT_MODEL_CHAT_VAR,
                value: "maverick/auto".to_owned()
            })
        );
        assert_eq!(
            load(&[(DEFAULT_MODEL_ZERO_RETENTION_VAR, "yes")]),
            Err(ConfigError::InvalidDefaultModelZeroRetention(
                "yes".to_owned()
            ))
        );
        assert_eq!(
            ConfigError::InvalidDefaultModel {
                var: DEFAULT_MODEL_MESSAGES_VAR,
                value: "maverick/x".to_owned()
            }
            .to_string(),
            "MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES: must be a provider model id, not a \
             maverick/* route, got \"maverick/x\""
        );
    }

    #[test]
    fn debug_redacts_keys() {
        let config = load_config(|name| {
            (name == "MAVERICK_API_KEY" || name == "OPENAI_API_KEY").then(|| "sk-secret".to_owned())
        })
        .expect("config");
        let shown = format!("{config:?}");
        assert!(!shown.contains("sk-secret"), "{shown}");
        assert!(shown.contains("api_key: \"<redacted>\""), "{shown}");
        assert!(shown.contains("gemini_api_key: \"<unset>\""), "{shown}");
    }
}
