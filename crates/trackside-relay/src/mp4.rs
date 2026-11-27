//! Fragmented MP4 for one audio track: the init segment (`ftyp`, `moov` with `mvex`) and media
//! segments (`moof`, `mdat`). FLAC uses the `fLaC` sample entry with a `dfLa` box (Xiph,
//! "Encapsulation of FLAC in ISO Base Media File Format"); AAC uses `mp4a` with `esds`
//! (ISO/IEC 14496-14). One MP4 sample per codec frame.

use crate::adts::AacConfig;
use crate::flac::StreamInfo;

const TRACK_ID: u32 = 1;
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// What the track carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    Flac(StreamInfo),
    Aac { config: AacConfig, bitrate: u32 },
}

impl Codec {
    pub fn timescale(&self) -> u32 {
        match self {
            Self::Flac(info) => info.sample_rate,
            Self::Aac { config, .. } => config.sample_rate(),
        }
    }

    /// The HLS `CODECS` value.
    pub fn rfc6381(&self) -> &'static str {
        match self {
            Self::Flac(_) => "fLaC",
            Self::Aac { .. } => "mp4a.40.2",
        }
    }
}

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self { out: Vec::new() }
    }

    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn bytes(&mut self, v: &[u8]) {
        self.out.extend_from_slice(v);
    }

    fn zeros(&mut self, n: usize) {
        self.out.resize(self.out.len() + n, 0);
    }

    fn boxed(&mut self, kind: [u8; 4], body: impl FnOnce(&mut Self)) {
        let start = self.out.len();
        self.u32(0);
        self.bytes(&kind);
        body(self);
        let size = u32::try_from(self.out.len() - start).unwrap_or(u32::MAX);
        self.out[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }

    fn full(&mut self, kind: [u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut Self)) {
        self.boxed(kind, |w| {
            w.u32((u32::from(version) << 24) | (flags & 0x00FF_FFFF));
            body(w);
        });
    }

    fn matrix(&mut self) {
        for v in MATRIX {
            self.u32(v);
        }
    }

    /// An MPEG-4 descriptor with a one-byte length (every descriptor here is short).
    fn descriptor(&mut self, tag: u8, body: impl FnOnce(&mut Self)) {
        self.u8(tag);
        let at = self.out.len();
        self.u8(0);
        body(self);
        self.out[at] = u8::try_from(self.out.len() - at - 1).unwrap_or(0x7F);
    }
}

fn audio_sample_entry(w: &mut Writer, channels: u16, sample_rate: u32) {
    w.zeros(6);
    w.u16(1);
    w.zeros(8);
    w.u16(channels);
    w.u16(16);
    w.u16(0);
    w.u16(0);
    w.u32(if sample_rate <= 0xFFFF {
        sample_rate << 16
    } else {
        0
    });
}

fn sample_entry(w: &mut Writer, codec: &Codec) {
    match codec {
        Codec::Flac(info) => w.boxed(*b"fLaC", |w| {
            audio_sample_entry(w, u16::from(info.channels), info.sample_rate);
            w.full(*b"dfLa", 0, 0, |w| {
                w.u8(0x80);
                w.bytes(&[0, 0, 34]);
                w.bytes(&info.raw);
            });
        }),
        Codec::Aac { config, bitrate } => w.boxed(*b"mp4a", |w| {
            audio_sample_entry(w, u16::from(config.channels), config.sample_rate());
            w.full(*b"esds", 0, 0, |w| {
                w.descriptor(0x03, |w| {
                    w.u16(0);
                    w.u8(0);
                    w.descriptor(0x04, |w| {
                        w.u8(0x40);
                        w.u8(0x15);
                        w.bytes(&[0, 0x18, 0]);
                        w.u32(*bitrate);
                        w.u32(*bitrate);
                        w.descriptor(0x05, |w| w.bytes(&config.audio_specific_config()));
                    });
                    w.descriptor(0x06, |w| w.u8(0x02));
                });
            });
        }),
    }
}

/// The init segment.
pub fn init_segment(codec: &Codec) -> Vec<u8> {
    let timescale = codec.timescale();
    let mut w = Writer::new();
    w.boxed(*b"ftyp", |w| {
        w.bytes(b"iso6");
        w.u32(0);
        w.bytes(b"iso6");
        w.bytes(b"mp41");
    });
    w.boxed(*b"moov", |w| {
        w.full(*b"mvhd", 0, 0, |w| {
            w.u32(0);
            w.u32(0);
            w.u32(1000);
            w.u32(0);
            w.u32(0x0001_0000);
            w.u16(0x0100);
            w.zeros(10);
            w.matrix();
            w.zeros(24);
            w.u32(TRACK_ID + 1);
        });
        w.boxed(*b"trak", |w| {
            w.full(*b"tkhd", 0, 0x03, |w| {
                w.u32(0);
                w.u32(0);
                w.u32(TRACK_ID);
                w.u32(0);
                w.u32(0);
                w.zeros(8);
                w.u16(0);
                w.u16(0);
                w.u16(0x0100);
                w.u16(0);
                w.matrix();
                w.u32(0);
                w.u32(0);
            });
            w.boxed(*b"mdia", |w| {
                w.full(*b"mdhd", 0, 0, |w| {
                    w.u32(0);
                    w.u32(0);
                    w.u32(timescale);
                    w.u32(0);
                    w.u16(0x55C4);
                    w.u16(0);
                });
                w.full(*b"hdlr", 0, 0, |w| {
                    w.u32(0);
                    w.bytes(b"soun");
                    w.zeros(12);
                    w.bytes(b"SoundHandler\0");
                });
                w.boxed(*b"minf", |w| {
                    w.full(*b"smhd", 0, 0, |w| {
                        w.u16(0);
                        w.u16(0);
                    });
                    w.boxed(*b"dinf", |w| {
                        w.full(*b"dref", 0, 0, |w| {
                            w.u32(1);
                            w.full(*b"url ", 0, 1, |_| {});
                        });
                    });
                    w.boxed(*b"stbl", |w| {
                        w.full(*b"stsd", 0, 0, |w| {
                            w.u32(1);
                            sample_entry(w, codec);
                        });
                        w.full(*b"stts", 0, 0, |w| w.u32(0));
                        w.full(*b"stsc", 0, 0, |w| w.u32(0));
                        w.full(*b"stsz", 0, 0, |w| {
                            w.u32(0);
                            w.u32(0);
                        });
                        w.full(*b"stco", 0, 0, |w| w.u32(0));
                    });
                });
            });
        });
        w.boxed(*b"mvex", |w| {
            w.full(*b"trex", 0, 0, |w| {
                w.u32(TRACK_ID);
                w.u32(1);
                w.u32(0);
                w.u32(0);
                w.u32(0);
            });
        });
    });
    w.out
}

/// One codec frame as an MP4 sample.
#[derive(Clone, Copy, Debug)]
pub struct Sample<'a> {
    pub duration: u32,
    pub data: &'a [u8],
}

/// A media segment: `moof` with one `traf` (`tfdt` at `decode_time`, a `trun` with each
/// sample's duration and size) and the `mdat` with the samples.
pub fn media_segment(sequence: u32, decode_time: u64, samples: &[Sample<'_>]) -> Vec<u8> {
    let payload: usize = samples.iter().map(|s| s.data.len()).sum();
    let mut w = Writer::new();
    let mut offset_at = 0;
    w.boxed(*b"moof", |w| {
        w.full(*b"mfhd", 0, 0, |w| w.u32(sequence));
        w.boxed(*b"traf", |w| {
            w.full(*b"tfhd", 0, 0x02_0000, |w| w.u32(TRACK_ID));
            w.full(*b"tfdt", 1, 0, |w| w.u64(decode_time));
            w.full(*b"trun", 0, 0x00_0301, |w| {
                w.u32(u32::try_from(samples.len()).unwrap_or(u32::MAX));
                offset_at = w.out.len();
                w.u32(0);
                for sample in samples {
                    w.u32(sample.duration);
                    w.u32(u32::try_from(sample.data.len()).unwrap_or(u32::MAX));
                }
            });
        });
    });
    let data_offset = u32::try_from(w.out.len() + 8).unwrap_or(u32::MAX);
    w.out[offset_at..offset_at + 4].copy_from_slice(&data_offset.to_be_bytes());
    w.out.reserve(payload + 8);
    w.boxed(*b"mdat", |w| {
        for sample in samples {
            w.bytes(sample.data);
        }
    });
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flac::tests::streaminfo;

    /// The (type, size) of each top-level box, checking that sizes add up.
    fn top_boxes(data: &[u8]) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().expect("size")) as usize;
            assert!(size >= 8 && at + size <= data.len(), "bad box at {at}");
            out.push((
                String::from_utf8_lossy(&data[at + 4..at + 8]).into_owned(),
                size,
            ));
            at += size;
        }
        out
    }

    fn find(data: &[u8], kind: [u8; 4]) -> usize {
        data.windows(4)
            .position(|w| w == kind)
            .expect("box present")
            - 4
    }

    fn flac() -> Codec {
        Codec::Flac(StreamInfo::from_codec_header(&streaminfo()).expect("info"))
    }

    #[test]
    fn the_flac_init_segment_is_ftyp_and_moov() {
        let init = init_segment(&flac());
        let boxes = top_boxes(&init);
        assert_eq!(
            boxes.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(),
            ["ftyp", "moov"]
        );
        let dfla = find(&init, *b"dfLa");
        // size 50 = header 8, version and flags 4, block header 4, STREAMINFO 34
        assert_eq!(
            u32::from_be_bytes(init[dfla..dfla + 4].try_into().expect("size")),
            50
        );
        assert_eq!(init[dfla + 12], 0x80);
        let mdhd = find(&init, *b"mdhd");
        assert_eq!(
            u32::from_be_bytes(init[mdhd + 20..mdhd + 24].try_into().expect("timescale")),
            44_100
        );
    }

    #[test]
    fn the_aac_init_segment_carries_the_audio_specific_config() {
        let codec = Codec::Aac {
            config: AacConfig {
                object_type: 2,
                frequency_index: 4,
                channels: 2,
            },
            bitrate: 256_000,
        };
        let init = init_segment(&codec);
        let esds = find(&init, *b"esds");
        let body = &init[esds + 12..];
        assert_eq!(&body[..2], &[0x03, 25]);
        let asc = body
            .windows(3)
            .position(|w| w == [0x05, 2, 0x12])
            .expect("asc");
        assert_eq!(body[asc + 3], 0x10);
        assert_eq!(codec.rfc6381(), "mp4a.40.2");
    }

    #[test]
    fn a_media_segment_points_trun_at_its_samples() {
        let samples = [
            Sample {
                duration: 4608,
                data: b"aaaa",
            },
            Sample {
                duration: 1152,
                data: b"bb",
            },
        ];
        let segment = media_segment(7, 9216, &samples);
        let boxes = top_boxes(&segment);
        assert_eq!(
            boxes.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(),
            ["moof", "mdat"]
        );
        let trun = find(&segment, *b"trun");
        let count = u32::from_be_bytes(segment[trun + 12..trun + 16].try_into().expect("count"));
        let offset =
            u32::from_be_bytes(segment[trun + 16..trun + 20].try_into().expect("off")) as usize;
        assert_eq!(count, 2);
        assert_eq!(&segment[offset..offset + 6], b"aaaabb");
        let tfdt = find(&segment, *b"tfdt");
        assert_eq!(segment[tfdt + 8], 1, "version 1");
        assert_eq!(
            u64::from_be_bytes(segment[tfdt + 12..tfdt + 20].try_into().expect("time")),
            9216
        );
        let mfhd = find(&segment, *b"mfhd");
        assert_eq!(
            u32::from_be_bytes(segment[mfhd + 12..mfhd + 16].try_into().expect("seq")),
            7
        );
    }
}
