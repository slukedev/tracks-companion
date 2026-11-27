//! A Sendspin player client, built for the phone relay: Trackside joins Music Assistant as one
//! player per listening phone and receives that player's audio in real time.
//!
//! [`Client::start`] connects through Music Assistant's authenticated `/sendspin` proxy (an
//! `auth` message with the Music Assistant token), runs the cleartext init exchange and the
//! Noise `Noise_KKpsk2_25519_ChaChaPoly_SHA256` handshake as responder with the published
//! Sentinel PSK (unpaired access, which Music Assistant grants to playback-only guests), and
//! then speaks the encrypted wire: `client/hello` with only `player@v1` and one FLAC format,
//! time-sync bursts into the spec's Kalman filter, `client/state` once the clock has a
//! measurement, and the player stream (`stream/start`, audio chunks, `stream/clear`,
//! `stream/end`). Re-handshakes are answered in band. A dropped connection is retried with
//! jittered backoff; inside Music Assistant's 180 s retention the same player comes back.
//!
//! Events go to a [`Handler`] synchronously on the connection task, in wire order. The handler
//! must never block, which keeps a slow consumer from stalling the socket.
//!
//! The token and the private key never appear in `Debug` output, logs or errors. Audio is
//! handed on as it arrives and kept nowhere by this crate.

mod b64;
mod client;
mod clock;
mod config;
mod event;
mod frame;
mod identity;
mod noise;
mod wire;

#[cfg(feature = "testing")]
pub mod testing;

pub use client::Client;
pub use clock::{Clock, Exchange, TimeFilter};
pub use config::{Backoff, Config, Token};
pub use event::{Error, Event, GoodbyeReason, Handler, Stage};
pub use frame::{AUDIO_HEADER_LEN, AudioChunk, FrameError};
pub use identity::{
    Identity, IdentityError, IdentityStore, MemoryIdentityStore, StoreFuture, load_or_create,
};
pub use noise::{NoiseError, SENTINEL_PSK, SUITE, psk_id_for};
pub use wire::{Format, GroupUpdate, HelloInfo, PlayerTiming, StreamFormat};
