//! Just enough FLAC to remux it (RFC 9639): STREAMINFO from the codec header, and frame headers
//! for each frame's block size. Nothing is decoded.

use bytes::Bytes;

/// The STREAMINFO block: 34 bytes after a 4-byte metadata block header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub min_block: u16,
    pub max_block: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    /// The raw 34 bytes, as they go into `dfLa`.
    pub raw: [u8; 34],
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FlacError {
    #[error("the codec header does not start with fLaC and a STREAMINFO block")]
    Header,
    #[error("the STREAMINFO block is invalid")]
    StreamInfo,
    #[error("a chunk does not start with a FLAC frame")]
    NoFrame,
    #[error("a chunk's frames fail their checksums")]
    Checksum,
}

impl StreamInfo {
    /// Parses a Sendspin FLAC `codec_header`: `fLaC` and then STREAMINFO (more blocks may follow).
    pub fn from_codec_header(header: &[u8]) -> Result<Self, FlacError> {
        let rest = header.strip_prefix(b"fLaC").ok_or(FlacError::Header)?;
        if rest.len() < 38 || rest[0] & 0x7F != 0 {
            return Err(FlacError::Header);
        }
        let len = u32::from_be_bytes([0, rest[1], rest[2], rest[3]]);
        if len != 34 {
            return Err(FlacError::StreamInfo);
        }
        let mut raw = [0u8; 34];
        raw.copy_from_slice(&rest[4..38]);
        let min_block = u16::from_be_bytes([raw[0], raw[1]]);
        let max_block = u16::from_be_bytes([raw[2], raw[3]]);
        let sample_rate =
            (u32::from(raw[10]) << 12) | (u32::from(raw[11]) << 4) | (u32::from(raw[12]) >> 4);
        let channels = ((raw[12] >> 1) & 0x07) + 1;
        let bits_per_sample = (((raw[12] & 0x01) << 4) | (raw[13] >> 4)) + 1;
        if sample_rate == 0 || min_block < 16 || max_block < min_block {
            return Err(FlacError::StreamInfo);
        }
        Ok(Self {
            min_block,
            max_block,
            sample_rate,
            channels,
            bits_per_sample,
            raw,
        })
    }

    /// `fLaC` and this block marked last: a header ffmpeg reads before the frames.
    pub fn stream_header(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(42);
        out.extend_from_slice(b"fLaC");
        out.extend_from_slice(&[0x80, 0, 0, 34]);
        out.extend_from_slice(&self.raw);
        out
    }
}

/// One FLAC frame and its length in samples per channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub samples: u32,
    pub data: Bytes,
}

fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc16_update(mut crc: u16, byte: u8) -> u16 {
    crc ^= u16::from(byte) << 8;
    for _ in 0..8 {
        crc = if crc & 0x8000 != 0 {
            (crc << 1) ^ 0x8005
        } else {
            crc << 1
        };
    }
    crc
}

/// The block size of a frame header at the start of `bytes`, if one is there and its CRC-8
/// checks out.
pub(crate) fn frame_header(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 6 || bytes[0] != 0xFF || bytes[1] & 0xFE != 0xF8 {
        return None;
    }
    let block_code = bytes[2] >> 4;
    let rate_code = bytes[2] & 0x0F;
    let channels = bytes[3] >> 4;
    let size_code = (bytes[3] >> 1) & 0x07;
    if block_code == 0 || rate_code == 15 || channels > 10 || size_code == 3 || bytes[3] & 1 != 0 {
        return None;
    }
    // The coded frame or sample number: one to seven bytes, UTF-8 style.
    let lead = bytes[4];
    let extra = match lead.leading_ones() {
        0 => 0,
        n @ 2..=7 => n as usize - 1,
        _ => return None,
    };
    let mut at = 5 + extra;
    if bytes.len() < at || !bytes[5..at].iter().all(|b| b & 0xC0 == 0x80) {
        return None;
    }
    let block = match block_code {
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => {
            let v = u32::from(*bytes.get(at)?) + 1;
            at += 1;
            v
        }
        7 => {
            let v = u32::from(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?])) + 1;
            at += 2;
            v
        }
        _ => 256 << (block_code - 8),
    };
    at += match rate_code {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    let crc = *bytes.get(at)?;
    (crc8(&bytes[..at]) == crc).then_some(block)
}

/// Splits a chunk of whole FLAC frames into frames. A boundary is a frame header whose CRC-8
/// holds where the CRC-16 of everything since the previous boundary is zero (each frame ends
/// with its own CRC-16), so sync-like bytes inside audio data are never taken for a frame.
pub fn split_frames(chunk: &Bytes) -> Result<Vec<Frame>, FlacError> {
    let bytes = chunk.as_ref();
    let mut samples = frame_header(bytes).ok_or(FlacError::NoFrame)?;
    let mut frames = Vec::new();
    let mut start = 0;
    let mut crc = 0u16;
    for (i, &b) in bytes.iter().enumerate() {
        if i > start + 1
            && crc == 0
            && b == 0xFF
            && let Some(next) = frame_header(&bytes[i..])
        {
            frames.push(Frame {
                samples,
                data: chunk.slice(start..i),
            });
            start = i;
            samples = next;
        }
        crc = crc16_update(crc, b);
    }
    if crc != 0 {
        return Err(FlacError::Checksum);
    }
    frames.push(Frame {
        samples,
        data: chunk.slice(start..),
    });
    Ok(frames)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A STREAMINFO for 44.1 kHz stereo 16 bit with 4608-sample blocks.
    pub(crate) fn streaminfo() -> Vec<u8> {
        let mut raw = [0u8; 34];
        raw[0..2].copy_from_slice(&4608u16.to_be_bytes());
        raw[2..4].copy_from_slice(&4608u16.to_be_bytes());
        let rate = 44_100u32;
        raw[10] = (rate >> 12) as u8;
        raw[11] = (rate >> 4) as u8;
        raw[12] = (((rate & 0x0F) as u8) << 4) | (1 << 1);
        raw[13] = 15 << 4;
        let mut header = b"fLaC".to_vec();
        header.extend_from_slice(&[0x80, 0, 0, 34]);
        header.extend_from_slice(&raw);
        header
    }

    /// A syntactically valid FLAC frame: header for `samples` (16-bit block size field),
    /// arbitrary body, and a correct CRC-16 footer. Not decodable audio.
    pub(crate) fn fake_frame(number: u8, samples: u16, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xF8, 0x79, 0x18, number & 0x7F];
        out.extend_from_slice(&(samples - 1).to_be_bytes());
        out.push(crc8(&out));
        out.extend_from_slice(body);
        let crc = out.iter().fold(0u16, |c, &b| crc16_update(c, b));
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    #[test]
    fn streaminfo_fields_are_read() {
        let info = StreamInfo::from_codec_header(&streaminfo()).expect("info");
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.max_block, 4608);
        assert_eq!(info.stream_header(), streaminfo());
    }

    #[test]
    fn a_header_without_the_marker_is_refused() {
        assert_eq!(
            StreamInfo::from_codec_header(&streaminfo()[4..]),
            Err(FlacError::Header)
        );
    }

    #[test]
    fn frames_split_at_real_boundaries_only() {
        // The first body contains a sync-like pattern that is not a frame boundary.
        let a = fake_frame(0, 4608, &[1, 2, 0xFF, 0xF8, 0x79, 0x18, 0, 3]);
        let b = fake_frame(1, 1152, &[9; 40]);
        let chunk = Bytes::from([a.clone(), b.clone()].concat());
        let frames = split_frames(&chunk).expect("frames");
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].samples, frames[0].data.as_ref()), (4608, &a[..]));
        assert_eq!((frames[1].samples, frames[1].data.as_ref()), (1152, &b[..]));
    }

    #[test]
    fn a_corrupt_chunk_is_refused() {
        let mut frame = fake_frame(0, 4608, &[1, 2, 3]);
        let last = frame.len() - 1;
        frame[last] ^= 1;
        assert_eq!(split_frames(&Bytes::from(frame)), Err(FlacError::Checksum));
        assert_eq!(
            split_frames(&Bytes::from_static(b"nope nope")),
            Err(FlacError::NoFrame)
        );
    }

    #[test]
    fn coded_block_sizes_are_decoded() {
        let mut header = vec![0xFF, 0xF8, 0xC9, 0x18, 0x00];
        header.push(crc8(&header));
        assert_eq!(frame_header(&header), Some(4096));
        let mut header = vec![0xFF, 0xF8, 0x59, 0x18, 0x00];
        header.push(crc8(&header));
        assert_eq!(frame_header(&header), Some(4608));
    }
}
