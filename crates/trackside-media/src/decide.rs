//! The playback decision as a pure function over what the caller knows.

use trackside_core::records::{Codec, SongFile};

use crate::{CachedTranscode, ClientProfile, Decision, DecisionReason, Profile};

/// Everything [`decide`] needs besides the file and the client.
pub(crate) struct DecideContext<'a> {
    /// Profiles the transcoder can produce; empty without one.
    pub profiles: &'a [Profile],
    /// The server's suggestion when the client names none.
    pub default_profile: Profile,
    /// A complete cached transcode for a profile, if there is one.
    pub cached: &'a dyn Fn(Profile) -> Option<CachedTranscode>,
    /// No transcode slot is free right now.
    pub busy: bool,
    /// The song failed to transcode repeatedly and is held back for a while.
    pub failing: bool,
}

/// The file's bitrate, estimated from size and duration when the tags have none.
pub(crate) fn file_bitrate_kbps(file: &SongFile) -> Option<u32> {
    file.format.bitrate_kbps.or_else(|| {
        (file.duration_ms > 0)
            .then(|| (file.size.saturating_mul(8) / u64::from(file.duration_ms)) as u32)
    })
}

fn combined_limit(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn client_plays(client: &ClientProfile, profile: Profile) -> bool {
    let (container, codec) = profile.output();
    let direct = client
        .direct
        .iter()
        .any(|d| d.container == container && d.codecs.contains(&codec));
    direct || (client.hls && codec == Codec::Aac)
}

fn within(profile: Profile, limit: Option<u32>) -> bool {
    match (profile.bitrate_kbps(), limit) {
        (_, None) => true,
        (Some(rate), Some(limit)) => rate <= limit,
        (None, Some(_)) => false,
    }
}

/// Picks the profile to transcode to: the client's preference, then the server default,
/// then AAC, keeping to the bitrate limit; the smallest playable profile when nothing fits.
fn choose_profile(
    client: &ClientProfile,
    limit: Option<u32>,
    ctx: &DecideContext<'_>,
) -> Option<Profile> {
    let order = [
        client.preferred_profile,
        Some(ctx.default_profile),
        Some(Profile::Aac256),
        Some(Profile::Aac128),
    ];
    let usable = |p: &Profile| ctx.profiles.contains(p) && client_plays(client, *p);
    order
        .iter()
        .flatten()
        .copied()
        .find(|p| usable(p) && within(*p, limit))
        .or_else(|| {
            ctx.profiles
                .iter()
                .copied()
                .filter(usable)
                .filter_map(|p| p.bitrate_kbps().map(|rate| (rate, p)))
                .min_by_key(|(rate, _)| *rate)
                .map(|(_, p)| p)
        })
}

/// Direct play when the client plays the original within the bitrate limit (the lower of the
/// client's and the user's), otherwise a cached file, otherwise HLS. Without a transcoder,
/// and when falling back is allowed while it is busy or failing, the original is sent with a
/// reason.
pub(crate) fn decide(
    file: &SongFile,
    client: &ClientProfile,
    user_limit_kbps: Option<u32>,
    ctx: &DecideContext<'_>,
) -> Decision {
    let limit = combined_limit(client.max_bitrate_kbps, user_limit_kbps);
    let playable = client
        .direct
        .iter()
        .any(|d| d.container == file.format.container && d.codecs.contains(&file.format.codec));
    let fits = match (limit, file_bitrate_kbps(file)) {
        (Some(limit), Some(rate)) => rate <= limit,
        _ => true,
    };
    if playable && fits {
        return Decision::Direct { reason: None };
    }
    let reason = if playable {
        DecisionReason::OverBitrate
    } else {
        DecisionReason::UnsupportedCodec
    };
    let Some(profile) = choose_profile(client, limit, ctx) else {
        return Decision::Direct {
            reason: Some(DecisionReason::NoTranscoder),
        };
    };
    if let Some(transcode) = (ctx.cached)(profile) {
        return Decision::File { transcode, reason };
    }
    if client.allow_original_fallback {
        if ctx.failing {
            return Decision::Direct {
                reason: Some(DecisionReason::NoTranscoder),
            };
        }
        if ctx.busy {
            return Decision::Direct {
                reason: Some(DecisionReason::TranscoderBusy),
            };
        }
    }
    Decision::Hls { profile, reason }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use trackside_core::records::{AudioFormat, Container};
    use trackside_core::{LibraryKey, PublicId, SongId};

    use super::*;
    use crate::DirectPlay;

    fn file(container: Container, codec: Codec, kbps: u32) -> SongFile {
        SongFile {
            song: SongId::from_hash(&[7; 32]),
            library: LibraryKey(1),
            path: PathBuf::from("/music/a"),
            size: u64::from(kbps) * 1000 / 8 * 200,
            mtime_ns: 0,
            content_version: 1,
            format: AudioFormat {
                container,
                codec,
                content_type: container.content_type(),
                lossless: codec.is_lossless(),
                bitrate_kbps: Some(kbps),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                size: 0,
            },
            duration_ms: 200_000,
        }
    }

    fn apple(max: Option<u32>, fallback: bool) -> ClientProfile {
        ClientProfile {
            direct: vec![
                DirectPlay {
                    container: Container::Flac,
                    codecs: vec![Codec::Flac],
                },
                DirectPlay {
                    container: Container::M4a,
                    codecs: vec![Codec::Aac, Codec::Alac],
                },
                DirectPlay {
                    container: Container::Mp3,
                    codecs: vec![Codec::Mp3],
                },
            ],
            hls: true,
            max_bitrate_kbps: max,
            preferred_profile: None,
            allow_original_fallback: fallback,
        }
    }

    fn cached_aac(p: Profile) -> CachedTranscode {
        CachedTranscode {
            path: PathBuf::from("/cache/x"),
            size: 10,
            etag: "\"x\"".into(),
            profile: p,
            format: file(Container::M4a, Codec::Aac, 256).format,
        }
    }

    struct Case {
        name: &'static str,
        file: SongFile,
        client: ClientProfile,
        user: Option<u32>,
        profiles: &'static [Profile],
        cached: bool,
        busy: bool,
        expected: Decision,
    }

    fn run(case: &Case) -> Decision {
        let cached = |p: Profile| case.cached.then(|| cached_aac(p));
        let ctx = DecideContext {
            profiles: case.profiles,
            default_profile: Profile::Aac256,
            cached: &cached,
            busy: case.busy,
            failing: false,
        };
        decide(&case.file, &case.client, case.user, &ctx)
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "a table with one entry per case")]
    fn decision_table() {
        let all: &'static [Profile] = &Profile::ALL;
        let flac = || file(Container::Flac, Codec::Flac, 900);
        let cases = vec![
            Case {
                name: "direct within limits",
                file: flac(),
                client: apple(None, true),
                user: None,
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Direct { reason: None },
            },
            Case {
                name: "mp3 under limit is direct",
                file: file(Container::Mp3, Codec::Mp3, 320),
                client: apple(Some(320), true),
                user: None,
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Direct { reason: None },
            },
            Case {
                name: "over bitrate goes to aac-256 hls",
                file: flac(),
                client: apple(Some(320), true),
                user: None,
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Hls {
                    profile: Profile::Aac256,
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "over bitrate with a cached file",
                file: flac(),
                client: apple(Some(320), true),
                user: None,
                profiles: all,
                cached: true,
                busy: false,
                expected: Decision::File {
                    transcode: cached_aac(Profile::Aac256),
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "unsupported codec to file",
                file: file(Container::WavPack, Codec::WavPack, 800),
                client: apple(None, true),
                user: None,
                profiles: all,
                cached: true,
                busy: false,
                expected: Decision::File {
                    transcode: cached_aac(Profile::Aac256),
                    reason: DecisionReason::UnsupportedCodec,
                },
            },
            Case {
                name: "unsupported codec to hls",
                file: file(Container::Ogg, Codec::Vorbis, 160),
                client: apple(None, false),
                user: None,
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Hls {
                    profile: Profile::Aac256,
                    reason: DecisionReason::UnsupportedCodec,
                },
            },
            Case {
                name: "user limit overrides the client",
                file: file(Container::Mp3, Codec::Mp3, 320),
                client: apple(Some(320), true),
                user: Some(192),
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Hls {
                    profile: Profile::Aac128,
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "user limit alone applies",
                file: flac(),
                client: apple(None, true),
                user: Some(256),
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Hls {
                    profile: Profile::Aac256,
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "limit below every profile picks the smallest",
                file: flac(),
                client: apple(Some(64), true),
                user: None,
                profiles: all,
                cached: false,
                busy: false,
                expected: Decision::Hls {
                    profile: Profile::Aac128,
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "busy with fallback is direct",
                file: flac(),
                client: apple(Some(320), true),
                user: None,
                profiles: all,
                cached: false,
                busy: true,
                expected: Decision::Direct {
                    reason: Some(DecisionReason::TranscoderBusy),
                },
            },
            Case {
                name: "busy without fallback is still hls",
                file: flac(),
                client: apple(Some(320), false),
                user: None,
                profiles: all,
                cached: false,
                busy: true,
                expected: Decision::Hls {
                    profile: Profile::Aac256,
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "busy but cached is file",
                file: flac(),
                client: apple(Some(320), true),
                user: None,
                profiles: all,
                cached: true,
                busy: true,
                expected: Decision::File {
                    transcode: cached_aac(Profile::Aac256),
                    reason: DecisionReason::OverBitrate,
                },
            },
            Case {
                name: "no ffmpeg is direct",
                file: flac(),
                client: apple(Some(320), false),
                user: None,
                profiles: &[],
                cached: false,
                busy: false,
                expected: Decision::Direct {
                    reason: Some(DecisionReason::NoTranscoder),
                },
            },
            Case {
                name: "no ffmpeg but playable is plain direct",
                file: flac(),
                client: apple(None, false),
                user: None,
                profiles: &[],
                cached: false,
                busy: false,
                expected: Decision::Direct { reason: None },
            },
        ];
        for case in &cases {
            assert_eq!(run(case), case.expected, "{}", case.name);
        }
    }

    #[test]
    fn preferred_profile_is_used_when_it_fits() {
        let mut client = apple(Some(320), true);
        client.preferred_profile = Some(Profile::Aac128);
        let cached = |_: Profile| None;
        let ctx = DecideContext {
            profiles: &Profile::ALL,
            default_profile: Profile::Aac256,
            cached: &cached,
            busy: false,
            failing: false,
        };
        let decision = decide(
            &file(Container::Flac, Codec::Flac, 900),
            &client,
            None,
            &ctx,
        );
        assert_eq!(
            decision,
            Decision::Hls {
                profile: Profile::Aac128,
                reason: DecisionReason::OverBitrate
            }
        );
    }

    #[test]
    fn failing_song_falls_back_when_allowed() {
        let cached = |_: Profile| None;
        let ctx = DecideContext {
            profiles: &Profile::ALL,
            default_profile: Profile::Aac256,
            cached: &cached,
            busy: false,
            failing: true,
        };
        let decision = decide(
            &file(Container::Flac, Codec::Flac, 900),
            &apple(Some(320), true),
            None,
            &ctx,
        );
        assert_eq!(
            decision,
            Decision::Direct {
                reason: Some(DecisionReason::NoTranscoder)
            }
        );
    }

    #[test]
    fn missing_bitrate_is_estimated_from_size() {
        let mut f = file(Container::Flac, Codec::Flac, 900);
        f.format.bitrate_kbps = None;
        assert_eq!(file_bitrate_kbps(&f), Some(900));
    }
}
