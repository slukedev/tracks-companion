//! The startup sequence and the listeners: lock, secret key, pre-migration backup, open and
//! migrate, setup token, first library, scanner, media, acquirer, routes, listeners, mDNS,
//! scheduler; then graceful shutdown.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::Extension;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use trackside_api::{ApiState, LocalState, ServerInfo, UrlBook, normalize_base_path};
use trackside_auth::{Auth, SecretKey};
use trackside_core::config::TlsMode;
use trackside_core::repo::libraries::{self, NewLibrary};
use trackside_core::repo::settings;
use trackside_core::{Access, Clock, Config, ConfigReport, Db, DbError, EventBus, SystemClock};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};

use crate::backup::{BackupKind, Backups, Ops};
use crate::discovery;
use crate::health::{BackupStatus, Health};
use crate::lock::DataDirLock;
use crate::scheduler::{self, Retention};
use crate::tls::{self, Tls};
use crate::web::{self, JsonOnly, ListenerTls, Metrics, WebState};

/// Timeouts on every connection, not just its first request. Over HTTP/1 the header timeout
/// also closes keep-alive connections that sit idle that long, so a client sending headers a
/// byte at a time or parking sockets cannot hold them. Over HTTP/2 pings find peers that are
/// gone, such as a phone that left Wi-Fi. A macro because the builder's type is hyper-util's.
macro_rules! connection_timeouts {
    ($builder:expr, $header_timeout:expr) => {{
        let builder = $builder;
        builder.http1().header_read_timeout($header_timeout);
        builder
            .http2()
            .keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(20));
    }};
}

/// The transcoder when ffmpeg is not found: direct play only.
#[derive(Debug)]
struct NoTranscoder;

impl Transcoder for NoTranscoder {
    fn version(&self) -> Option<String> {
        None
    }

    fn spawn(&self, _spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError> {
        Err(MediaError::NoTranscoder)
    }
}

/// Settings key set once the first library has been made from `music_dir`.
const FIRST_LIBRARY_DONE: &str = "setup.music_dir_library";

/// Runs the server until `shutdown` is cancelled.
pub async fn run(
    config: Config,
    report: ConfigReport,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    run_with(config, report, shutdown, |_| {}).await
}

/// Binds `address`, falling back from the IPv6 wildcard to the IPv4 one on hosts without IPv6.
pub async fn bind(address: SocketAddr) -> anyhow::Result<TcpListener> {
    match TcpListener::bind(address).await {
        Ok(listener) => Ok(listener),
        Err(e) if address.is_ipv6() && address.ip().is_unspecified() => {
            let fallback = SocketAddr::from((Ipv4Addr::UNSPECIFIED, address.port()));
            tracing::info!(%address, %fallback, error = %e, "IPv6 unavailable, listening on IPv4");
            TcpListener::bind(fallback)
                .await
                .with_context(|| format!("cannot listen on {fallback}"))
        }
        Err(e) => Err(e).with_context(|| format!("cannot listen on {address}")),
    }
}

/// Copies the database aside before a migration, when there is one to run.
pub fn pre_migration_backup(
    db_path: &Path,
    backups_dir: &Path,
    now: trackside_core::Millis,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let Some(found) =
        Db::inspect_schema_version(db_path).context("cannot read the database's schema version")?
    else {
        return Ok(None);
    };
    let known = trackside_core::db::SCHEMA_VERSION;
    if found == 0 || found >= known {
        return Ok(None);
    }
    std::fs::create_dir_all(backups_dir)?;
    let target = backups_dir.join(crate::backup::backup_name(
        BackupKind::PreMigrate {
            from: found,
            to: known,
        },
        now,
    ));
    std::fs::copy(db_path, &target).context("cannot write the pre-migration backup")?;
    let wal = std::path::PathBuf::from(format!("{}-wal", db_path.display()));
    if wal.exists() {
        std::fs::copy(&wal, format!("{}-wal", target.display()))?;
    }
    tracing::info!(path = %target.display(), from = found, to = known, "pre-migration backup written");
    Ok(Some(target))
}

/// Opens the database, explaining the failures a self-hoster can fix.
pub fn open_database(
    config: &Config,
    events: EventBus,
    clock: Arc<dyn Clock>,
) -> anyhow::Result<Db> {
    match Db::open(&config.database.options(), events, clock) {
        Ok(db) => Ok(db),
        Err(DbError::NetworkFilesystem) => anyhow::bail!(
            "the database {} is on a network filesystem, where SQLite locking is unreliable; move the data directory to a local disk or set database.allow_network_fs = true",
            config.database.path.display()
        ),
        Err(DbError::TooNew { found, known }) => anyhow::bail!(
            "the database {} is from a newer Trackside (schema {found}, this one knows {known}); restore a pre-migration backup from {} with `trackside backup restore` to downgrade",
            config.database.path.display(),
            config.backup.dir.display()
        ),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cannot open the database {}",
                config.database.path.display()
            )
        }),
    }
}

/// Makes the first library from `music_dir` once, when it exists, has files and no library
/// exists yet.
pub async fn first_library(db: &Db, music_dir: Option<&Path>) -> anyhow::Result<bool> {
    let Some(dir) = music_dir else {
        return Ok(false);
    };
    let done: Option<bool> = db.read(|c| settings::get(c, FIRST_LIBRARY_DONE)).await?;
    if done == Some(true) {
        return Ok(false);
    }
    let dir = dir.to_path_buf();
    let probe = dir.clone();
    let has_files = tokio::task::spawn_blocking(move || {
        std::fs::read_dir(&probe).is_ok_and(|mut e| e.next().is_some())
    })
    .await?;
    if !has_files {
        return Ok(false);
    }
    let root = tokio::task::spawn_blocking(move || std::fs::canonicalize(&dir)).await??;
    let created = db
        .write(move |tx| {
            if !libraries::list(tx.conn(), &Access::system())?.is_empty() {
                settings::put(tx, FIRST_LIBRARY_DONE, &true)?;
                return Ok(false);
            }
            libraries::create(
                tx,
                &NewLibrary {
                    name: String::from("Music"),
                    root,
                    writable: false,
                    watch: true,
                    grant_all_users: true,
                    granted_by: None,
                },
            )?;
            settings::put(tx, FIRST_LIBRARY_DONE, &true)?;
            Ok(true)
        })
        .await?;
    Ok(created)
}

fn tls_name(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Off => "off",
        TlsMode::Files => "files",
        TlsMode::Acme => "acme",
        TlsMode::SelfSigned => "self-signed",
    }
}

#[cfg(feature = "acquire")]
fn acquirer(
    db: &Db,
    scan: Arc<dyn trackside_core::ScanControl>,
    media: Arc<Media>,
    config: &Config,
    events: EventBus,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
) -> Arc<dyn trackside_core::AcquireControl> {
    let providers: Vec<Arc<dyn trackside_acquire::Provider>> =
        vec![Arc::new(trackside_acquire::YtdlpProvider::new(
            &config.acquire.ytdlp,
            &config.data_dir,
            config.acquire.max_file_bytes,
        ))];
    trackside_acquire::Acquirer::start(
        db.clone(),
        scan,
        media,
        &config.acquire,
        providers,
        events,
        clock,
        shutdown,
    )
}

#[cfg(not(feature = "acquire"))]
fn acquirer(
    _db: &Db,
    _scan: Arc<dyn trackside_core::ScanControl>,
    _media: Arc<Media>,
    _config: &Config,
    _events: EventBus,
    _clock: Arc<dyn Clock>,
    _shutdown: CancellationToken,
) -> Arc<dyn trackside_core::AcquireControl> {
    Arc::new(crate::noacq::NoAcquisition)
}

/// Sets up the first admin from `auth.admin_username` and `auth.admin_password`, or prints the
/// setup banner while no admin exists.
async fn setup(auth: &Auth, config: &Config, setup_url: &str) -> anyhow::Result<()> {
    if !auth.check_secret_key().await? {
        tracing::warn!("the secret key changed: app passwords and pending pairings no longer work");
    }
    let Some(token) = auth.ensure_setup_token(&config.data_dir).await? else {
        return Ok(());
    };
    if let (Some(username), Some(password)) =
        (&config.auth.admin_username, &config.auth.admin_password)
    {
        let admin = trackside_auth::NewUser {
            username: username.clone(),
            display_name: username.clone(),
            password: zeroize::Zeroizing::new(password.expose().to_owned()),
            role: trackside_core::Role::Admin,
            permissions: trackside_core::Permissions {
                can_download: true,
                can_upload: true,
                can_request: true,
                legacy_subsonic: true,
                max_bitrate_kbps: None,
            },
            libraries: Vec::new(),
        };
        auth.complete_setup(&token, admin, std::net::IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await
            .context("cannot create the admin from TRACKSIDE_ADMIN_USERNAME")?;
        auth.mark_setup_done().await?;
        auth.ensure_setup_token(&config.data_dir).await?;
        tracing::info!(%username, "admin created from the configuration");
        return Ok(());
    }
    let file = config.data_dir.join("setup-token");
    let banner = trackside_auth::setup_banner(&token, setup_url, &file);
    eprintln!("{banner}");
    tracing::warn!(url = %setup_url, file = %file.display(), "setup token printed; open the setup page to create the first admin");
    Ok(())
}

/// [`run`], calling `on_listening` with the bound addresses once every listener is up.
#[expect(
    clippy::too_many_lines,
    reason = "the startup sequence reads best in one place"
)]
pub async fn run_with(
    config: Config,
    report: ConfigReport,
    shutdown: CancellationToken,
    on_listening: impl FnOnce(&[SocketAddr]) + Send,
) -> anyhow::Result<()> {
    tls::install_provider();
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    std::fs::create_dir_all(&config.data_dir).with_context(|| {
        format!(
            "cannot create the data directory {}",
            config.data_dir.display()
        )
    })?;
    let lock = DataDirLock::acquire(&config.data_dir)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (secret, created) =
        SecretKey::load_or_create(&config.auth.secret_key_file).with_context(|| {
            format!(
                "cannot read or create the secret key {}",
                config.auth.secret_key_file.display()
            )
        })?;
    if created {
        tracing::info!(path = %config.auth.secret_key_file.display(), "secret key created");
    }
    let backups = Arc::new(Backups::new(
        config.backup.dir.clone(),
        config
            .backup
            .include_secret_key
            .then(|| config.auth.secret_key_file.clone()),
        config.backup.keep,
    ));
    backups.clean_partials();
    pre_migration_backup(&config.database.path, &config.backup.dir, clock.now())?;

    let events = EventBus::default();
    let db = open_database(&config, events.clone(), Arc::clone(&clock))?;
    tracing::info!(path = %config.database.path.display(), schema = db.schema_version()?, "database ready");
    let cloud =
        trackside_cloud::epoch::reconcile(&db).context("cannot set the cloud sync epoch")?;
    if cloud.new_epoch {
        tracing::info!(epoch = %cloud.epoch, "new cloud sync epoch: apps pull their cloud data again");
    }
    let server_id = db
        .read(settings::server_id)
        .await
        .context("cannot read the server id")?;

    let auth = Arc::new(
        Auth::new(
            db.clone(),
            &config.auth,
            secret,
            config.server.trusted_proxies.clone(),
            Arc::clone(&clock),
        )
        .with_local_networks(config.server.local_networks.clone())
        .with_tailscale_is_local(config.server.tailscale_is_local),
    );
    trackside_api::build::apply_stored_auth(&db, &auth, &report).await;
    let transcoder: Arc<dyn Transcoder> =
        match trackside_media::FfmpegTranscoder::detect(config.media.ffmpeg_path.as_deref()) {
            Some(found) => Arc::new(found),
            None => Arc::new(NoTranscoder),
        };
    let ffmpeg = transcoder.version();
    if ffmpeg.is_none() {
        tracing::warn!("ffmpeg not found; songs play in their original format only");
    }
    let media = Arc::new(Media::new(
        &config.media,
        &config.cache_dir,
        db.clone(),
        transcoder,
        Arc::clone(&clock),
    )?);
    if first_library(&db, config.music_dir.as_deref()).await? {
        tracing::info!(dir = ?config.music_dir, "first library created from music_dir");
    }
    let scanner = trackside_scan::Scanner::start(
        db.clone(),
        media.artwork_store().clone(),
        &config.scanner,
        &config.uploads,
        events.clone(),
        Arc::clone(&clock),
        shutdown.clone(),
    )?;
    let scan: Arc<dyn trackside_core::ScanControl> = scanner;
    let acquire = acquirer(
        &db,
        Arc::clone(&scan),
        Arc::clone(&media),
        &config,
        events.clone(),
        Arc::clone(&clock),
        shutdown.clone(),
    );

    let lastfm_key = match config.enrichment.lastfm_api_key.clone() {
        Some(key) => Some(key),
        None => {
            trackside_api::build::stored_secret(&db, &auth, trackside_api::build::LASTFM_API_KEY)
                .await
        }
    };
    let enricher = trackside_enrich::Enricher::start(
        db.clone(),
        lastfm_key,
        trackside_enrich::EnrichOptions::default(),
        shutdown.clone(),
    )
    .context("cannot start Last.fm enrichment")?;
    enricher.set_art_store(Arc::new(media.artwork_store().clone()));
    let enrich: Arc<dyn trackside_core::EnrichControl> = enricher;

    let tls = tls::setup(&config.tls, &config.data_dir, &shutdown)?;
    let mut listeners = Vec::new();
    for address in &config.server.listen {
        listeners.push(bind(*address).await?);
    }
    let addresses = listeners
        .iter()
        .map(TcpListener::local_addr)
        .collect::<Result<Vec<_>, _>>()?;
    let port = addresses.first().map_or(4747, SocketAddr::port);
    let base_path = normalize_base_path(&config.server.base_path);
    let is_tls = tls.is_some();
    let fingerprint = tls.as_ref().and_then(|t: &Tls| t.fingerprint.clone());
    let host = discovery::local_host_name();
    let mdns_host = if config.discovery.mdns {
        host.clone()
    } else {
        None
    };
    let urls = UrlBook::default();
    let refresh = {
        let urls = urls.clone();
        let base_path = base_path.clone();
        let fingerprint = fingerprint.clone();
        let mdns_host = mdns_host.clone();
        move || {
            urls.set(discovery::candidates(
                &discovery::interface_addresses(),
                port,
                is_tls,
                fingerprint.as_deref(),
                &base_path,
                mdns_host.as_deref(),
            ));
        }
    };
    refresh();

    let config = Arc::new(config);
    let started_at = clock.now();
    let mut info = ServerInfo::new(server_id, config.server.name.clone(), started_at);
    info.base_path.clone_from(&base_path);
    info.urls = urls.clone();
    info.report = Arc::new(report);
    info.fingerprint.clone_from(&fingerprint);
    let metrics = Arc::new(Metrics::default());
    let backup_status = Arc::new(Mutex::new(BackupStatus::default()));
    let health = Arc::new(Health {
        db: db.clone(),
        scan: Arc::clone(&scan),
        ffmpeg,
        name: config.server.name.clone(),
        server_id,
        version: info.version,
        started_at,
        data_dir: config.data_dir.clone(),
        cache_dir: config.cache_dir.clone(),
        tls: tls_name(config.tls.mode),
        listen: addresses.iter().map(ToString::to_string).collect(),
        backups: Arc::clone(&backups),
        backup_status: Arc::clone(&backup_status),
        metrics: Arc::clone(&metrics),
        media: Arc::clone(&media),
        auth: Arc::clone(&auth),
    });
    let ops = Arc::new(Ops {
        backups: Arc::clone(&backups),
        status: Arc::clone(&backup_status),
        db: db.clone(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .danger_accept_invalid_certs(fingerprint.is_some())
            .build()
            .context("cannot build the HTTP client")?,
        server_id,
    });
    let now_playing = trackside_core::now_playing::NowPlayingRegistry::new();
    let local = Arc::new(LocalState::with_now_playing(
        shutdown.clone(),
        now_playing.clone(),
    ));
    let api = ApiState {
        db: db.clone(),
        auth: Arc::clone(&auth),
        media: Arc::clone(&media),
        scan: Arc::clone(&scan),
        acquire,
        enrich: Arc::clone(&enrich),
        events: events.clone(),
        config: Arc::clone(&config),
        server: info,
        health: health.clone(),
        ops,
        local: Arc::clone(&local),
    };
    let subsonic = trackside_subsonic::SubsonicState {
        db: db.clone(),
        auth: Arc::clone(&auth),
        media: Arc::clone(&media),
        scan: Arc::clone(&scan),
        enrich,
        config: Arc::clone(&config),
        server_id,
    };
    let metrics_on_main = config.metrics.enabled && config.metrics.listen.is_none();
    let web_state = WebState {
        auth: Arc::clone(&auth),
        metrics: config.metrics.enabled.then(|| Arc::clone(&metrics)),
        request_timeout: config.server.request_timeout,
        playback: Arc::clone(local.playback()),
        hsts: matches!(config.tls.mode, TlsMode::Acme | TlsMode::Files),
        base_path: base_path.clone(),
    };
    let inner = Router::new()
        .merge(trackside_api::router(api))
        .merge(trackside_subsonic::router_with(subsonic, now_playing))
        .merge(crate::health::probes_router(
            Arc::clone(&health),
            metrics_on_main,
        ))
        .route_layer(axum::middleware::from_fn(web::mark_route))
        .fallback_service(web::dashboard_router());
    let app = if base_path.is_empty() {
        inner
    } else {
        Router::new()
            .nest_service(&base_path, inner)
            .merge(crate::health::health_router())
    };
    let app = app
        .layer(
            CompressionLayer::new()
                .br(true)
                .gzip(true)
                .compress_when(JsonOnly),
        )
        .layer(axum::middleware::from_fn_with_state(
            web_state,
            web::context,
        ));

    let setup_url = format!(
        "{}/setup",
        urls.get().first().map_or_else(
            || format!("http://localhost:{port}{base_path}"),
            |u| u.url.clone()
        )
    );
    setup(&auth, &config, &setup_url).await?;

    let mut handles = Vec::new();
    let mut servers = tokio::task::JoinSet::new();
    let connections = web::ConnectionLimit::new(config.server.max_connections);
    for listener in listeners {
        let address = listener.local_addr()?;
        tracing::info!(%address, tls = is_tls, "listening");
        let std_listener = listener.into_std()?;
        let handle = axum_server::Handle::<SocketAddr>::new();
        handles.push(handle.clone());
        let service = app
            .clone()
            .layer(Extension(ListenerTls(is_tls)))
            .into_make_service_with_connect_info::<SocketAddr>();
        let header_timeout = config.server.header_timeout;
        // The acceptor also sets TCP_NODELAY: without it a small response (a range probe's
        // headers and two bytes) waits for the client's delayed ACK, adding about 40 ms to
        // every stream start.
        match &tls {
            Some(tls) => {
                let limit = connections.clone();
                let mut server = axum_server::from_tcp_rustls(std_listener, tls.config.clone())?
                    .map(|rustls| rustls.acceptor(limit))
                    .handle(handle)
                    .first_request_timeout(header_timeout);
                connection_timeouts!(server.http_builder(), header_timeout);
                servers.spawn(async move { server.serve(service).await });
            }
            None => {
                let mut server = axum_server::from_tcp(std_listener)?
                    .acceptor(connections.clone())
                    .handle(handle)
                    .first_request_timeout(header_timeout);
                connection_timeouts!(server.http_builder(), header_timeout);
                servers.spawn(async move { server.serve(service).await });
            }
        }
    }
    if let (true, Some(address)) = (config.metrics.enabled, config.metrics.listen) {
        let listener = bind(address).await?;
        let router = crate::health::metrics_router(Arc::clone(&health));
        let stop = shutdown.clone();
        servers.spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });
    }
    on_listening(&addresses);

    let advertisement = match (&mdns_host, config.discovery.mdns) {
        (Some(host), true) => {
            let name = api_name(&db, &config).await;
            match discovery::Advertisement::start(
                &name,
                host,
                port,
                discovery::txt_records(&server_id, is_tls, &base_path),
            ) {
                Ok(ad) => Some(ad),
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS advertising is off");
                    None
                }
            }
        }
        _ => None,
    };
    let retention = Retention {
        tombstones: config.sync.tombstone_retention,
        missing: config.scanner.missing_grace,
    };
    tokio::spawn(scheduler::maintenance_loop(
        db.clone(),
        Arc::clone(&media),
        retention,
        shutdown.clone(),
    ));
    tokio::spawn(scheduler::backup_loop(
        db.clone(),
        Arc::clone(&backups),
        config.backup.schedule,
        backup_status,
        shutdown.clone(),
    ));
    tokio::spawn(scheduler::url_loop(refresh, shutdown.clone()));
    tokio::spawn(scheduler::stats_loop(
        scheduler::StatsSources {
            db: db.clone(),
            playback: Arc::clone(local.playback()),
            media: Arc::clone(&media),
            data_dir: config.data_dir.clone(),
            cache_dir: config.cache_dir.clone(),
        },
        shutdown.clone(),
    ));
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    if let Some(interval) = sd_notify::watchdog_enabled() {
        tokio::spawn(scheduler::watchdog_loop(interval, shutdown.clone()));
    }

    shutdown.cancelled().await;
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
    media.shutdown();
    tracing::info!(
        grace_s = config.server.shutdown_grace.as_secs(),
        "stopping; letting open requests finish"
    );
    for handle in &handles {
        handle.graceful_shutdown(Some(config.server.shutdown_grace));
    }
    if let Some(ad) = advertisement {
        ad.stop();
    }
    while let Some(result) = servers.join_next().await {
        if let Err(e) = result.context("listener task failed")? {
            tracing::warn!(error = %e, "listener stopped with an error");
        }
    }
    db.close_writes();
    tokio::task::spawn_blocking(move || db.checkpoint_and_optimize())
        .await
        .context("checkpoint task failed")?
        .context("checkpoint at shutdown failed")?;
    drop(lock);
    tracing::info!("stopped");
    Ok(())
}

async fn api_name(db: &Db, config: &Config) -> String {
    let stored: Option<String> = db
        .read(|c| settings::get(c, "server.name"))
        .await
        .ok()
        .flatten();
    stored
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| config.server.name.clone())
}
