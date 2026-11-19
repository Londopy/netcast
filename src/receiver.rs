use std::collections::HashMap;

use crate::codec::{self, Assembly};
use crate::frame::{Announce, CODEC_BROTLI_RAPTORQ, Frame, Mode};
use crate::message;

// how long to hold symbols for a delivery whose announcement hasn't decoded yet
const UNANNOUNCED_TTL_MS: u64 = 30 * 60_000;
const MAX_TTL_MS: u64 = 24 * 3_600_000;
const MAX_PENDING: usize = 4096;
const MAX_DELIVERIES: usize = 64;
// longer slots than this can only be a mistake or mischief
const MAX_SLOT_MS: u16 = 10_000;

/// A recipient-list index this station acknowledges for, and the callsign it stands for.
pub type Ack = (u8, String);

#[derive(Debug)]
pub enum Output {
    Announced {
        id: u32,
        mode: Mode,
        mid: String,
        k: u32,
    },
    Report {
        at_ms: u64,
        frame: Frame,
    },
    Decoded(Decoded),
    Failed {
        id: u32,
        why: String,
    },
}

#[derive(Debug)]
pub struct Decoded {
    pub id: u32,
    pub mid: String,
    pub mode: Mode,
    /// Local callsigns whose inbox gets the message.
    pub calls: Vec<String>,
    pub b2f: Vec<u8>,
    /// Our (index, callsign) entries in the recipient list, kept so a restarted
    /// receiver can still answer status requests.
    pub acks: Vec<Ack>,
    pub expires_ms: u64,
}

enum State {
    Waiting {
        announce: Option<(u16, usize, Assembly)>,
        pending: Vec<(u32, Vec<u8>)>,
    },
    Receiving {
        announce: Announce,
        calls: Vec<String>,
        acks: Vec<Ack>,
        payload: Assembly,
    },
    Done {
        acks: Vec<Ack>,
    },
    Ignored,
}

struct Delivery {
    state: State,
    /// Pushed back by `ttl_ms` every time the delivery is heard from.
    expires_ms: u64,
    ttl_ms: u64,
    answered: Option<u8>,
}

impl Delivery {
    fn new(state: State, now_ms: u64, ttl_ms: u64) -> Delivery {
        Delivery {
            state,
            expires_ms: now_ms + ttl_ms,
            ttl_ms,
            answered: None,
        }
    }
}

pub struct Receiver {
    calls: Vec<String>,
    groups: Vec<String>,
    deliveries: HashMap<u32, Delivery>,
}

impl Receiver {
    pub fn new(calls: Vec<String>, groups: Vec<String>) -> Receiver {
        Receiver {
            calls,
            groups,
            deliveries: HashMap::new(),
        }
    }

    /// Restores a delivery decoded before a restart.
    pub fn remember(&mut self, id: u32, acks: Vec<Ack>, expires_ms: u64) {
        let d = Delivery::new(State::Done { acks }, expires_ms, 0);
        self.deliveries.insert(id, d);
    }

    /// Drops a delivery entirely, so it is received again from scratch.
    pub fn forget(&mut self, id: u32) {
        self.deliveries.remove(&id);
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.deliveries.retain(|_, d| d.expires_ms > now_ms);
    }

    pub fn handle(&mut self, frame: Frame, now_ms: u64) -> Vec<Output> {
        let id = frame.id();
        let known = self.deliveries.contains_key(&id);
        match frame {
            Frame::Announce { .. } | Frame::Symbol { .. } if !known => {
                if !self.make_room() {
                    return vec![];
                }
                let state = State::Waiting {
                    announce: None,
                    pending: vec![],
                };
                let d = Delivery::new(state, now_ms, UNANNOUNCED_TTL_MS);
                self.deliveries.insert(id, d);
            }
            Frame::Report { .. } => return vec![],
            _ if !known => return vec![],
            _ => {}
        }
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        d.expires_ms = d.expires_ms.max(now_ms + d.ttl_ms);
        match frame {
            Frame::Announce {
                esi, len, symbol, ..
            } => self.on_announce(id, esi, len, symbol),
            Frame::Symbol { esi, data, .. } => self.on_symbol(id, esi, data),
            Frame::Status {
                round,
                slot_ms,
                polled,
                ..
            } => self.on_status(id, round, slot_ms, &polled, now_ms),
            Frame::Report { .. } => vec![],
        }
    }

    /// Makes space for one more delivery, giving up deliveries that aren't ours first,
    /// then the oldest one still waiting for its announcement.
    fn make_room(&mut self) -> bool {
        if self.deliveries.len() < MAX_DELIVERIES {
            return true;
        }
        let victim = self
            .deliveries
            .iter()
            .find(|(_, d)| matches!(d.state, State::Ignored))
            .map(|(id, _)| *id)
            .or_else(|| {
                self.deliveries
                    .iter()
                    .filter(|(_, d)| matches!(d.state, State::Waiting { .. }))
                    .min_by_key(|(_, d)| d.expires_ms)
                    .map(|(id, _)| *id)
            });
        victim.map(|id| self.deliveries.remove(&id)).is_some()
    }

    fn on_announce(&mut self, id: u32, esi: u32, len: u16, symbol: Vec<u8>) -> Vec<Output> {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        let State::Waiting { announce, pending } = &mut d.state else {
            return vec![];
        };
        if announce.is_none() {
            let Some(asm) = codec::oti(len as usize, symbol.len()).and_then(Assembly::new) else {
                return vec![];
            };
            *announce = Some((len, symbol.len(), asm));
        }
        let (want_len, want_t, asm) = announce.as_mut().expect("set above");
        if *want_len != len || *want_t != symbol.len() || !asm.add(esi, symbol) {
            return vec![];
        }
        let Some(a) = Announce::parse(asm.result().expect("just decoded")) else {
            // a bad symbol got in; start the announcement over and keep the payload symbols
            *announce = None;
            return vec![];
        };
        let pending = std::mem::take(pending);
        d.state = State::Ignored;
        if a.codec != CODEC_BROTLI_RAPTORQ {
            return vec![];
        }
        d.ttl_ms = (a.ttl_min as u64 * 60_000).min(MAX_TTL_MS);
        let Some((calls, acks)) = self.addressed_to_us(&a) else {
            return vec![];
        };
        let Some(payload) = Assembly::new(a.oti) else {
            return vec![];
        };
        let mut out = vec![Output::Announced {
            id,
            mode: a.mode,
            mid: a.mid.clone(),
            k: payload.k(),
        }];
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        d.state = State::Receiving {
            announce: a,
            calls,
            acks,
            payload,
        };
        for (esi, data) in pending {
            out.extend(self.on_symbol(id, esi, data));
        }
        out
    }

    /// Which local calls the message is for, and which list entries we acknowledge for.
    /// Groups get the message into every local inbox but never acknowledge.
    fn addressed_to_us(&self, a: &Announce) -> Option<(Vec<String>, Vec<Ack>)> {
        let mut calls = vec![];
        let mut acks = vec![];
        for (i, r) in a.recipients.iter().enumerate() {
            if let Some(c) = self.calls.iter().find(|c| c.eq_ignore_ascii_case(r)) {
                acks.push((i as u8, c.clone()));
                calls.push(c.clone());
            } else if self.groups.iter().any(|g| g.eq_ignore_ascii_case(r)) {
                calls.extend(self.calls.iter().cloned());
            }
        }
        calls.sort();
        calls.dedup();
        (!calls.is_empty()).then_some((calls, acks))
    }

    fn on_symbol(&mut self, id: u32, esi: u32, data: Vec<u8>) -> Vec<Output> {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        match &mut d.state {
            State::Waiting { pending, .. } => {
                if pending.len() < MAX_PENDING {
                    pending.push((esi, data));
                }
                vec![]
            }
            State::Receiving { payload, .. } => {
                if payload.add(esi, data) {
                    vec![self.finish(id)]
                } else {
                    vec![]
                }
            }
            State::Done { .. } | State::Ignored => vec![],
        }
    }

    fn finish(&mut self, id: u32) -> Output {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        let State::Receiving {
            announce,
            calls,
            acks,
            payload,
        } = &mut d.state
        else {
            unreachable!("finish is only called while receiving")
        };
        let checked = codec::decompress(payload.result().expect("decoded"))
            .map_err(|e| format!("decoded but did not decompress: {e}"))
            .and_then(|b2f| match message::check(&b2f) {
                Ok(mid) if mid == announce.mid => Ok(b2f),
                Ok(mid) => Err(format!(
                    "message MID {mid} does not match announced {}",
                    announce.mid
                )),
                Err(why) => Err(format!("not a message Pat can read: {why}")),
            });
        let b2f = match checked {
            Ok(b) => b,
            Err(why) => {
                // a bad symbol spoils the whole decode, so start over on fresh symbols
                *payload = Assembly::new(announce.oti).expect("worked when announced");
                return Output::Failed { id, why };
            }
        };
        let out = Decoded {
            id,
            mid: announce.mid.clone(),
            mode: announce.mode,
            calls: std::mem::take(calls),
            b2f,
            acks: acks.clone(),
            expires_ms: d.expires_ms,
        };
        d.state = State::Done {
            acks: std::mem::take(acks),
        };
        Output::Decoded(out)
    }

    fn on_status(
        &mut self,
        id: u32,
        round: u8,
        slot_ms: u16,
        polled: &[bool],
        now_ms: u64,
    ) -> Vec<Output> {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        if d.answered == Some(round) || slot_ms > MAX_SLOT_MS {
            return vec![];
        }
        let (acks, need) = match &d.state {
            State::Receiving { acks, payload, .. } => {
                (acks, Some(payload.need().min(u16::MAX as u32) as u16))
            }
            State::Done { acks } => (acks, None),
            _ => return vec![],
        };
        let out = acks
            .iter()
            .filter(|(i, _)| polled.get(*i as usize) == Some(&true))
            .map(|(i, _)| {
                let slot = polled[..*i as usize].iter().filter(|p| **p).count() as u64;
                Output::Report {
                    at_ms: now_ms + slot * slot_ms as u64,
                    frame: Frame::Report {
                        id,
                        round,
                        index: *i,
                        need,
                    },
                }
            })
            .collect();
        d.answered = Some(round);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Link;
    use crate::sender::{self, Delivery, Plan};

    /// Records what the sender transmits; nobody ever answers.
    struct Tape(Vec<Frame>);

    impl Link for Tape {
        fn send(&mut self, frame: &[u8]) -> std::io::Result<()> {
            self.0.push(Frame::parse(frame).unwrap());
            Ok(())
        }
        fn recv_until(&mut self, _: u64) -> std::io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn now_ms(&self) -> u64 {
            0
        }
        fn tx_done_ms(&self) -> u64 {
            0
        }
    }

    fn frames(to: &[&str], mode: Mode, margin: f64) -> (Vec<Frame>, Vec<u8>) {
        let to: Vec<String> = to.iter().map(|s| s.to_string()).collect();
        let (_, msg) = message::compose("N0NCS", &to, "test", "hello net", 0);
        let d = Delivery::new(77, &msg, to, mode, 60, 256).unwrap();
        let plan = Plan {
            margin,
            rounds: 0,
            acks: false,
            repeats: 0,
            repeat_gap_ms: 0,
            slot_ms: 500,
            guard_ms: 0,
        };
        let mut tape = Tape(vec![]);
        sender::run(&mut tape, &d, &plan, |_| {}).unwrap();
        (tape.0, msg)
    }

    fn feed(rx: &mut Receiver, frames: &[Frame]) -> Vec<Output> {
        frames
            .iter()
            .flat_map(|f| rx.handle(f.clone(), 0))
            .collect()
    }

    fn status(round: u8, polled: Vec<bool>) -> Frame {
        Frame::Status {
            id: 77,
            round,
            slot_ms: 500,
            polled,
        }
    }

    #[test]
    fn ignores_deliveries_for_other_stations() {
        let (f, _) = frames(&["K1ABC"], Mode::Addressed, 0.0);
        let mut rx = Receiver::new(vec!["W2XYZ".into()], vec![]);
        assert!(feed(&mut rx, &f).is_empty());
        assert!(rx.handle(status(1, vec![true]), 0).is_empty());
    }

    #[test]
    fn group_delivery_reaches_every_local_inbox_without_acking() {
        let (f, msg) = frames(&["NET1"], Mode::Bulletin, 0.0);
        let mut rx = Receiver::new(vec!["K1ABC".into(), "K1ABC-7".into()], vec!["NET1".into()]);
        let out = feed(&mut rx, &f);
        let Some(Output::Decoded(d)) = out.into_iter().find(|o| matches!(o, Output::Decoded(_)))
        else {
            panic!("bulletin did not decode");
        };
        assert_eq!(d.b2f, msg);
        assert_eq!(d.calls, vec!["K1ABC".to_string(), "K1ABC-7".to_string()]);
        assert!(d.acks.is_empty());
    }

    #[test]
    fn reports_land_in_list_order_among_polled_stations() {
        let (f, _) = frames(&["K1ABC", "W2XYZ", "N3OOO"], Mode::Addressed, 0.0);
        let mut rx = Receiver::new(vec!["N3OOO".into()], vec![]);
        // only the announcement arrives, so this station still needs every symbol
        feed(&mut rx, &f[..1]);
        let out = rx.handle(status(1, vec![true, false, true]), 1000);
        let [
            Output::Report {
                at_ms,
                frame: Frame::Report {
                    index, need, round, ..
                },
            },
        ] = &out[..]
        else {
            panic!("expected one report, got {out:?}");
        };
        assert_eq!((*at_ms, *index, *round), (1500, 2, 1));
        assert!(need.unwrap() >= 1);
        // the same request heard twice gets one answer
        assert!(
            rx.handle(status(1, vec![true, false, true]), 1200)
                .is_empty()
        );
        assert_eq!(rx.handle(status(2, vec![true, false, true]), 9000).len(), 1);
    }

    #[test]
    fn bad_symbol_costs_a_retry_not_the_delivery() {
        let (f, msg) = frames(&["K1ABC"], Mode::Addressed, 2.0);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        let mut symbols = f.iter().filter(|f| matches!(f, Frame::Symbol { .. }));
        let Some(Frame::Symbol { id, esi, data }) = symbols.next().cloned() else {
            panic!("no source symbol");
        };
        let forged = Frame::Symbol {
            id,
            esi,
            data: vec![0x55; data.len()],
        };
        let out = feed(&mut rx, &[f[0].clone(), forged]);
        assert!(matches!(out.last(), Some(Output::Failed { .. })), "{out:?}");
        let repair: Vec<Frame> = symbols.cloned().collect();
        let out = feed(&mut rx, &repair);
        let Some(Output::Decoded(d)) = out.into_iter().find(|o| matches!(o, Output::Decoded(_)))
        else {
            panic!("did not recover after the bad symbol");
        };
        assert_eq!(d.b2f, msg);
    }

    #[test]
    fn restarted_receiver_still_answers_decoded() {
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        rx.remember(77, vec![(0, "K1ABC".into())], 10_000);
        let out = rx.handle(status(2, vec![true]), 0);
        assert!(matches!(
            out[..],
            [Output::Report {
                frame: Frame::Report { need: None, .. },
                ..
            }]
        ));
        rx.expire(20_000);
        assert!(rx.handle(status(3, vec![true]), 20_000).is_empty());
    }

    #[test]
    fn a_long_delivery_outlives_its_ttl_while_it_is_still_on_the_air() {
        let (f, _) = frames(&["K1ABC"], Mode::Addressed, 0.0);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        rx.handle(f[0].clone(), 0);
        // the TTL is 60 minutes; hearing the sender again keeps it alive
        rx.handle(status(1, vec![true]), 50 * 60_000);
        rx.expire(100 * 60_000);
        assert_eq!(rx.handle(status(2, vec![true]), 100 * 60_000).len(), 1);
        rx.expire(200 * 60_000);
        assert!(rx.handle(status(3, vec![true]), 200 * 60_000).is_empty());
    }
}
