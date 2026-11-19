use std::collections::HashSet;
use std::io::{self, Read, Write};

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation, PayloadId};

/// Largest message netcast will carry, before or after compression.
pub const MAX_OBJECT: usize = 4 << 20;
// K'max from RFC 6330; we only ever use one source block
const MAX_SYMBOLS: u64 = 56403;
const ALIGN: u8 = 4;

pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut w = brotli::CompressorWriter::new(Vec::new(), 4096, 11, 22);
    w.write_all(data).expect("writing to a Vec can't fail");
    w.into_inner()
}

pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    brotli::Decompressor::new(data, 4096)
        .take(MAX_OBJECT as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > MAX_OBJECT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message decompresses to more than 4 MiB",
        ));
    }
    Ok(out)
}

pub fn round_down(n: usize) -> usize {
    n - n % ALIGN as usize
}

pub fn round_up(n: usize) -> usize {
    n.div_ceil(ALIGN as usize) * ALIGN as usize
}

/// Checks that an object of `len` bytes fits in one source block at this symbol size.
pub fn fits(len: usize, symbol_size: usize) -> bool {
    len > 0 && len <= MAX_OBJECT && (len as u64).div_ceil(symbol_size as u64) <= MAX_SYMBOLS
}

/// The OTI netcast uses for an object, or `None` if those numbers can't describe one.
pub fn oti(len: usize, symbol_size: usize) -> Option<ObjectTransmissionInformation> {
    let ok = symbol_size > 0
        && symbol_size.is_multiple_of(ALIGN as usize)
        && symbol_size <= u16::MAX as usize
        && fits(len, symbol_size);
    ok.then(|| ObjectTransmissionInformation::new(len as u64, symbol_size as u16, 1, 1, ALIGN))
}

/// The sending side of one fountain-coded object.
pub struct Object {
    encoder: Encoder,
    k: u32,
}

impl Object {
    /// `None` when the data is empty, too big, or the symbol size isn't a multiple of 4.
    pub fn new(data: &[u8], symbol_size: usize) -> Option<Object> {
        let oti = oti(data.len(), symbol_size)?;
        Some(Object {
            encoder: Encoder::new(data, oti),
            k: data.len().div_ceil(symbol_size) as u32,
        })
    }

    pub fn oti(&self) -> ObjectTransmissionInformation {
        self.encoder.get_config()
    }

    /// Number of source symbols.
    pub fn k(&self) -> u32 {
        self.k
    }

    pub fn source(&self) -> Vec<(u32, Vec<u8>)> {
        split(self.encoder.get_block_encoders()[0].source_packets())
    }

    /// Repair symbols number `first` to `first + n - 1`. Each number gives a distinct ESI.
    pub fn repair(&self, first: u32, n: u32) -> Vec<(u32, Vec<u8>)> {
        split(self.encoder.get_block_encoders()[0].repair_packets(first, n))
    }
}

fn split(packets: Vec<EncodingPacket>) -> Vec<(u32, Vec<u8>)> {
    packets
        .into_iter()
        .map(|p| {
            let (id, data) = p.split();
            (id.encoding_symbol_id(), data)
        })
        .collect()
}

/// The receiving side: collects symbols until the object decodes.
pub struct Assembly {
    decoder: Decoder,
    symbol_size: usize,
    k: u32,
    seen: HashSet<u32>,
    result: Option<Vec<u8>>,
}

impl Assembly {
    /// The OTI comes off the air, so anything raptorq would panic on is refused here.
    pub fn new(oti: ObjectTransmissionInformation) -> Option<Assembly> {
        let t = oti.symbol_size() as usize;
        let len = oti.transfer_length() as usize;
        (self::oti(len, t) == Some(oti)).then(|| Assembly {
            decoder: Decoder::new(oti),
            symbol_size: t,
            k: len.div_ceil(t) as u32,
            seen: HashSet::new(),
            result: None,
        })
    }

    pub fn k(&self) -> u32 {
        self.k
    }

    /// Adds one symbol. Returns true if this symbol finished the object.
    pub fn add(&mut self, esi: u32, data: Vec<u8>) -> bool {
        if self.result.is_some() || data.len() != self.symbol_size || !self.seen.insert(esi) {
            return false;
        }
        let packet = EncodingPacket::new(PayloadId::new(0, esi), data);
        self.result = self.decoder.decode(packet);
        self.result.is_some()
    }

    pub fn result(&self) -> Option<&[u8]> {
        self.result.as_deref()
    }

    /// How many more symbols this receiver wants. With K symbols in hand decoding
    /// can still fail, and then one more is the honest answer.
    pub fn need(&self) -> u32 {
        if self.result.is_some() {
            0
        } else {
            self.k.saturating_sub(self.seen.len() as u32).max(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};

    fn data(n: usize, seed: u64) -> Vec<u8> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..n).map(|_| rng.random()).collect()
    }

    #[test]
    fn decodes_from_any_mix_of_source_and_repair() {
        let msg = data(5000, 1);
        let obj = Object::new(&msg, 244).unwrap();
        assert_eq!(obj.k(), 21);
        let mut rng = StdRng::seed_from_u64(2);
        let mut rx = Assembly::new(obj.oti()).unwrap();
        // lose about a third of the source symbols, then top up with repair
        for (esi, s) in obj.source() {
            if rng.random_bool(0.66) {
                rx.add(esi, s);
            }
        }
        assert!(rx.result().is_none());
        let mut next = 0;
        while rx.result().is_none() {
            let need = rx.need();
            for (esi, s) in obj.repair(next, need) {
                rx.add(esi, s);
            }
            next += need;
            assert!(next < 40, "needed far more repair than expected");
        }
        assert_eq!(rx.result().unwrap(), &msg[..]);
    }

    #[test]
    fn repair_esis_never_repeat_or_collide_with_source() {
        let obj = Object::new(&data(3000, 3), 244).unwrap();
        let mut esis: Vec<u32> = obj.source().into_iter().map(|(e, _)| e).collect();
        esis.extend(obj.repair(0, 10).into_iter().map(|(e, _)| e));
        esis.extend(obj.repair(10, 10).into_iter().map(|(e, _)| e));
        let unique: HashSet<_> = esis.iter().collect();
        assert_eq!(unique.len(), esis.len());
    }

    #[test]
    fn need_counts_down_and_ignores_duplicates_and_bad_lengths() {
        let obj = Object::new(&data(1000, 4), 244).unwrap();
        let mut rx = Assembly::new(obj.oti()).unwrap();
        assert_eq!(rx.need(), 5);
        let src = obj.source();
        rx.add(src[0].0, src[0].1.clone());
        rx.add(src[0].0, src[0].1.clone());
        rx.add(src[1].0, vec![0; 10]);
        assert_eq!(rx.need(), 4);
    }

    #[test]
    fn refuses_hostile_oti() {
        let zero_symbol =
            ObjectTransmissionInformation::deserialize(&[0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 1, 4]);
        assert!(Assembly::new(zero_symbol).is_none());
        let two_blocks =
            ObjectTransmissionInformation::deserialize(&[0, 0, 0, 16, 0, 0, 0, 4, 2, 0, 1, 4]);
        assert!(Assembly::new(two_blocks).is_none());
        let huge =
            ObjectTransmissionInformation::deserialize(&[0, 0x40, 0, 0, 0, 0, 0, 4, 1, 0, 1, 4]);
        assert!(Assembly::new(huge).is_none());
    }

    #[test]
    fn brotli_round_trip() {
        let text = "QST QST net check-in at 1900 local on the repeater. ".repeat(50);
        let packed = compress(text.as_bytes());
        assert!(packed.len() < text.len() / 5);
        assert_eq!(decompress(&packed).unwrap(), text.as_bytes());
        assert!(decompress(b"not brotli at all").is_err());
    }
}
