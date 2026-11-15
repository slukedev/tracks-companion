//! Ranking candidates (architecture section 9.1): hard filters, then a score.

use tracks_model::{credits_by_name, folded, search_folded};
use trackside_core::control::Quality;
use trackside_core::records::Container;

use crate::provider::{Candidate, ProviderName, SearchQuery, SourceHints};

/// Ranking options from configuration and the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankOptions {
    pub allow_versions: bool,
    pub reject_words: Vec<String>,
    pub prefer: Vec<ProviderName>,
    pub max_file_bytes: u64,
    pub min_lossy_kbps: u32,
}

/// A candidate with its score; rejected candidates are not returned.
#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub candidate: Candidate,
    pub score: f32,
    pub exact: bool,
}

const LOSSLESS_BONUS: f32 = 40.0;
const EXACT_TITLE_BONUS: f32 = 20.0;
const EXACT_ARTIST_BONUS: f32 = 15.0;
const ALBUM_BONUS: f32 = 10.0;
const DURATION_PENALTY_PER_SECOND: f32 = 2.0;
const PREFERRED_PROVIDER_BONUS: f32 = 10.0;
const TOPIC_BONUS: f32 = 15.0;
const OFFICIAL_AUDIO_BONUS: f32 = 5.0;
const MUSIC_VIDEO_PENALTY: f32 = 5.0;
const FREE_SLOT_BONUS: f32 = 10.0;
const MAX_QUEUE_PENALTY: f32 = 10.0;
const MAX_SPEED_BONUS: f32 = 5.0;
/// Upload speed, in bytes a second, that earns the full speed bonus.
const FULL_SPEED: f32 = 1_000_000.0;
/// What lossy video audio usually is when the source does not say.
const ASSUMED_VIDEO_KBPS: u32 = 128;

/// Words in bracketed title suffixes that are noise, not part of the song's name.
const NOISE_WORDS: &[&str] = &[
    "feat",
    "ft",
    "featuring",
    "with",
    "official",
    "audio",
    "video",
    "lyric",
    "lyrics",
    "hd",
    "hq",
    "4k",
    "visualizer",
    "visualiser",
    "music",
    "mv",
    "explicit",
    "clean",
];

/// Containers a fetched file may have.
pub const ALLOWED_CONTAINERS: &[Container] = &[
    Container::Flac,
    Container::M4a,
    Container::Mp3,
    Container::Ogg,
    Container::Opus,
    Container::Wav,
    Container::Aiff,
];

/// The quality class Tracks sees.
pub fn quality_of(candidate: &Candidate) -> Quality {
    if candidate.lossless {
        Quality::Lossless
    } else if candidate.bitrate_kbps.is_some_and(|kbps| kbps >= 256) {
        Quality::High
    } else {
        Quality::Standard
    }
}

/// Filters and orders candidates by architecture section 9.1, best first.
pub fn rank(query: &SearchQuery, candidates: Vec<Candidate>, options: &RankOptions) -> Vec<Ranked> {
    let median = median_duration(&candidates);
    let wanted = Wanted::new(query);
    let mut ranked: Vec<Ranked> = candidates
        .into_iter()
        .filter_map(|candidate| {
            let facts = Facts::of(&candidate, &wanted);
            passes(&candidate, &facts, &wanted, query, median, options).then(|| {
                let score = score(&candidate, &facts, query, options);
                Ranked {
                    exact: facts.exact_title && facts.exact_artist,
                    candidate,
                    score,
                }
            })
        })
        .collect();
    // At equal scores (320 kbps lossy earns as much as lossless), lossless goes first.
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.candidate.lossless.cmp(&a.candidate.lossless))
    });
    ranked
}

/// The request, folded once.
struct Wanted {
    title: String,
    /// The request's lead identity; empty for free-text queries.
    artist: String,
    album: Option<String>,
    /// Words of a free-text query.
    words: Vec<String>,
    /// Folded words of the request title, to tell a requested "Live" from an unwanted one.
    title_words: Vec<String>,
}

impl Wanted {
    fn new(query: &SearchQuery) -> Self {
        let lead = tracks_model::credits::lead(&query.artist);
        let title = clean_title(&query.title);
        Self {
            title_words: words(&search_folded(&query.title)),
            title,
            artist: folded(&lead),
            album: query.album.as_deref().map(folded).filter(|a| !a.is_empty()),
            words: query
                .free_text
                .as_deref()
                .map(|text| words(&search_folded(text)))
                .unwrap_or_default(),
        }
    }

    fn is_free_text(&self) -> bool {
        self.artist.is_empty() && self.title.is_empty()
    }
}

/// What ranking learns about one candidate.
#[expect(clippy::struct_excessive_bools, reason = "independent match facts")]
struct Facts {
    exact_title: bool,
    title_matches: bool,
    exact_artist: bool,
    artist_matches: bool,
    album_matches: bool,
}

impl Facts {
    fn of(candidate: &Candidate, wanted: &Wanted) -> Self {
        if wanted.is_free_text() {
            let haystack = words(&search_folded(&format!(
                "{} {} {}",
                candidate.artist,
                candidate.title,
                folder_of(candidate)
            )));
            let all = wanted.words.iter().all(|w| haystack.contains(w));
            return Self {
                exact_title: false,
                title_matches: all,
                exact_artist: false,
                artist_matches: all,
                album_matches: false,
            };
        }
        let title = clean_title(&candidate.title);
        let exact_title = !title.is_empty() && title == wanted.title;
        let title_matches =
            exact_title || (!wanted.title.is_empty() && title.contains(&wanted.title));

        let credits = credits_by_name(&candidate.artist, &candidate.title);
        let exact_artist = credits
            .first()
            .is_some_and(|credit| credit.identity == wanted.artist)
            || is_topic_of(candidate, &wanted.artist);
        let artist_matches = exact_artist
            || credits
                .iter()
                .any(|credit| credit.identity == wanted.artist)
            || folder_names(candidate).any(|name| name == wanted.artist);
        let album_matches = match (&wanted.album, &candidate.album) {
            (Some(want), Some(have)) => &folded(have) == want,
            _ => false,
        };
        Self {
            exact_title,
            title_matches,
            exact_artist,
            artist_matches,
            album_matches,
        }
    }
}

fn passes(
    candidate: &Candidate,
    facts: &Facts,
    wanted: &Wanted,
    query: &SearchQuery,
    median: Option<u32>,
    options: &RankOptions,
) -> bool {
    if candidate
        .container
        .is_some_and(|c| !ALLOWED_CONTAINERS.contains(&c))
    {
        return false;
    }
    if candidate
        .size
        .is_some_and(|size| size > options.max_file_bytes)
    {
        return false;
    }
    if !candidate.lossless
        && candidate
            .bitrate_kbps
            .is_some_and(|kbps| kbps < options.min_lossy_kbps)
    {
        return false;
    }
    if !duration_fits(candidate.duration_ms, query.duration_ms, median) {
        return false;
    }
    if !options.allow_versions && has_unwanted_version(candidate, wanted, &options.reject_words) {
        return false;
    }
    facts.artist_matches && facts.title_matches
}

/// Within `max(3 s, 3 %)` of the request, or within 20 % of the median candidate.
pub fn duration_fits(found: Option<u32>, requested: Option<u32>, median: Option<u32>) -> bool {
    let Some(found) = found else {
        return true;
    };
    match (requested, median) {
        (Some(requested), _) => found.abs_diff(requested) <= tolerance_ms(requested),
        (None, Some(median)) => u64::from(found.abs_diff(median)) * 5 <= u64::from(median),
        (None, None) => true,
    }
}

/// `max(3 s, 3 %)` of a duration.
pub fn tolerance_ms(duration_ms: u32) -> u32 {
    (duration_ms / 100 * 3).max(3_000)
}

fn median_duration(candidates: &[Candidate]) -> Option<u32> {
    let mut durations: Vec<u32> = candidates.iter().filter_map(|c| c.duration_ms).collect();
    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    Some(durations[durations.len() / 2])
}

fn has_unwanted_version(candidate: &Candidate, wanted: &Wanted, reject_words: &[String]) -> bool {
    let text = words(&search_folded(&format!(
        "{} {}",
        candidate.title,
        folder_of(candidate)
    )));
    reject_words.iter().any(|phrase| {
        let phrase = words(&search_folded(phrase));
        !phrase.is_empty()
            && contains_run(&text, &phrase)
            && !contains_run(&wanted.title_words, &phrase)
    })
}

fn score(candidate: &Candidate, facts: &Facts, query: &SearchQuery, options: &RankOptions) -> f32 {
    let mut score = if candidate.lossless {
        LOSSLESS_BONUS
    } else {
        let kbps = candidate.bitrate_kbps.unwrap_or(match candidate.hints {
            SourceHints::Video { .. } => ASSUMED_VIDEO_KBPS,
            SourceHints::Peer { .. } => 0,
        });
        kbps.min(320) as f32 / 8.0
    };
    if facts.exact_title {
        score += EXACT_TITLE_BONUS;
    }
    if facts.exact_artist {
        score += EXACT_ARTIST_BONUS;
    }
    if facts.album_matches {
        score += ALBUM_BONUS;
    }
    if let (Some(found), Some(requested)) = (candidate.duration_ms, query.duration_ms) {
        score -= found.abs_diff(requested) as f32 / 1000.0 * DURATION_PENALTY_PER_SECOND;
    }
    if options.prefer.first() == Some(&candidate.provider) {
        score += PREFERRED_PROVIDER_BONUS;
    }
    match &candidate.hints {
        SourceHints::Video {
            topic_channel,
            official,
            music_video,
            ..
        } => {
            if *topic_channel {
                score += TOPIC_BONUS;
            }
            if *official {
                score += OFFICIAL_AUDIO_BONUS;
            }
            if *music_video {
                score -= MUSIC_VIDEO_PENALTY;
            }
        }
        SourceHints::Peer {
            free_slot,
            queue_length,
            upload_speed,
            ..
        } => {
            if *free_slot {
                score += FREE_SLOT_BONUS;
            }
            score -= (*queue_length as f32).min(MAX_QUEUE_PENALTY);
            score += (*upload_speed as f32 / FULL_SPEED).min(1.0) * MAX_SPEED_BONUS;
        }
    }
    score
}

/// A title folded with bracketed credits and noise ("(feat. B)", "[Official Audio]") removed.
pub fn clean_title(title: &str) -> String {
    let mut kept = String::with_capacity(title.len());
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(close) = rest[open..].find(close_char).map(|i| open + i) else {
            break;
        };
        kept.push_str(&rest[..open]);
        let inside = &rest[open + 1..close];
        if !is_noise(inside) {
            kept.push(' ');
            kept.push_str(inside);
            kept.push(' ');
        }
        rest = &rest[close + 1..];
    }
    kept.push_str(rest);
    let folded = search_folded(&kept);
    let mut out = words(&folded);
    while out.len() > 2 && is_trailing_noise(&out) {
        out.truncate(out.len() - 2);
    }
    if let Some(at) = out
        .iter()
        .position(|w| matches!(w.as_str(), "feat" | "ft" | "featuring"))
        && at > 0
    {
        out.truncate(at);
    }
    out.join(" ")
}

fn is_noise(inside: &str) -> bool {
    let inside = words(&search_folded(inside));
    inside
        .first()
        .is_some_and(|first| NOISE_WORDS.contains(&first.as_str()))
}

fn is_trailing_noise(words: &[String]) -> bool {
    let n = words.len();
    let pair = (words[n - 2].as_str(), words[n - 1].as_str());
    matches!(
        pair,
        ("official", "audio" | "video") | ("lyric" | "music", "video")
    )
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn is_topic_of(candidate: &Candidate, artist: &str) -> bool {
    match &candidate.hints {
        SourceHints::Video { uploader, .. } => {
            let uploader = folded(uploader);
            uploader
                .strip_suffix(" - topic")
                .is_some_and(|name| name == artist)
        }
        SourceHints::Peer { .. } => false,
    }
}

fn folder_of(candidate: &Candidate) -> &str {
    match &candidate.hints {
        SourceHints::Peer { folder, .. } => folder,
        SourceHints::Video { .. } => "",
    }
}

fn folder_names(candidate: &Candidate) -> impl Iterator<Item = String> + '_ {
    folder_of(candidate)
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .map(folded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::records::Codec;

    fn query(title: &str, artist: &str, duration_ms: Option<u32>) -> SearchQuery {
        SearchQuery {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration_ms,
            free_text: None,
        }
    }

    fn options() -> RankOptions {
        RankOptions {
            allow_versions: false,
            reject_words: trackside_core::Config::defaults("/data")
                .acquire
                .reject_words,
            prefer: vec![ProviderName::Soulseek, ProviderName::Ytdlp],
            max_file_bytes: 1 << 30,
            min_lossy_kbps: 256,
        }
    }

    fn video(title: &str, artist: &str, uploader: &str, duration_ms: u32) -> Candidate {
        Candidate {
            provider: ProviderName::Ytdlp,
            provider_ref: serde_json::json!({ "id": format!("{title}{uploader}") }),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration_ms: Some(duration_ms),
            container: None,
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            lossless: false,
            size: None,
            hints: SourceHints::Video {
                uploader: uploader.to_owned(),
                topic_channel: uploader.ends_with(" - Topic"),
                official: false,
                music_video: false,
            },
        }
    }

    fn peer(title: &str, artist: &str, folder: &str, lossless: bool, kbps: u32) -> Candidate {
        Candidate {
            provider: ProviderName::Soulseek,
            provider_ref: serde_json::json!({ "file": format!("{folder}{title}{lossless}") }),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration_ms: Some(200_000),
            container: Some(if lossless {
                Container::Flac
            } else {
                Container::Mp3
            }),
            codec: Some(if lossless { Codec::Flac } else { Codec::Mp3 }),
            bitrate_kbps: Some(kbps),
            sample_rate: Some(44_100),
            bit_depth: lossless.then_some(16),
            lossless,
            size: Some(10_000_000),
            hints: SourceHints::Peer {
                username: String::from("peer"),
                folder: folder.to_owned(),
                free_slot: true,
                queue_length: 0,
                upload_speed: 500_000,
            },
        }
    }

    fn titles(ranked: &[Ranked]) -> Vec<String> {
        ranked.iter().map(|r| r.candidate.title.clone()).collect()
    }

    #[test]
    fn version_words_are_rejected_unless_allowed() {
        let q = query("Halo", "Beyoncé", Some(261_000));
        let candidates = vec![
            video("Halo (Live)", "Beyoncé", "Beyoncé - Topic", 261_000),
            video("Halo (Remix)", "Beyoncé", "Beyoncé - Topic", 261_000),
            video("Halo Karaoke Version", "Beyoncé", "KaraokeHits", 261_000),
            video("Halo", "Beyoncé", "Beyoncé - Topic", 261_000),
        ];
        assert_eq!(titles(&rank(&q, candidates.clone(), &options())), ["Halo"]);
        let allow = RankOptions {
            allow_versions: true,
            ..options()
        };
        assert_eq!(rank(&q, candidates, &allow).len(), 4);
    }

    #[test]
    fn a_requested_version_word_is_not_rejected() {
        let q = query("Halo (Live)", "Beyoncé", None);
        let ranked = rank(
            &q,
            vec![video("Halo (Live)", "Beyoncé", "Beyoncé - Topic", 261_000)],
            &options(),
        );
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn version_words_in_a_peer_folder_are_rejected() {
        let q = query("Halo", "Beyoncé", None);
        let ranked = rank(
            &q,
            vec![peer(
                "Halo",
                "Beyoncé",
                "Music\\Beyoncé\\Live at Wembley",
                true,
                900,
            )],
            &options(),
        );
        assert_eq!(ranked.len(), 0);
    }

    #[test]
    fn duration_tolerance_is_three_seconds_or_three_percent() {
        assert!(duration_fits(Some(62_000), Some(59_000), None));
        assert!(!duration_fits(Some(62_001), Some(59_000), None));
        assert!(duration_fits(Some(206_000), Some(200_000), None));
        assert!(!duration_fits(Some(206_001), Some(200_000), None));
        assert!(duration_fits(Some(582_000), Some(600_000), None));
        assert!(!duration_fits(Some(581_999), Some(600_000), None));
        assert!(duration_fits(None, Some(600_000), None));
    }

    #[test]
    fn without_a_requested_duration_outliers_from_the_median_are_dropped() {
        let q = query("Halo", "Beyoncé", None);
        let candidates = vec![
            video("Halo", "Beyoncé", "Beyoncé - Topic", 261_000),
            video("Halo", "Beyoncé", "Beyoncé", 262_000),
            video("Halo", "Beyoncé", "Fan", 600_000),
        ];
        let ranked = rank(&q, candidates, &options());
        assert_eq!(ranked.len(), 2);
        assert!(
            ranked
                .iter()
                .all(|r| r.candidate.duration_ms != Some(600_000))
        );
    }

    #[test]
    fn topic_channel_is_preferred() {
        let q = query("Halo", "Beyoncé", Some(261_000));
        let ranked = rank(
            &q,
            vec![
                video("Beyoncé - Halo", "Beyoncé", "SomeUploader", 261_000),
                video("Halo", "Beyoncé", "Beyoncé - Topic", 261_000),
            ],
            &options(),
        );
        assert_eq!(ranked.len(), 2);
        let SourceHints::Video { uploader, .. } = &ranked[0].candidate.hints else {
            panic!("expected a video");
        };
        assert_eq!(uploader, "Beyoncé - Topic");
    }

    #[test]
    fn lossless_is_preferred_over_lossy() {
        let q = query("Halo", "Beyoncé", Some(200_000));
        let ranked = rank(
            &q,
            vec![
                peer("Halo", "Beyoncé", "Beyoncé\\Album", false, 320),
                peer("Halo", "Beyoncé", "Beyoncé\\Album", true, 900),
            ],
            &options(),
        );
        assert!(ranked[0].candidate.lossless);
        assert_eq!(quality_of(&ranked[0].candidate), Quality::Lossless);
        assert_eq!(quality_of(&ranked[1].candidate), Quality::High);
    }

    #[test]
    fn low_bitrate_lossy_files_are_dropped() {
        let q = query("Halo", "Beyoncé", None);
        let ranked = rank(
            &q,
            vec![peer("Halo", "Beyoncé", "x", false, 128)],
            &options(),
        );
        assert_eq!(ranked.len(), 0);
    }

    #[test]
    fn featured_credits_match_through_tracks_model() {
        let q = query("Crazy in Love", "Beyoncé", None);
        let ranked = rank(
            &q,
            vec![
                video(
                    "Crazy in Love (feat. JAY-Z)",
                    "Beyoncé feat. JAY-Z",
                    "Beyoncé",
                    236_000,
                ),
                video("Crazy in Love", "Someone Else", "Someone Else", 236_000),
            ],
            &options(),
        );
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].exact);
    }

    #[test]
    fn a_featured_artist_alone_matches_but_not_exactly() {
        let q = query("Crazy in Love", "JAY-Z", None);
        let ranked = rank(
            &q,
            vec![video(
                "Crazy in Love",
                "Beyoncé feat. JAY-Z",
                "Beyoncé",
                236_000,
            )],
            &options(),
        );
        assert_eq!(ranked.len(), 1);
        assert!(!ranked[0].exact);
    }

    #[test]
    fn a_requested_feat_title_matches_the_plain_title() {
        let q = query("Crazy in Love (feat. JAY-Z)", "Beyoncé", None);
        let ranked = rank(
            &q,
            vec![video(
                "Crazy In Love [Official Audio]",
                "Beyoncé",
                "Beyoncé",
                236_000,
            )],
            &options(),
        );
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].exact);
    }

    #[test]
    fn artist_in_a_peer_folder_name_matches() {
        let q = query("Halo", "Beyoncé", None);
        let ranked = rank(
            &q,
            vec![peer("Halo", "Unknown", "Music\\Beyonce\\I Am", true, 900)],
            &options(),
        );
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn oversized_and_disallowed_containers_are_dropped() {
        let q = query("Halo", "Beyoncé", None);
        let mut big = peer("Halo", "Beyoncé", "x", true, 900);
        big.size = Some(2 << 30);
        let mut wma = peer("Halo", "Beyoncé", "x", true, 900);
        wma.container = Some(Container::Ape);
        assert_eq!(rank(&q, vec![big, wma], &options()).len(), 0);
    }

    #[test]
    fn the_first_preferred_provider_gets_a_bonus() {
        let q = query("Halo", "Beyoncé", None);
        let a = peer("Halo", "Beyoncé", "x", true, 900);
        let base = rank(&q, vec![a.clone()], &options())[0].score;
        let prefer_ytdlp = RankOptions {
            prefer: vec![ProviderName::Ytdlp],
            ..options()
        };
        let other = rank(&q, vec![a], &prefer_ytdlp)[0].score;
        assert!((base - other - PREFERRED_PROVIDER_BONUS).abs() < f32::EPSILON);
    }

    #[test]
    fn free_text_needs_every_word() {
        let q = SearchQuery {
            title: String::new(),
            artist: String::new(),
            album: None,
            duration_ms: None,
            free_text: Some(String::from("beyonce halo")),
        };
        let ranked = rank(
            &q,
            vec![
                video("Halo", "Beyoncé", "Beyoncé - Topic", 261_000),
                video("Halo", "Lustra", "Lustra", 261_000),
            ],
            &options(),
        );
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn clean_title_drops_bracketed_credits_and_noise() {
        assert_eq!(clean_title("Halo (feat. X) [Official Video]"), "halo");
        assert_eq!(clean_title("Halo - Official Audio"), "halo");
        assert_eq!(clean_title("Halo (Acoustic)"), "halo acoustic");
        assert_eq!(clean_title("Song ft. Somebody"), "song");
    }
}
