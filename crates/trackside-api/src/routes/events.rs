//! `GET /api/v1/events`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{HeaderMap, HeaderValue, header};
use trackside_auth::extract::{Authed, ClientIp};
use trackside_auth::{RouteClass, Scopes};
use trackside_core::repo::libraries;

use crate::ApiState;
use crate::sse::{self, EventFilter, LibraryLookup};

const LIBRARY: u8 = Scopes::LIBRARY.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/events", get(events))
}

fn filter_for(state: &ApiState, principal: &trackside_auth::Principal) -> EventFilter {
    EventFilter {
        access: principal.access.clone(),
        device: principal.device.as_ref().map(|(key, _)| *key),
        acquisition: super::account::acquisition_feature(state, principal).is_some(),
        speakers: super::account::speakers_feature(state, principal).is_some(),
        catalog: super::account::catalog_feature(state, principal).is_some(),
        base_path: state.server.base_path.clone(),
    }
}

async fn events(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    parts: http::request::Parts,
    headers: HeaderMap,
) -> Result<Response, crate::error::ApiError> {
    let slot = state.auth.event_stream_slot(&principal)?;
    let receiver = state.events.subscribe();
    let filter = filter_for(&state, &principal);
    let parts = Arc::new(parts);
    let again = state.clone();
    let reauthenticate: sse::Reauthenticate = Arc::new(move || {
        let state = again.clone();
        let parts = Arc::clone(&parts);
        Box::pin(async move {
            let principal = state
                .auth
                .authenticate(&parts, ip, RouteClass::Api)
                .await
                .ok()
                .filter(|p| p.allows(Scopes::LIBRARY))?;
            Some(filter_for(&state, &principal))
        })
    });
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok());
    let seq = state.db.current_seq().0;
    let db = state.db.clone();
    let lookup: LibraryLookup = Arc::new(move |id| {
        let db = db.clone();
        Box::pin(async move {
            db.read(move |c| libraries::get(c, &trackside_core::Access::system(), &id))
                .await
                .ok()
                .flatten()
                .map(|l| l.key)
        })
    });
    let seq_db = state.db.clone();
    let sse = sse::stream(
        receiver,
        filter,
        reauthenticate,
        sse::hello(&state.server.server_id, seq),
        last_event_id,
        lookup,
        state.local.shutdown().clone(),
        move || seq_db.current_seq().0,
    );
    let mut response = sse.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    // The slot is held until the client goes away and the body is dropped.
    Ok(trackside_media::hold(response, slot))
}
