//! Transport framing inside the Noise plaintext (messaging.md, Binary Message ID Structure and
//! Fragmentation): byte 0 is the message id, id 1 carries fragments of a larger message.

use bytes::Bytes;

pub(crate) const JSON: u8 = 0;
pub(crate) const FRAGMENT: u8 = 1;
pub(crate) const AUDIO: u8 = 4;

const FLAG_LAST: u8 = 0x01;
const FLAG_FIRST: u8 = 0x02;
const FLAGS_RESERVED: u8 = 0xFC;

/// The bytes in an audio chunk before the codec payload.
pub const AUDIO_HEADER_LEN: usize = 13;

/// A framing violation. The connection closes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    #[error("an empty binary message")]
    Empty,
    #[error("a malformed fragment sequence: {0}")]
    Fragment(&'static str),
    #[error("a reassembled message is larger than {0} bytes")]
    TooLarge(usize),
    #[error("an audio chunk shorter than 13 bytes")]
    ShortAudio,
}

/// One complete message: its id and the bytes after the id.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub id: u8,
    pub body: Bytes,
}

/// Reassembles fragments; keeps at most one message in flight, bounded by `limit` bytes.
#[derive(Debug)]
pub(crate) struct Reassembler {
    limit: usize,
    in_flight: Option<(u8, Vec<u8>)>,
}

impl Reassembler {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            in_flight: None,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.in_flight = None;
    }

    /// Feeds one decrypted plaintext; returns a message when one is complete.
    pub(crate) fn push(&mut self, plaintext: Vec<u8>) -> Result<Option<Message>, FrameError> {
        let (&id, rest) = plaintext.split_first().ok_or(FrameError::Empty)?;
        if id != FRAGMENT {
            if self.in_flight.is_some() {
                return Err(FrameError::Fragment(
                    "a whole message inside a fragmented one",
                ));
            }
            let mut body = plaintext;
            body.remove(0);
            return Ok(Some(Message {
                id,
                body: Bytes::from(body),
            }));
        }
        let (&flags, rest) = rest
            .split_first()
            .ok_or(FrameError::Fragment("no flags byte"))?;
        if flags & FLAGS_RESERVED != 0 {
            return Err(FrameError::Fragment("reserved flag bits set"));
        }
        let data = if flags & FLAG_FIRST != 0 {
            if self.in_flight.is_some() {
                return Err(FrameError::Fragment(
                    "a first fragment while one is in flight",
                ));
            }
            let (&orig, data) = rest
                .split_first()
                .ok_or(FrameError::Fragment("no original type"))?;
            if orig == FRAGMENT {
                return Err(FrameError::Fragment("original type 1"));
            }
            self.in_flight = Some((orig, Vec::new()));
            data
        } else {
            if self.in_flight.is_none() {
                return Err(FrameError::Fragment("a later fragment with none in flight"));
            }
            rest
        };
        let Some((orig, buffer)) = self.in_flight.as_mut() else {
            return Err(FrameError::Fragment("no fragment in flight"));
        };
        if buffer.len() + data.len() > self.limit {
            self.in_flight = None;
            return Err(FrameError::TooLarge(self.limit));
        }
        buffer.extend_from_slice(data);
        if flags & FLAG_LAST == 0 {
            return Ok(None);
        }
        let id = *orig;
        let body = self.in_flight.take().map(|(_, b)| b).unwrap_or_default();
        Ok(Some(Message {
            id,
            body: Bytes::from(body),
        }))
    }
}

/// The plaintexts that carry one message of `id`: one when it fits, fragments when not.
pub(crate) fn split(id: u8, body: &[u8], max_plaintext: usize) -> Vec<Vec<u8>> {
    if body.len() < max_plaintext {
        let mut whole = Vec::with_capacity(body.len() + 1);
        whole.push(id);
        whole.extend_from_slice(body);
        return vec![whole];
    }
    let first_room = max_plaintext - 3;
    let later_room = max_plaintext - 2;
    let mut out = Vec::new();
    let (first, mut rest) = body.split_at(first_room.min(body.len()));
    let mut piece = vec![FRAGMENT, FLAG_FIRST, id];
    piece.extend_from_slice(first);
    if rest.is_empty() {
        piece[1] |= FLAG_LAST;
    }
    out.push(piece);
    while !rest.is_empty() {
        let (data, tail) = rest.split_at(later_room.min(rest.len()));
        let mut piece = vec![FRAGMENT, if tail.is_empty() { FLAG_LAST } else { 0 }];
        piece.extend_from_slice(data);
        out.push(piece);
        rest = tail;
    }
    out
}

/// A player audio chunk (binary id 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioChunk {
    /// Server clock, microseconds, when the first sample is to be audible.
    pub timestamp: i64,
    /// Microseconds the server had in hand when it sent the chunk; `None` when not measured.
    pub send_ahead: Option<u32>,
    /// The codec payload: whole FLAC frames for this client.
    pub data: Bytes,
}

impl AudioChunk {
    /// Parses the body that followed the id byte.
    pub(crate) fn parse(body: &Bytes) -> Result<Self, FrameError> {
        if body.len() < AUDIO_HEADER_LEN - 1 {
            return Err(FrameError::ShortAudio);
        }
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&body[0..8]);
        let mut ahead = [0u8; 4];
        ahead.copy_from_slice(&body[8..12]);
        let send_ahead = match u32::from_be_bytes(ahead) {
            0 | u32::MAX => None,
            n => Some(n),
        };
        Ok(Self {
            timestamp: i64::from_be_bytes(ts),
            send_ahead,
            data: body.slice(12..),
        })
    }

    /// The bytes of this chunk as a server sends it, id included.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + self.data.len());
        out.push(AUDIO);
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.send_ahead.unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// The size the server counts against `buffer_capacity`: header and payload.
    pub fn accounted_len(&self) -> usize {
        AUDIO_HEADER_LEN + self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(r: &mut Reassembler, pieces: Vec<Vec<u8>>) -> Vec<Result<Option<Message>, FrameError>> {
        pieces.into_iter().map(|p| r.push(p)).collect()
    }

    #[test]
    fn a_split_message_reassembles() {
        let body: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let pieces = split(JSON, &body, 64);
        assert!(pieces.len() > 3);
        let mut r = Reassembler::new(1 << 20);
        let results = feed(&mut r, pieces);
        let last = results.last().expect("pieces").as_ref().expect("ok");
        assert_eq!(
            last.as_ref().expect("complete"),
            &Message {
                id: JSON,
                body: Bytes::from(body)
            }
        );
        assert!(
            results[..results.len() - 1]
                .iter()
                .all(|r| matches!(r, Ok(None)))
        );
    }

    #[test]
    fn a_small_message_is_not_fragmented() {
        assert_eq!(split(JSON, b"{}", 64), vec![vec![0, b'{', b'}']]);
    }

    #[test]
    fn a_first_fragment_during_another_is_refused() {
        let mut r = Reassembler::new(1 << 20);
        assert!(matches!(r.push(vec![1, FLAG_FIRST, 0, 1]), Ok(None)));
        assert!(matches!(
            r.push(vec![1, FLAG_FIRST, 0, 1]),
            Err(FrameError::Fragment(_))
        ));
    }

    #[test]
    fn a_later_fragment_without_a_first_is_refused() {
        let mut r = Reassembler::new(1 << 20);
        assert!(matches!(
            r.push(vec![1, FLAG_LAST, 1]),
            Err(FrameError::Fragment(_))
        ));
    }

    #[test]
    fn a_whole_message_inside_a_fragmented_one_is_refused() {
        let mut r = Reassembler::new(1 << 20);
        r.push(vec![1, FLAG_FIRST, 0, 1]).expect("first");
        assert!(matches!(
            r.push(vec![0, b'{']),
            Err(FrameError::Fragment(_))
        ));
    }

    #[test]
    fn reserved_flags_and_type_one_are_refused() {
        let mut r = Reassembler::new(1 << 20);
        assert!(r.push(vec![1, 0x04 | FLAG_FIRST, 0]).is_err());
        assert!(r.push(vec![1, FLAG_FIRST, 1]).is_err());
        assert!(r.push(vec![1]).is_err());
        assert!(r.push(vec![1, FLAG_FIRST]).is_err());
    }

    #[test]
    fn an_empty_plaintext_is_refused() {
        assert_eq!(Reassembler::new(8).push(Vec::new()), Err(FrameError::Empty));
    }

    #[test]
    fn reassembly_is_bounded() {
        let mut r = Reassembler::new(4);
        r.push(vec![1, FLAG_FIRST, 0, 1, 2]).expect("first");
        assert_eq!(r.push(vec![1, 0, 3, 4, 5]), Err(FrameError::TooLarge(4)));
    }

    #[test]
    fn audio_headers_parse_and_round_trip() {
        let chunk = AudioChunk {
            timestamp: -5,
            send_ahead: Some(9),
            data: Bytes::from_static(b"frames"),
        };
        let encoded = chunk.encode();
        let body = Bytes::from(encoded[1..].to_vec());
        assert_eq!(AudioChunk::parse(&body), Ok(chunk));
    }

    #[test]
    fn saturated_send_ahead_means_unmeasured() {
        let mut body = vec![0u8; 12];
        body[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        let chunk = AudioChunk::parse(&Bytes::from(body)).expect("chunk");
        assert_eq!(chunk.send_ahead, None);
        assert!(chunk.data.is_empty());
    }

    #[test]
    fn short_audio_is_an_error() {
        assert_eq!(
            AudioChunk::parse(&Bytes::from_static(&[0; 11])),
            Err(FrameError::ShortAudio)
        );
    }
}
