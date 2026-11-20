use std::fmt;
use std::io;

use ed25519_dalek::SigningKey;

use crate::codec::{self, Object};
use crate::frame::{
    Announce, CODEC_BROTLI_RAPTORQ, Frame, HEADER_LEN, Mode, Precedence, valid_name,
};
use crate::keys;
use crate::link::{Channel, Link, idle};

// a late joiner needs the announcement, so it goes out again this often
const ANNOUNCE_EVERY: usize = 20;
// repair stops being sized for a station that missed this many polls running
const MAX_MISSED: u8 = 2;

/// Where one recipient stands, as far as the sender has heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not asked yet.
    Pending,
    /// Reported it still needs `need` symbols. `answered` is false when it
    /// missed the latest poll, so `need` is from an earlier round.
    Receiving { need: u16, answered: bool },
    /// Reported decoding the message: its acknowledgement.
    Decoded,
    /// Polled, and never heard from in this delivery.
    Silent,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            State::Pending => write!(f, "pending"),
            State::Receiving {
                need,
                answered: true,
            } => write!(f, "receiving (needs {need})"),
            State::Receiving {
                need,
                answered: false,
            } => write!(f, "receiving (needs {need}; no answer last round)"),
            State::Decoded => write!(f, "decoded"),
            State::Silent => write!(f, "silent"),
        }
    }
}

pub struct Plan {
    /// Extra symbols in each full pass, as a fraction of K.
    pub margin: f64,
    /// Status rounds after the first one, each preceded by a repair round.
    pub rounds: u32,
    pub acks: bool,
    /// Bulletin repeats: full passes of fresh symbols, `repeat_gap_ms` apart.
    pub repeats: u32,
    pub repeat_gap_ms: u64,
    pub slot_ms: u16,
    /// How long receivers wait after hearing a status request before slot 0.
    pub lead_ms: u16,
    pub guard_ms: u64,
}

pub enum Progress<'a> {
    Pass { symbols: u32 },
    Status { round: u8, states: &'a [State] },
}

/// Slot length for one report: keying up, the frame itself, and a guard.
pub fn slot_ms(ch: &Channel, guard_ms: u64) -> u16 {
    let report = Frame::Report {
        id: 0,
        round: 0,
        index: 0,
        need: None,
    }
    .encode()
    .len();
    (ch.burst_ms(report) + guard_ms).min(u16::MAX as u64) as u16
}

/// How long KISS stations wait after a status request before slot 0. The
/// sender's TNC is still sending its closing flags, and its radio still
/// switching back to receive, when a station decodes the request; an answer
/// right away runs into that.
pub fn kiss_lead_ms(ch: &Channel, guard_ms: u64) -> u16 {
    (ch.txdelay_ms() + guard_ms).min(u16::MAX as u64) as u16
}

/// Who a delivery is from and for, and how receivers treat it.
pub struct Envelope {
    pub from: String,
    pub recipients: Vec<String>,
    pub mode: Mode,
    pub precedence: Precedence,
    pub ttl_min: u16,
}

/// One message, encoded and ready to go.
pub struct Delivery {
    pub id: u32,
    pub announce: Announce,
    ann: Object,
    ann_len: u16,
    payload: Object,
}

impl Delivery {
    /// `frame_max` is the largest netcast frame the transport carries, header included.
    /// With a key, the announcement is signed.
    pub fn new(
        id: u32,
        b2f: &[u8],
        env: Envelope,
        frame_max: usize,
        key: Option<&SigningKey>,
    ) -> Result<Delivery, String> {
        let mid =
            crate::message::check(b2f).map_err(|e| format!("not a usable B2F message: {e}"))?;
        if b2f.len() > codec::MAX_OBJECT {
            return Err(format!(
                "message is {} bytes; netcast takes up to 4 MiB",
                b2f.len()
            ));
        }
        let n = env.recipients.len();
        if n == 0 || n > 255 {
            return Err(format!("{n} recipients; netcast takes 1 to 255"));
        }
        let status = Frame::Status {
            id,
            round: 1,
            lead_ms: 0,
            slot_ms: 0,
            polled: vec![true; n],
        };
        if status.encode().len() > frame_max {
            return Err(format!(
                "a status request for {n} recipients doesn't fit in one {frame_max}-byte frame; send to fewer at a time, or use a bigger FEC mode"
            ));
        }
        if let Some(bad) = std::iter::once(&env.from)
            .chain(&env.recipients)
            .find(|s| !valid_name(s))
        {
            return Err(format!("{bad:?} is not a callsign or group name"));
        }
        let packed = codec::compress(b2f);
        let t = codec::round_down(frame_max - HEADER_LEN);
        let payload = Object::new(&packed, t).ok_or_else(|| {
            format!(
                "message is {} bytes compressed, too big to send",
                packed.len()
            )
        })?;
        let mut announce = Announce {
            codec: CODEC_BROTLI_RAPTORQ,
            oti: payload.oti(),
            hash: codec::hash(&packed),
            mode: env.mode,
            precedence: env.precedence,
            ttl_min: env.ttl_min,
            from: env.from,
            mid,
            recipients: env.recipients,
            signature: None,
        };
        if let Some(key) = key {
            keys::sign(&mut announce, id, key);
        }
        let body = announce.to_bytes();
        // the announce frame spends 2 bytes on the announcement's length
        let room = frame_max - HEADER_LEN - 2;
        let t_ann = if codec::round_up(body.len()) <= room {
            codec::round_up(body.len())
        } else {
            codec::round_down(room)
        };
        let ann = Object::new(&body, t_ann).ok_or("recipient list is too long to announce")?;
        Ok(Delivery {
            id,
            announce,
            ann,
            ann_len: body.len() as u16,
            payload,
        })
    }

    pub fn k(&self) -> u32 {
        self.payload.k()
    }
}

struct Session<'a, L: Link + ?Sized> {
    link: &'a mut L,
    d: &'a Delivery,
    next_repair: u32,
    announces: u32,
    /// Without status rounds nothing re-announces later, so each pass ends with
    /// the announcement too: a station that missed the first one still has its
    /// symbols waiting and decodes the moment it hears this one.
    closing_announce: bool,
}

impl<L: Link + ?Sized> Session<'_, L> {
    fn send(&mut self, f: Frame) -> io::Result<()> {
        self.link.send(&f.encode())
    }

    fn announce(&mut self) -> io::Result<()> {
        let k = self.d.ann.k();
        // single-frame announcements repeat their only source symbol; bigger ones
        // send fresh repair each time so repeats help stations that caught part of it
        let symbols = if k == 1 || self.announces == 0 {
            self.d.ann.source()
        } else {
            self.d.ann.repair((self.announces - 1) * k, k)
        };
        self.announces += 1;
        for (esi, symbol) in symbols {
            self.send(Frame::Announce {
                id: self.d.id,
                esi,
                len: self.d.ann_len,
                symbol,
            })?;
        }
        Ok(())
    }

    fn fresh(&mut self, n: u32) -> Vec<(u32, Vec<u8>)> {
        let s = self.d.payload.repair(self.next_repair, n);
        self.next_repair += n;
        s
    }

    fn pass(&mut self, symbols: Vec<(u32, Vec<u8>)>) -> io::Result<()> {
        self.announce()?;
        for (i, (esi, data)) in symbols.into_iter().enumerate() {
            if i > 0 && i % ANNOUNCE_EVERY == 0 {
                self.announce()?;
            }
            self.send(Frame::Symbol {
                id: self.d.id,
                esi,
                data,
            })?;
        }
        if self.closing_announce {
            self.announce()?;
        }
        Ok(())
    }

    fn status(&mut self, round: u8, plan: &Plan, states: &mut [State]) -> io::Result<()> {
        let polled: Vec<bool> = states.iter().map(|s| *s != State::Decoded).collect();
        let n = polled.iter().filter(|p| **p).count();
        self.send(Frame::Status {
            id: self.d.id,
            round,
            lead_ms: plan.lead_ms,
            slot_ms: plan.slot_ms,
            polled: polled.clone(),
        })?;
        let slot = plan.slot_ms as u64;
        let start = self.link.tx_done_ms()? + plan.lead_ms as u64;
        // the airtime model and the TNC's carrier sense can both put us behind; when a
        // reply comes late, assume the rest will be just as late and wait for them
        let mut late = 0;
        let mut heard = vec![false; states.len()];
        while heard.iter().filter(|h| **h).count() < n {
            let deadline = start + n as u64 * slot + plan.guard_ms + late;
            let Some(bytes) = self.link.recv_until(deadline)? else {
                break;
            };
            let Some(Frame::Report {
                id,
                round: r,
                index,
                need,
            }) = Frame::parse(&bytes)
            else {
                continue;
            };
            let i = index as usize;
            // a late answer to an earlier round still counts if it says decoded
            if id != self.d.id || polled.get(i) != Some(&true) || (r != round && need.is_some()) {
                continue;
            }
            if r == round {
                let pos = polled[..=i].iter().filter(|p| **p).count() as u64;
                let behind = self.link.now_ms().saturating_sub(start + pos * slot);
                late = late.max(behind).min(n as u64 * slot);
            }
            heard[i] = true;
            states[i] = need.map_or(State::Decoded, |need| State::Receiving {
                need,
                answered: true,
            });
        }
        if heard.iter().filter(|h| **h).count() == n {
            // everyone answered early; the last one's TNC may still be sending after its
            // report (an ARDOP ID frame), so don't key up on top of it
            let until = self.link.now_ms() + plan.lead_ms as u64;
            idle(self.link, until)?;
        }
        for (s, _) in states.iter_mut().zip(&heard).filter(|(_, h)| !**h) {
            // a station heard from before stays receiving: its report may be all
            // that got lost, so it isn't written off as silent
            *s = match *s {
                State::Receiving { need, .. } => State::Receiving {
                    need,
                    answered: false,
                },
                State::Decoded => State::Decoded,
                State::Pending | State::Silent => State::Silent,
            };
        }
        Ok(())
    }
}

/// How many symbols each station may still be short, for sizing repair. A
/// station that answered is short what it said. One whose report went missing
/// keeps its last figure, less what it probably heard of the repair since, at
/// the rate it has been hearing the delivery so far. Silent and decoded
/// stations need nothing sent for them.
struct Shortfall {
    k: u32,
    sent: u32,
    short: Vec<u32>,
    rate: Vec<f64>,
    missed: Vec<u8>,
}

impl Shortfall {
    fn new(k: u32, sent: u32, n: usize) -> Shortfall {
        Shortfall {
            k,
            sent,
            short: vec![0; n],
            rate: vec![0.0; n],
            missed: vec![0; n],
        }
    }

    fn learn(&mut self, states: &[State]) {
        for (i, st) in states.iter().enumerate() {
            match *st {
                State::Receiving {
                    need,
                    answered: true,
                } => {
                    // a report says at least 1 until it says decoded, and never more than
                    // K: one forged report mustn't put hours of repair on the air
                    let need = (need as u32).clamp(1, self.k);
                    self.short[i] = need;
                    self.rate[i] = (self.k - need) as f64 / self.sent as f64;
                    self.missed[i] = 0;
                }
                State::Receiving {
                    answered: false, ..
                } => self.missed[i] = self.missed[i].saturating_add(1),
                State::Pending | State::Decoded | State::Silent => self.short[i] = 0,
            }
        }
    }

    /// max(need) + 2 over the stations known to be short; nothing when there are none.
    fn want(&self) -> u32 {
        self.short
            .iter()
            .zip(&self.missed)
            .filter(|(s, m)| **s > 0 && **m < MAX_MISSED)
            .map(|(s, _)| *s)
            .max()
            .map_or(0, |n| n + 2)
    }

    fn repaired(&mut self, symbols: u32) {
        self.sent += symbols;
        for (s, r) in self.short.iter_mut().zip(&self.rate) {
            *s = s.saturating_sub((symbols as f64 * r).floor() as u32);
        }
    }
}

/// Runs one delivery to the end and returns each recipient's final state.
/// Without acks every state stays `Pending`, since nobody tells us.
pub fn run(
    link: &mut (impl Link + ?Sized),
    d: &Delivery,
    plan: &Plan,
    mut progress: impl FnMut(Progress),
) -> io::Result<Vec<State>> {
    let mut s = Session {
        link,
        d,
        next_repair: 0,
        announces: 0,
        closing_announce: !plan.acks,
    };
    let k = d.k();
    let extra = (k as f64 * plan.margin).ceil() as u32;
    let mut first = d.payload.source();
    first.extend(s.fresh(extra));
    s.pass(first)?;
    progress(Progress::Pass { symbols: k + extra });

    let mut states = vec![State::Pending; d.announce.recipients.len()];
    if plan.acks {
        let mut short = Shortfall::new(k, k + extra, states.len());
        s.status(1, plan, &mut states)?;
        short.learn(&states);
        progress(Progress::Status {
            round: 1,
            states: &states,
        });
        for round in 2..=plan.rounds.min(254) as u8 + 1 {
            if states.iter().all(|s| *s == State::Decoded) {
                break;
            }
            let want = short.want();
            let repair = s.fresh(want);
            s.pass(repair)?;
            progress(Progress::Pass { symbols: want });
            short.repaired(want);
            s.status(round, plan, &mut states)?;
            short.learn(&states);
            progress(Progress::Status {
                round,
                states: &states,
            });
        }
    } else {
        for _ in 0..plan.repeats {
            let until = s.link.tx_done_ms()? + plan.repeat_gap_ms;
            idle(s.link, until)?;
            let again = s.fresh(k + extra);
            s.pass(again)?;
            progress(Progress::Pass { symbols: k + extra });
        }
    }
    let done = s.link.tx_done_ms()?;
    idle(s.link, done)?;
    s.link.sign_off()?;
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard(need: u16) -> State {
        State::Receiving {
            need,
            answered: true,
        }
    }

    #[test]
    fn a_lost_report_leaves_what_the_station_probably_missed() {
        // K = 100, 110 sent; one station heard 55 of them (half), one heard 99
        let mut s = Shortfall::new(100, 110, 3);
        s.learn(&[heard(45), heard(1), State::Silent]);
        assert_eq!(s.want(), 47);
        s.repaired(47);
        // neither answers this time
        let unheard = |need| State::Receiving {
            need,
            answered: false,
        };
        s.learn(&[unheard(45), unheard(1), State::Silent]);
        // the half-deaf one probably got about 23 of the 47, the other all of them
        assert_eq!(s.short, vec![22, 0, 0]);
        assert_eq!(s.want(), 24);
        s.learn(&[State::Decoded, State::Decoded, State::Silent]);
        assert_eq!(s.want(), 0);
    }

    #[test]
    fn a_station_unheard_for_two_rounds_is_not_repaired_blind() {
        let mut s = Shortfall::new(100, 110, 1);
        s.learn(&[heard(100)]);
        let unheard = State::Receiving {
            need: 100,
            answered: false,
        };
        s.repaired(102);
        s.learn(&[unheard]);
        assert_eq!(s.want(), 102, "one lost report is still allowed for");
        s.repaired(102);
        s.learn(&[unheard]);
        assert_eq!(s.want(), 0, "two in a row stops the guessing");
        s.learn(&[heard(7)]);
        assert_eq!(s.want(), 9, "an answer brings it back");
    }

    #[test]
    fn a_report_can_ask_for_at_most_the_whole_message() {
        let mut s = Shortfall::new(6, 7, 2);
        // anyone on frequency can send a report; this one claims 65535 missing
        s.learn(&[heard(u16::MAX), heard(0)]);
        assert_eq!(s.want(), 6 + 2);
        s.learn(&[State::Decoded, heard(0)]);
        assert_eq!(s.want(), 1 + 2, "need 0 isn't decoded");
    }
}
