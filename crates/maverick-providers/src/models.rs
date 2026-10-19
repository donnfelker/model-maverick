//! Provider model lists for provider model discovery (spec §4.6, D51).
//!
//! Each hand-written client lists its provider's models through the provider's own
//! model-list API and keeps only the text models (the filter below). A list call sends no
//! prompt and costs nothing, so G8 does not apply. Discovery compares these lists with the
//! catalog; this crate only lists.
//!
//! A list call goes through the shared retry policy, but not through the provider call
//! metrics: `maverick_provider_calls_total` counts benchmark and classifier calls only.
//! A failed call's error carries the provider, the error class, and the HTTP status, never
//! a response body or a key (G5).

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use maverick_domain::gofmt::quote;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{ProviderError, ProviderErrorKind, ProviderResult, wrap_error};

pub type ModelListFuture<'a> =
    Pin<Box<dyn Future<Output = ProviderResult<Vec<ListedModel>>> + Send + 'a>>;

/// The most pages one list call reads. A list that is still not finished after this many
/// pages is an error, so a provider that keeps answering `has_more` cannot loop forever,
/// and a cut-off list never reaches discovery (it would read as withdrawn models).
pub const MAX_LIST_PAGES: usize = 20;

/// The id parts that name a non-text model class (spec §4.6): embeddings, moderation,
/// images, audio and speech, transcription, realtime, and video.
pub const NON_TEXT_PARTS: [&str; 11] = [
    "embedding",
    "moderation",
    "image",
    "dall",
    "audio",
    "tts",
    "whisper",
    "transcribe",
    "realtime",
    "sora",
    "video",
];

/// One text model a provider lists, after its client's filter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedModel {
    /// The provider label (`anthropic`, `openai`, `google`, `xai`).
    pub provider: String,
    /// The id as the provider lists it (Google's `models/` prefix removed).
    pub model_id: String,
    /// The provider's display name, when it gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// When the provider says the model was created, when it gives that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

/// A provider client that can list its provider's text models (spec §4.6).
pub trait ModelLister: Send + Sync {
    /// The provider label each [`ListedModel`] and each error carries.
    fn provider(&self) -> &str;

    /// Every page of the provider's model list, filtered to text models.
    fn list_models<'a>(&'a self) -> ModelListFuture<'a>;
}

/// A moving alias: an id that ends in `-latest`, or one of OpenAI's `chatgpt-` ids. The
/// model behind it changes without notice, so discovery never offers it for enrollment.
pub fn is_moving_alias(model_id: &str) -> bool {
    model_id.ends_with("-latest") || model_id.starts_with("chatgpt-")
}

/// Whether discovery keeps `model_id` (spec §4.6): not fine-tuned (`ft:`), not a moving
/// alias, and no `-`, `.`, `:`, `_`, or `/` separated part names a non-text class
/// ([`NON_TEXT_PARTS`], compared without case).
pub fn is_text_model_id(model_id: &str) -> bool {
    if model_id.starts_with("ft:") || is_moving_alias(model_id) {
        return false;
    }
    !model_id.split(['-', '.', ':', '_', '/']).any(|part| {
        NON_TEXT_PARTS
            .iter()
            .any(|class| class.eq_ignore_ascii_case(part))
    })
}

/// The error for a list that did not end within [`MAX_LIST_PAGES`] pages, or that said
/// it had more pages without saying where they start.
pub(crate) fn unfinished_list(provider: &str, reason: &str) -> ProviderError {
    ProviderError::new(provider, ProviderErrorKind::ProviderError)
        .with_source(format!("model list: {reason}"))
}

/// `url` with `query` appended, form-encoded (`/` is `%2F`, `=` is `%3D`). A base URL that
/// does not parse is an `invalid_request`: retrying cannot fix the configuration.
pub(crate) fn with_query(
    provider: &str,
    url: &str,
    query: &[(&str, &str)],
) -> Result<reqwest::Url, ProviderError> {
    let mut target = reqwest::Url::parse(url).map_err(|e| {
        ProviderError::new(provider, ProviderErrorKind::InvalidRequest)
            .with_source(format!("model list url {}: {e}", quote(url)))
    })?;
    {
        let mut pairs = target.query_pairs_mut();
        for (name, value) in query {
            pairs.append_pair(name, value);
        }
    }
    Ok(target)
}

/// Sends one list request and returns its 2xx body. A non-2xx answer is
/// `GET "<url>": <status> <reason>`, classified by status like a completion call, and
/// never carries the response body (G5).
pub(crate) async fn get_page(
    provider: &str,
    url: &str,
    request: reqwest::RequestBuilder,
) -> Result<String, ProviderError> {
    let response = request
        .send()
        .await
        .map_err(|e| wrap_error(provider, 0, e))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| wrap_error(provider, 0, e))?;
    if !status.is_success() {
        return Err(wrap_error(
            provider,
            i32::from(status.as_u16()),
            format!(
                "GET {}: {} {}",
                quote(url),
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            ),
        ));
    }
    Ok(text)
}

/// Parses a 2xx list page. Malformed JSON is a `provider_error` with status 0, so the
/// page is retried.
pub(crate) fn parse_page<T: DeserializeOwned>(
    provider: &str,
    body: &str,
) -> Result<T, ProviderError> {
    serde_json::from_str(body)
        .map_err(|e| wrap_error(provider, 0, format!("error parsing model list json: {e}")))
}

/// An RFC 3339 time, or nothing when the provider sent something else: a bad time never
/// fails the list.
pub(crate) fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// A Unix time in seconds, or nothing when it is absent, zero, or negative.
pub(crate) fn unix_time(seconds: Option<i64>) -> Option<DateTime<Utc>> {
    seconds
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
}

/// A display name, or nothing when it is absent or empty.
pub(crate) fn display_name(value: Option<String>) -> Option<String> {
    value.filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DIVERGENCE: D51. One kept and one dropped id per class, so each class has a test.
    #[test]
    fn the_filter_drops_every_non_text_class() {
        for dropped in [
            "text-embedding-3-small",
            "gemini-embedding-001",
            "omni-moderation-2024-09-26",
            "gpt-image-1",
            "grok-2-image-1212",
            "gemini-2.5-flash-image",
            "dall-e-3",
            "gpt-4o-audio-preview",
            "tts-1-hd",
            "gpt-4o-mini-tts",
            "whisper-1",
            "gpt-4o-transcribe",
            "gpt-realtime",
            "sora-2",
            "grok-imagine-video",
            "ft:gpt-4o-mini:acme::abc123",
            "gemini-flash-latest",
            "chatgpt-4o",
            "GPT-IMAGE-1",
        ] {
            assert!(!is_text_model_id(dropped), "{dropped} is not a text model");
        }
        for kept in [
            "claude-haiku-4-5-20251001",
            "gpt-5.5",
            "gpt-4o-mini-search-preview",
            "o3",
            "grok-4-0709",
            "grok-imagine",
            "gemini-2.5-pro",
            "gemma-3-27b-it",
            "imagen-gpt",
            "latest-model",
        ] {
            assert!(is_text_model_id(kept), "{kept} is a text model");
        }
    }

    /// DIVERGENCE: D51. A moving alias is a suffix or a prefix, never a part in the middle.
    #[test]
    fn moving_aliases_are_a_suffix_or_a_prefix() {
        assert!(is_moving_alias("claude-sonnet-4-5-latest"));
        assert!(is_moving_alias("chatgpt-4o-latest"));
        assert!(is_moving_alias("chatgpt-5"));
        assert!(!is_moving_alias("gpt-latest-preview"));
        assert!(!is_moving_alias("my-chatgpt-4o"));
        assert!(!is_moving_alias("claude-opus-4-1"));
    }

    #[test]
    fn a_query_is_form_encoded_and_a_bad_base_is_invalid() {
        let url = with_query(
            "google",
            "http://127.0.0.1:9/v1beta/models",
            &[("pageSize", "1000"), ("pageToken", "tok/2= x")],
        )
        .expect("parses");
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:9/v1beta/models?pageSize=1000&pageToken=tok%2F2%3D+x"
        );
        let error = with_query("google", "not a url", &[]).expect_err("bad base");
        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(
            error
                .to_string()
                .starts_with("google: invalid_request: model list url \"not a url\": ")
        );
    }

    #[test]
    fn a_listed_model_omits_absent_fields() {
        let model = ListedModel {
            provider: "google".to_owned(),
            model_id: "gemini-2.5-pro".to_owned(),
            display_name: None,
            created_at: None,
        };
        assert_eq!(
            serde_json::to_string(&model).expect("encodes"),
            r#"{"provider":"google","model_id":"gemini-2.5-pro"}"#
        );
    }

    #[test]
    fn an_unfinished_list_is_a_provider_error() {
        let error = unfinished_list("anthropic", "has_more without last_id");
        assert_eq!(error.kind(), ProviderErrorKind::ProviderError);
        assert_eq!(
            error.to_string(),
            "anthropic: provider_error: model list: has_more without last_id"
        );
    }
}
