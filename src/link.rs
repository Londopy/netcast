use std::io;

/// One half-duplex radio channel as the sender sees it.
pub trait Link {
    fn send(&mut self, frame: &[u8]) -> io::Result<()>;
    /// Waits for the next netcast frame until `deadline_ms`.
    fn recv_until(&mut self, deadline_ms: u64) -> io::Result<Option<Vec<u8>>>;
    fn now_ms(&self) -> u64;
    /// When everything handed to `send` so far will have left the transmitter.
    fn tx_done_ms(&self) -> u64;
}

/// Waits out the clock, dropping whatever arrives.
pub fn idle(link: &mut impl Link, until_ms: u64) -> io::Result<()> {
    while link.now_ms() < until_ms {
        link.recv_until(until_ms)?;
    }
    Ok(())
}

// two addresses, control, PID, FCS and two flags
const AX25_OVERHEAD: u64 = 7 + 7 + 1 + 1 + 2 + 2;

/// Airtime model for AX.25 UI frames: TXDELAY once per transmission, then the
/// frames back to back. TXTAIL is left out.
pub struct Channel {
    baud: u64,
    txdelay_ms: u64,
    busy_until: u64,
    pub airtime_ms: u64,
}

impl Channel {
    pub fn new(baud: u32, txdelay_ms: u64) -> Channel {
        Channel {
            baud: baud as u64,
            txdelay_ms,
            busy_until: 0,
            airtime_ms: 0,
        }
    }

    pub fn txdelay_ms(&self) -> u64 {
        self.txdelay_ms
    }

    pub fn frame_ms(&self, info_len: usize) -> u64 {
        // bit stuffing adds about 3% on compressed data
        ((info_len as u64 + AX25_OVERHEAD) * 8 * 1030).div_ceil(self.baud)
    }

    /// Airtime of a frame sent on its own, keying up included.
    pub fn burst_ms(&self, info_len: usize) -> u64 {
        self.txdelay_ms + self.frame_ms(info_len)
    }

    /// Queues a frame at `now`. Returns when it ends.
    pub fn transmit(&mut self, now: u64, info_len: usize) -> u64 {
        let ms = if self.busy_until > now {
            self.frame_ms(info_len)
        } else {
            self.busy_until = now;
            self.burst_ms(info_len)
        };
        self.busy_until += ms;
        self.airtime_ms += ms;
        self.busy_until
    }

    pub fn busy_until(&self) -> u64 {
        self.busy_until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_to_back_frames_share_one_txdelay() {
        let mut ch = Channel::new(1200, 300);
        let one = ch.frame_ms(256);
        // about 1.9 s per full frame at 1200 baud, stuffing included
        assert!((1850..1950).contains(&one));
        assert_eq!(ch.transmit(0, 256), 300 + one);
        assert_eq!(ch.transmit(0, 256), 300 + 2 * one);
        // after the channel goes quiet, the next frame keys up again
        let later = ch.busy_until() + 5000;
        assert_eq!(ch.transmit(later, 256), later + 300 + one);
        assert_eq!(ch.airtime_ms, 2 * 300 + 3 * one);
    }
}
