//! `GET /live/stream` (Server-Sent Events) and `GET /live/recent` (JSON).
//!
//! Stream protocol:
//! - `event: snapshot` once on connect, `{"blocks":[...]}` with recent blocks, oldest first;
//! - `event: blocks` for every batch of new blocks, same shape;
//! - `event: lagged` when the client fell behind and batches were dropped; the
//!   client should keep going (the next batch follows) or reconnect for a fresh snapshot.
//!
//! Browsers reconnect automatically via `EventSource`; every reconnect starts with a
//! new snapshot, so no event ids or replay are needed.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
};
use tokio_stream::{
    wrappers::{errors::BroadcastStreamRecvError, BroadcastStream},
    Stream, StreamExt,
};

use crate::{error::ApiError, live::LiveHub, AppState};

fn hub(state: &AppState) -> Result<&std::sync::Arc<LiveHub>, ApiError> {
    state.live.as_ref().ok_or(ApiError::Unavailable(
        "live feed requires a karlsend connection",
    ))
}

pub async fn stream(State(state): State<AppState>) -> Result<Response, ApiError> {
    let hub = hub(&state)?;
    if hub.clients() >= hub.max_clients() {
        return Err(ApiError::Unavailable("too many live clients, retry later"));
    }
    // Subscribe before taking the snapshot so no batch falls between the two.
    let rx = hub.subscribe();
    let snapshot = hub.snapshot_json();

    let first = tokio_stream::once(Ok::<_, Infallible>(
        Event::default().event("snapshot").data(snapshot),
    ));
    let updates = BroadcastStream::new(rx).map(|item| {
        Ok::<_, Infallible>(match item {
            Ok(json) => Event::default().event("blocks").data(&*json),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                Event::default().event("lagged").data(n.to_string())
            }
        })
    });
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(first.chain(updates));

    let mut res = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    let h = res.headers_mut();
    // nginx must not buffer the stream; caches must not store it.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    Ok(res)
}

/// Recent blocks as plain JSON, for clients that cannot use SSE.
pub async fn recent(State(state): State<AppState>) -> Result<Response, ApiError> {
    let hub = hub(&state)?;
    let mut res = hub.snapshot_json().into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        "x-live-connected",
        HeaderValue::from_static(if hub.is_connected() { "true" } else { "false" }),
    );
    Ok(res)
}
