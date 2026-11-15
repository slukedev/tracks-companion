//! Admin wire types (api.md section 16). Where api.md leaves a shape open, these follow the
//! dashboard's fixtures in `Server/dashboard/dev/fixtures/`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Time;
use super::acquisition::{Acquisition, CandidateFormat};
use super::auth::Permissions;

/// `GET /admin/overview`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub server: OverviewServer,
    pub storage: OverviewStorage,
    pub libraries: Vec<OverviewLibrary>,
    pub media: OverviewMedia,
    /// Left out when the server cannot do acquisition at all; `enabled: false` when it can but
    /// it is switched off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<OverviewAcquisition>,
    pub users: OverviewUsers,
    pub backups: OverviewBackups,
    pub problems: Vec<Problem>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewServer {
    pub name: String,
    pub server_id: String,
    pub version: String,
    pub uptime_s: u64,
    pub started_at: String,
    pub data_dir: String,
    /// `off`, `files`, `acme` or `self-signed`.
    pub tls: String,
    pub listen: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewStorage {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub cache_bytes: u64,
    pub data_free_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewLibrary {
    pub id: String,
    pub name: String,
    pub state: super::LibraryState,
    pub song_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan: Option<ScanRun>,
    pub scanning: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ffmpeg {
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcodes {
    pub running: u32,
    pub queued: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewMedia {
    pub ffmpeg: Ffmpeg,
    pub transcodes: Transcodes,
    pub transcode_cache_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OverviewAcquisition {
    pub enabled: bool,
    pub providers: Vec<ProviderStatus>,
    pub active: u32,
    pub today: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewUsers {
    pub count: u64,
    pub devices: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSummary {
    pub name: String,
    pub at: Time,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewBackups {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<BackupSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Something the admin should look at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// `info`, `warning` or `error`.
    pub severity: String,
    pub code: String,
    /// Six words or fewer, for the problem's heading.
    pub title: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// A user as other records name them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRef {
    pub id: String,
    pub username: String,
    pub display_name: String,
}

/// Counts shown on a single user's page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCounts {
    pub devices: u64,
    pub api_keys: u64,
    pub sessions: u64,
}

/// `AdminUser`. `GET /admin/users/{id}` adds the counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUser {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub permissions: Permissions,
    pub library_ids: Vec<String>,
    pub created_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<Time>,
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", flatten)]
    pub counts: Option<UserCounts>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUsers {
    pub users: Vec<AdminUser>,
}

/// Permission flags to set; absent flags keep their value. `max_bitrate_kbps: null` removes
/// the limit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsPatch {
    #[serde(default)]
    pub can_download: Option<bool>,
    #[serde(default)]
    pub can_upload: Option<bool>,
    #[serde(default)]
    pub can_request: Option<bool>,
    #[serde(default)]
    pub legacy_subsonic: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub max_bitrate_kbps: Option<Option<u32>>,
}

/// `Some(None)` for an explicit `null`, `None` for an absent field (with `#[serde(default)]`).
#[expect(
    clippy::option_option,
    reason = "absent and null mean different things in a patch"
)]
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `POST /admin/users`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewUser {
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub permissions: Option<PermissionsPatch>,
    #[serde(default)]
    pub library_ids: Vec<String>,
}

/// `PATCH /admin/users/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPatch {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub permissions: Option<PermissionsPatch>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// `PUT /admin/users/{id}/libraries`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryIds {
    pub library_ids: Vec<String>,
}

/// `POST /admin/users/{id}/password-reset`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetLink {
    pub url: String,
    pub expires_at: Time,
}

/// `POST /admin/users/{id}/sign-out`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignOut {
    #[serde(default)]
    pub devices: bool,
}

/// An invite. `url` and `qr_svg_url` only in the response that creates it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminInvite {
    pub id: String,
    pub role: String,
    pub permissions: Permissions,
    pub library_ids: Vec<String>,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: Time,
    pub expires_at: Time,
    /// `open`, `used`, `expired` or `revoked`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr_svg_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminInvites {
    pub invites: Vec<AdminInvite>,
}

/// `POST /admin/invites`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewInvite {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub permissions: Option<PermissionsPatch>,
    #[serde(default)]
    pub library_ids: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

/// `AdminLibrary`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags on the wire"
)]
pub struct AdminLibrary {
    pub id: String,
    pub name: String,
    pub state: super::LibraryState,
    pub song_count: u64,
    pub root: String,
    pub enabled: bool,
    pub writable: bool,
    pub watch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_scan_interval_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_since: Option<Time>,
    /// `events`, `polling` or `off`.
    pub watching: String,
    pub uploads_target: bool,
    pub acquire_target: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminLibraries {
    pub libraries: Vec<AdminLibrary>,
}

/// `POST /admin/libraries`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewLibrary {
    pub name: String,
    pub root: String,
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub watch: Option<bool>,
    #[serde(default)]
    pub grant_all_users: bool,
}

/// `PATCH /admin/libraries/{id}`. `quick_scan_interval_s: null` returns to the default.
/// `root` cannot change and is refused.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub writable: Option<bool>,
    #[serde(default)]
    pub watch: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub quick_scan_interval_s: Option<Option<u64>>,
    #[serde(default)]
    pub root: Option<String>,
}

/// `POST /admin/libraries/{id}/scan`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRequest {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub force_empty: bool,
}

/// `ScanRun`. `errors` is the number of files that failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRun {
    pub id: String,
    pub library_id: String,
    pub kind: String,
    pub trigger: String,
    pub state: String,
    pub started_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Time>,
    pub folders_seen: u64,
    pub files_seen: u64,
    pub added: u64,
    pub updated: u64,
    pub moved: u64,
    pub missing: u64,
    pub errors: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanError {
    pub path: String,
    pub error: String,
}

/// `GET /admin/scans/{id}`: `ScanRun` with the first 1000 errors in place of their count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanDetail {
    pub id: String,
    pub library_id: String,
    pub kind: String,
    pub trigger: String,
    pub state: String,
    pub started_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Time>,
    pub folders_seen: u64,
    pub files_seen: u64,
    pub added: u64,
    pub updated: u64,
    pub moved: u64,
    pub missing: u64,
    pub errors: Vec<ScanError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scans {
    pub scans: Vec<ScanRun>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagProblem {
    pub song_id: String,
    pub path: String,
    pub problem: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantined {
    pub path: String,
    pub error: String,
    pub at: Time,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableFolder {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Time>,
}

/// `GET /admin/libraries/{id}/problems`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryProblems {
    pub tag_problems: Vec<TagProblem>,
    pub quarantined: Vec<Quarantined>,
    pub unreadable_folders: Vec<UnreadableFolder>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateSong {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub library_id: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub songs: Vec<DuplicateSong>,
}

/// `GET /admin/duplicates`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Duplicates {
    pub groups: Vec<DuplicateGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `PUT /admin/settings/targets` and its answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploads_library: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquire_library: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDevice {
    pub id: String,
    pub name: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    pub user: UserRef,
    pub created_at: Time,
    pub last_seen_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDevices {
    pub devices: Vec<AdminDevice>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminKey {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub user: UserRef,
    pub created_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminKeys {
    pub keys: Vec<AdminKey>,
}

/// A dashboard session. `id` is opaque.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSession {
    pub id: String,
    pub user: UserRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    pub created_at: Time,
    pub last_seen_at: Time,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSessions {
    pub sessions: Vec<AdminSession>,
}

/// One dashboard-editable setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    pub value: serde_json::Value,
    /// `default`, `dashboard` or `config`.
    pub source: String,
    pub editable: bool,
}

/// `GET /admin/settings`: every key marked D in architecture section 11.
pub type Settings = BTreeMap<String, Setting>;

/// A candidate URL with a reachability check from the server's side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUrlStatus {
    pub url: String,
    pub kind: super::auth::UrlKind,
    pub tls: bool,
    /// `detected` or `configured`.
    pub source: String,
    pub reachable: bool,
    pub checked_at: Time,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUrls {
    pub urls: Vec<ServerUrlStatus>,
}

/// A backup file. `kind` is `scheduled`, `manual` or `pre_migrate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backup {
    pub name: String,
    pub kind: String,
    pub at: Time,
    pub bytes: u64,
    pub has_key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backups {
    pub backups: Vec<Backup>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: Time,
    pub actor: String,
    pub ip: String,
    pub action: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// A short sentence for the list, when the action has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Audit {
    pub entries: Vec<AuditEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminImport {
    pub id: String,
    pub file_name: String,
    /// `upload` or `inbox`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserRef>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: Time,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminImports {
    pub imports: Vec<AdminImport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStats {
    pub searches_today: u32,
    pub fetches_today: u32,
    pub failures_today: u32,
}

/// `ProviderStatus`. Admin only: it names the provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub name: String,
    pub enabled: bool,
    pub configured: bool,
    pub health: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_known: Option<String>,
    pub settings: serde_json::Value,
    pub stats: ProviderStats,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Providers {
    pub providers: Vec<ProviderStatus>,
}

/// `PATCH /admin/acquire/providers/{name}`. `api_key` is write-only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPatch {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// `POST /admin/acquire/providers/{name}/test`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTestRequest {
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTestResult {
    pub ok: bool,
    pub elapsed_ms: u64,
    pub candidates: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `PATCH /admin/acquire`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquirePatch {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub instant_play: Option<bool>,
    #[serde(default)]
    pub member_daily: Option<u32>,
    #[serde(default)]
    pub member_active: Option<u32>,
    #[serde(default)]
    pub global_daily: Option<u32>,
    #[serde(default)]
    pub prefer: Option<Vec<String>>,
}

/// The chosen candidate of an acquisition, with its source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminCandidate {
    pub token: String,
    pub provider: String,
    /// The source reference, as text for display.
    pub source: String,
    pub title: String,
    pub artist: String,
    pub score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<CandidateFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// One candidate that was tried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub provider: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An acquisition as admins see it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminAcquisition {
    #[serde(flatten)]
    pub acquisition: Acquisition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<AdminCandidate>,
    pub attempts: Vec<Attempt>,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminAcquisitions {
    pub acquisitions: Vec<AdminAcquisition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One user's plays per day, for `GET /admin/stats/listening`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsUser {
    pub id: String,
    pub username: String,
    pub display_name: String,
    /// Aligned with `days`; `null` where nothing was recorded.
    pub plays: Vec<Option<u32>>,
}

/// `GET /admin/stats/listening`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsListening {
    pub tz: String,
    pub bucket: String,
    pub days: Vec<String>,
    pub users: Vec<StatsUser>,
}

/// `GET /admin/stats/streams`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsStreams {
    pub bucket: String,
    pub hours: Vec<Time>,
    pub streams: Vec<Option<u32>>,
    pub transcodes: Vec<Option<u32>>,
    pub failed: Vec<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode_limit: Option<u32>,
}

/// `GET /admin/stats/storage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsStorage {
    pub bucket: String,
    pub days: Vec<String>,
    pub data_free_bytes: Vec<Option<u64>>,
    pub database_bytes: Vec<Option<u64>>,
    pub cache_bytes: Vec<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_bytes: Option<u64>,
}

/// `GET /admin/stats/requests`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsRequests {
    pub tz: String,
    pub bucket: String,
    pub days: Vec<String>,
    pub added: Vec<Option<u32>>,
    pub failed: Vec<Option<u32>>,
    pub cancelled: Vec<Option<u32>>,
}
