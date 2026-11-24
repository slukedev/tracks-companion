//! Setup, sign-in, pairing, invites, resets, the signed-in user, devices and keys (api.md
//! sections 6 and 7).

use serde::{Deserialize, Serialize};

use super::{Library, Time};

/// `GET /setup/status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStatus {
    pub setup_required: bool,
}

/// `POST /setup`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRequest {
    pub setup_token: String,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub password: String,
}

/// `POST /auth/session`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRequest {
    pub username: String,
    pub password: String,
}

/// What a client says about itself when signing in or pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub client_device_id: String,
    pub name: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub app_version: Option<String>,
}

/// `POST /auth/login`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    pub device: DeviceInfo,
}

/// A user's permission flags. `max_bitrate_kbps` is left out when unlimited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags on the wire"
)]
pub struct Permissions {
    pub can_download: bool,
    pub can_upload: bool,
    pub can_request: bool,
    pub legacy_subsonic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
}

/// The user shape of sections 6 and 7.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub permissions: Permissions,
    /// For the dashboard's own session: until when sensitive actions need no password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_auth_until: Option<Time>,
}

/// `POST /auth/reauth`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReauthRequest {
    pub password: String,
}

/// The user's device, as `PairResult` and `/me` name it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// A media token for `mt=`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaToken {
    pub token: String,
    pub expires_at: Time,
}

/// `lan`, `tailnet` or `public`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlKind {
    Lan,
    Tailnet,
    Public,
}

/// A base URL a client may use. It includes `server.base_path`; clients append `/api/v1/...`.
/// `fingerprint` is the SHA-256 of a self-signed certificate, lowercase hex with colons, for
/// pinning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUrl {
    pub url: String,
    pub kind: UrlKind,
    pub tls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// `GET /server/urls`: the addresses a paired device may use, and the version naming this list
/// (also the `ETag`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUrls {
    pub server_id: String,
    pub version: String,
    pub urls: Vec<ServerUrl>,
}

/// The server as `PairResult` and `/me` describe it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSummary {
    pub server_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub urls: Vec<ServerUrl>,
}

/// **`PairResult`**: what every app sign-in ends with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResult {
    pub api_key: String,
    pub device: Device,
    pub user: User,
    pub media_token: MediaToken,
    pub server: ServerSummary,
}

/// `POST /me/pairing-codes`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCodeRequest {
    #[serde(default)]
    pub device_name: Option<String>,
}

/// The created pairing offer. `qr_svg_url` is relative to the server and includes
/// `server.base_path`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    pub id: String,
    pub code: String,
    pub qr_payload: String,
    pub qr_svg_url: String,
    pub expires_at: Time,
}

/// The JSON inside a pairing QR's `d` parameter (architecture section 10). `exp` is Unix
/// seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrPayload {
    pub v: u8,
    pub server_id: String,
    pub name: String,
    pub urls: Vec<ServerUrl>,
    /// The offer's id, for the endpoint proof the app asks for before it sends `code`.
    pub offer: String,
    pub code: String,
    pub exp: i64,
}

/// `POST /pair/claim`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub code: String,
    pub device: DeviceInfo,
}

/// `POST /pair/requests`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequestBody {
    pub device: DeviceInfo,
}

/// The created device-initiated pairing. `code` is six digits with a space in the middle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub id: String,
    pub code: String,
    pub poll_secret: String,
    pub expires_at: Time,
    pub poll_interval_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    Pending,
    Approved,
    Expired,
}

/// `GET /pair/requests/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairPoll {
    pub state: PairState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<PairResult>,
}

/// `POST /pair/requests/approve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveRequest {
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedDevice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// What the approver sees after approving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approved {
    pub device: ApprovedDevice,
}

/// `GET /invites/{token}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteInfo {
    pub valid: bool,
    pub role: String,
    pub expires_at: Time,
}

/// `POST /invites/{token}/accept`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptInvite {
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub password: String,
}

/// `GET /password-resets/{token}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetInfo {
    pub valid: bool,
    pub expires_at: Time,
}

/// `POST /password-resets/{token}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPassword {
    pub password: String,
}

/// `features.acquisition`, present only when every condition of api.md section 7 holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionFeature {
    pub instant_play: bool,
    #[serde(default)]
    pub external_play: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeFeatures {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<AcquisitionFeature>,
}

/// `GET /me`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeResponse {
    pub user: User,
    pub libraries: Vec<Library>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    pub server: ServerSummary,
    pub features: MeFeatures,
}

/// `PATCH /me`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MePatch {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /me/password`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
    #[serde(default)]
    pub sign_out_devices: bool,
}

/// One of the caller's devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    pub created_at: Time,
    pub last_seen_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
    pub current: bool,
}

/// `GET /me/devices`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<DeviceRecord>,
}

/// `PATCH /me/devices/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRename {
    pub name: String,
}

/// An API key or app password, without its secret. `secret` and `username` are filled only
/// in the response that creates it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub created_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// `GET /me/api-keys`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keys {
    pub keys: Vec<KeyRecord>,
}

/// `POST /me/api-keys`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewKey {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
}
