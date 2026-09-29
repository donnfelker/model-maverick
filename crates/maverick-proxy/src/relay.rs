//! Relay of the settled attempt (reference part 6 §8.5, §9.2, §9.3).
//!
//! The response carries `X-Maverick-Recommendation-Id` (set even when the id is empty,
//! QUIRK 1) and `X-Maverick-Model-Used` (the executed model). Then every upstream header
//! except the hop-by-hop set is ADDED, so an upstream `X-Maverick-Model-Used` gives a
//! second value (QUIRK 2). Headers that the upstream's `Connection` header names are kept.
//! The status is the upstream's.
//!
//! - **Streaming**, iff the status is 2xx and the first `Content-Type` value, trimmed of
//!   ASCII whitespace, starts with `text/event-stream` (case-sensitive, QUIRK 8): the
//!   upstream bytes pass through unchanged, chunk by chunk, with no SSE parsing. An
//!   upstream read error ends the body normally, so the client sees what arrived (QUIRK 9).
//!   Tokens are not parsed.
//! - **Buffered**, otherwise: the whole upstream body is read. A read error makes it
//!   empty, and the upstream headers still apply (QUIRK 10). A 2xx body's token counts
//!   are parsed for telemetry.
//!
//! The `done` callback runs once, when the relay has ended: after the buffered read, or
//! when the stream ends, fails, or is dropped because the client went away.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use hyper::body::{Body as HttpBody, Frame};
use maverick_core::telemetry::{UsageShape, parse_usage};

use crate::chain::Surface;
use crate::credentials::is_hop_by_hop;
use crate::envelope::{MODEL_USED_HEADER, RECOMMENDATION_ID_HEADER};

/// Token counts of the relayed attempt: `(tokens_in, tokens_out)`.
pub type Usage = (Option<i64>, Option<i64>);

/// Runs once when the relay has ended, with the token counts (buffered 2xx only).
pub type Done = Box<dyn FnOnce(Usage) + Send>;

/// The client response for the settled attempt.
pub async fn relay(
    surface: Surface,
    recommendation_id: &str,
    model_used: &str,
    upstream: reqwest::Response,
    done: Done,
) -> Response {
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    if let Some(value) = header_value(recommendation_id) {
        headers.insert(RECOMMENDATION_ID_HEADER, value);
    }
    if let Some(value) = header_value(model_used) {
        headers.insert(MODEL_USED_HEADER, value);
    }
    for (name, value) in upstream.headers() {
        if !is_hop_by_hop(name) {
            headers.append(name.clone(), value.clone());
        }
    }

    let body = if is_event_stream(status, upstream.headers()) {
        Body::new(StreamRelay {
            upstream: reqwest::Body::from(upstream),
            done: Some(done),
        })
    } else {
        let bytes = upstream.bytes().await.unwrap_or_default();
        let usage = if status.is_success() {
            parse_usage(usage_shape(surface), &bytes)
        } else {
            (None, None)
        };
        done(usage);
        Body::from(bytes)
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// QUIRK 8: 2xx and the first `Content-Type` value, ASCII-trimmed, starts with
/// `text/event-stream`, byte for byte.
fn is_event_stream(status: StatusCode, headers: &HeaderMap) -> bool {
    status.is_success()
        && headers.get(CONTENT_TYPE).is_some_and(|value| {
            value
                .as_bytes()
                .trim_ascii()
                .starts_with(b"text/event-stream")
        })
}

fn usage_shape(surface: Surface) -> UsageShape {
    match surface {
        Surface::Chat => UsageShape::Chat,
        Surface::Messages => UsageShape::Messages,
    }
}

/// A header value for a correlation header. The empty string is a valid value. A value
/// that cannot be a header (a control character) is left out.
fn header_value(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_bytes(value.as_bytes()).ok()
}

/// The streamed body: upstream data frames unchanged, trailers dropped, and an upstream
/// error treated as the end (QUIRK 9). `done` runs at the end or on drop.
struct StreamRelay {
    upstream: reqwest::Body,
    done: Option<Done>,
}

impl StreamRelay {
    fn finish(&mut self) {
        if let Some(done) = self.done.take() {
            done((None, None));
        }
    }
}

impl HttpBody for StreamRelay {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.done.is_none() {
            return Poll::Ready(None);
        }
        loop {
            match Pin::new(&mut this.upstream).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        return Poll::Ready(Some(Ok(Frame::data(data))));
                    }
                }
                Poll::Ready(Some(Err(_)) | None) => {
                    this.finish();
                    return Poll::Ready(None);
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done.is_none()
    }
}

impl Drop for StreamRelay {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_TYPE, HeaderValue::from_str(value).expect("value"));
        headers
    }

    #[test]
    fn streaming_needs_2xx_and_an_exact_case_prefix() {
        let ok = StatusCode::OK;
        assert!(is_event_stream(ok, &content_type("text/event-stream")));
        assert!(is_event_stream(
            StatusCode::CREATED,
            &content_type("text/event-stream; charset=utf-8")
        ));
        assert!(is_event_stream(ok, &content_type(" text/event-streamX")));
        assert!(!is_event_stream(ok, &content_type("Text/Event-Stream")));
        assert!(!is_event_stream(ok, &content_type("application/json")));
        assert!(!is_event_stream(ok, &HeaderMap::new()));
        assert!(!is_event_stream(
            StatusCode::BAD_REQUEST,
            &content_type("text/event-stream")
        ));
        // Only the first value counts.
        let mut two = content_type("application/json");
        two.append(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        assert!(!is_event_stream(ok, &two));
    }

    #[test]
    fn correlation_values_allow_empty() {
        assert_eq!(header_value("").map(|v| v.len()), Some(0));
        assert!(header_value("rec_1").is_some());
        assert!(header_value("bad\nvalue").is_none());
    }
}
