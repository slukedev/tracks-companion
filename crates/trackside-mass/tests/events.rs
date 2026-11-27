//! Events, snapshots, lag handling and reconnecting.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{FakeServer, PATIENCE, player, queue, wait_state};
use trackside_mass::{
    Client, ConnectError, ConnectionState, Event, SnapshotReason, Subscription, Update,
};

async fn next(subscription: &mut Subscription) -> Update {
    tokio::time::timeout(PATIENCE, subscription.recv())
        .await
        .expect("an update in time")
        .expect("client running")
}

async fn next_event(subscription: &mut Subscription) -> Event {
    loop {
        if let Update::Event(event) = next(subscription).await {
            return (*event).clone();
        }
    }
}

async fn next_snapshot(subscription: &mut Subscription) -> trackside_mass::Snapshot {
    loop {
        if let Update::Snapshot(snapshot) = next(subscription).await {
            return (*snapshot).clone();
        }
    }
}

#[tokio::test]
async fn the_first_snapshot_holds_every_player_and_queue() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut updates = client.subscribe();
    let mut conn = server.accept().await;
    conn.handshake().await;
    conn.serve_snapshot(
        json!([player("kitchen", "Kitchen", 30), player("den", "Den", 50)]),
        json!([queue("kitchen", 12)]),
    )
    .await;

    let snapshot = next_snapshot(&mut updates).await;
    assert_eq!(snapshot.reason, SnapshotReason::Connected);
    assert_eq!(snapshot.players.len(), 2);
    assert_eq!(snapshot.queue("kitchen").map(|q| q.items), Some(12));
    client.shutdown().await;
}

#[tokio::test]
async fn events_reach_every_subscriber() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut first = client.subscribe();
    let mut second = client.subscribe();
    let mut conn = server.accept_ready().await;

    conn.event(
        "player_updated",
        Some("kitchen"),
        player("kitchen", "Kitchen", 44),
    )
    .await;
    for subscription in [&mut first, &mut second] {
        let Event::PlayerUpdated(player) = next_event(subscription).await else {
            panic!("not a player update")
        };
        assert_eq!(player.volume_level, Some(44));
    }
    client.shutdown().await;
}

#[tokio::test]
async fn each_kind_of_event_decodes() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut updates = client.subscribe();
    let mut conn = server.accept_ready().await;

    conn.event("player_added", Some("den"), player("den", "Den", 5))
        .await;
    conn.event("player_removed", Some("den"), json!(null)).await;
    conn.event("queue_updated", Some("kitchen"), queue("kitchen", 3))
        .await;
    conn.event("queue_items_updated", Some("kitchen"), queue("kitchen", 4))
        .await;
    conn.event("queue_time_updated", Some("kitchen"), json!(61.5))
        .await;
    conn.event(
        "media_item_added",
        Some("library://track/9"),
        json!({"item_id": "9", "provider": "library", "name": "Song", "media_type": "track", "provider_mappings": []}),
    )
    .await;
    conn.event("music_sync_completed", None, json!(null)).await;
    conn.event("provider_event", Some("spotify--a/auth"), json!({"x": 1}))
        .await;
    conn.event("dashboard_show", None, json!({"anything": true}))
        .await;

    assert!(
        matches!(next_event(&mut updates).await, Event::PlayerAdded(p) if p.player_id == "den")
    );
    assert!(
        matches!(next_event(&mut updates).await, Event::PlayerRemoved { player_id } if player_id == "den")
    );
    assert!(matches!(next_event(&mut updates).await, Event::QueueUpdated(q) if q.items == 3));
    assert!(matches!(next_event(&mut updates).await, Event::QueueItemsUpdated(q) if q.items == 4));
    assert!(matches!(
        next_event(&mut updates).await,
        Event::QueueTimeUpdated { elapsed_time, .. } if (elapsed_time - 61.5).abs() < f64::EPSILON
    ));
    assert!(
        matches!(next_event(&mut updates).await, Event::MediaItemAdded { uri, .. } if uri == "library://track/9")
    );
    assert!(matches!(
        next_event(&mut updates).await,
        Event::MusicSyncCompleted
    ));
    assert!(
        matches!(next_event(&mut updates).await, Event::ProviderEvent { instance_id, .. } if instance_id == "spotify--a")
    );
    assert!(
        matches!(next_event(&mut updates).await, Event::Other(raw) if raw.event == "dashboard_show")
    );
    client.shutdown().await;
}

#[tokio::test]
async fn a_lagged_subscriber_is_told_and_gets_a_fresh_snapshot() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.event_capacity = 4;
    let client = Client::start(options);
    let mut observer = client.subscribe();
    let mut conn = server.accept_ready().await;
    next_snapshot(&mut observer).await;
    let mut slow = client.subscribe();

    for volume in 0..20 {
        conn.event(
            "player_updated",
            Some("kitchen"),
            player("kitchen", "Kitchen", volume),
        )
        .await;
    }
    // Make sure the burst went out before the slow subscriber reads.
    conn.event(
        "player_updated",
        Some("kitchen"),
        player("kitchen", "Kitchen", 20),
    )
    .await;
    next_event(&mut observer).await;

    let mut lagged = false;
    while !lagged {
        lagged = matches!(next(&mut slow).await, Update::Lagged { missed } if missed > 0);
    }
    conn.serve_snapshot(json!([player("kitchen", "Kitchen", 20)]), json!([]))
        .await;
    let snapshot = next_snapshot(&mut slow).await;
    assert_eq!(snapshot.reason, SnapshotReason::Resync);
    assert_eq!(
        snapshot.player("kitchen").and_then(|p| p.volume_level),
        Some(20)
    );
    client.shutdown().await;
}

#[tokio::test]
async fn an_undecodable_state_event_triggers_a_resync() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut updates = client.subscribe();
    let mut conn = server.accept_ready().await;
    next_snapshot(&mut updates).await;

    conn.event("player_updated", Some("kitchen"), json!("garbage"))
        .await;
    assert!(
        matches!(next_event(&mut updates).await, Event::Other(raw) if raw.event == "player_updated")
    );
    conn.serve_snapshot(json!([player("kitchen", "Kitchen", 9)]), json!([]))
        .await;
    assert_eq!(
        next_snapshot(&mut updates).await.reason,
        SnapshotReason::Resync
    );
    client.shutdown().await;
}

#[tokio::test]
async fn events_during_a_snapshot_fetch_are_applied_to_it() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut updates = client.subscribe();
    let mut conn = server.accept().await;
    conn.handshake().await;

    let first = conn.recv().await;
    let second = conn.recv().await;
    conn.event(
        "player_updated",
        Some("kitchen"),
        player("kitchen", "Kitchen", 77),
    )
    .await;
    conn.event("player_added", Some("den"), player("den", "Den", 1))
        .await;
    for sent in [first, second] {
        match sent.command.as_str() {
            "players/all" => {
                conn.reply(&sent.id, json!([player("kitchen", "Kitchen", 10)]))
                    .await;
            }
            _ => conn.reply(&sent.id, json!([])).await,
        }
    }

    let snapshot = next_snapshot(&mut updates).await;
    assert_eq!(
        snapshot.player("kitchen").and_then(|p| p.volume_level),
        Some(77)
    );
    assert!(snapshot.player("den").is_some());
    client.shutdown().await;
}

#[tokio::test]
async fn a_dropped_connection_reconnects_and_resyncs() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut updates = client.subscribe();
    let conn = server.accept_ready().await;
    next_snapshot(&mut updates).await;

    conn.close().await;
    let mut conn = server.accept().await;
    conn.handshake().await;
    conn.serve_snapshot(json!([player("kitchen", "Kitchen", 3)]), json!([]))
        .await;

    let snapshot = next_snapshot(&mut updates).await;
    assert_eq!(snapshot.reason, SnapshotReason::Connected);
    let state = wait_state(&client, ConnectionState::is_connected).await;
    assert_eq!(state.session().map(|s| s.generation), Some(2));
    client.shutdown().await;
}

#[tokio::test]
async fn reconnect_replaces_a_working_connection() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept_ready().await;
    client.reconnect();
    conn.closed().await;
    let _conn = server.accept_ready().await;
    let state = wait_state(&client, |s| s.session().is_some_and(|s| s.generation == 2)).await;
    assert!(state.is_connected());
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_silent_connection_is_declared_dead() {
    let mut server = FakeServer::start().await;
    let mut options = server.options_without_retry();
    options.ping_interval = Duration::from_secs(2);
    options.dead_after = Duration::from_secs(6);
    let client = Client::start(options);
    let _conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;

    // The fake stops reading, so no pong ever comes back.
    let state = wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::Unresponsive),
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        state,
        ConnectionState::Disconnected {
            retry_in: Some(_),
            ..
        }
    ));
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn pongs_keep_a_quiet_connection_alive() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.ping_interval = Duration::from_secs(5);
    options.dead_after = Duration::from_secs(12);
    let client = Client::start(options);
    let mut conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;

    // Reading lets the fake answer pings; the client must stay on its first connection.
    conn.pump(Duration::from_secs(55)).await;
    let state = client.current_state();
    assert_eq!(state.session().map(|s| s.generation), Some(1), "{state:?}");
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn retries_back_off_up_to_the_cap() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let endpoint = trackside_mass::Endpoint::parse(&format!("http://{addr}")).expect("endpoint");
    let mut options = trackside_mass::Options::new(endpoint, trackside_mass::Token::new("t"));
    options.backoff = trackside_mass::Backoff {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(60),
    };
    let client = Client::start(options);
    let mut state = client.state();
    let mut delays = Vec::new();
    while delays.len() < 10 {
        state.changed().await.expect("running");
        if let ConnectionState::Disconnected {
            retry_in: Some(delay),
            ..
        } = &*state.borrow()
        {
            delays.push(*delay);
        }
    }
    assert!(
        delays.iter().all(|d| *d <= Duration::from_secs(60)),
        "{delays:?}"
    );
    assert!(delays[9] >= Duration::from_secs(30), "{delays:?}");
    assert!(delays[0] <= Duration::from_secs(1), "{delays:?}");
    client.shutdown().await;
}

#[tokio::test]
async fn subscriptions_end_when_the_client_shuts_down() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut updates = client.subscribe();
    let _conn = server.accept_ready().await;
    client.shutdown().await;
    let mut ended = false;
    for _ in 0..10 {
        if updates.recv().await.is_none() {
            ended = true;
            break;
        }
    }
    assert!(ended);
}
