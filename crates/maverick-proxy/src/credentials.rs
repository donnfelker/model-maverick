//! Provider credentials and upstream headers (reference part 6 §8.1 to §8.3 and §9.1;
//! D12, D30).
//!
//! # Credentials
//!
//! The caller's provider credential arrives on its surface's native slot: the chat
//! surface's `Authorization: Bearer <key>` (native provider `openai`), the messages
//! surface's `x-api-key` (native provider `anthropic`). Go's `resolveKey` then picks the key
//! for each chain entry: the caller's credential for the native provider, else the
//! provider's env key, else skip.
//!
//! Hosted mode (D12) changes two things. It loads no provider env keys, so a cross-provider
//! entry has no key and is skipped: the chain is restricted to the credential's provider,
//! and a hosted instance given only an OpenAI key never calls Anthropic. And a caller key
//! that starts with [`OPENROUTER_KEY_PREFIX`] is an OpenRouter credential: it selects
//! OpenRouter execution (bead 6.10) and is never sent to a direct provider.
//!
//! # Upstream headers
//!
//! Start from every client header, then drop the headers the client's `Connection` header
//! names, the hop-by-hop set, `Host`, `Content-Length`, `Authorization`, `X-Api-Key`, and
//! (D30) `X-Maverick-API-Key`, and (D45) `X-Maverick-Settings`. Every other header
//! keeps all its values in order.
//! `Content-Type` defaults to `application/json`. Each attempt then sets its credential:
//! chat `Authorization: Bearer <key>`; messages `X-Api-Key: <key>` and a default
//! `anthropic-version`. Credential values are marked sensitive. Nothing here logs.

use std::collections::HashSet;
use std::fmt;

use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, HeaderName, HeaderValue};

use crate::chain::Surface;
use crate::config::{Config, DeploymentMode};

/// The key prefix that marks a caller credential as an OpenRouter key (OpenRouter's
/// documented `sk-or-v1-...` format). Recognized in hosted mode only.
pub const OPENROUTER_KEY_PREFIX: &str = "sk-or-";

/// The `anthropic-version` sent when the client sent none.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
const X_MAVERICK_API_KEY: HeaderName = HeaderName::from_static("x-maverick-api-key");

/// Hop-by-hop headers (Go's static list, compared case-insensitively).
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// True for a name in Go's static hop-by-hop list. The relay drops these from upstream
/// responses too (reference part 6 §9.2).
pub(crate) fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

/// Headers never forwarded: framing, credentials, the tenant key (D30), and settings (D45).
const NEVER_FORWARDED: [&str; 6] = [
    "host",
    "content-length",
    "authorization",
    "x-api-key",
    "x-maverick-api-key",
    "x-maverick-settings",
];

/// Go `bearerToken`: `""` when the value is 7 bytes or shorter or does not start with
/// `Bearer ` (case-insensitive); otherwise everything after those 7 bytes, unmodified.
#[must_use]
pub fn bearer_token(header: &str) -> &str {
    match header.as_bytes().get(..7) {
        Some(scheme) if header.len() > 7 && scheme.eq_ignore_ascii_case(b"Bearer ") => &header[7..],
        _ => "",
    }
}

/// The surface's native provider.
#[must_use]
pub fn native_provider(surface: Surface) -> &'static str {
    match surface {
        Surface::Chat => "openai",
        Surface::Messages => "anthropic",
    }
}

/// The caller's provider credential. `Debug` never shows the key.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum CallerCredential {
    /// No usable credential on the surface's native slot.
    #[default]
    None,
    /// A credential for the surface's native provider.
    Native(String),
    /// An OpenRouter key (hosted mode only): OpenRouter execution, never a direct call.
    OpenRouter(String),
}

impl CallerCredential {
    /// Read the credential from the surface's native slot: the first `Authorization`
    /// value's bearer token on chat, the first `x-api-key` value as is on messages.
    #[must_use]
    pub fn from_headers(surface: Surface, headers: &HeaderMap, mode: DeploymentMode) -> Self {
        let first = |name: &HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        };
        let raw = match surface {
            Surface::Chat => bearer_token(first(&AUTHORIZATION)),
            Surface::Messages => first(&X_API_KEY),
        };
        if raw.is_empty() {
            Self::None
        } else if mode == DeploymentMode::Hosted && raw.starts_with(OPENROUTER_KEY_PREFIX) {
            Self::OpenRouter(raw.to_owned())
        } else {
            Self::Native(raw.to_owned())
        }
    }
}

impl fmt::Debug for CallerCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::Native(_) => "Native(<redacted>)",
            Self::OpenRouter(_) => "OpenRouter(<redacted>)",
        })
    }
}

/// The per-provider env keys. Always empty in hosted mode (D12). `Debug` never shows a key.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct EnvKeys {
    anthropic: String,
    openai: String,
    google: String,
    xai: String,
}

impl EnvKeys {
    /// The env keys the configuration loaded (none in hosted mode).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            anthropic: config.anthropic_api_key.clone(),
            openai: config.openai_api_key.clone(),
            google: config.gemini_api_key.clone(),
            xai: config.xai_api_key.clone(),
        }
    }

    /// Go `envKeyFor`: `""` for an unknown provider. Provider names are exact.
    #[must_use]
    pub fn for_provider(&self, provider: &str) -> &str {
        match provider {
            "anthropic" => &self.anthropic,
            "openai" => &self.openai,
            "google" => &self.google,
            "xai" => &self.xai,
            _ => "",
        }
    }
}

impl fmt::Debug for EnvKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |key: &str| !key.is_empty();
        f.debug_struct("EnvKeys")
            .field("anthropic", &set(&self.anthropic))
            .field("openai", &set(&self.openai))
            .field("google", &set(&self.google))
            .field("xai", &set(&self.xai))
            .finish()
    }
}

/// Go `resolveKey` for a direct attempt: the caller's native credential for the surface's
/// native provider, else the provider's env key. `None` means skip the entry. An OpenRouter
/// credential is never returned for a direct attempt.
#[must_use]
pub fn resolve_key<'a>(
    provider: &str,
    surface: Surface,
    credential: &'a CallerCredential,
    env: &'a EnvKeys,
) -> Option<&'a str> {
    if let CallerCredential::Native(key) = credential {
        if provider == native_provider(surface) {
            return Some(key);
        }
    }
    Some(env.for_provider(provider)).filter(|key| !key.is_empty())
}

/// Go `buildUpstreamHeaders` plus D30: the base header set for every upstream attempt.
#[must_use]
pub fn upstream_base_headers(client: &HeaderMap) -> HeaderMap {
    let connection_named: HashSet<HeaderName> = client
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
        .collect();
    let mut base = HeaderMap::new();
    for (name, value) in client {
        let dropped = connection_named.contains(name)
            || is_hop_by_hop(name)
            || NEVER_FORWARDED.contains(&name.as_str());
        if !dropped {
            base.append(name.clone(), value.clone());
        }
    }
    if base.get(CONTENT_TYPE).is_none_or(|value| value.is_empty()) {
        base.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    debug_assert!(!base.contains_key(X_MAVERICK_API_KEY));
    base
}

/// The headers for one direct attempt: the base set plus the surface's credential header
/// (and, on messages, the default `anthropic-version`). `None` when the key cannot be a
/// header value; the entry is then skipped.
#[must_use]
pub fn attempt_headers(surface: Surface, base: &HeaderMap, key: &str) -> Option<HeaderMap> {
    let mut headers = base.clone();
    match surface {
        Surface::Chat => {
            let mut value = HeaderValue::from_str(&format!("Bearer {key}")).ok()?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        Surface::Messages => {
            if headers
                .get(ANTHROPIC_VERSION)
                .is_none_or(|value| value.is_empty())
            {
                headers.insert(
                    ANTHROPIC_VERSION,
                    HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION),
                );
            }
            let mut value = HeaderValue::from_str(key).ok()?;
            value.set_sensitive(true);
            headers.insert(X_API_KEY, value);
        }
    }
    Some(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    #[test]
    fn credentials_come_from_the_native_slot() {
        let both = headers(&[
            ("authorization", "Bearer sk-openai"),
            ("x-api-key", "sk-ant"),
        ]);
        let self_hosted = DeploymentMode::SelfHosted;
        assert_eq!(
            CallerCredential::from_headers(Surface::Chat, &both, self_hosted),
            CallerCredential::Native("sk-openai".to_owned())
        );
        assert_eq!(
            CallerCredential::from_headers(Surface::Messages, &both, self_hosted),
            CallerCredential::Native("sk-ant".to_owned())
        );
        let basic = headers(&[("authorization", "Basic abc")]);
        assert_eq!(
            CallerCredential::from_headers(Surface::Chat, &basic, self_hosted),
            CallerCredential::None
        );
        assert_eq!(
            CallerCredential::from_headers(Surface::Messages, &HeaderMap::new(), self_hosted),
            CallerCredential::None
        );
    }

    #[test]
    fn openrouter_keys_are_recognized_only_when_hosted() {
        let key = headers(&[("authorization", "Bearer sk-or-v1-abc")]);
        assert_eq!(
            CallerCredential::from_headers(Surface::Chat, &key, DeploymentMode::Hosted),
            CallerCredential::OpenRouter("sk-or-v1-abc".to_owned())
        );
        assert_eq!(
            CallerCredential::from_headers(Surface::Chat, &key, DeploymentMode::SelfHosted),
            CallerCredential::Native("sk-or-v1-abc".to_owned())
        );
    }

    #[test]
    fn resolve_key_follows_go_and_d12() {
        let env = EnvKeys {
            anthropic: "env-ant".to_owned(),
            openai: "env-oai".to_owned(),
            google: String::new(),
            xai: "env-xai".to_owned(),
        };
        let native = CallerCredential::Native("client".to_owned());
        // The client credential wins for the native provider only.
        assert_eq!(
            resolve_key("openai", Surface::Chat, &native, &env),
            Some("client")
        );
        assert_eq!(
            resolve_key("anthropic", Surface::Chat, &native, &env),
            Some("env-ant")
        );
        assert_eq!(
            resolve_key("anthropic", Surface::Messages, &native, &env),
            Some("client")
        );
        assert_eq!(
            resolve_key("openai", Surface::Messages, &native, &env),
            Some("env-oai")
        );
        // No key: skip. Unknown providers and case differences never match.
        assert_eq!(resolve_key("google", Surface::Chat, &native, &env), None);
        assert_eq!(resolve_key("mistral", Surface::Chat, &native, &env), None);
        assert_eq!(resolve_key("OpenAI", Surface::Chat, &native, &env), None);
        assert_eq!(
            resolve_key("openai", Surface::Chat, &CallerCredential::None, &env),
            Some("env-oai")
        );
        // Hosted: no env keys, so only the credential's provider runs.
        let hosted = EnvKeys::default();
        for provider in ["anthropic", "google", "xai"] {
            assert_eq!(
                resolve_key(provider, Surface::Chat, &native, &hosted),
                None,
                "{provider}"
            );
        }
        // An OpenRouter credential is never used for a direct attempt.
        let openrouter = CallerCredential::OpenRouter("sk-or-v1-x".to_owned());
        assert_eq!(
            resolve_key("openai", Surface::Chat, &openrouter, &hosted),
            None
        );
    }

    #[test]
    fn attempt_headers_set_the_surface_credential() {
        let base = upstream_base_headers(&headers(&[("anthropic-version", "2024-01-01")]));
        let chat = attempt_headers(Surface::Chat, &base, "k1").expect("chat headers");
        assert_eq!(
            chat.get(AUTHORIZATION).map(|value| value.as_bytes()),
            Some(&b"Bearer k1"[..])
        );
        assert!(
            chat.get(AUTHORIZATION)
                .is_some_and(HeaderValue::is_sensitive)
        );
        assert!(chat.get(X_API_KEY).is_none());

        let messages = attempt_headers(Surface::Messages, &base, "k2").expect("messages headers");
        assert_eq!(
            messages.get(X_API_KEY).map(|value| value.as_bytes()),
            Some(&b"k2"[..])
        );
        assert_eq!(
            messages
                .get(ANTHROPIC_VERSION)
                .map(|value| value.as_bytes()),
            Some(&b"2024-01-01"[..])
        );
        assert!(messages.get(AUTHORIZATION).is_none());

        let defaulted =
            attempt_headers(Surface::Messages, &HeaderMap::new(), "k3").expect("headers");
        assert_eq!(
            defaulted
                .get(ANTHROPIC_VERSION)
                .map(|value| value.as_bytes()),
            Some(DEFAULT_ANTHROPIC_VERSION.as_bytes())
        );
        assert_eq!(attempt_headers(Surface::Chat, &base, "bad\nkey"), None);
    }

    #[test]
    fn debug_output_redacts_keys() {
        let shown = format!(
            "{:?} {:?}",
            CallerCredential::Native("sk-secret".to_owned()),
            EnvKeys {
                openai: "sk-env-secret".to_owned(),
                ..EnvKeys::default()
            }
        );
        assert!(!shown.contains("secret"), "{shown}");
    }
}
