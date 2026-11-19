use raptorq::{ObjectTransmissionInformation, PayloadId};

pub const HEADER_LEN: usize = 12;
const MAGIC: &[u8; 2] = b"NC";
const VERSION: u8 = 1;

/// Codec 1: brotli, then RFC 6330 RaptorQ.
pub const CODEC_BROTLI_RAPTORQ: u8 = 1;

const TYPE_ANNOUNCE: u8 = 1;
const TYPE_SYMBOL: u8 = 2;
const TYPE_STATUS: u8 = 3;
const TYPE_REPORT: u8 = 4;

const REPORT_DECODED: u8 = 1;
const REPORT_NEED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Addressed = 1,
    Bulletin = 2,
    Emergency = 3,
}

impl Mode {
    fn from_byte(b: u8) -> Option<Mode> {
        match b {
            1 => Some(Mode::Addressed),
            2 => Some(Mode::Bulletin),
            3 => Some(Mode::Emergency),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Addressed => "addressed",
            Mode::Bulletin => "bulletin",
            Mode::Emergency => "emergency",
        }
    }
}

/// What a delivery is: carried as its own small fountain-coded object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announce {
    pub codec: u8,
    pub oti: ObjectTransmissionInformation,
    /// SHA-256 of the compressed message, so a signature covers the message too.
    pub hash: [u8; 32],
    pub mode: Mode,
    pub ttl_min: u16,
    pub from: String,
    pub mid: String,
    pub recipients: Vec<String>,
    pub signature: Option<[u8; 64]>,
}

impl Announce {
    /// Everything but the signature: the part a signature covers.
    pub fn body(&self) -> Vec<u8> {
        let mut b = vec![self.codec];
        b.extend_from_slice(&self.oti.serialize());
        b.extend_from_slice(&self.hash);
        b.push(self.mode as u8);
        b.extend_from_slice(&self.ttl_min.to_be_bytes());
        push_str(&mut b, &self.from);
        push_str(&mut b, &self.mid);
        b.push(self.recipients.len() as u8);
        for r in &self.recipients {
            push_str(&mut b, r);
        }
        b
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = self.body();
        match &self.signature {
            Some(sig) => {
                b.push(64);
                b.extend_from_slice(sig);
            }
            None => b.push(0),
        }
        b
    }

    pub fn parse(b: &[u8]) -> Option<Announce> {
        let mut r = Reader(b);
        let codec = r.u8()?;
        let oti = ObjectTransmissionInformation::deserialize(r.take(12)?.try_into().ok()?);
        let hash = r.take(32)?.try_into().ok()?;
        let mode = Mode::from_byte(r.u8()?)?;
        let ttl_min = r.u16()?;
        let from = r.str().filter(|s| valid_name(s))?;
        let mid = r.str().filter(|m| crate::message::valid_mid(m))?;
        let n = r.u8()?;
        let recipients = (0..n)
            .map(|_| r.str().filter(|s| valid_name(s)))
            .collect::<Option<Vec<_>>>()?;
        let signature = match r.u8()? {
            0 => None,
            64 => Some(r.take(64)?.try_into().ok()?),
            _ => return None,
        };
        r.rest().is_empty().then_some(Announce {
            codec,
            oti,
            hash,
            mode,
            ttl_min,
            from,
            mid,
            recipients,
            signature,
        })
    }
}

/// Callsigns and group names as they travel in an announcement: upper case, so a
/// name means one thing everywhere, and nothing that could upset a screen or log.
pub fn valid_name(s: &str) -> bool {
    (1..=9).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// One symbol of the announcement object. `len` is the announcement's byte length;
    /// the symbol size is whatever is left of the frame.
    Announce {
        id: u32,
        esi: u32,
        len: u16,
        symbol: Vec<u8>,
    },
    Symbol {
        id: u32,
        esi: u32,
        data: Vec<u8>,
    },
    /// Asks the stations flagged in `polled` (indexes into the recipient list) to report,
    /// each in its own slot of `slot_ms`, in list order.
    Status {
        id: u32,
        round: u8,
        slot_ms: u16,
        polled: Vec<bool>,
    },
    /// An answer to the status request of `round`. `need: None` means decoded.
    Report {
        id: u32,
        round: u8,
        index: u8,
        need: Option<u16>,
    },
}

impl Frame {
    pub fn id(&self) -> u32 {
        match self {
            Frame::Announce { id, .. }
            | Frame::Symbol { id, .. }
            | Frame::Status { id, .. }
            | Frame::Report { id, .. } => *id,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let (kind, esi) = match self {
            Frame::Announce { esi, .. } => (TYPE_ANNOUNCE, Some(*esi)),
            Frame::Symbol { esi, .. } => (TYPE_SYMBOL, Some(*esi)),
            Frame::Status { .. } => (TYPE_STATUS, None),
            Frame::Report { .. } => (TYPE_REPORT, None),
        };
        let mut b = Vec::with_capacity(HEADER_LEN + 256);
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        b.push(kind);
        b.extend_from_slice(&self.id().to_be_bytes());
        match esi {
            Some(esi) => b.extend_from_slice(&PayloadId::new(0, esi).serialize()),
            None => b.extend_from_slice(&[0; 4]),
        }
        match self {
            Frame::Announce { len, symbol, .. } => {
                b.extend_from_slice(&len.to_be_bytes());
                b.extend_from_slice(symbol);
            }
            Frame::Symbol { data, .. } => b.extend_from_slice(data),
            Frame::Status {
                round,
                slot_ms,
                polled,
                ..
            } => {
                b.push(*round);
                b.extend_from_slice(&slot_ms.to_be_bytes());
                b.push(polled.len() as u8);
                let mut bits = vec![0u8; polled.len().div_ceil(8)];
                for (i, _) in polled.iter().enumerate().filter(|(_, p)| **p) {
                    bits[i / 8] |= 0x80 >> (i % 8);
                }
                b.extend_from_slice(&bits);
            }
            Frame::Report {
                round, index, need, ..
            } => {
                match need {
                    None => b.push(REPORT_DECODED),
                    Some(_) => b.push(REPORT_NEED),
                }
                b.extend_from_slice(&need.unwrap_or(0).to_be_bytes());
                b.push(*index);
                b.push(*round);
            }
        }
        b
    }

    /// Anything that doesn't parse is someone else's traffic or noise, so it's just `None`.
    pub fn parse(b: &[u8]) -> Option<Frame> {
        let mut r = Reader(b);
        if r.take(2)? != MAGIC || r.u8()? != VERSION {
            return None;
        }
        let kind = r.u8()?;
        let id = u32::from_be_bytes(r.take(4)?.try_into().ok()?);
        let pid = PayloadId::deserialize(r.take(4)?.try_into().ok()?);
        // one source block per object, always
        if pid.source_block_number() != 0 {
            return None;
        }
        let esi = pid.encoding_symbol_id();
        match kind {
            TYPE_ANNOUNCE => {
                let len = r.u16()?;
                let symbol = r.rest().to_vec();
                (!symbol.is_empty()).then_some(Frame::Announce {
                    id,
                    esi,
                    len,
                    symbol,
                })
            }
            TYPE_SYMBOL => {
                let data = r.rest().to_vec();
                (!data.is_empty()).then_some(Frame::Symbol { id, esi, data })
            }
            TYPE_STATUS => {
                let round = r.u8()?;
                let slot_ms = r.u16()?;
                let n = r.u8()? as usize;
                let bits = r.take(n.div_ceil(8))?;
                let polled = (0..n)
                    .map(|i| bits[i / 8] & (0x80 >> (i % 8)) != 0)
                    .collect();
                Some(Frame::Status {
                    id,
                    round,
                    slot_ms,
                    polled,
                })
            }
            TYPE_REPORT => {
                let status = r.u8()?;
                let n = r.u16()?;
                let index = r.u8()?;
                let round = r.u8()?;
                let need = match status {
                    REPORT_DECODED => None,
                    REPORT_NEED => Some(n),
                    _ => return None,
                };
                Some(Frame::Report {
                    id,
                    round,
                    index,
                    need,
                })
            }
            _ => None,
        }
    }
}

fn push_str(b: &mut Vec<u8>, s: &str) {
    b.push(s.len() as u8);
    b.extend_from_slice(s.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announce() -> Announce {
        Announce {
            codec: CODEC_BROTLI_RAPTORQ,
            oti: ObjectTransmissionInformation::new(5000, 244, 1, 1, 4),
            hash: [7; 32],
            mode: Mode::Addressed,
            ttl_min: 60,
            from: "N0NCS".into(),
            mid: "ABCDEF123456".into(),
            recipients: vec!["K1ABC".into(), "W2XYZ-7".into()],
            signature: None,
        }
    }

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Announce {
                id: 7,
                esi: 3,
                len: 40,
                symbol: vec![9; 40],
            },
            Frame::Symbol {
                id: 0xDEADBEEF,
                esi: 70_000,
                data: vec![1, 2, 3, 4],
            },
            Frame::Status {
                id: 1,
                round: 2,
                slot_ms: 900,
                polled: vec![true, false, true],
            },
            Frame::Report {
                id: 1,
                round: 3,
                index: 4,
                need: Some(12),
            },
            Frame::Report {
                id: 1,
                round: 1,
                index: 0,
                need: None,
            },
        ];
        for f in frames {
            assert_eq!(Frame::parse(&f.encode()), Some(f));
        }
    }

    #[test]
    fn header_is_twelve_bytes() {
        let f = Frame::Symbol {
            id: 0x01020304,
            esi: 5,
            data: vec![0xAA; 244],
        };
        let b = f.encode();
        assert_eq!(b.len(), HEADER_LEN + 244);
        assert_eq!(&b[..12], &[b'N', b'C', 1, 2, 1, 2, 3, 4, 0, 0, 0, 5]);
    }

    #[test]
    fn status_bitmap_spans_bytes() {
        let polled: Vec<bool> = (0..20).map(|i| i % 3 == 0).collect();
        let f = Frame::Status {
            id: 1,
            round: 1,
            slot_ms: 1,
            polled: polled.clone(),
        };
        let Some(Frame::Status { polled: back, .. }) = Frame::parse(&f.encode()) else {
            panic!("status frame did not parse");
        };
        assert_eq!(back, polled);
    }

    #[test]
    fn announce_round_trips() {
        let a = announce();
        assert_eq!(Announce::parse(&a.to_bytes()), Some(a.clone()));
        let signed = Announce {
            signature: Some([3; 64]),
            ..a
        };
        let b = signed.to_bytes();
        assert_eq!(b.len(), signed.body().len() + 65);
        assert_eq!(Announce::parse(&b), Some(signed));
    }

    #[test]
    fn announce_names_are_plain() {
        for bad in ["\u{1b}[2J", "K1 ABC", "", "TOOLONGCALL", "k1abc"] {
            let a = Announce {
                from: bad.into(),
                ..announce()
            };
            assert_eq!(Announce::parse(&a.to_bytes()), None, "{bad:?}");
            let a = Announce {
                recipients: vec![bad.into()],
                ..announce()
            };
            assert_eq!(Announce::parse(&a.to_bytes()), None, "{bad:?}");
        }
    }

    #[test]
    fn rejects_foreign_and_truncated_frames() {
        assert_eq!(Frame::parse(b"hello world, not ours"), None);
        let b = Frame::Report {
            id: 1,
            round: 1,
            index: 0,
            need: Some(3),
        }
        .encode();
        assert_eq!(Frame::parse(&b[..b.len() - 1]), None);
        let mut other_block = Frame::Symbol {
            id: 1,
            esi: 0,
            data: vec![1],
        }
        .encode();
        other_block[8] = 1;
        assert_eq!(Frame::parse(&other_block), None);
        let a = announce().to_bytes();
        assert_eq!(Announce::parse(&a[..a.len() - 1]), None);
    }
}
