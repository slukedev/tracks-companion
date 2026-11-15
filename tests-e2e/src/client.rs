//! A small HTTP client that talks to Trackside the way its clients do.

use std::time::{Duration, Instant};

use anyhow::Context;
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// How a request authenticates.
#[derive(Clone, Debug, Default)]
pub enum Auth {
    #[default]
    None,
    /// `Authorization: Bearer <key>`: device keys and API keys.
    Key(String),
    /// The dashboard's session cookie, with the CSRF header and a matching `Origin`.
    Session(String),
}

/// A response read in full.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// From sending the request to the response headers.
    pub headers_after: Duration,
}

impl Reply {
    /// The body as JSON, or `Null` when it is not JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The `error.code` of an error envelope.
    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// The session cookie a sign-in set, as `name=value`.
    pub fn cookie(&self) -> Option<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.contains("trackside_session"))
            .and_then(|v| v.split(';').next())
            .map(str::to_owned)
    }
}

/// One server, at `origin` plus `base_path`.
#[derive(Clone, Debug)]
pub struct Client {
    pub http: reqwest::Client,
    pub origin: String,
    pub base_path: String,
}

impl Client {
    pub fn new(origin: &str, base_path: &str) -> Self {
        crate::install_tls_provider();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            http,
            origin: origin.trim_end_matches('/').to_owned(),
            base_path: base_path.to_owned(),
        }
    }

    /// The absolute URL of a path under the base path (`/api/v1/...`, `/rest/...`).
    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_owned();
        }
        if !self.base_path.is_empty() && path.starts_with(&self.base_path) {
            return format!("{}{path}", self.origin);
        }
        format!("{}{}{path}", self.origin, self.base_path)
    }

    /// The absolute URL of a native API path such as `/me`.
    pub fn api(&self, path: &str) -> String {
        let under_base = !self.base_path.is_empty()
            && path
                .strip_prefix(&self.base_path)
                .is_some_and(|rest| rest.starts_with("/api/"));
        if path.starts_with("/api/") || path.starts_with("http") || under_base {
            self.url(path)
        } else {
            self.url(&format!("/api/v1{path}"))
        }
    }

    /// A request with credentials applied; the caller adds the rest.
    pub fn request(&self, method: Method, url: &str, auth: &Auth) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, url);
        match auth {
            Auth::None => builder,
            Auth::Key(key) => builder.bearer_auth(key),
            Auth::Session(cookie) => builder
                .header("cookie", cookie)
                .header("x-trackside-csrf", "1")
                .header("origin", &self.origin),
        }
    }

    /// Sends a request to a native API path and reads the whole response.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        auth: &Auth,
        body: Option<&Value>,
    ) -> anyhow::Result<Reply> {
        let mut builder = self.request(method.clone(), &self.api(path), auth);
        if let Some(body) = body {
            builder = builder.json(body);
        }
        send(builder)
            .await
            .with_context(|| format!("{method} {path}"))
    }

    pub async fn get(&self, path: &str, auth: &Auth) -> anyhow::Result<Reply> {
        self.call(Method::GET, path, auth, None).await
    }

    pub async fn post(&self, path: &str, auth: &Auth, body: &Value) -> anyhow::Result<Reply> {
        self.call(Method::POST, path, auth, Some(body)).await
    }

    pub async fn put(&self, path: &str, auth: &Auth, body: &Value) -> anyhow::Result<Reply> {
        self.call(Method::PUT, path, auth, Some(body)).await
    }

    pub async fn patch(&self, path: &str, auth: &Auth, body: &Value) -> anyhow::Result<Reply> {
        self.call(Method::PATCH, path, auth, Some(body)).await
    }

    pub async fn delete(&self, path: &str, auth: &Auth) -> anyhow::Result<Reply> {
        self.call(Method::DELETE, path, auth, None).await
    }

    /// Polls `path` until `done` accepts the reply or `timeout` passes.
    pub async fn poll(
        &self,
        path: &str,
        auth: &Auth,
        timeout: Duration,
        done: impl Fn(&Reply) -> bool,
    ) -> anyhow::Result<Reply> {
        let deadline = Instant::now() + timeout;
        loop {
            let reply = self.get(path, auth).await?;
            if done(&reply) {
                return Ok(reply);
            }
            if Instant::now() > deadline {
                anyhow::bail!(
                    "timed out polling {path}; last {}: {}",
                    reply.status,
                    reply.text()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Sends and reads the whole body.
pub async fn send(builder: reqwest::RequestBuilder) -> anyhow::Result<Reply> {
    let started = Instant::now();
    let response = builder.send().await?;
    let headers_after = started.elapsed();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.bytes().await?.to_vec();
    Ok(Reply {
        status,
        headers,
        body,
        headers_after,
    })
}

/// A device as Tracks describes itself when it signs in or pairs.
pub fn device(id: &str, name: &str) -> Value {
    serde_json::json!({
        "client_device_id": id,
        "name": name,
        "platform": "ios",
        "model": "iPhone18,1",
        "app_name": "Tracks",
        "app_version": "3.2"
    })
}
