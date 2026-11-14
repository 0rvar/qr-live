//! HTTP routes: the static client, the SSE event stream and the reference
//! source of each step.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Router,
};
use futures_util::stream::{self, Stream, StreamExt};
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

fn snapshot_event(snapshot: &serde_json::Value) -> Event {
    Event::default()
        .event("snapshot")
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
async fn events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Deserialize)]
struct SourceQuery {
    limit: Option<usize>,
}

/// The reference input of a step, optionally truncated to `limit` bytes.
async fn source(
    State(st): State<AppState>,
    Path(index): Path<usize>,
    Query(query): Query<SourceQuery>,
) -> Response {
    let Some(step) = st.relay.steps.get(index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = st.relay.reference_dir.join(&step.input);
    let mut bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let total = bytes.len();
    if let Some(limit) = query.limit {
        if limit < bytes.len() {
            bytes.truncate(limit);
            if !step.binary {
                if let Err(e) = std::str::from_utf8(&bytes) {
                    if e.error_len().is_none() {
                        bytes.truncate(e.valid_up_to());
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
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response();
    response.headers_mut().insert(
        "x-total-size",
        HeaderValue::from_str(&total.to_string()).expect("digits are a valid header"),
    );
    response
}
