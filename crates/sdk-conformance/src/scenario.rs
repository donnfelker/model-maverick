//! The language-neutral scenario model (sdk-v0 plan §2.3).
//!
//! A scenario scripts how the mock engine and the mock provider answer. The servers are
//! dumb: they return exactly what the scenario names and record what they received, so the
//! wire shapes live in the scenario JSON (frozen by bead 8.2), not in this code. That keeps
//! the harness free of provider schema translation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One frozen conformance scenario.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Scenario {
    /// The scenario name, such as `declared_happy_path`.
    pub name: String,
    /// How the mock engine answers.
    #[serde(default)]
    pub engine: EngineScript,
    /// How the mock provider answers.
    #[serde(default)]
    pub provider: ProviderScript,
}

impl Scenario {
    /// Parse a scenario from JSON bytes.
    ///
    /// # Errors
    /// Returns the serde error when the bytes are not a valid scenario.
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// The mock engine's script.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EngineScript {
    /// Responses to `POST /v1/recommend`, consumed in order; the last one repeats. Empty
    /// means a `200` with an empty JSON object (an empty chain).
    #[serde(default)]
    pub recommend: Vec<Reply>,
    /// The status `POST /v1/telemetry` returns (default `200`). The body is always recorded
    /// so a scenario can assert its shape, or assert zero bytes when telemetry is off.
    #[serde(default)]
    pub telemetry_status: Option<u16>,
}

/// The mock provider's script.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderScript {
    /// Responses consumed in order across every provider surface; the last one repeats.
    /// Empty means a `200` with an empty JSON object.
    #[serde(default)]
    pub responses: Vec<Reply>,
}

/// One scripted reply. The `kind` field selects the behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    /// Send an immediate HTTP response. A string `body` is sent as its own bytes; any other
    /// JSON `body` is serialized as `application/json`. A `headers` entry overrides the
    /// content type.
    Respond {
        /// The HTTP status. Default `200`.
        #[serde(default = "ok_status")]
        status: u16,
        /// Extra response headers, such as `Retry-After`.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// The response body.
        #[serde(default)]
        body: Value,
    },
    /// Send a Server-Sent-Events stream: one `data:` frame per event, then close. The bytes
    /// are exact, so a scenario can assert byte-identical pass-through.
    Sse {
        /// The HTTP status. Default `200`.
        #[serde(default = "ok_status")]
        status: u16,
        /// Extra response headers.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// The event payloads; each becomes `data: <event>\n\n`.
        events: Vec<String>,
    },
    /// Wait `delay_ms`, then answer with `then`. A delay past the client's deadline is how a
    /// scenario drives a timeout or slow headers.
    Delay {
        /// The wait before answering.
        delay_ms: u64,
        /// The reply to send after the wait.
        then: Box<Reply>,
    },
}

impl Default for Reply {
    fn default() -> Self {
        Reply::Respond {
            status: ok_status(),
            headers: BTreeMap::new(),
            body: Value::Object(serde_json::Map::new()),
        }
    }
}

fn ok_status() -> u16 {
    200
}

/// Pick the reply for the call at `index` (zero based): the reply at that position, or the
/// last one when the list is shorter, or the default `200` when the list is empty.
#[must_use]
pub fn reply_at(list: &[Reply], index: usize) -> Reply {
    if list.is_empty() {
        Reply::default()
    } else {
        list[index.min(list.len() - 1)].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_gives_the_default_ok() {
        assert_eq!(reply_at(&[], 0), Reply::default());
        match Reply::default() {
            Reply::Respond { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, Value::Object(serde_json::Map::new()));
            }
            other => panic!("default is not Respond: {other:?}"),
        }
    }

    #[test]
    fn last_reply_repeats_past_the_end() {
        let list = vec![
            Reply::Respond {
                status: 200,
                headers: BTreeMap::new(),
                body: Value::from("first"),
            },
            Reply::Respond {
                status: 500,
                headers: BTreeMap::new(),
                body: Value::from("last"),
            },
        ];
        assert_eq!(reply_at(&list, 0), list[0]);
        assert_eq!(reply_at(&list, 1), list[1]);
        assert_eq!(reply_at(&list, 9), list[1], "the last reply repeats");
    }

    #[test]
    fn scenario_round_trips_through_json() {
        let json = br#"{
            "name": "declared_happy_path",
            "engine": {
                "recommend": [
                    {"kind": "respond", "status": 200,
                     "body": {"recommendation_id": "rec_1", "recommendations": []}}
                ]
            },
            "provider": {
                "responses": [
                    {"kind": "respond", "status": 429, "headers": {"Retry-After": "1"}, "body": {}},
                    {"kind": "delay", "delay_ms": 50, "then": {"kind": "respond", "body": {"ok": true}}}
                ]
            }
        }"#;
        let scenario = Scenario::from_json(json).expect("parse");
        assert_eq!(scenario.name, "declared_happy_path");
        assert_eq!(scenario.engine.recommend.len(), 1);
        assert_eq!(scenario.provider.responses.len(), 2);
        // Round-trips back to an equal value.
        let bytes = serde_json::to_vec(&scenario).expect("serialize");
        assert_eq!(Scenario::from_json(&bytes).expect("reparse"), scenario);
    }
}
