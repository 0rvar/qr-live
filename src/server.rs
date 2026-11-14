//! HTTP routes: the static client, the SSE event stream, a status summary
//! and the reference source of each step.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Router,
};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, watch};

use crate::relay::{Relay, RelayEvent};

/// The browser client served at `/`, `/app.js` and `/style.css`.
#[derive(Clone, Copy)]
pub struct Assets {
    pub index_html: &'static str,
    pub app_js: &'static str,
    pub style_css: &'static str,
}

#[derive(Clone)]
struct AppState {
    relay: Arc<Relay>,
    assets: Assets,
    shutdown: watch::Receiver<bool>,
}

/// Builds the router. SSE streams end when `shutdown` becomes true, so a
/// graceful shutdown is not held up by connected browsers.
pub fn router(relay: Arc<Relay>, assets: Assets, shutdown: watch::Receiver<bool>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/events", get(events))
        .route("/api/status", get(status))
        .route("/api/source/{index}", get(source))
        .with_state(AppState {
            relay,
            assets,
            shutdown,
        })
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn index(State(st): State<AppState>) -> Response {
    asset("text/html; charset=utf-8", st.assets.index_html)
}

async fn app_js(State(st): State<AppState>) -> Response {
    asset("text/javascript; charset=utf-8", st.assets.app_js)
}

async fn style_css(State(st): State<AppState>) -> Response {
    asset("text/css; charset=utf-8", st.assets.style_css)
}

/// The relay's health: totals, the last lap and the most recent failures.
async fn status(State(st): State<AppState>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(st.relay.status()),
    )
        .into_response()
}

fn snapshot_event(snapshot: &serde_json::Value) -> Event {
    Event::default()
        .event("snapshot")
        .retry(Duration::from_millis(1000))
        .json_data(snapshot)
        .expect("snapshot serializes")
}

fn relay_event(event: &RelayEvent) -> Event {
    Event::default()
        .event(event.name())
        .json_data(event.data())
        .expect("event serializes")
}

/// Streams a `snapshot` followed by every relay event. A receiver that falls
/// behind is replaced by a fresh subscription and a new snapshot, so the
/// client never applies events older than its latest snapshot.
async fn events(State(st): State<AppState>) -> impl IntoResponse {
    let (rx, snapshot) = st.relay.subscribe();
    let relay = st.relay.clone();
    let updates = stream::unfold(
        (Some(snapshot), rx, relay),
        |(pending, mut rx, relay)| async move {
            if let Some(snapshot) = pending {
                return Some((snapshot_event(&snapshot), (None, rx, relay)));
            }
            match rx.recv().await {
                Ok(event) => Some((relay_event(&event), (None, rx, relay))),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (rx, snapshot) = relay.subscribe();
                    Some((snapshot_event(&snapshot), (None, rx, relay)))
                }
                Err(broadcast::error::RecvError::Closed) => None,
            }
        },
    );
    let mut shutdown = st.shutdown.clone();
    let stream = updates
        .take_until(async move {
            let _ = shutdown.wait_for(|stop| *stop).await;
        })
        .map(Ok::<_, Infallible>);
    (
        [("x-accel-buffering", "no")],
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
}

#[derive(Deserialize)]
struct SourceQuery {
    limit: Option<usize>,
}

/// Whether an `If-None-Match` header value matches `etag`.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    if_none_match.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// The reference input of a step, optionally truncated to `limit` bytes,
/// served from memory with an ETag of the input's SHA-1 and the limit.
async fn source(
    State(st): State<AppState>,
    Path(index): Path<usize>,
    Query(query): Query<SourceQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(step) = st.relay.steps.get(index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let full = &step.exec.input_bytes;
    let etag = match query.limit {
        Some(limit) => format!("\"{}-{limit}\"", step.exec.input_sha1),
        None => format!("\"{}-full\"", step.exec.input_sha1),
    };
    let etag_value = HeaderValue::from_str(&etag).expect("hex and digits are a valid header");
    let cache = (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));

    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| etag_matches(v, &etag));
    if not_modified {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag_value), cache]).into_response();
    }

    let mut end = full.len();
    if let Some(limit) = query.limit {
        if limit < end {
            end = limit;
            if !step.binary {
                if let Err(e) = std::str::from_utf8(&full[..end]) {
                    if e.error_len().is_none() {
                        end = e.valid_up_to();
                    }
                }
            }
        }
    }
    let content_type = if !step.binary {
        "text/plain; charset=utf-8"
    } else if step.input.ends_with(".png") {
        "image/png"
    } else {
        "application/octet-stream"
    };
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            cache,
            (header::ETAG, etag_value),
            (
                HeaderName::from_static("x-total-size"),
                HeaderValue::from(full.len()),
            ),
        ],
        full.slice(..end),
    )
        .into_response()
}
