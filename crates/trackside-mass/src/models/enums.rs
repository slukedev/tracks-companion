//! Music Assistant's string enums. Every enum here keeps values it does not know as `Other`, so a
//! newer server never fails decoding.

macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        $name:ident $(default $default:ident)? { $($(#[$vmeta:meta])* $variant:ident => $text:literal),* $(,)? }
    ) => {
        $(
            impl Default for $name {
                fn default() -> Self {
                    Self::$default
                }
            }
        )?

        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($(#[$vmeta])* $variant,)*
            /// A value this client does not know, kept as sent.
            Other(String),
        }

        impl $name {
            /// The value as Music Assistant spells it.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $text,)*
                    Self::Other(other) => other,
                }
            }

            /// Parses a value, keeping an unknown one as [`Self::Other`].
            pub fn parse(text: &str) -> Self {
                match text {
                    $($text => Self::$variant,)*
                    other => Self::Other(other.to_owned()),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(match serde_json::Value::deserialize(deserializer)? {
                    serde_json::Value::String(text) => Self::parse(&text),
                    other => Self::Other(other.to_string()),
                })
            }
        }
    };
}

open_enum!(
    /// What kind of media an item or URI is.
    MediaType default Unknown {
        Artist => "artist",
        Album => "album",
        Track => "track",
        Playlist => "playlist",
        Radio => "radio",
        Audiobook => "audiobook",
        Podcast => "podcast",
        PodcastEpisode => "podcast_episode",
        Folder => "folder",
        Genre => "genre",
        Unknown => "unknown",
    }
);

impl MediaType {
    /// The plural used in per-type command names such as `music/tracks/library_items`.
    pub(crate) fn plural(&self) -> String {
        format!("{}s", self.as_str())
    }
}

open_enum!(
    /// Whether a player or queue is playing.
    PlaybackState default Unknown {
        Idle => "idle",
        Paused => "paused",
        Playing => "playing",
        Unknown => "unknown",
    }
);

open_enum!(
    /// What a player is.
    PlayerType default Unknown {
        Player => "player",
        StereoPair => "stereo_pair",
        Group => "group",
        Protocol => "protocol",
        Unknown => "unknown",
    }
);

open_enum!(
    /// A capability a player declares.
    PlayerFeature {
        Power => "power",
        VolumeSet => "volume_set",
        VolumeMute => "volume_mute",
        Pause => "pause",
        SetMembers => "set_members",
        Seek => "seek",
        NextPrevious => "next_previous",
        Enqueue => "enqueue",
        SelectSource => "select_source",
        PlayMedia => "play_media",
        Unknown => "unknown",
    }
);

open_enum!(
    /// How a queue repeats.
    RepeatMode default Off {
        Off => "off",
        One => "one",
        All => "all",
        Unknown => "unknown",
    }
);

open_enum!(
    /// Where `player_queues/play_media` puts the media (models `enums.py`, `QueueOption`).
    QueueOption {
        /// Insert at the current position and play it now.
        Play => "play",
        /// Replace the queue and play from the start.
        Replace => "replace",
        /// Insert after the current (or buffered) item.
        Next => "next",
        /// Replace everything after the current item.
        ReplaceNext => "replace_next",
        /// Append at the end.
        Add => "add",
    }
);

open_enum!(
    /// What a provider does.
    ProviderType default Unknown {
        Music => "music",
        Player => "player",
        Metadata => "metadata",
        Plugin => "plugin",
        Core => "core",
        Unknown => "unknown",
    }
);

open_enum!(
    /// What an image shows.
    ImageType default Thumb {
        Thumb => "thumb",
        Landscape => "landscape",
        Fanart => "fanart",
        Logo => "logo",
        Clearart => "clearart",
        Banner => "banner",
        Cutout => "cutout",
        Back => "back",
        Discart => "discart",
        /// Music Assistant's own `other` image type.
        Misc => "other",
    }
);

open_enum!(
    /// The kind of step a provider setup flow is on.
    FlowStepType default Unknown {
        Form => "form",
        External => "external",
        Progress => "progress",
        Finish => "finish",
        Abort => "abort",
        Unknown => "unknown",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_are_kept() {
        let state: PlaybackState = serde_json::from_str("\"buffering\"").expect("decodes");
        assert_eq!(state, PlaybackState::Other("buffering".into()));
        assert_eq!(
            serde_json::to_string(&state).expect("encodes"),
            "\"buffering\""
        );
    }

    #[test]
    fn non_string_values_do_not_fail() {
        let kind: MediaType = serde_json::from_str("7").expect("decodes");
        assert_eq!(kind, MediaType::Other("7".into()));
    }

    #[test]
    fn known_values_round_trip() {
        for text in ["play", "replace", "next", "replace_next", "add"] {
            assert_eq!(QueueOption::parse(text).as_str(), text);
        }
    }
}
