//! `/healthz`, `/readyz`, `/metrics`, and the server side of the admin overview.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{HeaderValue, StatusCode, header};
use trackside_api::HealthSource;
use trackside_api::wire::admin::{
    BackupSummary, Ffmpeg, Overview, OverviewBackups, OverviewMedia, OverviewServer,
    OverviewStorage, Problem,
};
use trackside_api::wire::{Ready, ReadyStatus, Time};
use trackside_core::{BoxFuture, Db, Millis, ScanControl};

use crate::backup::Backups;
use crate::web::Metrics;

/// What the backup scheduler last did, for the overview.
#[derive(Debug, Default)]
pub struct BackupStatus {
    pub next_at: Option<Millis>,
    pub last_error: Option<String>,
}

/// The server's [`HealthSource`].
pub struct Health {
    pub db: Db,
    pub scan: Arc<dyn ScanControl>,
    pub ffmpeg: Option<String>,
    pub name: String,
    pub server_id: uuid::Uuid,
    pub version: &'static str,
    pub started_at: Millis,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub tls: &'static str,
    pub listen: Vec<String>,
    pub backups: Arc<Backups>,
    pub backup_status: Arc<Mutex<BackupStatus>>,
    pub metrics: Arc<Metrics>,
    pub media: Arc<trackside_media::Media>,
    pub auth: Arc<trackside_auth::Auth>,
}

impl std::fmt::Debug for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Health")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Under this much free space the overview warns.
const LOW_SPACE: u64 = 2 << 30;

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

pub(crate) fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => total += entry.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    total
}

impl Health {
    fn database_path(&self) -> Option<PathBuf> {
        self.db.path().map(Path::to_path_buf)
    }

    fn storage(&self) -> OverviewStorage {
        let db = self.database_path();
        let database_bytes = db.as_deref().map_or(0, file_size);
        let wal_bytes = db.as_deref().map_or(0, |p| {
            file_size(&PathBuf::from(format!("{}-wal", p.display())))
        });
        let free = fs4::available_space(&self.data_dir).unwrap_or(0);
        let mut warnings = Vec::new();
        if free < LOW_SPACE {
            warnings.push(format!("Only {} MiB free on the data volume.", free >> 20));
        }
        OverviewStorage {
            database_bytes,
            wal_bytes,
            cache_bytes: dir_size(&self.cache_dir),
            data_free_bytes: free,
            warnings,
        }
    }

    fn backups_summary(&self) -> OverviewBackups {
        let last = self.backups.list().ok().and_then(|l| l.into_iter().next());
        let status = self
            .backup_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        OverviewBackups {
            last: last.map(|b| BackupSummary {
                name: b.name,
                at: b.at,
                bytes: b.bytes,
            }),
            next_at: status.next_at.map(Time),
            last_error: status.last_error.clone(),
        }
    }

    fn problems(&self, storage: &OverviewStorage) -> Vec<Problem> {
        let mut problems = Vec::new();
        if storage.data_free_bytes < LOW_SPACE {
            problems.push(Problem {
                severity: String::from("warning"),
                code: String::from("low_disk"),
                title: String::from("Data volume nearly full"),
                message: String::from("The data volume is nearly full."),
                help: Some(String::from("Free some space or move the cache with cache_dir. Writes stop when it is full.")),
            });
        }
        if self.ffmpeg.is_none() {
            problems.push(Problem {
                severity: String::from("info"),
                code: String::from("no_ffmpeg"),
                title: String::from("ffmpeg not found"),
                message: String::from(
                    "ffmpeg was not found, so songs play only in their original format.",
                ),
                help: Some(String::from(
                    "Install ffmpeg or set media.ffmpeg_path, then restart.",
                )),
            });
        }
        if let Some(error) = &self
            .backup_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_error
        {
            problems.push(Problem {
                severity: String::from("error"),
                code: String::from("backup_failed"),
                title: String::from("Last backup failed"),
                message: format!("The last backup failed: {error}"),
                help: None,
            });
        }
        problems
    }

    /// The `/metrics` text.
    pub fn metrics_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        self.metrics.render(&mut out);
        let storage = self.storage();
        let now = self.db.clock().now();
        let _ = writeln!(out, "# TYPE trackside_uptime_seconds gauge");
        let _ = writeln!(
            out,
            "trackside_uptime_seconds {}",
            now.since(self.started_at).as_secs()
        );
        let _ = writeln!(out, "# TYPE trackside_database_bytes gauge");
        let _ = writeln!(out, "trackside_database_bytes {}", storage.database_bytes);
        let _ = writeln!(out, "# TYPE trackside_wal_bytes gauge");
        let _ = writeln!(out, "trackside_wal_bytes {}", storage.wal_bytes);
        let _ = writeln!(out, "# TYPE trackside_cache_bytes gauge");
        let _ = writeln!(out, "trackside_cache_bytes {}", storage.cache_bytes);
        let _ = writeln!(out, "# TYPE trackside_transcode_cache_bytes gauge");
        let _ = writeln!(
            out,
            "trackside_transcode_cache_bytes {}",
            self.media.transcode_cache_bytes()
        );
        let _ = writeln!(out, "# TYPE trackside_library_available gauge");
        for status in self.scan.status() {
            let _ = writeln!(
                out,
                "trackside_library_available{{library=\"{}\"}} {}",
                status.library,
                u8::from(status.available)
            );
        }
        out
    }
}

impl HealthSource for Health {
    fn overview(&self) -> BoxFuture<'_, Overview> {
        Box::pin(async move {
            let storage = blocking(|| self.storage());
            let now = self.db.clock().now();
            Overview {
                server: OverviewServer {
                    name: self.name.clone(),
                    server_id: self.server_id.to_string(),
                    version: self.version.to_owned(),
                    uptime_s: now.since(self.started_at).as_secs(),
                    started_at: self.started_at.to_rfc3339(),
                    data_dir: self.data_dir.display().to_string(),
                    tls: self.tls.to_owned(),
                    listen: self.listen.clone(),
                },
                problems: self.problems(&storage),
                storage,
                libraries: Vec::new(),
                media: OverviewMedia {
                    ffmpeg: Ffmpeg {
                        found: self.ffmpeg.is_some(),
                        version: self.ffmpeg.clone(),
                    },
                    transcodes: {
                        let (running, queued) = self.media.transcode_load();
                        trackside_api::wire::admin::Transcodes {
                            running: running as u32,
                            queued: queued as u32,
                        }
                    },
                    transcode_cache_bytes: self.media.transcode_cache_bytes(),
                },
                acquisition: None,
                users: trackside_api::wire::admin::OverviewUsers::default(),
                backups: self.backups_summary(),
            }
        })
    }

    fn ready(&self) -> Ready {
        let mut checks = BTreeMap::new();
        let mut notes = Vec::new();
        let version = self.db.schema_version().ok();
        checks.insert(
            String::from("database"),
            String::from(if version.is_some() { "ok" } else { "failed" }),
        );
        let migrated = version == Some(trackside_core::db::SCHEMA_VERSION);
        checks.insert(
            String::from("migrations"),
            String::from(if migrated { "ok" } else { "failed" }),
        );
        let statuses = self.scan.status();
        let offline: Vec<_> = statuses.iter().filter(|s| !s.available).collect();
        for status in &offline {
            notes.push(format!("Library {} is unavailable", status.library));
        }
        checks.insert(
            String::from("libraries"),
            String::from(if offline.is_empty() { "ok" } else { "degraded" }),
        );
        if self.auth.untrusted_proxy_seen() {
            notes.push(String::from(
                "Requests arrive through a proxy that is not in server.trusted_proxies. Add it so each client is limited and logged by its own address.",
            ));
        }
        checks.insert(
            String::from("ffmpeg"),
            String::from(if self.ffmpeg.is_some() {
                "ok"
            } else {
                "degraded"
            }),
        );
        Ready {
            status: if migrated {
                ReadyStatus::Ready
            } else {
                ReadyStatus::Starting
            },
            checks,
            notes,
        }
    }
}

/// Runs filesystem work off the async worker when the runtime allows it.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    let multi = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(health): State<Arc<Health>>) -> Response {
    let ready = match tokio::task::spawn_blocking(move || health.ready()).await {
        Ok(ready) => ready,
        Err(_) => Ready {
            status: ReadyStatus::Starting,
            checks: BTreeMap::new(),
            notes: Vec::new(),
        },
    };
    let status = if ready.status == ReadyStatus::Ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(ready)).into_response()
}

async fn metrics(State(health): State<Arc<Health>>) -> Response {
    let text = tokio::task::spawn_blocking(move || health.metrics_text())
        .await
        .unwrap_or_default();
    let mut response = text.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

/// `/healthz` alone, which needs nothing but a running process.
pub fn health_router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

/// `/healthz` and `/readyz`, and `/metrics` when enabled on the main listener.
pub fn probes_router(health: Arc<Health>, with_metrics: bool) -> Router {
    let mut router = Router::new().route("/readyz", get(readyz));
    if with_metrics {
        router = router.route("/metrics", get(metrics));
    }
    router.with_state(health).merge(health_router())
}

/// `/metrics` alone, for `metrics.listen`.
pub fn metrics_router(health: Arc<Health>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .with_state(health)
}
