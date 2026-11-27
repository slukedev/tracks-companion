//! A FLAC tone (6 s, 44.1 kHz stereo, 4608-sample frames, encoded once with ffmpeg) split the
//! way Music Assistant sends it, a recording `PlayerControl`, and a relay wired to the fake
//! Sendspin server.

#![allow(dead_code, clippy::missing_panics_doc)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use trackside_relay::{
    ControlFuture, OpenRequest, PlayerControl, RelayConfig, RelayManager, StreamInfo, Tier,
    split_frames,
};
use trackside_sendspin::testing::{FakeConn, FakeServer, TOKEN};
use trackside_sendspin::{Backoff, MemoryIdentityStore, Token};

pub const TONE: &[u8] = include_bytes!("../data/tone.flac");
/// 4608 samples at 44.1 kHz.
pub const FRAME_US: i64 = 104_489;

/// The codec header Sendspin sends (`fLaC` and STREAMINFO marked last) and the frames.
pub fn tone() -> (Vec<u8>, Vec<Bytes>) {
    assert_eq!(&TONE[..4], b"fLaC");
    let mut at = 4;
    let mut streaminfo = None;
    loop {
        let header = TONE[at];
        let len = usize::from(TONE[at + 1]) << 16
            | usize::from(TONE[at + 2]) << 8
            | usize::from(TONE[at + 3]);
        if matches!(header & 0x7F, 0) {
            streaminfo = Some(TONE[at + 4..at + 4 + len].to_vec());
        }
        at += 4 + len;
        if header & 0x80 != 0 {
            break;
        }
    }
    let mut codec_header = b"fLaC".to_vec();
    codec_header.extend_from_slice(&[0x80, 0, 0, 34]);
    codec_header.extend_from_slice(&streaminfo.expect("streaminfo"));
    StreamInfo::from_codec_header(&codec_header).expect("valid header");
    let frames = split_frames(&Bytes::copy_from_slice(&TONE[at..]))
        .expect("frames")
        .into_iter()
        .map(|f| f.data)
        .collect();
    (codec_header, frames)
}

#[derive(Debug, Default)]
pub struct Control {
    pub calls: Mutex<Vec<String>>,
}

impl Control {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

impl PlayerControl for Control {
    fn pause<'a>(&'a self, player_id: &'a str) -> ControlFuture<'a> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("pause {player_id}"));
        Box::pin(async { Ok(()) })
    }

    fn resume<'a>(&'a self, player_id: &'a str) -> ControlFuture<'a> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("resume {player_id}"));
        Box::pin(async { Ok(()) })
    }
}

pub struct Rig {
    pub server: FakeServer,
    pub manager: RelayManager,
    pub control: Arc<Control>,
    pub cancel: CancellationToken,
}

pub async fn rig(edit: impl FnOnce(&mut RelayConfig)) -> Rig {
    let server = FakeServer::start().await;
    let mut config = RelayConfig::new(server.url(), Some(Token::new(TOKEN)));
    config.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
    };
    edit(&mut config);
    let control = Arc::new(Control::default());
    let cancel = CancellationToken::new();
    let manager = RelayManager::new(
        config,
        Arc::new(MemoryIdentityStore::default()),
        Arc::clone(&control) as Arc<dyn PlayerControl>,
        cancel.clone(),
    );
    Rig {
        server,
        manager,
        control,
        cancel,
    }
}

pub fn request(device: &str, tier: Tier) -> OpenRequest {
    OpenRequest {
        id: format!("h-{device}"),
        user: "luke".into(),
        device: device.into(),
        name: "Luke's iPhone".into(),
        tier,
    }
}

/// Accepts the relay's connection and brings it to an activated, synced player.
pub async fn connect(server: &mut FakeServer) -> FakeConn {
    let mut conn = server.accept().await;
    conn.expect_auth().await;
    conn.auth_ok().await;
    conn.ready().await;
    conn
}

/// Sends `frames` as chunks of one frame each, timed from `start` on the server clock.
pub async fn send_frames(conn: &mut FakeConn, start: i64, frames: &[Bytes]) {
    for (i, frame) in frames.iter().enumerate() {
        conn.send_audio(start + i as i64 * FRAME_US, frame).await;
    }
}

/// Polls `check` until it is true.
pub async fn eventually(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("in time");
}
