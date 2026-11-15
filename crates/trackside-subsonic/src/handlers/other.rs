//! Scans, users, and the parts of Subsonic Trackside does not offer.

use rusqlite::Connection;
use trackside_auth::Scopes;
use trackside_core::access::Role;
use trackside_core::control::{ScanKind, ScanTrigger};
use trackside_core::repo::users::User;
use trackside_core::{Access, LibraryId, Millis, PublicId, repo};

use crate::error::ApiError;
use crate::map::time;
use crate::response::{Body, Node, Value};
use crate::{Call, Reply};

/// Endpoints answered with an empty list, an error, or a pointer elsewhere.
pub fn is_unsupported(name: &str) -> bool {
    empty_list(name).is_some()
        || matches!(
            name,
            "createshare"
                | "updateshare"
                | "deleteshare"
                | "getpodcastepisode"
                | "refreshpodcasts"
                | "createpodcastchannel"
                | "deletepodcastchannel"
                | "deletepodcastepisode"
                | "downloadpodcastepisode"
                | "createinternetradiostation"
                | "updateinternetradiostation"
                | "deleteinternetradiostation"
                | "getvideoinfo"
                | "getcaptions"
                | "addchatmessage"
                | "jukeboxcontrol"
                | "createuser"
                | "updateuser"
                | "deleteuser"
                | "changepassword"
                | "getanimatedart"
                | "getavatar"
        )
}

fn empty_list(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "getshares" => ("shares", "share"),
        "getpodcasts" => ("podcasts", "channel"),
        "getnewestpodcasts" => ("newestPodcasts", "episode"),
        "getinternetradiostations" => ("internetRadioStations", "internetRadioStation"),
        "getvideos" => ("videos", "video"),
        "getchatmessages" => ("chatMessages", "chatMessage"),
        _ => return None,
    })
}

pub fn unsupported(name: &str) -> Result<Reply, ApiError> {
    if let Some((element, child)) = empty_list(name) {
        return Ok(Body::one(element, Node::new().many(child, Vec::new())).into());
    }
    Err(match name {
        "createuser" | "updateuser" | "deleteuser" | "changepassword" => {
            ApiError::not_authorized("Manage users and passwords in the Trackside dashboard")
        }
        "jukeboxcontrol" => ApiError::not_authorized("Jukebox mode is not available"),
        "getanimatedart" => ApiError::not_found("Animated art"),
        "getavatar" => ApiError::not_found("Avatar"),
        other => ApiError::not_supported(other),
    })
}

fn visible_library_ids(c: &Connection, a: &Access) -> Result<Vec<LibraryId>, ApiError> {
    Ok(repo::libraries::list(c, a)?
        .into_iter()
        .map(|l| l.id)
        .collect())
}

pub async fn scan_status(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let visible = call.read(visible_library_ids).await?;
    Ok(Body::one("scanStatus", status_node(call, &visible)).into())
}

fn status_node(call: &Call<'_>, visible: &[LibraryId]) -> Node {
    let statuses: Vec<_> = call
        .state()
        .scan
        .status()
        .into_iter()
        .filter(|s| visible.contains(&s.library))
        .collect();
    let running: Vec<_> = statuses
        .iter()
        .filter_map(|s| s.scanning.as_ref())
        .collect();
    let scanning = !running.is_empty();
    let (files, folders) = if scanning {
        running.iter().fold((0, 0), |(f, d), p| {
            (f + p.counts.files_seen, d + p.counts.folders_seen)
        })
    } else {
        statuses
            .iter()
            .filter_map(|s| s.last_run.as_ref())
            .fold((0, 0), |(f, d), r| {
                (f + r.counts.files_seen, d + r.counts.folders_seen)
            })
    };
    let last_scan: Option<Millis> = statuses
        .iter()
        .filter_map(|s| s.last_run.as_ref().and_then(|r| r.finished_at))
        .max();
    Node::new()
        .attr("scanning", scanning)
        .attr("count", files)
        .attr("folderCount", folders)
        .attr_opt("lastScan", last_scan.map(time))
}

pub async fn start_scan(call: &Call<'_>) -> Result<Reply, ApiError> {
    if !call.is_admin() {
        return Err(ApiError::not_authorized("Only admins can start a scan"));
    }
    let kind = if call.params.bool("fullScan")?.unwrap_or(false) {
        ScanKind::Full
    } else {
        ScanKind::Quick
    };
    let libraries = call.read(visible_library_ids).await?;
    for library in &libraries {
        if let Err(error) = call
            .state()
            .scan
            .request_scan(library, kind, ScanTrigger::Admin, false)
        {
            tracing::warn!(library = %library, %error, "scan request refused");
        }
    }
    Ok(Body::one(
        "scanStatus",
        status_node(call, &libraries).attr("scanning", true),
    )
    .into())
}

fn user_node(c: &Connection, user: &User) -> Result<Node, ApiError> {
    let admin = user.role == Role::Admin;
    let access = if admin {
        Access::system()
    } else {
        repo::users::access(c, user.key)?.ok_or_else(|| ApiError::not_found("User"))?
    };
    let folders = visible_library_ids(c, &access)?
        .iter()
        .map(|id| Value::from(id.as_str()))
        .collect();
    let p = &user.permissions;
    Ok(Node::new()
        .attr("username", user.username.as_str())
        .attr("email", "")
        .attr("scrobblingEnabled", true)
        .attr_opt("maxBitRate", p.max_bitrate_kbps.filter(|_| !admin))
        .attr("adminRole", admin)
        .attr("settingsRole", false)
        .attr("downloadRole", admin || p.can_download)
        .attr("uploadRole", admin || p.can_upload)
        .attr("playlistRole", true)
        .attr("coverArtRole", false)
        .attr("commentRole", false)
        .attr("podcastRole", false)
        .attr("streamRole", true)
        .attr("jukeboxRole", false)
        .attr("shareRole", false)
        .attr("videoConversionRole", false)
        .texts("folder", folders))
}

pub async fn user(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let username = call.params.required("username")?.to_owned();
    let own = tracks_model::folded(&username) == tracks_model::folded(&call.principal.username);
    if !own && !call.is_admin() {
        return Err(ApiError::not_authorized(
            "You can only look up your own account",
        ));
    }
    let node = call
        .read(move |c, _| {
            let user = repo::users::by_username(c, &username)?
                .ok_or_else(|| ApiError::not_found("User"))?;
            user_node(c, &user)
        })
        .await?;
    Ok(Body::one("user", node).into())
}

pub async fn users(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let admin = call.is_admin();
    let nodes = call
        .read(move |c, a| {
            let users = if admin {
                repo::users::list(c)?
            } else {
                repo::users::get(c, a.user)?.into_iter().collect()
            };
            users
                .iter()
                .map(|u| user_node(c, u))
                .collect::<Result<Vec<_>, _>>()
        })
        .await?;
    Ok(Body::one("users", Node::new().many("user", nodes)).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_endpoints_answer_as_documented() {
        assert!(matches!(unsupported("getpodcasts"), Ok(Reply::Body(_))));
        assert_eq!(unsupported("createshare").err().map(|e| e.code), Some(0));
        assert_eq!(
            unsupported("changepassword").err().map(|e| e.code),
            Some(50)
        );
        assert_eq!(
            unsupported("jukeboxcontrol").err().map(|e| e.code),
            Some(50)
        );
        assert_eq!(
            unsupported("getanimatedart").err().map(|e| e.code),
            Some(70)
        );
        assert_eq!(unsupported("getavatar").err().map(|e| e.code), Some(70));
    }
}
