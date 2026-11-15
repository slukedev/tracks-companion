//! The `trackside` binary's library: the command line, wiring, listeners, TLS, discovery,
//! backups, the scheduler, health and metrics, the embedded dashboard and shutdown. No
//! business logic lives here.

pub mod backup;
pub mod cli;
pub mod discovery;
pub mod health;
pub mod lock;
#[cfg(not(feature = "acquire"))]
mod noacq;
pub mod run;
pub mod scheduler;
pub mod tls;
pub mod web;

pub use health::health_router;
pub use run::{run, run_with};
