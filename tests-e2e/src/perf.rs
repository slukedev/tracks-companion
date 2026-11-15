//! The performance harness: a synthetic library of tiny valid audio files with realistic tag
//! distributions, the budgets of architecture section 15 measured against the real binary,
//! a JSON report, and the comparison with a stored baseline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, ItemKey, Tag, TagExt, TagType};
use serde::{Deserialize, Serialize};

use crate::library::nfd;

/// Words titles and names are built from, so multi-word searches hit realistic overlaps.
const WORDS: &[&str] = &[
    "love",
    "night",
    "the",
    "heart",
    "dream",
    "fire",
    "rain",
    "summer",
    "blue",
    "light",
    "down",
    "time",
    "home",
    "road",
    "girl",
    "boy",
    "dance",
    "lonely",
    "golden",
    "river",
    "moon",
    "sun",
    "stars",
    "city",
    "wild",
    "young",
    "forever",
    "again",
    "baby",
    "world",
    "money",
    "ghost",
    "glass",
    "silver",
    "midnight",
    "morning",
    "ocean",
    "shadow",
    "electric",
    "paper",
    "honey",
    "sugar",
    "storm",
    "winter",
    "echo",
    "velvet",
    "neon",
    "satellite",
    "angel",
    "devil",
    "kiss",
    "letter",
    "window",
    "garden",
    "highway",
    "thunder",
    "paradise",
    "crystal",
    "radio",
    "tonight",
    "yesterday",
    "tomorrow",
    "secret",
    "broken",
    "little",
    "big",
    "black",
    "white",
    "red",
    "green",
    "sweet",
    "cold",
    "hot",
    "run",
    "fall",
    "rise",
    "dreamer",
    "lover",
    "stranger",
    "hero",
    "queen",
    "king",
    "child",
    "mother",
    "father",
];
const GENRES: &[&str] = &[
    "Rock",
    "Pop",
    "Jazz",
    "Electronic",
    "Hip-Hop",
    "Classical",
    "Folk",
    "Metal",
    "Soul",
    "R&B",
    "Country",
    "Blues",
    "Reggae",
    "Punk",
    "Indie",
    "Ambient",
    "House",
    "Techno",
    "Post-Rock",
    "J-Pop",
];
const ODD_NAMES: &[&str] = &[
    "Beyoncé",
    "Sigur Rós",
    "Björk",
    "Mötley Crüe",
    "坂本龍一",
    "Café Tacvba",
    "Ólafur Arnalds",
    "Röyksopp",
    "Zoë Keating",
    "Наутилус",
];

/// A splitmix64 stream, so the same seed builds the same library.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    fn words(&mut self, min: usize, max: usize) -> String {
        let n = min + self.below(max - min + 1);
        let mut out: Vec<String> = Vec::with_capacity(n);
        for _ in 0..n {
            let w = WORDS[self.below(WORDS.len())];
            let mut c = w.chars();
            let cap = c
                .next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default();
            out.push(cap);
        }
        out.join(" ")
    }
}

/// What the generator wrote.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Synth {
    pub songs: usize,
    pub files: usize,
    pub artists: usize,
    pub albums: usize,
    pub bytes: u64,
    /// Every file's path relative to the root, in writing order.
    #[serde(skip)]
    pub paths: Vec<PathBuf>,
}

struct Template {
    ext: &'static str,
    bytes: Vec<u8>,
    tag: TagType,
}

fn template(
    ffmpeg: &Path,
    dir: &Path,
    ext: &'static str,
    args: &[&str],
) -> anyhow::Result<Template> {
    let path = dir.join(format!("template.{ext}"));
    let status = Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=523:duration=0.5:sample_rate=22050",
        ])
        .args(["-map_metadata", "-1", "-ac", "1"])
        .args(args)
        .arg(&path)
        .status()?;
    if !status.success() {
        bail!("ffmpeg could not make the {ext} template");
    }
    let bytes = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    let tag = match ext {
        "mp3" => TagType::Id3v2,
        "flac" => TagType::VorbisComments,
        _ => TagType::Mp4Ilst,
    };
    Ok(Template { ext, bytes, tag })
}

/// Writes `count` songs under `root`: about 12 per album and 2 albums per artist, nested
/// `artist/album/NN title.ext`, 70 % MP3, 20 % FLAC, 10 % M4A, a share of accented,
/// decomposed and CJK names, compilations, untagged files and exact duplicates. Each file's
/// audio differs, so each is its own song.
#[expect(
    clippy::too_many_lines,
    reason = "the whole library reads best in one place"
)]
pub fn synth_library(ffmpeg: &Path, root: &Path, count: usize, seed: u64) -> anyhow::Result<Synth> {
    std::fs::create_dir_all(root)?;
    let templates = [
        template(ffmpeg, root, "mp3", &["-c:a", "libmp3lame", "-b:a", "32k"])?,
        template(
            ffmpeg,
            root,
            "flac",
            &["-c:a", "flac", "-sample_fmt", "s16"],
        )?,
        template(
            ffmpeg,
            root,
            "m4a",
            &["-c:a", "aac", "-b:a", "32k", "-movflags", "+faststart"],
        )?,
    ];
    let mut rng = Rng::new(seed);
    let mut out = Synth::default();
    let mut written = 0usize;
    let mut artist_n = 0usize;
    while written < count {
        artist_n += 1;
        let artist = if artist_n.is_multiple_of(40) {
            format!("{} {}", ODD_NAMES[rng.below(ODD_NAMES.len())], artist_n)
        } else {
            format!("{} {artist_n}", rng.words(1, 2))
        };
        let compilation = artist_n.is_multiple_of(50);
        out.artists += 1;
        let albums = 1 + rng.below(3);
        for album_n in 0..albums {
            if written >= count {
                break;
            }
            out.albums += 1;
            let album = format!("{} {album_n}", rng.words(1, 3));
            let year = 1960 + rng.below(66) as u32;
            let genre = GENRES[rng.below(GENRES.len())];
            let tracks = (8 + rng.below(9)).min(count - written);
            let folder_artist = if artist_n.is_multiple_of(40) {
                nfd(&artist)
            } else {
                artist.clone()
            };
            let folder = if compilation {
                PathBuf::from("Compilations").join(&album)
            } else {
                PathBuf::from(&folder_artist).join(format!("{year} - {album}"))
            };
            std::fs::create_dir_all(root.join(&folder))?;
            for track in 1..=tracks {
                let title = rng.words(1, 4);
                let index = written;
                let t = &templates[match index % 10 {
                    0..=6 => 0,
                    7 | 8 => 1,
                    _ => 2,
                }];
                let rel = folder.join(format!("{track:02} {}.{}", title.replace('/', "-"), t.ext));
                let mut bytes = t.bytes.clone();
                let at = bytes.len() / 2;
                let stamp = (index as u64 ^ seed.rotate_left(17)).to_le_bytes();
                for (i, b) in stamp.iter().enumerate() {
                    if let Some(slot) = bytes.get_mut(at + i) {
                        *slot = *b;
                    }
                }
                let path = root.join(&rel);
                std::fs::write(&path, &bytes)?;
                if index % 200 != 7 {
                    let song_artist = if compilation {
                        format!("{} {}", rng.words(1, 2), rng.below(1000))
                    } else {
                        artist.clone()
                    };
                    write_tags(
                        &path,
                        t.tag,
                        &TagFields {
                            title: &title,
                            artist: &song_artist,
                            album: &album,
                            album_artist: if compilation {
                                "Various Artists"
                            } else {
                                &artist
                            },
                            track: track as u32,
                            year,
                            genre,
                        },
                    )?;
                }
                out.bytes += std::fs::metadata(&path).map_or(0, |m| m.len());
                out.paths.push(rel.clone());
                written += 1;
                out.songs += 1;
                out.files += 1;
                if index % 100 == 50 {
                    let dup = PathBuf::from("Duplicates").join(format!("{index} copy.{}", t.ext));
                    std::fs::create_dir_all(root.join("Duplicates"))?;
                    std::fs::copy(&path, root.join(&dup))?;
                    out.files += 1;
                    out.songs += 1;
                    out.paths.push(dup);
                }
            }
        }
    }
    Ok(out)
}

struct TagFields<'a> {
    title: &'a str,
    artist: &'a str,
    album: &'a str,
    album_artist: &'a str,
    track: u32,
    year: u32,
    genre: &'a str,
}

fn write_tags(path: &Path, kind: TagType, f: &TagFields<'_>) -> anyhow::Result<()> {
    let mut tag = Tag::new(kind);
    tag.set_title(f.title.to_owned());
    tag.set_artist(f.artist.to_owned());
    tag.set_album(f.album.to_owned());
    tag.insert_text(ItemKey::AlbumArtist, f.album_artist.to_owned());
    tag.set_track(f.track);
    tag.set_genre(f.genre.to_owned());
    tag.insert_text(ItemKey::Year, f.year.to_string());
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("cannot tag {}", path.display()))?;
    Ok(())
}

/// Rewrites one file's title in place, for retag scenarios on a synthetic library.
pub fn retitle(path: &Path, title: &str) -> anyhow::Result<()> {
    let mut file = lofty::read_from_path(path)?;
    if let Some(tag) = file.primary_tag_mut() {
        tag.set_title(title.to_owned());
        tag.save_to_path(path, WriteOptions::default())?;
    }
    Ok(())
}

/// Which way a metric should move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Lower,
    Higher,
}

/// One measurement in the report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub value: f64,
    pub unit: String,
    pub better: Better,
    /// The architecture's budget, at the reference library size noted in `budget_note`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_note: Option<String>,
    /// Compared with the baseline. Off for values too small or noisy to compare across
    /// machines.
    #[serde(default = "yes")]
    pub gate: bool,
}

fn yes() -> bool {
    true
}

/// The report the harness writes and the baseline it compares with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub songs: usize,
    pub files: usize,
    pub machine: BTreeMap<String, String>,
    pub metrics: BTreeMap<String, Metric>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub regressions: Vec<String>,
    #[serde(default)]
    pub over_budget: Vec<String>,
}

impl Report {
    pub fn put(
        &mut self,
        name: &str,
        value: f64,
        unit: &str,
        better: Better,
        budget: Option<(f64, &str)>,
    ) {
        self.metrics.insert(
            name.to_owned(),
            Metric {
                value: (value * 1000.0).round() / 1000.0,
                unit: unit.to_owned(),
                better,
                budget: budget.map(|b| b.0),
                budget_note: budget.map(|b| b.1.to_owned()),
                gate: true,
            },
        );
    }

    pub fn ungate(&mut self, name: &str) {
        if let Some(m) = self.metrics.get_mut(name) {
            m.gate = false;
        }
    }

    /// Fills `over_budget` with every metric outside its budget.
    pub fn check_budgets(&mut self) {
        self.over_budget = self
            .metrics
            .iter()
            .filter_map(|(name, m)| {
                let budget = m.budget?;
                let over = match m.better {
                    Better::Lower => m.value > budget,
                    Better::Higher => m.value < budget,
                };
                over.then(|| {
                    format!(
                        "{name}: {} {} against a budget of {budget}",
                        m.value, m.unit
                    )
                })
            })
            .collect();
    }
}

/// Metrics that moved the wrong way by more than `max_pct` percent of the baseline. Values
/// in milliseconds also need to move by more than `floor_ms`, since a 1 ms timing doubling
/// on a shared CI runner is noise, not a regression.
pub fn regressions(
    current: &Report,
    baseline: &Report,
    max_pct: f64,
    floor_ms: f64,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, base) in &baseline.metrics {
        if !base.gate {
            continue;
        }
        let Some(now) = current.metrics.get(name) else {
            out.push(format!("{name}: missing from this run"));
            continue;
        };
        let limit = max_pct / 100.0;
        let (worse, delta) = match base.better {
            Better::Lower => (
                now.value > base.value * (1.0 + limit),
                now.value - base.value,
            ),
            Better::Higher => (
                now.value < base.value * (1.0 - limit),
                base.value - now.value,
            ),
        };
        let noise = base.unit == "ms" && delta.abs() <= floor_ms;
        if worse && !noise {
            out.push(format!(
                "{name}: {} {} against a baseline of {} ({:+.0} %)",
                now.value,
                now.unit,
                base.value,
                (now.value / base.value.max(f64::MIN_POSITIVE) - 1.0) * 100.0
            ));
        }
    }
    out
}

/// Latency percentiles of a sample, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Percentiles {
    pub p50: f64,
    pub p99: f64,
    pub max: f64,
}

pub fn percentiles(samples: &mut [Duration]) -> Percentiles {
    samples.sort();
    let pick = |q: f64| -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let i = ((samples.len() as f64 - 1.0) * q).round() as usize;
        samples[i.min(samples.len() - 1)].as_secs_f64() * 1000.0
    };
    Percentiles {
        p50: pick(0.5),
        p99: pick(0.99),
        max: pick(1.0),
    }
}

/// Keeps one key under the server's per-key limit (50 requests a second): waits until at
/// least 21 ms passed since the previous call.
pub async fn pace() {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let wait = {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let next = last.map_or(now, |l| (l + Duration::from_millis(21)).max(now));
        *last = Some(next);
        next.saturating_duration_since(now)
    };
    tokio::time::sleep(wait).await;
}

/// Times `f` `n` times after `warm` unmeasured runs, paced by [`pace`] (the wait is not
/// timed).
pub async fn sample<F, Fut>(n: usize, warm: usize, mut f: F) -> anyhow::Result<Percentiles>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    for i in 0..warm {
        pace().await;
        f(i).await?;
    }
    let mut times = Vec::with_capacity(n);
    for i in 0..n {
        pace().await;
        let started = Instant::now();
        f(warm + i).await?;
        times.push(started.elapsed());
    }
    Ok(percentiles(&mut times))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(value: f64, unit: &str, better: Better) -> Report {
        let mut r = Report::default();
        r.put("m", value, unit, better, None);
        r
    }

    #[test]
    fn regressions_respect_the_direction_percentage_and_noise_floor() {
        let base = report(100.0, "files/s", Better::Higher);
        assert_eq!(
            regressions(&report(80.0, "files/s", Better::Higher), &base, 25.0, 2.0),
            Vec::<String>::new()
        );
        assert_eq!(
            regressions(&report(70.0, "files/s", Better::Higher), &base, 25.0, 2.0).len(),
            1
        );
        let base = report(2.0, "ms", Better::Lower);
        assert!(
            regressions(&report(3.5, "ms", Better::Lower), &base, 25.0, 2.0).is_empty(),
            "1.5 ms is noise"
        );
        assert_eq!(
            regressions(&report(9.0, "ms", Better::Lower), &base, 25.0, 2.0).len(),
            1
        );
        assert_eq!(
            regressions(&Report::default(), &base, 25.0, 2.0).len(),
            1,
            "a missing metric fails"
        );
    }

    #[test]
    fn percentiles_pick_nearest_ranks() {
        let mut samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let p = percentiles(&mut samples);
        assert!(
            (p.p50 - 51.0).abs() < 1.0
                && (p.p99 - 99.0).abs() < 1.1
                && (p.max - 100.0).abs() < 0.01
        );
    }

    #[test]
    fn the_rng_repeats_for_a_seed() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert_eq!(
            (0..5).map(|_| a.next_u64()).collect::<Vec<_>>(),
            (0..5).map(|_| b.next_u64()).collect::<Vec<_>>()
        );
    }
}
