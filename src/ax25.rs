use std::fmt;
use std::str::FromStr;

const CONTROL_UI: u8 = 0x03;
const PID_NO_L3: u8 = 0xF0;
const MAX_DIGIS: usize = 8;

/// A callsign with SSID, which is also how net group names like `NET1` travel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addr {
    call: String,
    ssid: u8,
}

impl FromStr for Addr {
    type Err = String;

    fn from_str(s: &str) -> Result<Addr, String> {
        let s = s.trim().to_ascii_uppercase();
        let (call, ssid) = match s.split_once('-') {
            Some((c, n)) => (c, n.parse::<u8>().ok().filter(|n| *n <= 15)),
            None => (s.as_str(), Some(0)),
        };
        let ok = (1..=6).contains(&call.len()) && call.bytes().all(|b| b.is_ascii_alphanumeric());
        match ssid {
            Some(ssid) if ok => Ok(Addr {
                call: call.to_string(),
                ssid,
            }),
            _ => Err(format!(
                "{s:?} is not a callsign (up to 6 letters/digits, optional -0 to -15)"
            )),
        }
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.ssid {
            0 => write!(f, "{}", self.call),
            n => write!(f, "{}-{n}", self.call),
        }
    }
}

impl Addr {
    fn encode(&self, command: bool, last: bool) -> [u8; 7] {
        let mut b = [b' ' << 1; 7];
        for (i, c) in self.call.bytes().enumerate() {
            b[i] = c << 1;
        }
        b[6] = 0x60 | (self.ssid << 1) | (u8::from(command) << 7) | u8::from(last);
        b
    }

    fn decode(b: &[u8]) -> Option<Addr> {
        let call: String = b[..6].iter().map(|c| (c >> 1) as char).collect();
        let call = call.trim_end();
        let addr = Addr {
            call: call.to_string(),
            ssid: (b[6] >> 1) & 0x0F,
        };
        (!call.is_empty() && call.bytes().all(|c| c.is_ascii_alphanumeric())).then_some(addr)
    }
}

/// A UI frame without FCS; the TNC adds that.
pub fn ui(dest: &Addr, src: &Addr, info: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(16 + info.len());
    f.extend_from_slice(&dest.encode(true, false));
    f.extend_from_slice(&src.encode(false, true));
    f.push(CONTROL_UI);
    f.push(PID_NO_L3);
    f.extend_from_slice(info);
    f
}

/// Destination, source and info field of a UI frame with no layer 3. Digipeater
/// addresses are skipped.
pub fn parse_ui(f: &[u8]) -> Option<(Addr, Addr, &[u8])> {
    let mut end = 0;
    loop {
        let a = f.get(end..end + 7)?;
        end += 7;
        if a[6] & 1 == 1 {
            break;
        }
        if end >= 7 * (2 + MAX_DIGIS) {
            return None;
        }
    }
    if end < 14 || f.get(end..end + 2)? != [CONTROL_UI, PID_NO_L3] {
        return None;
    }
    Some((
        Addr::decode(&f[0..7])?,
        Addr::decode(&f[7..14])?,
        &f[end + 2..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Addr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_callsigns() {
        assert_eq!(addr("k1abc-7").to_string(), "K1ABC-7");
        assert_eq!(addr("NET1").to_string(), "NET1");
        for bad in ["", "TOOLONG1", "K1ABC-16", "K1/ABC", "K1ABC-"] {
            assert!(bad.parse::<Addr>().is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn ui_frame_layout_matches_ax25() {
        let f = ui(&addr("NET1"), &addr("N0CALL-3"), b"hi");
        assert_eq!(&f[..7], &[0x9C, 0x8A, 0xA8, 0x62, 0x40, 0x40, 0xE0]);
        assert_eq!(f[13], 0x60 | (3 << 1) | 1);
        assert_eq!(&f[14..], &[0x03, 0xF0, b'h', b'i']);
    }

    #[test]
    fn round_trips_and_skips_digipeaters() {
        let f = ui(&addr("NET1"), &addr("K1ABC"), b"payload");
        assert_eq!(
            parse_ui(&f),
            Some((addr("NET1"), addr("K1ABC"), &b"payload"[..]))
        );

        let mut digi = f[..14].to_vec();
        digi[13] &= !1;
        digi.extend_from_slice(&addr("WIDE1-1").encode(false, true));
        digi.extend_from_slice(&f[14..]);
        assert_eq!(
            parse_ui(&digi),
            Some((addr("NET1"), addr("K1ABC"), &b"payload"[..]))
        );
    }

    #[test]
    fn ignores_other_frame_types() {
        let mut f = ui(&addr("NET1"), &addr("K1ABC"), b"x");
        f[15] = 0xCF;
        assert_eq!(parse_ui(&f), None);
        assert_eq!(parse_ui(&f[..10]), None);
    }
}
