//! `GET /api/v1/events`: server-sent events filtered by who may see them (api.md section 13).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;
use trackside_core::{Access, Audience, DeviceKey, LibraryKey, ServerEvent};

use crate::wire::events::{
    Changed, Hello, LibraryEvent, ScanEvent, ServerUrlsEvent, SessionEvent, UploadEvent,
};

/// Who is listening, and what they may hear.
#[derive(Clone, Debug)]
pub struct EventFilter {
    pub access: Access,
    pub device: Option<DeviceKey>,
    /// Whether acquisition events reach this caller (the feature is available to them).
    pub acquisition: bool,
    /// Prefixed in front of instant-play URLs.
    pub base_path: String,
}

/// What a bus event becomes for one listener.
#[derive(Clone, Debug, PartialEq)]
pub enum Outgoing {
    Send {
        event: &'static str,
        data: String,
    },
    /// A library event whose visibility needs the library's key.
    Library {
        library: trackside_core::LibraryId,
        available: bool,
    },
    /// The credential was revoked: send `session` and end.
    Revoked,
    /// The listener's role, permissions or grants changed: read them again.
    AccessChanged,
    Skip,
}

fn audience_matches(audience: &Audience, access: &Access) -> bool {
    match audience {
        Audience::Everyone { library: None } => true,
        Audience::Everyone {
            library: Some(library),
        } => access.can_see(*library),
        Audience::User(user) => *user == access.user,
        Audience::Admins => access.is_admin(),
    }
}

fn data<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("{}"))
}

/// Maps one bus event for one listener. Pure, so it is tested without a stream.
pub fn visible_event(event: &ServerEvent, filter: &EventFilter) -> Outgoing {
    let access = &filter.access;
    match event {
        ServerEvent::Changed { seq, audiences } => {
            if audiences.iter().any(|a| audience_matches(a, access)) {
                Outgoing::Send {
                    event: "changed",
                    data: data(&Changed { seq: seq.0 }),
                }
            } else {
                Outgoing::Skip
            }
        }
        ServerEvent::LibraryState { library, available } => Outgoing::Library {
            library: library.clone(),
            available: *available,
        },
        ServerEvent::Scan(progress) if access.is_admin() => Outgoing::Send {
            event: "scan",
            data: data(&ScanEvent {
                library_id: progress.library.to_string(),
                state: progress.state.as_str().to_owned(),
                files_seen: progress.counts.files_seen,
                added: progress.counts.added,
                updated: progress.counts.updated,
                missing: progress.counts.missing,
            }),
        },
        ServerEvent::Acquisition { user, view }
            if filter.acquisition && (*user == access.user || access.is_admin()) =>
        {
            Outgoing::Send {
                event: "acquisition",
                data: data(&crate::map::acquisition(view, &filter.base_path)),
            }
        }
        ServerEvent::Upload {
            user,
            import,
            state,
            song,
        } if *user == access.user => Outgoing::Send {
            event: "upload",
            data: data(&UploadEvent {
                id: import.to_string(),
                state: state.as_str().to_owned(),
                song_id: song.as_ref().map(ToString::to_string),
            }),
        },
        ServerEvent::CredentialRevoked { user, device }
            if *user == access.user && (device.is_none() || *device == filter.device) =>
        {
            Outgoing::Revoked
        }
        ServerEvent::AccessChanged { user } if *user == access.user => Outgoing::AccessChanged,
        ServerEvent::ServerUrls { version } => Outgoing::Send {
            event: "server_urls",
            data: data(&ServerUrlsEvent {
                version: version.clone(),
            }),
        },
        _ => Outgoing::Skip,
    }
}

/// Resolves a library id to the key access is checked by.
pub type LibraryLookup = Arc<
    dyn Fn(
            trackside_core::LibraryId,
        ) -> futures_util::future::BoxFuture<'static, Option<LibraryKey>>
        + Send
        + Sync,
>;

/// Reads the listener's access again, or `None` when its credential no longer works.
pub type Reauthenticate =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<EventFilter>> + Send + Sync>;

/// The stream for one listener: `hello`, then `changed` when the client's `Last-Event-ID` is
/// behind, then filtered events with `resync` after a lag or a change to what the listener
/// may see, keep-alive comments every 20 s.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one of the stream's inputs"
)]
pub fn stream(
    mut receiver: tokio::sync::broadcast::Receiver<Arc<ServerEvent>>,
    filter: EventFilter,
    reauthenticate: Reauthenticate,
    hello: Hello,
    last_event_id: Option<i64>,
    lookup: LibraryLookup,
    shutdown: tokio_util::sync::CancellationToken,
    current_seq: impl Fn() -> i64 + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream(move |tx| async move {
        let mut filter = filter;
        let seq = hello.seq;
        let mut latest = seq;
        let _ = tx.send(event("hello", &data(&hello), seq)).await;
        if let Some(last) = last_event_id
            && last != seq
        {
            let _ = tx
                .send(event("changed", &data(&Changed { seq }), seq))
                .await;
        }
        loop {
            let next = tokio::select! {
                () = shutdown.cancelled() => return,
                () = tx.closed() => return,
                next = receiver.recv() => next,
            };
            let outgoing = match next {
                Ok(event) => {
                    if let ServerEvent::Changed { seq, .. } = &*event {
                        latest = latest.max(seq.0);
                    }
                    visible_event(&event, &filter)
                }
                Err(RecvError::Lagged(_)) => Outgoing::Send {
                    event: "resync",
                    data: String::from("{}"),
                },
                Err(RecvError::Closed) => return,
            };
            let id = latest.max(current_seq());
            let sent = match outgoing {
                Outgoing::Skip => continue,
                Outgoing::Send { event: name, data } => tx.send(event(name, &data, id)).await,
                Outgoing::Library { library, available } => {
                    let Some(key) = lookup(library.clone()).await else {
                        continue;
                    };
                    if !filter.access.can_see(key) {
                        continue;
                    }
                    let body = LibraryEvent {
                        id: library.to_string(),
                        state: crate::map::library_state(available),
                    };
                    tx.send(event("library", &data(&body), id)).await
                }
                Outgoing::AccessChanged => match reauthenticate().await {
                    Some(fresh) => {
                        filter = fresh;
                        tx.send(event("resync", "{}", id)).await
                    }
                    None => {
                        send_revoked(&tx, id).await;
                        return;
                    }
                },
                Outgoing::Revoked => {
                    send_revoked(&tx, id).await;
                    return;
                }
            };
            if sent.is_err() {
                return;
            }
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("keep-alive"),
    )
}

async fn send_revoked(tx: &tokio::sync::mpsc::Sender<Event>, id: i64) {
    let body = SessionEvent {
        reason: String::from("revoked"),
    };
    let _ = tx.send(event("session", &data(&body), id)).await;
}

fn event(name: &str, data: &str, id: i64) -> Event {
    Event::default().event(name).data(data).id(id.to_string())
}

/// A stream fed by a task through a small channel, so the producer can await.
fn async_stream<F, Fut>(produce: F) -> impl Stream<Item = Result<Event, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Event>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(produce(tx));
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    })
}

/// The `hello` for a listener.
pub fn hello(server_id: &uuid::Uuid, seq: i64) -> Hello {
    Hello {
        server_id: server_id.to_string(),
        seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::control::{AcquisitionState, AcquisitionView};
    use trackside_core::{AcquisitionId, LibrarySet, Millis, Role, Seq, UserKey, random_id};

    fn member(user: i64, libraries: &[i64]) -> EventFilter {
        EventFilter {
            access: Access {
                user: UserKey(user),
                role: Role::Member,
                libraries: LibrarySet::Only(libraries.iter().map(|k| LibraryKey(*k)).collect()),
                sync_epoch: 1,
            },
            device: Some(DeviceKey(9)),
            acquisition: false,
            base_path: String::new(),
        }
    }

    fn changed(audiences: Vec<Audience>) -> ServerEvent {
        ServerEvent::Changed {
            seq: Seq(42),
            audiences,
        }
    }

    #[test]
    fn changed_reaches_only_its_audiences() {
        let filter = member(1, &[3]);
        let visible = |audiences| visible_event(&changed(audiences), &filter);
        assert!(matches!(
            visible(vec![Audience::Everyone {
                library: Some(LibraryKey(3))
            }]),
            Outgoing::Send {
                event: "changed",
                ..
            }
        ));
        assert_eq!(
            visible(vec![Audience::Everyone {
                library: Some(LibraryKey(4))
            }]),
            Outgoing::Skip
        );
        assert_eq!(visible(vec![Audience::User(UserKey(2))]), Outgoing::Skip);
        assert_eq!(visible(vec![Audience::Admins]), Outgoing::Skip);
        assert!(matches!(
            visible(vec![Audience::Admins, Audience::User(UserKey(1))]),
            Outgoing::Send { .. }
        ));
    }

    #[test]
    fn revocation_of_another_device_does_not_end_the_stream() {
        let filter = member(1, &[]);
        let revoked = |device| ServerEvent::CredentialRevoked {
            user: UserKey(1),
            device,
        };
        assert_eq!(visible_event(&revoked(None), &filter), Outgoing::Revoked);
        assert_eq!(
            visible_event(&revoked(Some(DeviceKey(9))), &filter),
            Outgoing::Revoked
        );
        assert_eq!(
            visible_event(&revoked(Some(DeviceKey(8))), &filter),
            Outgoing::Skip
        );
    }

    async fn next_text(
        body: &mut (impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
        want: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use futures_util::StreamExt;
        let mut seen = String::new();
        while !seen.contains(want) {
            let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await?
                .ok_or("the stream ended")??;
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        Ok(seen)
    }

    #[tokio::test]
    async fn a_listener_whose_grants_change_hears_with_its_new_access()
    -> Result<(), Box<dyn std::error::Error>> {
        use axum::response::IntoResponse;
        let (sender, receiver) = tokio::sync::broadcast::channel(16);
        let reauthenticate: Reauthenticate =
            Arc::new(|| Box::pin(async { Some(member(1, &[3, 4])) }));
        let lookup: LibraryLookup = Arc::new(|_| Box::pin(async { None }));
        let sse = stream(
            receiver,
            member(1, &[3]),
            reauthenticate,
            hello(&uuid::Uuid::nil(), 1),
            None,
            lookup,
            tokio_util::sync::CancellationToken::new(),
            || 1,
        );
        let mut body = sse.into_response().into_body().into_data_stream();
        next_text(&mut body, "event: hello").await?;
        let library_4 = || {
            Arc::new(changed(vec![Audience::Everyone {
                library: Some(LibraryKey(4)),
            }]))
        };
        sender.send(library_4())?;
        sender.send(Arc::new(ServerEvent::AccessChanged { user: UserKey(1) }))?;
        sender.send(library_4())?;
        let text = next_text(&mut body, "event: changed").await?;
        let resync = text.find("event: resync").ok_or("no resync")?;
        assert!(text.find("event: changed").is_some_and(|c| c > resync));
        Ok(())
    }

    #[tokio::test]
    async fn a_listener_whose_credential_stops_working_is_signed_out()
    -> Result<(), Box<dyn std::error::Error>> {
        use axum::response::IntoResponse;
        let (sender, receiver) = tokio::sync::broadcast::channel(16);
        let reauthenticate: Reauthenticate = Arc::new(|| Box::pin(async { None }));
        let lookup: LibraryLookup = Arc::new(|_| Box::pin(async { None }));
        let sse = stream(
            receiver,
            member(1, &[3]),
            reauthenticate,
            hello(&uuid::Uuid::nil(), 1),
            None,
            lookup,
            tokio_util::sync::CancellationToken::new(),
            || 1,
        );
        let mut body = sse.into_response().into_body().into_data_stream();
        next_text(&mut body, "event: hello").await?;
        sender.send(Arc::new(ServerEvent::AccessChanged { user: UserKey(2) }))?;
        sender.send(Arc::new(ServerEvent::AccessChanged { user: UserKey(1) }))?;
        let text = next_text(&mut body, "event: session").await?;
        assert!(text.contains("revoked"));
        Ok(())
    }

    #[test]
    fn acquisition_events_need_the_feature() {
        let view = AcquisitionView {
            id: random_id::<AcquisitionId>(),
            state: AcquisitionState::Queued,
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            album: None,
            progress: None,
            quality: None,
            song: None,
            existing: false,
            failure: None,
            stream: None,
            created_at: Millis(0),
            updated_at: Millis(0),
        };
        let event = ServerEvent::Acquisition {
            user: UserKey(1),
            view,
        };
        let mut filter = member(1, &[]);
        assert_eq!(visible_event(&event, &filter), Outgoing::Skip);
        filter.acquisition = true;
        assert!(matches!(
            visible_event(&event, &filter),
            Outgoing::Send {
                event: "acquisition",
                ..
            }
        ));
        let other = member(2, &[]);
        assert_eq!(visible_event(&event, &other), Outgoing::Skip);
    }

    #[test]
    fn address_changes_reach_every_listener() {
        let event = ServerEvent::ServerUrls {
            version: String::from("a746df40"),
        };
        assert_eq!(
            visible_event(&event, &member(1, &[])),
            Outgoing::Send {
                event: "server_urls",
                data: String::from("{\"version\":\"a746df40\"}"),
            }
        );
    }

    #[test]
    fn scan_progress_is_for_admins() {
        let progress = trackside_core::events::ScanProgress {
            library: random_id(),
            run: random_id(),
            state: trackside_core::records::ScanState::Running,
            counts: trackside_core::records::ScanCounts::default(),
        };
        let event = ServerEvent::Scan(progress);
        let mut filter = member(1, &[]);
        assert_eq!(visible_event(&event, &filter), Outgoing::Skip);
        filter.access.role = Role::Admin;
        assert!(matches!(
            visible_event(&event, &filter),
            Outgoing::Send { event: "scan", .. }
        ));
    }
}
