//! `/admin/music-assistant` (api.md section 16, contract section 4): the connection to Music
//! Assistant, a connection test, Trackside's registration as a source there, and Music
//! Assistant's providers, read only. The token is write-only: no response, log or error
//! carries it.

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::StatusCode;
use trackside_auth::extract::{AdminSession, ClientIp};
use trackside_core::assistant::{
    AssistantChange, AssistantOverview, AssistantStatus, AssistantTest, ConnectionState,
    ProviderLists, RegistrationState, RegistrationView, SettingSource,
};
use trackside_core::config::Secret;

use super::{audit, require_recent_auth};
use crate::ApiState;
use crate::error::ApiResult;
use crate::extract::{Json, OptionalJson, json_with};
use crate::wire::Time;
use crate::wire::admin::{
    AssistantConnection, AssistantLanUrl, AssistantMusicProvider, AssistantPlayerProvider,
    AssistantRelays, AssistantServer, AssistantSource, AssistantTestServer, AssistantUser,
    LastError, MusicAssistant, MusicAssistantPatch, MusicAssistantProviders,
    MusicAssistantRegistration, MusicAssistantTest, MusicAssistantTestRequest,
    OverviewMusicAssistant, Problem, RegistrationKey, ServiceUser,
};

pub(super) fn admin_routes() -> Router<ApiState> {
    Router::new()
        .route("/music-assistant", get(status).patch(update).delete(remove))
        .route("/music-assistant/test", post(test))
        .route(
            "/music-assistant/registration",
            get(registration).post(register).delete(unregister),
        )
        .route("/music-assistant/providers", get(providers))
}

fn last_error(error: Option<&(trackside_core::Millis, String)>) -> Option<LastError> {
    error.map(|(at, message)| LastError {
        at: Time(*at),
        message: message.clone(),
    })
}

fn source(source: SettingSource) -> String {
    source.as_str().to_owned()
}

/// `GET /admin/music-assistant`.
pub fn status_body(status: &AssistantStatus) -> MusicAssistant {
    if !status.configured {
        return MusicAssistant {
            auto_register: None,
            configured: false,
            connection: AssistantConnection {
                last_error: None,
                since: None,
                state: ConnectionState::NotSetUp.as_str().to_owned(),
            },
            custom_ca: status.custom_ca,
            enabled: false,
            hidden_services: None,
            lan_url: None,
            relays: None,
            server: None,
            services: None,
            source: (status.url.is_some() || status.token_set).then(|| AssistantSource {
                token: source(status.token_source),
                url: source(status.url_source),
            }),
            speakers: None,
            token_set: status.token_set,
            token_set_at: status.token_set_at.map(Time),
            ui_url: None,
            url: status.url.clone(),
            user: None,
        };
    }
    MusicAssistant {
        auto_register: Some(status.auto_register),
        configured: true,
        connection: AssistantConnection {
            last_error: last_error(status.last_error.as_ref()),
            since: status.since.map(Time),
            state: status.state.as_str().to_owned(),
        },
        custom_ca: status.custom_ca,
        enabled: status.enabled,
        hidden_services: Some(status.hidden_services.clone()),
        lan_url: status.lan_url.as_ref().map(|l| AssistantLanUrl {
            checked_at: l.checked_at.map(Time),
            reachable: l.reachable,
            source: if l.configured {
                "configured"
            } else {
                "detected"
            }
            .to_owned(),
            url: l.url.clone(),
            warning: l.warning.clone(),
        }),
        relays: Some(AssistantRelays {
            active: status.relays_active,
            max: status.relays_max,
        }),
        server: status.server.as_ref().map(|s| AssistantServer {
            compatible: s.compatible,
            min_supported_schema_version: s.min_supported_schema_version,
            name: s.name.clone(),
            relay_supported: s.relay_supported,
            schema_version: s.schema_version,
            server_id: s.server_id.clone(),
            tested: s.tested,
            version: s.version.clone(),
        }),
        services: status.services,
        source: Some(AssistantSource {
            token: source(status.token_source),
            url: source(status.url_source),
        }),
        speakers: status.speakers,
        token_set: status.token_set,
        token_set_at: status.token_set_at.map(Time),
        ui_url: status.ui_url.clone(),
        url: status.url.clone(),
        user: status.user.as_ref().map(|u| AssistantUser {
            display_name: u.display_name.clone(),
            role: u.role.clone(),
            username: u.username.clone(),
        }),
    }
}

pub fn test_body(test: &AssistantTest) -> MusicAssistantTest {
    MusicAssistantTest {
        detail: test.detail.clone(),
        elapsed_ms: u64::try_from(test.elapsed.as_millis()).unwrap_or(u64::MAX),
        ok: test.ok,
        players: test.players,
        providers: test.providers,
        server: test.server.as_ref().map(|s| AssistantTestServer {
            compatible: s.compatible,
            name: s.name.clone(),
            schema_version: s.schema_version,
            tested: s.tested,
            version: s.version.clone(),
        }),
    }
}

pub fn registration_body(view: &RegistrationView) -> MusicAssistantRegistration {
    MusicAssistantRegistration {
        base_url: view.base_url.clone(),
        key: view.key.as_ref().map(|k| RegistrationKey {
            created_at: Time(k.created_at),
            id: k.id.to_string(),
            last_used_at: k.last_used_at.map(Time),
            prefix: k.prefix.clone(),
            scopes: k.scopes.iter().map(|s| (*s).to_owned()).collect(),
        }),
        last_error: last_error(view.last_error.as_ref()),
        provider_instance_id: view.provider_instance_id.clone(),
        registered_at: view.registered_at.map(Time),
        service_user: view.service_user.as_ref().map(|u| ServiceUser {
            display_name: u.display_name.clone(),
            id: u.id.to_string(),
            username: u.username.clone(),
        }),
        state: view.state.as_str().to_owned(),
        ui_url: view.ui_url.clone(),
    }
}

pub fn providers_body(lists: &ProviderLists) -> MusicAssistantProviders {
    MusicAssistantProviders {
        music: lists
            .music
            .iter()
            .map(|p| AssistantMusicProvider {
                available: p.available,
                domain: p.domain.clone(),
                hidden: p.hidden,
                instance_id: p.instance_id.clone(),
                name: p.name.clone(),
                service_id: p.service_id.as_ref().map(ToString::to_string),
                streaming: p.streaming,
                trackside: p.trackside,
                ui_url: p.ui_url.clone(),
            })
            .collect(),
        players: lists
            .players
            .iter()
            .map(|p| AssistantPlayerProvider {
                available: p.available,
                domain: p.domain.clone(),
                instance_id: p.instance_id.clone(),
                name: p.name.clone(),
                speakers: p.speakers,
                ui_url: p.ui_url.clone(),
            })
            .collect(),
        ui_url: lists.ui_url.clone(),
    }
}

/// `music_assistant` in the overview.
pub fn overview_body(overview: &AssistantOverview) -> OverviewMusicAssistant {
    OverviewMusicAssistant {
        registration: overview.registration.map(|r| r.as_str().to_owned()),
        relays: overview.relays,
        services: overview.services,
        speakers: overview.speakers,
        state: overview.state.as_str().to_owned(),
    }
}

fn problem(code: &str, title: &str, message: String, help: &str) -> Problem {
    Problem {
        severity: String::from("warning"),
        code: code.to_owned(),
        title: title.to_owned(),
        message,
        help: Some(help.to_owned()),
        target: None,
    }
}

/// The overview's problems about Music Assistant.
pub fn overview_problems(overview: &AssistantOverview) -> Vec<Problem> {
    let mut out = Vec::new();
    match overview.state {
        ConnectionState::Unreachable => out.push(problem(
            "music_assistant_unreachable",
            "Music Assistant is unreachable",
            String::from("Trackside cannot reach Music Assistant, so speakers and music services are unavailable."),
            "Check that Music Assistant runs and that its address under Speakers is right.",
        )),
        ConnectionState::AuthFailed => out.push(problem(
            "music_assistant_auth_failed",
            "Music Assistant refused the token",
            String::from("Music Assistant does not accept the saved token."),
            "Create a long-lived token in Music Assistant (Settings, Profile) and save it under Speakers.",
        )),
        ConnectionState::Incompatible => out.push(problem(
            "music_assistant_incompatible",
            "Music Assistant version unsupported",
            String::from("This Music Assistant version does not work with Trackside."),
            "Trackside needs Music Assistant 2.10 or newer.",
        )),
        _ => {}
    }
    if let Some(
        state @ (RegistrationState::Failed
        | RegistrationState::Stale
        | RegistrationState::NeedsAdmin),
    ) = overview.registration
    {
        let message = match state {
            RegistrationState::NeedsAdmin => {
                "The Music Assistant token is not an admin's, so Trackside cannot add itself as a source."
            }
            RegistrationState::Stale => "Music Assistant no longer has Trackside's source.",
            _ => "Trackside could not add itself to Music Assistant as a source.",
        };
        out.push(problem(
            "music_assistant_registration",
            "Library not on speakers",
            message.to_owned(),
            "Open Speakers in the dashboard and register again.",
        ));
    }
    out
}

async fn status(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let status = state.assistant.status().await;
    Ok(json_with(StatusCode::OK, &status_body(&status)))
}

async fn update(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<MusicAssistantPatch>,
) -> ApiResult<Response> {
    let before = state.assistant.status().await;
    let url_changes = body.url.as_ref().is_some_and(|u| {
        u.as_deref().map(|s| s.trim().trim_end_matches('/')) != before.url.as_deref()
    });
    if body.token.is_some() || (url_changes && before.token_set) {
        require_recent_auth(&state, &principal)?;
    }
    let token_changed = body.token.is_some();
    let change = AssistantChange {
        auto_register: body.auto_register,
        enabled: body.enabled,
        hidden_services: body.hidden_services,
        lan_url: body.lan_url,
        relay_max_sessions: body.relay_max_sessions,
        token: body.token.map(|t| t.map(Secret::new)),
        url: body.url,
        ca_pem: body.ca_pem,
    };
    let status = state.assistant.update(change).await?;
    audit(
        &state,
        Some(&principal),
        Some(ip),
        "music_assistant.update",
        status.url.clone(),
        None,
    )
    .await;
    if token_changed {
        audit(
            &state,
            Some(&principal),
            Some(ip),
            "music_assistant.token",
            None,
            None,
        )
        .await;
    }
    Ok(json_with(StatusCode::OK, &status_body(&status)))
}

async fn remove(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    state.assistant.remove().await?;
    audit(
        &state,
        Some(&principal),
        Some(ip),
        "music_assistant.remove",
        None,
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn test(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    OptionalJson(body): OptionalJson<MusicAssistantTestRequest>,
) -> ApiResult<Response> {
    state.auth.assistant_test(&principal)?;
    let result = state
        .assistant
        .test(body.url, body.token.map(Secret::new))
        .await;
    Ok(json_with(StatusCode::OK, &test_body(&result)))
}

async fn registration(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let view = state.assistant.registration().await;
    Ok(json_with(StatusCode::OK, &registration_body(&view)))
}

async fn register(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    let view = state.assistant.register().await?;
    audit(
        &state,
        Some(&principal),
        Some(ip),
        "music_assistant.register",
        None,
        None,
    )
    .await;
    Ok(json_with(StatusCode::ACCEPTED, &registration_body(&view)))
}

async fn unregister(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    state.assistant.unregister().await?;
    audit(
        &state,
        Some(&principal),
        Some(ip),
        "music_assistant.unregister",
        None,
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn providers(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let lists = state.assistant.providers().await?;
    Ok(json_with(StatusCode::OK, &providers_body(&lists)))
}
