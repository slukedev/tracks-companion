//! An epoch: one continuous run of audio with its own playlist, init segment and media
//! sequence. Frames wait here until the publication rule lets them out as segments, and
//! published segments live in a short ring. Pure state with explicit times, so the rules are
//! tested without a clock.

use std::collections::VecDeque;

use bytes::Bytes;

use crate::Tier;
use crate::adts::AacConfig;
use crate::flac::{self, StreamInfo};
use crate::hls;
use crate::mp4::{self, Codec, Sample};

/// AAC frames carry 1024 samples.
const AAC_FRAME: u32 = 1024;

/// How segments are cut and held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// The reported `output_delay_ms`, microseconds: audio is published that long before Music
    /// Assistant's timestamp says it is audible.
    pub output_delay_us: i64,
    /// A segment closes once it holds this much audio...
    pub segment_us: i64,
    /// ...and never grows past this, so every `EXTINF` rounds to the target duration.
    pub max_segment_us: i64,
    /// Segments listed in the playlist.
    pub window: usize,
    /// Segments kept after leaving the playlist, for a player a little behind.
    pub history: usize,
    /// FLAC goes to the AAC encoder this long before its publication time. It must stay below
    /// `max(min_buffer_ms, required_lead_time_ms)`, so audio a `stream/clear` with
    /// `from_timestamp` removes was never encoded.
    pub encode_lead_us: i64,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            output_delay_us: 4_000_000,
            segment_us: 1_000_000,
            max_segment_us: 1_400_000,
            window: 6,
            history: 3,
            encode_lead_us: 800_000,
        }
    }
}

#[derive(Clone, Debug)]
struct Frame {
    /// Music Assistant's timestamp for the first sample, server clock µs.
    ts: i64,
    samples: u32,
    data: Bytes,
}

/// A published segment.
#[derive(Clone, Debug)]
pub struct Segment {
    pub sequence: u64,
    pub bytes: Bytes,
    pub samples: u32,
    pub first_ts: i64,
    pub program_date_ms: Option<i64>,
}

/// Maps Music Assistant's clock to the local one, and says what time it is.
pub trait Timeline {
    /// A server timestamp on the local clock (µs), once the clock is synced.
    fn to_local(&self, server_us: i64) -> Option<i64>;
    fn now_local(&self) -> i64;
    /// Unix ms now, for `EXT-X-PROGRAM-DATE-TIME`.
    fn wall_ms(&self) -> i64;
}

#[derive(Debug)]
pub struct Epoch {
    pub number: u32,
    pub tier: Tier,
    shape: Shape,
    flac: StreamInfo,
    codec: Option<Codec>,
    init: Option<Bytes>,
    aac_bitrate: u32,
    /// Cellular: FLAC frames waiting to be encoded.
    input: VecDeque<Frame>,
    /// Frames in the output codec, waiting for publication.
    frames: VecDeque<Frame>,
    pending_bytes: usize,
    segments: VecDeque<Segment>,
    published_bytes: usize,
    next_sequence: u64,
    decode_time: u64,
    /// The end of the newest FLAC frame taken in, server µs.
    input_end: Option<i64>,
    /// The end of the newest frame that left the pending queues (encoded or published).
    committed_end: Option<i64>,
    /// Cellular: the first FLAC frame fed to the encoder, the zero of its output.
    encode_start: Option<i64>,
    fed_end: Option<i64>,
    /// Cellular: when each fed frame went in (local µs) and where it ends (server µs).
    fed: VecDeque<(i64, i64)>,
    aac_out: u64,
    pub ended: bool,
}

fn us(samples: u64, rate: u32) -> i64 {
    i64::try_from(samples.saturating_mul(1_000_000) / u64::from(rate.max(1))).unwrap_or(i64::MAX)
}

impl Epoch {
    pub fn new(number: u32, tier: Tier, flac: StreamInfo, shape: Shape, aac_bitrate: u32) -> Self {
        let (codec, init) = match tier {
            Tier::Lossless => {
                let codec = Codec::Flac(flac.clone());
                let init = Bytes::from(mp4::init_segment(&codec));
                (Some(codec), Some(init))
            }
            Tier::Cellular => (None, None),
        };
        Self {
            number,
            tier,
            shape,
            flac,
            codec,
            init,
            aac_bitrate,
            input: VecDeque::new(),
            frames: VecDeque::new(),
            pending_bytes: 0,
            segments: VecDeque::new(),
            published_bytes: 0,
            next_sequence: 0,
            decode_time: 0,
            input_end: None,
            committed_end: None,
            encode_start: None,
            fed_end: None,
            fed: VecDeque::new(),
            aac_out: 0,
            ended: false,
        }
    }

    pub fn codecs(&self) -> &'static str {
        match self.tier {
            Tier::Lossless => "fLaC",
            Tier::Cellular => "mp4a.40.2",
        }
    }

    pub fn bandwidth(&self) -> u32 {
        match self.tier {
            Tier::Lossless => {
                self.flac.sample_rate
                    * u32::from(self.flac.channels)
                    * u32::from(self.flac.bits_per_sample)
            }
            Tier::Cellular => self.aac_bitrate + self.aac_bitrate / 10,
        }
    }

    pub fn init(&self) -> Option<Bytes> {
        self.init.clone()
    }

    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    pub fn published_bytes(&self) -> usize {
        self.published_bytes
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn segment(&self, sequence: u64) -> Option<Bytes> {
        self.segments
            .iter()
            .find(|s| s.sequence == sequence)
            .map(|s| s.bytes.clone())
    }

    pub fn has_segments(&self) -> bool {
        !self.segments.is_empty()
    }

    /// The playlist entries: the newest `window` segments.
    pub fn entries(&self) -> Vec<hls::Entry> {
        let skip = self.segments.len().saturating_sub(self.shape.window);
        let rate = self.codec.as_ref().map_or(1, Codec::timescale);
        self.segments
            .iter()
            .skip(skip)
            .map(|s| hls::Entry {
                sequence: s.sequence,
                duration_secs: f64::from(s.samples) / f64::from(rate),
                program_date_ms: s.program_date_ms,
            })
            .collect()
    }

    pub fn playlist(&self, query: Option<&str>) -> String {
        hls::media(&self.entries(), self.next_sequence, self.ended, query)
    }

    /// Takes in one audio chunk's FLAC frames, timestamped from the chunk. A frame that starts
    /// before the end of audio already taken in (overlap after a `from_timestamp` clear) is
    /// dropped. Returns how many frames were dropped that way.
    pub fn push_flac(&mut self, chunk_ts: i64, frames: Vec<flac::Frame>) -> usize {
        if self.ended {
            return 0;
        }
        let rate = self.flac.sample_rate;
        let mut offset = 0u64;
        let mut dropped = 0;
        for frame in frames {
            let ts = chunk_ts + us(offset, rate);
            offset += u64::from(frame.samples);
            let end = ts + us(u64::from(frame.samples), rate);
            let half = us(u64::from(frame.samples), rate) / 2;
            if self.input_end.is_some_and(|e| ts + half < e) {
                dropped += 1;
                continue;
            }
            self.input_end = Some(end);
            self.pending_bytes += frame.data.len();
            let frame = Frame {
                ts,
                samples: frame.samples,
                data: frame.data,
            };
            match self.tier {
                Tier::Lossless => self.frames.push_back(frame),
                Tier::Cellular => self.input.push_back(frame),
            }
        }
        dropped
    }

    /// Takes in one AAC frame from the encoder. The first is the encoder's priming and goes.
    pub fn push_aac(&mut self, config: AacConfig, data: Bytes) {
        if self.ended || self.tier != Tier::Cellular {
            return;
        }
        let index = self.aac_out;
        self.aac_out += 1;
        if index == 0 {
            return;
        }
        if self.codec.is_none() {
            let codec = Codec::Aac {
                config,
                bitrate: self.aac_bitrate,
            };
            self.init = Some(Bytes::from(mp4::init_segment(&codec)));
            self.codec = Some(codec);
        }
        let Some(start) = self.encode_start else {
            return;
        };
        let ts = start + us((index - 1) * u64::from(AAC_FRAME), config.sample_rate());
        self.pending_bytes += data.len();
        self.frames.push_back(Frame {
            ts,
            samples: AAC_FRAME,
            data,
        });
    }

    /// Whether the encoder sat on fed audio for longer than `limit_us` of local time without
    /// producing it: it fell behind real time.
    pub fn encoder_stalled(&mut self, now_local: i64, limit_us: i64) -> bool {
        let (Some(start), Some(codec)) = (self.encode_start, self.codec.as_ref()) else {
            return self
                .fed
                .front()
                .is_some_and(|(at, _)| now_local - at > limit_us);
        };
        let produced = start
            + us(
                self.aac_out.saturating_sub(1) * u64::from(AAC_FRAME),
                codec.timescale(),
            );
        while self.fed.front().is_some_and(|(_, end)| *end <= produced) {
            self.fed.pop_front();
        }
        self.fed
            .front()
            .is_some_and(|(at, _)| now_local - at > limit_us)
    }

    /// `stream/clear` with `from_timestamp`: audio at or after it goes, earlier audio stays.
    pub fn clear_from(&mut self, from: i64) {
        let mut freed = 0;
        for queue in [&mut self.input, &mut self.frames] {
            queue.retain(|f| {
                let keep = f.ts < from;
                if !keep {
                    freed += f.data.len();
                }
                keep
            });
        }
        self.pending_bytes -= freed;
        let flac_rate = self.flac.sample_rate;
        let out_rate = self.codec.as_ref().map_or(flac_rate, Codec::timescale);
        let input = self
            .input
            .back()
            .map(|f| f.ts + us(u64::from(f.samples), flac_rate));
        let output = self
            .frames
            .back()
            .map(|f| f.ts + us(u64::from(f.samples), out_rate));
        self.input_end = [input, output, self.fed_end, self.committed_end]
            .into_iter()
            .flatten()
            .max();
    }

    /// `stream/end`, or the epoch was replaced: unpublished audio goes, the playlist ends.
    pub fn end(&mut self) {
        self.ended = true;
        self.input.clear();
        self.frames.clear();
        self.pending_bytes = 0;
    }

    /// Drops the oldest pending frames until at most `cap` bytes wait. Returns how many went.
    pub fn enforce_pending_cap(&mut self, cap: usize) -> usize {
        let mut dropped = 0;
        while self.pending_bytes > cap {
            let frame = if self.input.is_empty() {
                self.frames.pop_front()
            } else {
                self.input.pop_front()
            };
            let Some(frame) = frame else { break };
            self.pending_bytes -= frame.data.len();
            dropped += 1;
        }
        dropped
    }

    /// Drops the oldest published segment. Returns the bytes freed.
    pub fn trim_oldest(&mut self) -> usize {
        self.segments.pop_front().map_or(0, |s| {
            self.published_bytes -= s.bytes.len();
            s.bytes.len()
        })
    }

    /// Cellular: the FLAC frames due at the encoder now.
    /// Each frame with its length in samples.
    pub fn take_due_input(&mut self, timeline: &dyn Timeline) -> Vec<(Bytes, u32)> {
        let now = timeline.now_local();
        let rate = self.flac.sample_rate;
        let mut out = Vec::new();
        while let Some(front) = self.input.front() {
            let end = front.ts + us(u64::from(front.samples), rate);
            let Some(local) = timeline.to_local(end) else {
                break;
            };
            if local - self.shape.output_delay_us - self.shape.encode_lead_us > now {
                break;
            }
            let Some(frame) = self.input.pop_front() else {
                break;
            };
            self.pending_bytes -= frame.data.len();
            self.encode_start.get_or_insert(frame.ts);
            self.fed_end = Some(end);
            self.committed_end = Some(end);
            self.fed.push_back((now, end));
            if self.fed.len() > 4096 {
                self.fed.pop_front();
            }
            out.push((frame.data, frame.samples));
        }
        out
    }

    /// The frames that make the next segment, if it is complete, and its end timestamp. A
    /// segment is complete at the target length, or when the next frame would take it past
    /// the maximum.
    fn next_segment(&self) -> Option<(usize, u32, i64)> {
        let rate = self.codec.as_ref()?.timescale();
        let first = self.frames.front()?;
        let to_samples = |us: i64| u64::try_from(us).unwrap_or(0) * u64::from(rate) / 1_000_000;
        let (target, max) = (
            to_samples(self.shape.segment_us),
            to_samples(self.shape.max_segment_us),
        );
        let mut samples = 0u64;
        let mut count = 0;
        let mut complete = false;
        for frame in &self.frames {
            if count > 0 && samples + u64::from(frame.samples) > max {
                complete = true;
                break;
            }
            samples += u64::from(frame.samples);
            count += 1;
            if samples >= target {
                complete = true;
                break;
            }
        }
        if !complete {
            return None;
        }
        let samples = u32::try_from(samples).unwrap_or(u32::MAX);
        Some((count, samples, first.ts + us(u64::from(samples), rate)))
    }

    /// When the next segment is due on the local clock, if it is complete. Cellular epochs
    /// also wake for their next encoder feed.
    pub fn next_deadline(&self, timeline: &dyn Timeline) -> Option<i64> {
        let publish = self
            .next_segment()
            .and_then(|(_, _, end)| timeline.to_local(end))
            .map(|local| local - self.shape.output_delay_us);
        let feed = self.input.front().and_then(|front| {
            let end = front.ts + us(u64::from(front.samples), self.flac.sample_rate);
            timeline
                .to_local(end)
                .map(|l| l - self.shape.output_delay_us - self.shape.encode_lead_us)
        });
        match (publish, feed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Publishes every complete segment whose end, minus the output delay, has passed.
    /// Returns how many went out.
    pub fn publish_due(&mut self, timeline: &dyn Timeline) -> usize {
        let mut published = 0;
        if self.ended {
            return 0;
        }
        let now = timeline.now_local();
        while let Some((count, samples, end)) = self.next_segment() {
            let Some(local_end) = timeline.to_local(end) else {
                break;
            };
            if local_end - self.shape.output_delay_us > now {
                break;
            }
            let frames: Vec<Frame> = self.frames.drain(..count).collect();
            let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
            self.pending_bytes -= bytes;
            let first_ts = frames[0].ts;
            let samples_list: Vec<Sample<'_>> = frames
                .iter()
                .map(|f| Sample {
                    duration: f.samples,
                    data: &f.data,
                })
                .collect();
            let sequence = u32::try_from(self.next_sequence + 1).unwrap_or(u32::MAX);
            let segment = mp4::media_segment(sequence, self.decode_time, &samples_list);
            let program_date_ms = timeline
                .to_local(first_ts)
                .map(|local| timeline.wall_ms() + (local - now) / 1000);
            self.decode_time += u64::from(samples);
            self.committed_end = Some(self.committed_end.map_or(end, |c| c.max(end)));
            self.published_bytes += segment.len();
            self.segments.push_back(Segment {
                sequence: self.next_sequence,
                bytes: Bytes::from(segment),
                samples,
                first_ts,
                program_date_ms,
            });
            self.next_sequence += 1;
            published += 1;
            while self.segments.len() > self.shape.window + self.shape.history {
                self.trim_oldest();
            }
        }
        published
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::flac::tests::{fake_frame, streaminfo};

    /// Server and local clocks are the same here; `now` moves by hand.
    struct Fixed {
        now: Cell<i64>,
    }

    impl Timeline for Fixed {
        fn to_local(&self, server_us: i64) -> Option<i64> {
            Some(server_us)
        }
        fn now_local(&self) -> i64 {
            self.now.get()
        }
        fn wall_ms(&self) -> i64 {
            1_000_000
        }
    }

    fn info() -> StreamInfo {
        StreamInfo::from_codec_header(&streaminfo()).expect("info")
    }

    fn frames(n: usize) -> Vec<flac::Frame> {
        (0..n)
            .map(|i| flac::Frame {
                samples: 4608,
                data: Bytes::from(fake_frame(i as u8, 4608, &[i as u8; 100])),
            })
            .collect()
    }

    /// 4608 samples at 44.1 kHz.
    const FRAME_US: i64 = 104_489;

    fn lossless() -> Epoch {
        Epoch::new(1, Tier::Lossless, info(), Shape::default(), 256_000)
    }

    #[test]
    fn a_segment_waits_until_its_end_minus_the_output_delay() {
        let mut epoch = lossless();
        epoch.push_flac(10_000_000, frames(20));
        // Ten frames make 1.045 s; the first segment ends at 10_000_000 + 1_044_897.
        let end = 10_000_000 + 1_044_897;
        let clock = Fixed {
            now: Cell::new(end - 4_000_000 - 1),
        };
        assert_eq!(epoch.next_deadline(&clock), Some(end - 4_000_000));
        assert_eq!(epoch.publish_due(&clock), 0);
        clock.now.set(end - 4_000_000);
        assert_eq!(epoch.publish_due(&clock), 1);
        let entries = epoch.entries();
        assert_eq!(entries.len(), 1);
        assert!((entries[0].duration_secs - 1.044_897).abs() < 1e-6);
    }

    #[test]
    fn an_incomplete_segment_is_never_published() {
        let mut epoch = lossless();
        epoch.push_flac(0, frames(9));
        let clock = Fixed {
            now: Cell::new(i64::MAX / 2),
        };
        assert_eq!(epoch.publish_due(&clock), 0);
        assert_eq!(epoch.next_deadline(&clock), None);
    }

    #[test]
    fn decode_times_follow_the_samples() {
        let mut epoch = lossless();
        epoch.push_flac(0, frames(30));
        let clock = Fixed {
            now: Cell::new(i64::MAX / 2),
        };
        assert_eq!(epoch.publish_due(&clock), 3);
        let second = epoch.segment(1).expect("segment");
        let tfdt = second.windows(4).position(|w| w == b"tfdt").expect("tfdt") - 4;
        let time = u64::from_be_bytes(second[tfdt + 12..tfdt + 20].try_into().expect("time"));
        assert_eq!(time, 46_080);
    }

    #[test]
    fn a_clear_from_a_timestamp_drops_only_later_unpublished_audio() {
        let mut epoch = lossless();
        epoch.push_flac(0, frames(30));
        let clock = Fixed {
            now: Cell::new(1_100_000 - 4_000_000),
        };
        assert_eq!(epoch.publish_due(&clock), 1);
        let from = 15 * FRAME_US + 5;
        epoch.clear_from(from);
        // Frames 10..15 stay; 15.. go. New audio from `from` is taken in, earlier audio is not.
        assert_eq!(epoch.push_flac(from - 2 * FRAME_US, frames(1)), 1);
        assert_eq!(epoch.push_flac(from, frames(10)), 0);
        clock.now.set(i64::MAX / 2);
        assert_eq!(epoch.publish_due(&clock), 1);
        assert_eq!(epoch.segment(1).map(|_| ()), Some(()));
    }

    #[test]
    fn segments_never_pass_the_maximum_length() {
        let mut epoch = lossless();
        let big: Vec<flac::Frame> = (0..4)
            .map(|i| flac::Frame {
                samples: 16_000,
                data: Bytes::from(vec![i; 10]),
            })
            .collect();
        epoch.push_flac(0, big);
        let clock = Fixed {
            now: Cell::new(i64::MAX / 2),
        };
        epoch.publish_due(&clock);
        for entry in epoch.entries() {
            assert!(entry.duration_secs <= 1.4, "{entry:?}");
        }
    }

    #[test]
    fn the_ring_keeps_window_and_history() {
        let mut epoch = lossless();
        epoch.push_flac(0, frames(200));
        let clock = Fixed {
            now: Cell::new(i64::MAX / 2),
        };
        assert_eq!(epoch.publish_due(&clock), 20);
        assert_eq!(epoch.entries().len(), 6);
        assert!(epoch.segment(10).is_none());
        assert!(epoch.segment(11).is_some());
        assert!(epoch.playlist(None).contains("#EXT-X-MEDIA-SEQUENCE:14\n"));
    }

    #[test]
    fn the_pending_cap_drops_the_oldest() {
        let mut epoch = lossless();
        epoch.push_flac(0, frames(50));
        let before = epoch.pending_bytes();
        let dropped = epoch.enforce_pending_cap(before / 2);
        assert!(dropped > 0);
        assert!(epoch.pending_bytes() <= before / 2);
    }

    #[test]
    fn ending_lists_endlist_and_drops_pending_audio() {
        let mut epoch = lossless();
        epoch.push_flac(0, frames(15));
        epoch.end();
        assert_eq!(epoch.pending_bytes(), 0);
        assert!(epoch.playlist(None).ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(epoch.push_flac(0, frames(1)), 0);
    }

    #[test]
    fn cellular_feeds_ahead_and_times_aac_from_the_first_fed_frame() {
        let mut epoch = Epoch::new(1, Tier::Cellular, info(), Shape::default(), 256_000);
        assert!(epoch.init().is_none());
        epoch.push_flac(10_000_000, frames(3));
        let clock = Fixed {
            now: Cell::new(10_000_000 + FRAME_US - 4_000_000 - 800_000),
        };
        assert_eq!(epoch.take_due_input(&clock).len(), 1);
        let config = AacConfig {
            object_type: 2,
            frequency_index: 4,
            channels: 2,
        };
        for _ in 0..50 {
            epoch.push_aac(config, Bytes::from_static(b"aac"));
        }
        assert!(epoch.init().is_some());
        clock.now.set(i64::MAX / 2);
        assert_eq!(epoch.publish_due(&clock), 1);
        let segment = &epoch.segments[0];
        assert_eq!(segment.first_ts, 10_000_000);
        assert_eq!(segment.samples % 1024, 0);
    }
}
