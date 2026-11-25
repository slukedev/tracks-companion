//! Apple Music developer tokens (api.md sections 7 and 16): the owner saves a `MusicKit` key in
//! the dashboard, sealed like the other secret settings, and Tracks asks this server for a
//! short-lived ES256 token, so the key never ships in an app. One token is signed and shared
//! until it is due for renewal or the key changes.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http::{HeaderValue, StatusCode, header};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use serde::{Deserialize, Serialize};
use trackside_auth::extract::{AdminSession, Authed, ClientIp};
use trackside_auth::{Auth, Scopes};
use trackside_core::Millis;
use trackside_core::repo::settings as stored;
use zeroize::Zeroizing;

use super::{audit, read, require_recent_auth, write};
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, json_with};
use crate::wire::Time;
use crate::wire::admin::{AppleMusicKey, AppleMusicKeyUpload};
use crate::wire::auth::DeveloperToken;

const LIBRARY: u8 = Scopes::LIBRARY.bits();

/// The settings row holding the sealed key, its ids and when it was saved. It is not one of
/// the generic dashboard settings, so `GET /admin/settings` never lists it.
pub const SETTING: &str = "apple_music.developer_key";
/// How long a token lasts. Apple allows up to six months; a day keeps a leaked one short.
pub const TOKEN_LIFETIME: Duration = Duration::from_hours(24);
/// How long before expiry clients are told to ask again, and the server signs a new one.
pub const REFRESH_BEFORE: Duration = Duration::from_hours(1);
/// The longest lifetime this server ever signs.
pub const MAX_LIFETIME: Duration = Duration::from_hours(7 * 24);

pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/me/apple-music/developer-token", get(developer_token))
}

pub(super) fn admin_routes() -> Router<ApiState> {
    Router::new().route(
        "/apple-music",
        get(admin_get).put(admin_put).delete(admin_delete),
    )
}

/// What the settings row holds.
#[derive(Clone, Serialize, Deserialize)]
struct StoredKey {
    /// The PKCS#8 DER, base64, sealed with the server's secret key, then base64.
    sealed: String,
    key_id: String,
    team_id: String,
    set_at: i64,
}

/// A parsed key and the ids that go in its tokens.
struct Signer {
    key: EcdsaKeyPair,
    key_id: String,
    team_id: String,
}

/// A signed token and its times.
#[derive(Clone)]
pub(crate) struct Issued {
    token: String,
    signed_at: Millis,
    expires_at: Millis,
    refresh_after: Millis,
}

struct Loaded {
    /// Names the stored key: the SHA-256 of its sealed text, which a new save always changes.
    identity: [u8; 32],
    /// `None` when the stored key no longer opens or parses.
    signer: Option<Arc<Signer>>,
    token: Option<Issued>,
}

/// The opened key and the current token, shared by every request.
#[derive(Default)]
pub struct DeveloperTokens {
    loaded: Mutex<Option<Loaded>>,
}

impl std::fmt::Debug for DeveloperTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeveloperTokens").finish_non_exhaustive()
    }
}

impl DeveloperTokens {
    fn lock(&self) -> MutexGuard<'_, Option<Loaded>> {
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Brings the cache in line with the stored row: forgets it when there is none, and
    /// opens the key again when it changed.
    fn sync(&self, auth: &Auth, row: Option<&StoredKey>) -> MutexGuard<'_, Option<Loaded>> {
        let mut loaded = self.lock();
        match row {
            None => *loaded = None,
            Some(row) => {
                let identity = super::account::sha256(&row.sealed);
                if loaded.as_ref().map(|l| l.identity) != Some(identity) {
                    *loaded = Some(Loaded {
                        identity,
                        signer: open(auth, row).map(Arc::new),
                        token: None,
                    });
                }
            }
        }
        loaded
    }

    /// The current token, signing a new one when there is none, it is due, or the clock went
    /// back past when it was signed. `None` without a usable key.
    fn token(&self, auth: &Auth, row: Option<&StoredKey>, now: Millis) -> Option<Issued> {
        let mut guard = self.sync(auth, row);
        let loaded = guard.as_mut()?;
        let signer = loaded.signer.clone()?;
        if let Some(token) = &loaded.token
            && token.signed_at <= now
            && now < token.refresh_after
        {
            return Some(token.clone());
        }
        let issued = match sign(&signer, now, TOKEN_LIFETIME) {
            Ok(issued) => issued,
            Err(e) => {
                tracing::warn!(error = %e, "could not sign an Apple Music developer token");
                return None;
            }
        };
        loaded.token = Some(issued.clone());
        Some(issued)
    }
}

/// Why a `.p8` was refused, in words for the person who pasted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError(pub &'static str);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// The PKCS#8 DER of a `.p8` file's text (PEM, or the base64 body alone), checked to be a
/// P-256 key that signs.
pub fn parse_private_key(text: &str) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let text = text.trim();
    if text.len() > 16 * 1024 {
        return Err(KeyError("private_key is too long for a .p8 key."));
    }
    if text.contains("ENCRYPTED PRIVATE KEY") {
        return Err(KeyError(
            "private_key is encrypted. Use the .p8 file as Apple gave it.",
        ));
    }
    if text.contains("BEGIN EC PRIVATE KEY") {
        return Err(KeyError(
            "private_key is an EC key in SEC1 form. Use the .p8 file as Apple gave it.",
        ));
    }
    let body = match (text.find(BEGIN), text.find(END)) {
        (Some(start), Some(end)) if start < end => &text[start + BEGIN.len()..end],
        (None, None) if !text.contains("-----") => text,
        _ => {
            return Err(KeyError(
                "private_key must be the whole .p8 file, from BEGIN PRIVATE KEY to END PRIVATE KEY.",
            ));
        }
    };
    let compact: Zeroizing<String> =
        Zeroizing::new(body.chars().filter(|c| !c.is_whitespace()).collect());
    let der = Zeroizing::new(
        STANDARD
            .decode(compact.as_bytes())
            .map_err(|_| KeyError("private_key is not a .p8 key: its text is not base64."))?,
    );
    key_pair(&der)?;
    Ok(der)
}

fn key_pair(der: &[u8]) -> Result<EcdsaKeyPair, KeyError> {
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &SystemRandom::new()).map_err(
        |_| {
            KeyError(
                "private_key is not a P-256 private key in PKCS#8 form. Use the .p8 file from Apple.",
            )
        },
    )
}

/// A key or team id: 10 letters and digits, as Apple shows them.
fn apple_id(raw: &str, field: &str) -> ApiResult<String> {
    let id = raw.trim().to_ascii_uppercase();
    if id.len() == 10 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(id)
    } else {
        Err(ApiError::bad_field(
            field,
            format!("{field} must be the 10 letters and digits Apple shows."),
        ))
    }
}

/// Signs a `MusicKit` developer token: header `alg` `ES256` and `kid`, claims `iss`, `iat` and
/// `exp` only. Never an `origin` claim: the token is for apps, not web pages.
fn sign(signer: &Signer, now: Millis, lifetime: Duration) -> Result<Issued, KeyError> {
    let lifetime = lifetime.min(MAX_LIFETIME);
    let iat = now.0.div_euclid(1000);
    let exp = iat + i64::try_from(lifetime.as_secs()).unwrap_or(0);
    let header = serde_json::json!({ "alg": "ES256", "kid": signer.key_id });
    let claims = serde_json::json!({ "iss": signer.team_id, "iat": iat, "exp": exp });
    let mut text = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = signer
        .key
        .sign(&SystemRandom::new(), text.as_bytes())
        .map_err(|_| KeyError("signing failed"))?;
    text.push('.');
    text.push_str(&URL_SAFE_NO_PAD.encode(signature.as_ref()));
    let signed_at = Millis(iat * 1000);
    let expires_at = Millis(exp * 1000);
    Ok(Issued {
        token: text,
        signed_at,
        expires_at,
        refresh_after: Millis(
            expires_at.0 - i64::try_from(REFRESH_BEFORE.as_millis()).unwrap_or(0),
        ),
    })
}

/// Signs a token with a PKCS#8 DER key: for tests and tools that check the format.
pub fn sign_with(
    der: &[u8],
    key_id: &str,
    team_id: &str,
    now: Millis,
    lifetime: Duration,
) -> Result<(String, Millis, Millis), KeyError> {
    let signer = Signer {
        key: key_pair(der)?,
        key_id: key_id.to_owned(),
        team_id: team_id.to_owned(),
    };
    let issued = sign(&signer, now, lifetime)?;
    Ok((issued.token, issued.expires_at, issued.refresh_after))
}

/// The stored key opened and parsed, or `None` with a warning when it no longer opens (the
/// secret key was replaced) or parses.
fn open(auth: &Auth, row: &StoredKey) -> Option<Signer> {
    let sealed = STANDARD.decode(&row.sealed).ok()?;
    let plain = match auth.open_setting(SETTING, &sealed) {
        Ok(plain) => Zeroizing::new(plain),
        Err(e) => {
            tracing::warn!(error = %e, "the saved Apple Music key cannot be opened; save it again");
            return None;
        }
    };
    let der = Zeroizing::new(STANDARD.decode(plain.as_bytes()).ok()?);
    match key_pair(&der) {
        Ok(key) => Some(Signer {
            key,
            key_id: row.key_id.clone(),
            team_id: row.team_id.clone(),
        }),
        Err(e) => {
            tracing::warn!(error = %e, "the saved Apple Music key does not parse; save it again");
            None
        }
    }
}

async fn stored_key(state: &ApiState) -> ApiResult<Option<StoredKey>> {
    let value = read(state, |c| stored::get::<serde_json::Value>(c, SETTING)).await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Whether `/me` should advertise `features.apple_music`: a key is saved and opens.
pub(crate) async fn configured(state: &ApiState) -> bool {
    let Ok(row) = stored_key(state).await else {
        return false;
    };
    let tokens = state.local.developer_tokens();
    let loaded = tokens.sync(&state.auth, row.as_ref());
    loaded.as_ref().is_some_and(|l| l.signer.is_some())
}

async fn developer_token(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    state.auth.developer_token_request(&principal)?;
    let row = stored_key(&state).await?;
    let now = state.db.clock().now();
    let issued = state
        .local
        .developer_tokens()
        .token(&state.auth, row.as_ref(), now)
        .ok_or_else(ApiError::not_found)?;
    let mut response = json_with(
        StatusCode::OK,
        &DeveloperToken {
            token: issued.token,
            expires_at: Time(issued.expires_at),
            refresh_after: Time(issued.refresh_after),
        },
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn admin_body(row: Option<&StoredKey>) -> AppleMusicKey {
    match row {
        None => AppleMusicKey::default(),
        Some(row) => AppleMusicKey {
            configured: true,
            key_id: Some(row.key_id.clone()),
            team_id: Some(row.team_id.clone()),
            set_at: Some(Time(Millis(row.set_at))),
        },
    }
}

/// The admin view of a saved key, for the fixture test.
pub fn admin_key_body(key_id: &str, team_id: &str, set_at: Millis) -> AppleMusicKey {
    AppleMusicKey {
        configured: true,
        key_id: Some(key_id.to_owned()),
        team_id: Some(team_id.to_owned()),
        set_at: Some(Time(set_at)),
    }
}

async fn admin_get(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let row = stored_key(&state).await?;
    Ok(json_with(StatusCode::OK, &admin_body(row.as_ref())))
}

async fn admin_put(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<AppleMusicKeyUpload>,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    let key_id = apple_id(&body.key_id, "key_id")?;
    let team_id = apple_id(&body.team_id, "team_id")?;
    let der = parse_private_key(&body.private_key)
        .map_err(|e| ApiError::bad_field("private_key", e.0))?;
    let plain = Zeroizing::new(STANDARD.encode(der.as_slice()));
    let sealed = state
        .auth
        .seal_setting(SETTING, &plain)
        .map_err(|e| ApiError::internal(&e))?;
    let row = StoredKey {
        sealed: STANDARD.encode(sealed),
        key_id: key_id.clone(),
        team_id,
        set_at: state.db.clock().now().0,
    };
    let value = serde_json::to_value(&row).map_err(|e| ApiError::internal(&e))?;
    write(&state, move |tx| stored::put(tx, SETTING, &value)).await?;
    drop(state.local.developer_tokens().sync(&state.auth, Some(&row)));
    audit(
        &state,
        Some(&principal),
        Some(ip),
        "apple_music.update",
        Some(key_id),
        None,
    )
    .await;
    Ok(json_with(StatusCode::OK, &admin_body(Some(&row))))
}

async fn admin_delete(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    write(&state, |tx| stored::delete(tx, SETTING)).await?;
    drop(state.local.developer_tokens().sync(&state.auth, None));
    audit(
        &state,
        Some(&principal),
        Some(ip),
        "apple_music.remove",
        None,
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use ring::signature::{ECDSA_P256_SHA256_FIXED, KeyPair, UnparsedPublicKey};

    use super::*;

    fn pem(der: &[u8]) -> String {
        let body = STANDARD.encode(der);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        )
    }

    fn new_key() -> Vec<u8> {
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .expect("generate")
            .as_ref()
            .to_vec()
    }

    fn part(token: &str, i: usize) -> serde_json::Value {
        let text = token.split('.').nth(i).expect("part");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(text).expect("base64")).expect("json")
    }

    #[test]
    fn tokens_verify_with_the_public_key_and_carry_only_the_expected_claims() {
        let der = new_key();
        let parsed = parse_private_key(&pem(&der)).expect("parses");
        assert_eq!(parsed.as_slice(), der.as_slice());
        let now = Millis(1_794_648_600_123);
        let (token, expires_at, refresh_after) =
            sign_with(&der, "ABC123DEFG", "TEAM456XYZ", now, TOKEN_LIFETIME).expect("signs");
        let header = part(&token, 0);
        assert_eq!(
            header,
            serde_json::json!({"alg": "ES256", "kid": "ABC123DEFG"})
        );
        let claims = part(&token, 1);
        let iat = now.0 / 1000;
        assert_eq!(
            claims,
            serde_json::json!({"iss": "TEAM456XYZ", "iat": iat, "exp": iat + 86_400})
        );
        assert!(claims.get("origin").is_none());
        assert_eq!(expires_at, Millis((iat + 86_400) * 1000));
        assert_eq!(refresh_after, Millis((iat + 86_400 - 3600) * 1000));

        let (message, signature) = token.rsplit_once('.').expect("signed");
        let signature = URL_SAFE_NO_PAD.decode(signature).expect("base64");
        assert_eq!(signature.len(), 64, "raw r||s, as JWS wants");
        let public = key_pair(&der).expect("key").public_key().as_ref().to_vec();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &public)
            .verify(message.as_bytes(), &signature)
            .expect("verifies");
    }

    #[test]
    fn lifetimes_never_pass_seven_days() {
        let der = new_key();
        let now = Millis(1_794_648_600_000);
        let (token, _, _) = sign_with(
            &der,
            "ABC123DEFG",
            "TEAM456XYZ",
            now,
            Duration::from_hours(24 * 365),
        )
        .expect("signs");
        let claims = part(&token, 1);
        let span = claims["exp"].as_i64().unwrap_or(0) - claims["iat"].as_i64().unwrap_or(0);
        assert_eq!(span, 7 * 86_400);
    }

    #[test]
    fn keys_that_are_not_p256_pkcs8_are_refused() {
        let der = new_key();
        assert!(
            parse_private_key(&STANDARD.encode(&der)).is_ok(),
            "bare base64"
        );
        for bad in [
            String::new(),
            String::from("hello"),
            String::from("-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END PRIVATE KEY-----"),
            pem(b"not a key at all"),
            pem(&der[..der.len() - 4]),
            String::from("-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----"),
            String::from(
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----",
            ),
        ] {
            assert!(parse_private_key(&bad).is_err(), "{bad}");
        }
        let p384 = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P384_SHA384_FIXED_SIGNING,
            &SystemRandom::new(),
        )
        .expect("generate");
        assert!(parse_private_key(&pem(p384.as_ref())).is_err(), "P-384");
    }

    #[test]
    fn ids_are_ten_letters_and_digits() {
        assert_eq!(
            apple_id(" abc123defg ", "key_id").ok().as_deref(),
            Some("ABC123DEFG")
        );
        assert!(apple_id("ABC123DEF", "key_id").is_err());
        assert!(apple_id("ABC123DEF!", "key_id").is_err());
    }
}
