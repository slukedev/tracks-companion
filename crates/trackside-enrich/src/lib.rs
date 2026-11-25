//! Last.fm enrichment with one server-wide API key: similar artists, similar songs, top songs
//! and biographies for what is in the library, fetched in the background and stored, so a
//! restart fetches nothing again until it expires.
//!
//! Only unauthenticated read methods are called, with artist and track names. No session,
//! shared secret or user data is ever sent, and nothing is written to Last.fm.

pub mod art;
pub mod lastfm;
mod worker;

pub use worker::{EnrichOptions, Enricher, StartError};
