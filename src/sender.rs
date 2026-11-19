use std::fmt;
use std::io;

use ed25519_dalek::SigningKey;

use crate::codec::{self, Object};
use crate::frame::{Announce, CODEC_BROTLI_RAPTORQ, Frame, HEADER_LEN, Mode, valid_name};
use crate::keys;
use crate::link::{Channel, Link, idle};

// a late joiner needs the announcement, so it goes out again this often
const ANNOUNCE_EVERY: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Need(u16),
    Decoded,
    Silent,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            State::Pending => write!(f, "pending"),
            State::Need(n) => write!(f, "need {n}"),
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

/// Who a delivery is from and for, and how receivers treat it.
pub struct Envelope {
    pub from: String,
    pub recipients: Vec<String>,
    pub mode: Mode,
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

struct Session<'a, L: Link> {
    link: &'a mut L,
    d: &'a Delivery,
    next_repair: u32,
    announces: u32,
    /// Without status rounds nothing re-announces later, so each pass ends with
    /// the announcement too: a station that missed the first one still has its
    /// symbols waiting and decodes the moment it hears this one.
    closing_announce: bool,
}

impl<L: Link> Session<'_, L> {
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
            slot_ms: plan.slot_ms,
            polled: polled.clone(),
        })?;
        let slot = plan.slot_ms as u64;
        let start = self.link.tx_done_ms();
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
            states[i] = need.map_or(State::Decoded, State::Need);
        }
        for (i, s) in states.iter_mut().enumerate() {
            if polled[i] && !heard[i] {
                *s = State::Silent;
            }
        }
        Ok(())
    }
}

/// Runs one delivery to the end and returns each recipient's final state.
/// Without acks every state stays `Pending`, since nobody tells us.
pub fn run(
    link: &mut impl Link,
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
        s.status(1, plan, &mut states)?;
        progress(Progress::Status {
            round: 1,
            states: &states,
        });
        for round in 2..=plan.rounds.min(254) as u8 + 1 {
            if states.iter().all(|s| *s == State::Decoded) {
                break;
            }
            let worst = states.iter().filter_map(|s| match s {
                State::Need(n) => Some(*n as u32),
                _ => None,
            });
            let want = worst.max().map_or(0, |n| n + 2);
            let repair = s.fresh(want);
            s.pass(repair)?;
            progress(Progress::Pass { symbols: want });
            s.status(round, plan, &mut states)?;
            progress(Progress::Status {
                round,
                states: &states,
            });
        }
    } else {
        for _ in 0..plan.repeats {
            let until = s.link.tx_done_ms() + plan.repeat_gap_ms;
            idle(s.link, until)?;
            let again = s.fresh(k + extra);
            s.pass(again)?;
            progress(Progress::Pass { symbols: k + extra });
        }
    }
    let done = s.link.tx_done_ms();
    idle(s.link, done)?;
    Ok(states)
}
