//! Audio digests (architecture section 6): BLAKE3 over the payload length and three 64 KiB
//! windows of the audio payload, the bytes that are not tags. Payload locators are written
//! defensively: any structure they do not understand makes them give up, and the digest then
//! covers the whole file instead.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use trackside_core::AudioDigest;
use trackside_core::records::Container;

use crate::DigestMode;

const DOMAIN: &[u8] = b"trackside audio v1";
const WINDOW: u64 = 64 * 1024;

/// A byte range of the file, `start..end`.
pub type Span = (u64, u64);

/// The audio digest of the file at `path`.
pub fn audio_digest(path: &Path, container: Container) -> io::Result<(AudioDigest, DigestMode)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    digest_reader(&mut reader, len, container)
}

/// [`audio_digest`] over any seekable reader of `len` bytes.
pub fn digest_reader<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    container: Container,
) -> io::Result<(AudioDigest, DigestMode)> {
    let located = locate_payload(reader, len, container)?;
    let (spans, mode) = match located {
        Some(spans) if spans.iter().any(|(s, e)| e > s) => (spans, DigestMode::Payload),
        _ => (vec![(0, len)], DigestMode::WholeFile),
    };
    Ok((digest_spans(reader, &spans)?, mode))
}

/// Where the audio payload is, or `None` when the structure is not understood.
pub fn locate_payload<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    container: Container,
) -> io::Result<Option<Vec<Span>>> {
    match container {
        Container::Mp3 | Container::Aac | Container::WavPack | Container::Ape => {
            locate_framed(reader, len)
        }
        Container::Flac => locate_flac(reader, len),
        Container::M4a => locate_mp4(reader, len),
        Container::Ogg | Container::Opus => {
            reader.seek(SeekFrom::Start(0))?;
            locate_ogg(reader, len)
        }
        Container::Wav => locate_riff(reader, len),
        Container::Aiff => locate_aiff(reader, len),
    }
}

/// BLAKE3(domain ‖ length ‖ window(0) ‖ window(mid) ‖ window(end)) over the concatenation of
/// `spans`. A payload of at most three windows is hashed whole.
fn digest_spans<R: Read + Seek>(reader: &mut R, spans: &[Span]) -> io::Result<AudioDigest> {
    let total: u64 = spans.iter().map(|(s, e)| e.saturating_sub(*s)).sum();
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(&total.to_le_bytes());
    if total <= 3 * WINDOW {
        hash_virtual(reader, spans, 0, total, &mut hasher)?;
    } else {
        hash_virtual(reader, spans, 0, WINDOW, &mut hasher)?;
        hash_virtual(reader, spans, total / 2 - WINDOW / 2, WINDOW, &mut hasher)?;
        hash_virtual(reader, spans, total - WINDOW, WINDOW, &mut hasher)?;
    }
    Ok(AudioDigest(*hasher.finalize().as_bytes()))
}

/// Hashes `count` bytes starting at `offset` of the virtual concatenation of `spans`.
fn hash_virtual<R: Read + Seek>(
    reader: &mut R,
    spans: &[Span],
    mut offset: u64,
    mut count: u64,
    hasher: &mut blake3::Hasher,
) -> io::Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    for &(start, end) in spans {
        if count == 0 {
            break;
        }
        let span_len = end.saturating_sub(start);
        if offset >= span_len {
            offset -= span_len;
            continue;
        }
        let take = (span_len - offset).min(count);
        reader.seek(SeekFrom::Start(start + offset))?;
        let mut left = take;
        while left > 0 {
            let n = (left as usize).min(buf.len());
            reader.read_exact(&mut buf[..n])?;
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
        count -= take;
        offset = 0;
    }
    if count > 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "payload shorter than located",
        ));
    }
    Ok(())
}

fn read_at<R: Read + Seek>(reader: &mut R, pos: u64, buf: &mut [u8]) -> io::Result<bool> {
    reader.seek(SeekFrom::Start(pos))?;
    match reader.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn syncsafe(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0u64, |acc, b| {
        (*b < 0x80).then_some((acc << 7) | u64::from(*b))
    })
}

/// Skips `ID3v2` tags at `pos` (there may be several), returning where the next thing starts.
fn skip_id3v2<R: Read + Seek>(reader: &mut R, mut pos: u64, len: u64) -> io::Result<u64> {
    let mut header = [0u8; 10];
    for _ in 0..8 {
        if pos + 10 > len || !read_at(reader, pos, &mut header)? {
            break;
        }
        if &header[..3] != b"ID3" || header[3] == 0xFF || header[4] == 0xFF {
            break;
        }
        let Some(size) = syncsafe(&header[6..10]) else {
            break;
        };
        let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
        let next = pos + 10 + size + footer;
        if next > len {
            break;
        }
        pos = next;
    }
    Ok(pos)
}

/// Removes trailing `ID3v1`, `APEv2`, `Lyrics3v2` and appended `ID3v2` tags, returning the new end.
fn strip_trailers<R: Read + Seek>(reader: &mut R, start: u64, mut end: u64) -> io::Result<u64> {
    for _ in 0..16 {
        let room = end.saturating_sub(start);
        let mut tag = [0u8; 3];
        if room >= 128 && read_at(reader, end - 128, &mut tag)? && &tag == b"TAG" {
            end -= 128;
            continue;
        }
        let mut ape = [0u8; 32];
        if room >= 32 && read_at(reader, end - 32, &mut ape)? && &ape[..8] == b"APETAGEX" {
            let size = u64::from(u32::from_le_bytes([ape[12], ape[13], ape[14], ape[15]]));
            let flags = u32::from_le_bytes([ape[20], ape[21], ape[22], ape[23]]);
            let total = size + if flags & 0x8000_0000 != 0 { 32 } else { 0 };
            if total >= 32 && total <= room {
                end -= total;
                continue;
            }
        }
        let mut lyrics = [0u8; 15];
        if room >= 15 && read_at(reader, end - 15, &mut lyrics)? && &lyrics[6..] == b"LYRICS200" {
            let size = std::str::from_utf8(&lyrics[..6])
                .ok()
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(size) = size {
                let total = size + 15;
                if total <= room {
                    end -= total;
                    continue;
                }
            }
        }
        let mut footer = [0u8; 10];
        if room >= 10
            && read_at(reader, end - 10, &mut footer)?
            && &footer[..3] == b"3DI"
            && let Some(size) = syncsafe(&footer[6..10])
            && size + 20 <= room
        {
            end -= size + 20;
            continue;
        }
        break;
    }
    Ok(end)
}

/// MP3, raw AAC, `WavPack` and Monkey's Audio: tags only at the start and end.
fn locate_framed<R: Read + Seek>(reader: &mut R, len: u64) -> io::Result<Option<Vec<Span>>> {
    let start = skip_id3v2(reader, 0, len)?;
    let end = strip_trailers(reader, start, len)?;
    Ok((end > start).then(|| vec![(start, end)]))
}

fn locate_flac<R: Read + Seek>(reader: &mut R, len: u64) -> io::Result<Option<Vec<Span>>> {
    let mut pos = skip_id3v2(reader, 0, len)?;
    let mut magic = [0u8; 4];
    if !read_at(reader, pos, &mut magic)? || &magic != b"fLaC" {
        return Ok(None);
    }
    pos += 4;
    let mut header = [0u8; 4];
    loop {
        if !read_at(reader, pos, &mut header)? {
            return Ok(None);
        }
        let block_len = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        pos += 4 + block_len;
        if pos > len {
            return Ok(None);
        }
        if header[0] & 0x80 != 0 {
            break;
        }
    }
    let end = strip_trailers(reader, pos, len)?;
    Ok((end > pos).then(|| vec![(pos, end)]))
}

/// The contents of every top-level `mdat` box.
fn locate_mp4<R: Read + Seek>(reader: &mut R, len: u64) -> io::Result<Option<Vec<Span>>> {
    let mut spans = Vec::new();
    let mut pos = 0u64;
    let mut header = [0u8; 8];
    while pos + 8 <= len {
        if !read_at(reader, pos, &mut header)? {
            return Ok(None);
        }
        let size32 = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let kind = [header[4], header[5], header[6], header[7]];
        let (size, header_len) = match size32 {
            0 => (len - pos, 8),
            1 => {
                let mut large = [0u8; 8];
                if !read_at(reader, pos + 8, &mut large)? {
                    return Ok(None);
                }
                (u64::from_be_bytes(large), 16)
            }
            n => (n, 8),
        };
        if size < header_len || pos + size > len {
            return Ok(None);
        }
        if &kind == b"mdat" {
            spans.push((pos + header_len, pos + size));
        }
        pos += size;
    }
    Ok((!spans.is_empty()).then_some(spans))
}

/// RIFF WAVE: the `data` chunk.
fn locate_riff<R: Read + Seek>(reader: &mut R, len: u64) -> io::Result<Option<Vec<Span>>> {
    let mut head = [0u8; 12];
    if !read_at(reader, 0, &mut head)? || &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
        return Ok(None);
    }
    find_chunk(reader, 12, len, *b"data", false)
}

/// AIFF and AIFF-C: the `SSND` chunk.
fn locate_aiff<R: Read + Seek>(reader: &mut R, len: u64) -> io::Result<Option<Vec<Span>>> {
    let mut head = [0u8; 12];
    if !read_at(reader, 0, &mut head)?
        || &head[..4] != b"FORM"
        || !(&head[8..] == b"AIFF" || &head[8..] == b"AIFC")
    {
        return Ok(None);
    }
    find_chunk(reader, 12, len, *b"SSND", true)
}

fn find_chunk<R: Read + Seek>(
    reader: &mut R,
    mut pos: u64,
    len: u64,
    want: [u8; 4],
    big_endian: bool,
) -> io::Result<Option<Vec<Span>>> {
    let mut header = [0u8; 8];
    while pos + 8 <= len {
        if !read_at(reader, pos, &mut header)? {
            return Ok(None);
        }
        let raw = [header[4], header[5], header[6], header[7]];
        let size = u64::from(if big_endian {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        });
        let body = pos + 8;
        if header[..4] == want {
            let end = (body + size).min(len);
            return Ok((end > body).then(|| vec![(body, end)]));
        }
        pos = body + size + (size & 1);
    }
    Ok(None)
}

/// Ogg Vorbis and Opus: the bodies of the pages after the header packets, of the first logical
/// stream. Page headers are left out because rewriting the comment packet renumbers every
/// following page and recomputes its checksum.
fn locate_ogg<R: Read>(reader: &mut R, len: u64) -> io::Result<Option<Vec<Span>>> {
    let mut pos = 0u64;
    let mut serial: Option<u32> = None;
    let mut headers_needed = 0usize;
    let mut headers_done = 0usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut header = [0u8; 27];
    let mut lacing = [0u8; 255];
    loop {
        if pos + 27 > len {
            break;
        }
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        if &header[..4] != b"OggS" {
            return Ok(None);
        }
        let page_serial = u32::from_le_bytes([header[14], header[15], header[16], header[17]]);
        let segments = usize::from(header[26]);
        reader.read_exact(&mut lacing[..segments])?;
        let body_len: u64 = lacing[..segments].iter().map(|b| u64::from(*b)).sum();
        let body_start = pos + 27 + segments as u64;
        if body_start + body_len > len {
            return Ok(None);
        }
        let ours = *serial.get_or_insert(page_serial) == page_serial;
        let mut skip = body_len;
        if ours && headers_done < headers_needed.max(1) {
            if headers_needed == 0 {
                let mut magic = [0u8; 8];
                let n = (body_len as usize).min(8);
                reader.read_exact(&mut magic[..n])?;
                skip -= n as u64;
                headers_needed = if magic[..n].starts_with(b"\x01vorbis") {
                    3
                } else if magic[..n].starts_with(b"OpusHead") {
                    2
                } else {
                    return Ok(None);
                };
            }
            headers_done += lacing[..segments].iter().filter(|b| **b < 255).count();
        } else if ours {
            spans.push((body_start, body_start + body_len));
        }
        io::copy(&mut reader.by_ref().take(skip), &mut io::sink())?;
        pos = body_start + body_len;
    }
    Ok((headers_done >= headers_needed && !spans.is_empty()).then_some(spans))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use proptest::prelude::*;

    use super::*;

    fn digest_of(bytes: &[u8], container: Container) -> (AudioDigest, DigestMode) {
        let mut cursor = Cursor::new(bytes);
        let result = digest_reader(&mut cursor, bytes.len() as u64, container);
        assert!(result.is_ok());
        result.unwrap_or((AudioDigest([0; 32]), DigestMode::WholeFile))
    }

    fn id3v2(body_len: usize) -> Vec<u8> {
        let mut tag = b"ID3\x04\x00\x00".to_vec();
        let n = body_len as u32;
        tag.extend_from_slice(&[
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]);
        tag.extend(std::iter::repeat_n(0u8, body_len));
        tag
    }

    fn frames(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    #[test]
    fn mp3_digest_ignores_id3v2_and_id3v1() {
        let audio = frames(1, 300_000);
        let mut plain = id3v2(100);
        plain.extend_from_slice(&audio);
        let mut retagged = id3v2(5000);
        retagged.extend_from_slice(&audio);
        retagged.extend_from_slice(b"TAG");
        retagged.extend(std::iter::repeat_n(b' ', 125));
        let a = digest_of(&plain, Container::Mp3);
        let b = digest_of(&retagged, Container::Mp3);
        assert_eq!(a, b);
        assert_eq!(a.1, DigestMode::Payload);
    }

    #[test]
    fn mp3_digest_ignores_an_apev2_footer() {
        let audio = frames(2, 1000);
        let mut tagged = audio.clone();
        tagged.extend(std::iter::repeat_n(9u8, 8));
        tagged.extend_from_slice(b"APETAGEX");
        tagged.extend_from_slice(&2000u32.to_le_bytes());
        tagged.extend_from_slice(&40u32.to_le_bytes());
        tagged.extend_from_slice(&0u32.to_le_bytes());
        tagged.extend_from_slice(&0u32.to_le_bytes());
        tagged.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            digest_of(&tagged, Container::Mp3),
            digest_of(&audio, Container::Mp3)
        );
    }

    #[test]
    fn long_payloads_use_three_windows_and_see_the_middle() {
        let audio = frames(3, 1_000_000);
        let mut changed = audio.clone();
        changed[500_000] ^= 0xff;
        let mut outside = audio.clone();
        outside[200_000] ^= 0xff;
        let base = digest_of(&audio, Container::Mp3);
        assert_ne!(base, digest_of(&changed, Container::Mp3));
        assert_eq!(base, digest_of(&outside, Container::Mp3));
    }

    #[test]
    fn unknown_structure_falls_back_to_the_whole_file() {
        let (_, mode) = digest_of(b"not a flac file at all", Container::Flac);
        assert_eq!(mode, DigestMode::WholeFile);
        let (_, mode) = digest_of(b"RIFF....WAVEfmt ", Container::Wav);
        assert_eq!(mode, DigestMode::WholeFile);
    }

    #[test]
    fn mp4_payload_is_the_mdat_contents() {
        let mut file = Vec::new();
        file.extend_from_slice(&16u32.to_be_bytes());
        file.extend_from_slice(b"ftypM4A \0\0\0\0");
        let data = frames(4, 4000);
        file.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&data);
        let mut cursor = Cursor::new(&file);
        let spans = locate_payload(&mut cursor, file.len() as u64, Container::M4a);
        assert_eq!(spans.ok().flatten(), Some(vec![(24, 24 + 4000)]));
    }

    proptest! {
        #[test]
        fn locators_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..2048), which in 0usize..10) {
            let container = Container::ALL[which];
            let mut cursor = Cursor::new(&bytes);
            let located = locate_payload(&mut cursor, bytes.len() as u64, container);
            if let Ok(Some(spans)) = located {
                for (s, e) in spans {
                    prop_assert!(s <= e && e <= bytes.len() as u64);
                }
            }
        }

        #[test]
        fn locators_survive_magic_prefixes(tail in proptest::collection::vec(any::<u8>(), 0..1024), which in 0usize..6) {
            let prefixes: [&[u8]; 6] = [b"ID3\x04\x00\x10", b"fLaC", b"OggS\x00\x02", b"RIFF\xff\xff\xff\xffWAVE", b"FORM\x00\x00\x00\x00AIFF", b"\x00\x00\x00\x01mdat"];
            let mut bytes = prefixes[which].to_vec();
            bytes.extend_from_slice(&tail);
            for container in Container::ALL {
                let mut cursor = Cursor::new(&bytes);
                let _ = digest_reader(&mut cursor, bytes.len() as u64, *container);
            }
        }
    }
}
