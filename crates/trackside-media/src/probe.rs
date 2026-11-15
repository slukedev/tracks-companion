//! Audio properties for acquisition: lofty for the numbers, a small MP4 box walk for the
//! codec and video tracks.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use trackside_core::records::{Codec, Container};

use crate::{MediaError, Probe};

/// The audio tracks' sample entry type and whether a video track exists, from `moov`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Mp4Tracks {
    pub audio: Option<[u8; 4]>,
    pub video: bool,
}

/// moov boxes larger than this are not walked.
const MOOV_LIMIT: u64 = 64 * 1024 * 1024;

fn read_box_header<R: Read>(r: &mut R) -> std::io::Result<Option<(u64, [u8; 4], u64)>> {
    let mut head = [0u8; 8];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
    let kind = [head[4], head[5], head[6], head[7]];
    if size == 1 {
        let mut large = [0u8; 8];
        r.read_exact(&mut large)?;
        return Ok(Some((u64::from_be_bytes(large), kind, 16)));
    }
    Ok(Some((size, kind, 8)))
}

/// Walks the top level for `moov` and reads every track's handler and first sample entry.
pub(crate) fn mp4_tracks(path: &Path) -> std::io::Result<Mp4Tracks> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut pos = 0u64;
    while pos < len {
        file.seek(SeekFrom::Start(pos))?;
        let Some((size, kind, header)) = read_box_header(&mut file)? else {
            break;
        };
        let size = if size == 0 { len - pos } else { size };
        if size < header {
            break;
        }
        if &kind == b"moov" {
            let body_len = size - header;
            if body_len > MOOV_LIMIT {
                return Err(std::io::Error::other("moov too large"));
            }
            let mut body = vec![0u8; usize::try_from(body_len).unwrap_or(0)];
            file.read_exact(&mut body)?;
            let mut tracks = Mp4Tracks::default();
            walk(&body, &mut tracks, None);
            return Ok(tracks);
        }
        pos = pos.saturating_add(size);
    }
    Ok(Mp4Tracks::default())
}

fn children(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let head = data.get(pos..pos + 8)?;
        let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = [head[4], head[5], head[6], head[7]];
        if size < 8 {
            return None;
        }
        let body = data.get(pos + 8..pos + size)?;
        pos += size;
        Some((kind, body))
    })
}

fn walk(data: &[u8], tracks: &mut Mp4Tracks, mut handler: Option<[u8; 4]>) {
    let boxes: Vec<_> = children(data).collect();
    for (kind, body) in &boxes {
        if kind == b"hdlr"
            && let Some(h) = body.get(8..12)
        {
            handler = Some([h[0], h[1], h[2], h[3]]);
            if h == b"vide" {
                tracks.video = true;
            }
        }
    }
    for (kind, body) in boxes {
        match &kind {
            b"trak" => walk(body, tracks, None),
            b"mdia" | b"minf" | b"stbl" => walk(body, tracks, handler),
            b"stsd" => {
                if handler == Some(*b"soun")
                    && tracks.audio.is_none()
                    && let Some(entry) = body.get(12..16)
                {
                    tracks.audio = Some([entry[0], entry[1], entry[2], entry[3]]);
                }
            }
            _ => {}
        }
    }
}

fn mp4_codec(entry: Option<[u8; 4]>) -> Result<Codec, MediaError> {
    match entry.as_ref().map(<[u8; 4]>::as_slice) {
        Some(b"mp4a") => Ok(Codec::Aac),
        Some(b"alac") => Ok(Codec::Alac),
        Some(b"fLaC") => Ok(Codec::Flac),
        Some(b".mp3") => Ok(Codec::Mp3),
        Some(b"Opus") => Ok(Codec::Opus),
        Some(other) => Err(MediaError::Unsupported(format!(
            "mp4 audio {}",
            String::from_utf8_lossy(other)
        ))),
        None => Err(MediaError::Unsupported("mp4 without audio".into())),
    }
}

/// Reads properties without tags or pictures. Blocking.
pub(crate) fn probe_blocking(path: &Path) -> Result<Probe, MediaError> {
    let tagged = lofty::probe::Probe::open(path)
        .map_err(|e| MediaError::Unsupported(e.to_string()))?
        .options(ParseOptions::new().read_tags(false).read_cover_art(false))
        .guess_file_type()?
        .read()
        .map_err(|e| MediaError::Unsupported(e.to_string()))?;
    let (container, codec, has_video) = match tagged.file_type() {
        FileType::Flac => (Container::Flac, Codec::Flac, false),
        FileType::Mpeg => (Container::Mp3, Codec::Mp3, false),
        FileType::Aac => (Container::Aac, Codec::Aac, false),
        FileType::Opus => (Container::Opus, Codec::Opus, false),
        FileType::Vorbis => (Container::Ogg, Codec::Vorbis, false),
        FileType::Wav => (Container::Wav, Codec::Pcm, false),
        FileType::Aiff => (Container::Aiff, Codec::Pcm, false),
        FileType::WavPack => (Container::WavPack, Codec::WavPack, false),
        FileType::Ape => (Container::Ape, Codec::Ape, false),
        FileType::Mp4 => {
            let tracks = mp4_tracks(path)?;
            (Container::M4a, mp4_codec(tracks.audio)?, tracks.video)
        }
        other => return Err(MediaError::Unsupported(format!("file type {other:?}"))),
    };
    let properties = tagged.properties();
    Ok(Probe {
        container,
        codec,
        duration_ms: u32::try_from(properties.duration().as_millis()).unwrap_or(u32::MAX),
        bitrate_kbps: properties.audio_bitrate().or(properties.overall_bitrate()),
        sample_rate: properties.sample_rate(),
        bit_depth: properties.bit_depth(),
        channels: properties.channels(),
        has_video,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&kind);
        out.extend_from_slice(body);
        out
    }

    fn trak(handler: [u8; 4], entry: [u8; 4]) -> Vec<u8> {
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(&handler);
        hdlr.extend_from_slice(&[0u8; 12]);
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&boxed(entry, &[0u8; 8]));
        let stbl = boxed(*b"stbl", &boxed(*b"stsd", &stsd));
        let minf = boxed(*b"minf", &stbl);
        let mut mdia = boxed(*b"hdlr", &hdlr);
        mdia.extend_from_slice(&minf);
        boxed(*b"trak", &boxed(*b"mdia", &mdia))
    }

    #[test]
    fn mp4_walk_finds_the_audio_codec_and_video_tracks() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.mp4");
        let mut moov = trak(*b"vide", *b"avc1");
        moov.extend_from_slice(&trak(*b"soun", *b"alac"));
        let mut file = boxed(*b"ftyp", b"M4A \0\0\0\0");
        file.extend_from_slice(&boxed(*b"mdat", &[1, 2, 3]));
        file.extend_from_slice(&boxed(*b"moov", &moov));
        std::fs::write(&path, file)?;
        let tracks = mp4_tracks(&path)?;
        assert_eq!(
            tracks,
            Mp4Tracks {
                audio: Some(*b"alac"),
                video: true
            }
        );
        assert_eq!(mp4_codec(tracks.audio)?, Codec::Alac);
        Ok(())
    }

    #[test]
    fn probing_garbage_is_unsupported() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.flac");
        std::fs::write(&path, b"PK\x03\x04 zip file pretending")?;
        assert!(probe_blocking(&path).is_err());
        Ok(())
    }
}
