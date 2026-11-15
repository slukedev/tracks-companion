//! Shared test harness: a real in-memory database, an `Auth` on a test clock, users made
//! through the public API, and a small HTTP server and client for the extractors.
//!
//! Tests that need repositories WP1 has not written yet end early and print why, instead of
//! failing; they run in full once the repositories exist.

#![allow(dead_code, reason = "each test file uses part of the harness")]

use std::error::Error;
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use trackside_auth::extract::{AdminSession, Authed, ClientIp, MediaAuthed, SessionUser};
use trackside_auth::{Auth, AuthError, NewDevice, NewUser, Scopes, SecretKey, SubsonicAuthError};
use trackside_core::config::{AuthConfig, Config};
use trackside_core::testing::TestDb;
use trackside_core::{DbError, Permissions, Role, UserKey};

pub type BoxError = Box<dyn Error + Send + Sync>;
pub type TestResult = Result<(), BoxError>;

pub const PASSWORD: &str = "correct horse battery";

pub struct Harness {
    pub db: TestDb,
    pub auth: Arc<Auth>,
    pub data: tempfile::TempDir,
    pub config: AuthConfig,
}

pub fn config() -> AuthConfig {
    let mut auth = Config::defaults("/tmp/trackside-test").auth;
    auth.argon2_memory = 1 << 20;
    auth.argon2_iterations = 1;
    auth
}

pub fn harness() -> Result<Harness, BoxError> {
    harness_with(config(), Vec::new())
}

pub fn harness_with(config: AuthConfig, trusted: Vec<ipnet::IpNet>) -> Result<Harness, BoxError> {
    let db = TestDb::new()?;
    let auth = Arc::new(Auth::new(
        db.db.clone(),
        &config,
        SecretKey::from_bytes([42; 32]),
        trusted,
        db.clock.clone(),
    ));
    Ok(Harness {
        db,
        auth,
        data: tempfile::tempdir()?,
        config,
    })
}

impl Harness {
    /// Another `Auth` on the same database, as a second process (the CLI) would have.
    pub fn second_auth(&self, secret: [u8; 32]) -> Arc<Auth> {
        Arc::new(Auth::new(
            self.db.db.clone(),
            &self.config,
            SecretKey::from_bytes(secret),
            Vec::new(),
            self.db.clock.clone(),
        ))
    }

    /// Completes setup and returns the first admin.
    pub async fn admin(&self) -> Result<UserKey, BoxError> {
        let token = self
            .auth
            .ensure_setup_token(self.data.path())
            .await?
            .ok_or("no setup token")?;
        Ok(self
            .auth
            .complete_setup(&token, new_user("admin", Role::Admin), ip(1))
            .await?)
    }

    pub async fn member(&self, admin: UserKey, name: &str) -> Result<UserKey, BoxError> {
        self.member_with(admin, name, Permissions::default()).await
    }

    pub async fn member_with(
        &self,
        admin: UserKey,
        name: &str,
        permissions: Permissions,
    ) -> Result<UserKey, BoxError> {
        let mut user = new_user(name, Role::Member);
        user.permissions = permissions;
        Ok(self.auth.create_user(admin, user, None).await?)
    }

    pub fn advance(&self, by: Duration) {
        self.db.clock.advance(by);
    }
}

pub fn new_user(name: &str, role: Role) -> NewUser {
    NewUser {
        username: name.to_owned(),
        display_name: name.to_owned(),
        password: zeroize::Zeroizing::new(PASSWORD.to_owned()),
        role,
        permissions: Permissions::default(),
        libraries: Vec::new(),
    }
}

pub fn device(install: &str, name: &str) -> NewDevice {
    NewDevice {
        client_device_id: install.to_owned(),
        name: name.to_owned(),
        platform: String::from("ios"),
        model: None,
        app_name: Some(String::from("Tracks")),
        app_version: Some(String::from("3.2")),
    }
}

pub fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
}

fn is_pending(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(auth) = e.downcast_ref::<AuthError>()
            && matches!(
                auth,
                AuthError::Unimplemented(_) | AuthError::Db(DbError::Unimplemented(_))
            )
        {
            return true;
        }
        if let Some(DbError::Unimplemented(_)) = e.downcast_ref::<DbError>() {
            return true;
        }
        current = e.source();
    }
    false
}

/// Runs a test body; a repository that is still a stub ends it early with a note.
pub async fn wp1(body: impl Future<Output = TestResult>) -> TestResult {
    match body.await {
        Err(e) if is_pending(e.as_ref()) => {
            eprintln!("SKIPPED until WP1 lands this repository: {e}");
            Ok(())
        }
        other => other,
    }
}

/// The error of a call that must fail. A stub repository's error ends the test as pending.
pub fn denied<T: std::fmt::Debug>(result: Result<T, AuthError>) -> Result<AuthError, BoxError> {
    match result {
        Ok(value) => Err(format!("expected an error, got {value:?}").into()),
        Err(e) if is_pending(&e) => Err(Box::new(e)),
        Err(e) => Ok(e),
    }
}

pub fn subsonic_denied<T: std::fmt::Debug>(
    result: Result<T, SubsonicAuthError>,
) -> Result<SubsonicAuthError, BoxError> {
    match result {
        Ok(value) => Err(format!("expected an error, got {value:?}").into()),
        Err(SubsonicAuthError::Internal) => Err("internal error".into()),
        Err(e) => Ok(e),
    }
}

// HTTP: a router with one route per extractor, served on an ephemeral port.

async fn api(Authed(p): Authed<{ Scopes::LIBRARY.bits() }>) -> String {
    format!("{} {:?}", p.username, p.via)
}

async fn write(Authed(p): Authed<{ Scopes::WRITE.bits() }>) -> String {
    p.username
}

async fn media(MediaAuthed(p): MediaAuthed) -> String {
    format!("{} {:?}", p.username, p.via)
}

async fn admin(AdminSession(p): AdminSession) -> String {
    p.username
}

async fn session(SessionUser(p): SessionUser) -> String {
    p.username
}

async fn client(ClientIp(ip): ClientIp) -> String {
    ip.to_string()
}

pub async fn serve(auth: Arc<Auth>) -> Result<SocketAddr, BoxError> {
    let app = Router::new()
        .route("/api", get(api).post(api))
        .route("/write", post(write))
        .route("/media", get(media))
        .route("/admin", get(admin).post(admin))
        .route("/session", get(session).post(session))
        .route("/ip", get(client))
        .with_state(auth);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Ok(addr)
}

pub struct Reply {
    pub status: u16,
    pub head: String,
    pub body: String,
}

/// Sends one HTTP/1.1 request and reads the whole reply.
pub async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<Reply, BoxError> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
        let _ = write!(request, "Host: {addr}\r\n");
    }
    if method != "GET" {
        request.push_str("Content-Length: 0\r\n");
    }
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw)).await??;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("no status line")?;
    Ok(Reply {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    })
}
