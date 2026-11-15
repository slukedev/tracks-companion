//! Music libraries of real audio files made with ffmpeg, plus files that are broken on
//! purpose.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, TagExt};
use unicode_normalization::UnicodeNormalization;

/// The ffmpeg to generate audio with, or `None` (and a message) when there is none. With
/// `TRACKSIDE_TEST_FFMPEG=1` a missing ffmpeg is an error instead, as in CI.
///
/// # Panics
///
/// When `TRACKSIDE_TEST_FFMPEG=1` and there is no ffmpeg.
pub fn ffmpeg() -> Option<PathBuf> {
    let path = std::env::var_os("TRACKSIDE_E2E_FFMPEG")
        .map_or_else(|| PathBuf::from("ffmpeg"), PathBuf::from);
    let works = Command::new(&path)
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if works {
        return Some(path);
    }
    assert!(
        std::env::var("TRACKSIDE_TEST_FFMPEG").as_deref() != Ok("1"),
        "TRACKSIDE_TEST_FFMPEG=1 but ffmpeg was not found"
    );
    eprintln!("skipping: ffmpeg not found");
    None
}

/// Audio formats the generator writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp3,
    Flac,
    M4a,
    Ogg,
    Opus,
    Wav,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::Wav => "wav",
        }
    }

    fn codec(self) -> &'static [&'static str] {
        match self {
            Self::Mp3 => &["-c:a", "libmp3lame", "-b:a", "128k", "-id3v2_version", "3"],
            Self::Flac => &["-c:a", "flac"],
            Self::M4a => &["-c:a", "aac", "-b:a", "128k"],
            Self::Ogg => &["-c:a", "libvorbis", "-q:a", "2"],
            Self::Opus => &["-c:a", "libopus", "-b:a", "64k"],
            Self::Wav => &["-c:a", "pcm_s16le"],
        }
    }

    /// Whether ffmpeg can embed a cover picture in this container.
    fn takes_cover(self) -> bool {
        matches!(self, Self::Mp3 | Self::Flac | Self::M4a)
    }
}

/// One song to write.
#[derive(Clone, Debug)]
pub struct Track {
    /// Relative to the library root.
    pub rel: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: Option<String>,
    pub track: u32,
    pub disc: u32,
    pub year: i32,
    pub genre: String,
    pub format: Format,
    pub seconds: f32,
    /// Makes the audio, and so the song id, unique.
    pub freq: u32,
    /// An image to embed as the front cover.
    pub cover: Option<PathBuf>,
}

impl Track {
    pub fn new(rel: impl Into<PathBuf>, title: &str, artist: &str, album: &str) -> Self {
        Self {
            rel: rel.into(),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
            album_artist: None,
            track: 1,
            disc: 1,
            year: 2008,
            genre: String::from("Pop"),
            format: Format::Mp3,
            seconds: 2.0,
            freq: 440,
            cover: None,
        }
    }
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output().context("cannot run ffmpeg")?;
    if !output.status.success() {
        bail!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Writes `track` under `root` and returns its path.
pub fn write_track(ffmpeg: &Path, root: &Path, track: &Track) -> anyhow::Result<PathBuf> {
    let path = root.join(&track.rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source = format!(
        "sine=frequency={}:duration={}:sample_rate=44100",
        track.freq, track.seconds
    );
    let mut command = Command::new(ffmpeg);
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    command.args(["-f", "lavfi", "-i", &source]);
    let cover = track.cover.as_ref().filter(|_| track.format.takes_cover());
    if let Some(cover) = cover {
        command.arg("-i").arg(cover);
        command.args(["-map", "0:a", "-map", "1:v", "-c:v", "mjpeg"]);
        command.args(["-disposition:v", "attached_pic"]);
    }
    command.args(["-ac", "2"]);
    command.args(track.format.codec());
    let mut tags = vec![
        ("title", track.title.clone()),
        ("artist", track.artist.clone()),
        ("album", track.album.clone()),
        ("track", track.track.to_string()),
        ("disc", track.disc.to_string()),
        ("date", track.year.to_string()),
        ("genre", track.genre.clone()),
    ];
    if let Some(album_artist) = &track.album_artist {
        tags.push(("album_artist", album_artist.clone()));
    }
    for (key, value) in tags {
        command.arg("-metadata").arg(format!("{key}={value}"));
    }
    command.arg(&path);
    run(&mut command)?;
    Ok(path)
}

/// A small JPEG of one colour, as a cover.
pub fn write_cover(ffmpeg: &Path, path: &Path, color: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    run(Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", &format!("color=c={color}:s=640x640")])
        .args(["-frames:v", "1"])
        .arg(path))
}

/// Changes a file's title in place, leaving the audio alone (so its id stays).
pub fn retag(path: &Path, title: &str) -> anyhow::Result<()> {
    let mut file = lofty::read_from_path(path)?;
    let tag = match file.primary_tag_mut() {
        Some(tag) => tag,
        None => file.first_tag_mut().context("the file has no tag")?,
    };
    tag.set_title(title.to_owned());
    tag.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

/// The name with its accents decomposed, as macOS writes file names to SMB shares.
pub fn nfd(name: &str) -> String {
    name.nfd().collect()
}

/// What [`small_library`] wrote.
#[derive(Clone, Debug)]
pub struct Library {
    pub root: PathBuf,
    pub tracks: Vec<Track>,
    /// Files that should not become songs.
    pub broken: Vec<PathBuf>,
}

impl Library {
    pub fn track(&self, title: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.title == title)
    }
}

/// About twenty songs in every format the scanner reads, nested folders, an NFD folder name,
/// a sidecar cover, an embedded cover, lyrics, an exact duplicate in another folder, and
/// broken files.
#[expect(
    clippy::too_many_lines,
    reason = "the whole library reads best in one place"
)]
pub fn small_library(ffmpeg: &Path, root: &Path) -> anyhow::Result<Library> {
    std::fs::create_dir_all(root)?;
    let beyonce = nfd("Beyoncé");
    let sasha = "I Am... Sasha Fierce";
    let sidecar = root.join(&beyonce).join(sasha).join("cover.jpg");
    write_cover(ffmpeg, &sidecar, "red")?;
    let embedded = root.join("covers-src").join("blue.jpg");
    write_cover(ffmpeg, &embedded, "blue")?;

    let mut tracks = Vec::new();
    let mut freq = 300;
    let mut add = |mut track: Track| {
        freq += 37;
        track.freq = freq;
        tracks.push(track);
    };
    for (n, (title, format)) in [
        ("Halo", Format::Flac),
        ("If I Were a Boy", Format::Mp3),
        ("Single Ladies", Format::M4a),
        ("Sweet Dreams", Format::Ogg),
    ]
    .into_iter()
    .enumerate()
    {
        let mut t = Track::new(
            Path::new(&beyonce)
                .join(sasha)
                .join(format!("{:02} {title}.{}", n + 1, format.ext())),
            title,
            "Beyoncé",
            sasha,
        );
        t.track = n as u32 + 1;
        t.format = format;
        t.album_artist = Some(String::from("Beyoncé"));
        add(t);
    }
    for (n, (title, format)) in [
        ("Sigur Song", Format::Opus),
        ("Glósóli", Format::Flac),
        ("Hoppípolla", Format::Wav),
    ]
    .into_iter()
    .enumerate()
    {
        let mut t = Track::new(
            Path::new("Sigur Rós").join("Takk...").join(format!(
                "{:02} {title}.{}",
                n + 1,
                format.ext()
            )),
            title,
            "Sigur Rós",
            "Takk...",
        );
        t.track = n as u32 + 1;
        t.format = format;
        t.year = 2005;
        t.genre = String::from("Post-Rock");
        add(t);
    }
    for (n, title) in ["Love Song One", "Love Song Two", "The Night Is Young"]
        .into_iter()
        .enumerate()
    {
        let mut t = Track::new(
            Path::new("Various")
                .join("Disc Set")
                .join(format!("CD{}", n % 2 + 1))
                .join(format!("{:02} {title}.mp3", n + 1)),
            title,
            "The Lovers",
            "Love Collection",
        );
        t.track = n as u32 + 1;
        t.disc = n as u32 % 2 + 1;
        t.year = 1999;
        t.genre = String::from("Rock");
        t.cover = Some(embedded.clone());
        add(t);
    }
    for (n, title) in ["東京", "Ünïcödé Störm 🎵"].into_iter().enumerate() {
        let mut t = Track::new(
            Path::new("Ünïcödé").join(format!("{:02} {}.mp3", n + 1, nfd(title))),
            title,
            "アーティスト",
            "世界",
        );
        t.track = n as u32 + 1;
        t.genre = String::from("J-Pop");
        add(t);
    }
    for n in 0..6 {
        let title = format!("Filler {n}");
        let mut t = Track::new(
            Path::new("Fillers")
                .join(format!("Album {}", n / 3))
                .join(format!("{:02} {title}.mp3", n % 3 + 1)),
            &title,
            &format!("Filler Artist {}", n / 3),
            &format!("Filler Album {}", n / 3),
        );
        t.track = n % 3 + 1;
        t.seconds = 1.0;
        add(t);
    }

    for t in &tracks {
        write_track(ffmpeg, root, t)?;
    }
    std::fs::remove_dir_all(root.join("covers-src"))?;

    let halo = root.join(&tracks[0].rel);
    let lrc = halo.with_extension("lrc");
    std::fs::write(
        &lrc,
        "[00:00.50]Remember those walls I built\n[00:01.20]Well, baby, they're tumbling down\n",
    )?;
    let dup_dir = root.join("Duplicates");
    std::fs::create_dir_all(&dup_dir)?;
    std::fs::copy(
        root.join(&tracks[1].rel),
        dup_dir.join("If I Were a Boy (copy).mp3"),
    )?;

    let broken_dir = root.join("Broken");
    std::fs::create_dir_all(&broken_dir)?;
    let mut broken = Vec::new();
    let mut garbage = |name: &str, bytes: &[u8]| -> anyhow::Result<()> {
        let path = broken_dir.join(name);
        std::fs::write(&path, bytes)?;
        broken.push(path);
        Ok(())
    };
    garbage(
        "not audio.mp3",
        b"this is a text file pretending to be an mp3\n",
    )?;
    garbage("empty.flac", b"")?;
    garbage("header only.flac", b"fLaC\x00\x00\x00\x22")?;
    garbage("zeros.m4a", &[0u8; 4096])?;
    let mut noise = Vec::with_capacity(8192);
    let mut x: u32 = 0x9E37_79B9;
    for _ in 0..8192 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        noise.push(x as u8);
    }
    garbage("noise.ogg", &noise)?;
    let flac = std::fs::read(&halo)?;
    garbage("truncated.flac", &flac[..flac.len().min(600)])?;
    std::fs::write(broken_dir.join("notes.txt"), b"not music")?;

    Ok(Library {
        root: root.to_path_buf(),
        tracks,
        broken,
    })
}

/// The tag title of a file, for checks.
pub fn read_title(path: &Path) -> anyhow::Result<Option<String>> {
    let file = lofty::read_from_path(path)?;
    Ok(file
        .primary_tag()
        .or_else(|| file.first_tag())
        .and_then(|t| t.title().map(std::borrow::Cow::into_owned)))
}
