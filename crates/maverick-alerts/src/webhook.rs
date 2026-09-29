//! The HMAC-signed operator webhook (reference Part 3 §4.3).

use hmac::{Hmac, KeyInit, Mac};
use maverick_domain::gojson;
use maverick_store::alerts::Alert;
use sha2::Sha256;

use crate::wire::{compact_escaped, hex, rfc3339_nano};

/// The signature header (exact name).
pub const SIGNATURE_HEADER: &str = "X-Maverick-Signature";

/// How many response bytes are read and discarded.
const DRAIN_LIMIT: usize = 4096;

/// A configured webhook destination.
#[derive(Clone)]
pub struct Webhook {
    url: reqwest::Url,
    secret: Vec<u8>,
    client: reqwest::Client,
}

impl std::fmt::Debug for Webhook {
    /// Never prints the secret (G5).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook")
            .field("url", &self.url.as_str())
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Webhook {
    /// Validates the URL (http or https) and requires a secret.
    pub fn new(url: &str, secret: &str) -> Result<Self, String> {
        let url = reqwest::Url::parse(url).map_err(|err| format!("invalid webhook url: {err}"))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(format!(
                "webhook url scheme {:?}, want http or https",
                url.scheme()
            ));
        }
        if secret.is_empty() {
            return Err(
                "MAVERICK_ALERTS_WEBHOOK_SECRET is required when the webhook URL is set".to_owned(),
            );
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| format!("webhook client: {err}"))?;
        Ok(Self {
            url,
            secret: secret.as_bytes().to_vec(),
            client,
        })
    }

    /// Posts the signed body. Any 2xx is success.
    pub(crate) async fn deliver(&self, alert: &Alert) -> Result<(), String> {
        let body = body(alert);
        let response = self
            .client
            .post(self.url.clone())
            .header("Content-Type", "application/json")
            .header(SIGNATURE_HEADER, signature(&self.secret, body.as_bytes()))
            .body(body)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        let status = response.status();
        drain(response).await;
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("webhook responded {}", status.as_u16()))
        }
    }
}

/// Reads and discards up to [`DRAIN_LIMIT`] bytes of the response body.
async fn drain(mut response: reqwest::Response) {
    let mut read = 0;
    while read < DRAIN_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => read += chunk.len(),
            _ => break,
        }
    }
}

/// The request body: Go `json.Marshal` of `{id, kind, severity, subject, message,
/// created_at}` in that order. Strings are HTML-escaped (QUIRK A2), `subject` is the
/// stored jsonb text compacted, and `created_at` is RFC 3339 with trimmed nanoseconds.
pub fn body(alert: &Alert) -> String {
    let mut out = String::with_capacity(256 + alert.message.len());
    let field = |out: &mut String, name: &str, value: &str| {
        gojson::write_string(out, name);
        out.push(':');
        gojson::write_string(out, value);
    };
    out.push('{');
    field(&mut out, "id", &alert.id);
    out.push(',');
    field(&mut out, "kind", &alert.kind);
    out.push(',');
    field(&mut out, "severity", &alert.severity);
    out.push(',');
    gojson::write_string(&mut out, "subject");
    out.push(':');
    out.push_str(&compact_escaped(alert.subject.get()));
    out.push(',');
    field(&mut out, "message", &alert.message);
    out.push(',');
    field(&mut out, "created_at", &rfc3339_nano(alert.created_at));
    out.push('}');
    out
}

/// `sha256=` and the lowercase hex HMAC-SHA256 of the exact body bytes.
pub fn signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize().into_bytes()))
}
