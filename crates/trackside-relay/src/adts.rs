//! ADTS parsing for ffmpeg's AAC output: strips the 7 or 9 byte headers, keeps the raw frames,
//! and builds the `AudioSpecificConfig` the `esds` box needs.

use bytes::{Bytes, BytesMut};

const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The stream parameters every ADTS header repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// MPEG-4 audio object type (2 for AAC-LC).
    pub object_type: u8,
    pub frequency_index: u8,
    pub channels: u8,
}

impl AacConfig {
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATES
            .get(usize::from(self.frequency_index))
            .copied()
            .unwrap_or(0)
    }

    /// The two-byte `AudioSpecificConfig`.
    pub fn audio_specific_config(&self) -> [u8; 2] {
        let v = (u16::from(self.object_type) << 11)
            | (u16::from(self.frequency_index) << 7)
            | (u16::from(self.channels) << 3);
        v.to_be_bytes()
    }
}

/// Collects ADTS bytes as they arrive and hands out whole frames. Holds at most one
/// incomplete frame (8191 bytes).
#[derive(Debug, Default)]
pub struct AdtsParser {
    buffer: BytesMut,
}

const MAX_FRAME: usize = 8191;

impl AdtsParser {
    pub fn push(&mut self, data: &[u8]) -> Vec<(AacConfig, Bytes)> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let Some(sync) = self
                .buffer
                .windows(2)
                .position(|w| w[0] == 0xFF && w[1] & 0xF6 == 0xF0)
            else {
                let keep = usize::from(self.buffer.last() == Some(&0xFF));
                let drop = self.buffer.len() - keep;
                let _ = self.buffer.split_to(drop);
                break;
            };
            let _ = self.buffer.split_to(sync);
            if self.buffer.len() < 7 {
                break;
            }
            let b = &self.buffer;
            let protection_absent = b[1] & 1 == 1;
            let header_len = if protection_absent { 7 } else { 9 };
            let config = AacConfig {
                object_type: (b[2] >> 6) + 1,
                frequency_index: (b[2] >> 2) & 0x0F,
                channels: ((b[2] & 1) << 2) | (b[3] >> 6),
            };
            let frame_len = (usize::from(b[3] & 0x03) << 11)
                | (usize::from(b[4]) << 3)
                | (usize::from(b[5]) >> 5);
            let blocks = b[6] & 0x03;
            if frame_len < header_len
                || frame_len > MAX_FRAME
                || config.sample_rate() == 0
                || blocks != 0
            {
                let _ = self.buffer.split_to(1);
                continue;
            }
            if self.buffer.len() < frame_len {
                break;
            }
            let frame = self.buffer.split_to(frame_len).freeze();
            out.push((config, frame.slice(header_len..)));
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn adts_frame(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() + 7;
        let mut out = vec![
            0xFF,
            0xF1,
            (1 << 6) | (4 << 2),
            (2 << 6) | ((len >> 11) as u8 & 0x03),
            (len >> 3) as u8,
            ((len as u8 & 0x07) << 5) | 0x1F,
            0xFC,
        ];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn frames_come_out_whole_across_reads() {
        let stream = [adts_frame(b"first"), adts_frame(b"second frame")].concat();
        let mut parser = AdtsParser::default();
        let mut frames = Vec::new();
        for piece in stream.chunks(5) {
            frames.extend(parser.push(piece));
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1, "first");
        assert_eq!(frames[1].1, "second frame");
        let config = frames[0].0;
        assert_eq!(config.sample_rate(), 44_100);
        assert_eq!(config.channels, 2);
        assert_eq!(config.audio_specific_config(), [0x12, 0x10]);
    }

    #[test]
    fn garbage_before_a_frame_is_skipped() {
        let mut parser = AdtsParser::default();
        let mut stream = vec![1, 2, 3, 0xFF, 0x00];
        stream.extend(adts_frame(b"x"));
        let frames = parser.push(&stream);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, "x");
    }
}
