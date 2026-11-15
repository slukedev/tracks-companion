//! Who is asking and what they may see. Every repository read takes an [`Access`].

use std::sync::Arc;

use crate::ids::{LibraryKey, UserKey};

/// A user's role. Admins see every library and reach the admin endpoints.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    /// The value stored in `users.role` and sent on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// Parses a stored or wire value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

/// A user's permission flags. Admins have every permission regardless of these.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags, one per users column"
)]
pub struct Permissions {
    pub can_download: bool,
    pub can_upload: bool,
    pub can_request: bool,
    pub legacy_subsonic: bool,
    pub max_bitrate_kbps: Option<u32>,
}

impl Default for Permissions {
    /// The defaults of the `users` table: downloads and requests on, uploads and legacy
    /// Subsonic off, no bitrate limit.
    fn default() -> Self {
        Self {
            can_download: true,
            can_upload: false,
            can_request: true,
            legacy_subsonic: false,
            max_bitrate_kbps: None,
        }
    }
}

/// The libraries a caller may see.
#[derive(Clone, Debug)]
pub enum LibrarySet {
    All,
    Only(Arc<[LibraryKey]>),
}

impl LibrarySet {
    /// Whether `library` is in the set.
    pub fn contains(&self, library: LibraryKey) -> bool {
        match self {
            Self::All => true,
            Self::Only(keys) => keys.contains(&library),
        }
    }
}

/// What a caller may see. Every repository read takes one and filters by it.
#[derive(Clone, Debug)]
pub struct Access {
    pub user: UserKey,
    pub role: Role,
    pub libraries: LibrarySet,
    /// `users.sync_epoch`, bumped when the visible libraries change.
    pub sync_epoch: i64,
}

impl Access {
    /// Whether the caller may see songs in `library`.
    pub fn can_see(&self, library: LibraryKey) -> bool {
        self.libraries.contains(library)
    }

    /// Whether the caller is an admin.
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    /// The server itself: an admin that sees everything, for background work and the CLI.
    /// Its user key is 0, which no real user has.
    pub fn system() -> Self {
        Self {
            user: UserKey(0),
            role: Role::Admin,
            libraries: LibrarySet::All,
            sync_epoch: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_sees_only_granted_libraries() {
        let access = Access {
            user: UserKey(3),
            role: Role::Member,
            libraries: LibrarySet::Only(Arc::from([LibraryKey(1), LibraryKey(4)])),
            sync_epoch: 1,
        };
        assert!(access.can_see(LibraryKey(4)));
        assert!(!access.can_see(LibraryKey(2)));
        assert!(!access.is_admin());
    }

    #[test]
    fn system_sees_everything() {
        let access = Access::system();
        assert!(access.can_see(LibraryKey(99)));
        assert!(access.is_admin());
    }
}
