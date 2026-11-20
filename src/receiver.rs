use std::collections::{HashMap, HashSet};

use ed25519_dalek::VerifyingKey;

use crate::codec::{self, Assembly};
use crate::frame::{Announce, CODEC_BROTLI_RAPTORQ, Frame, Mode, Precedence};
use crate::{keys, message};

// how long to hold symbols for a delivery whose announcement hasn't decoded yet
const UNANNOUNCED_TTL_MS: u64 = 30 * 60_000;
const MAX_TTL_MS: u64 = 24 * 3_600_000;
const MAX_PENDING: usize = 4096;
const MAX_DELIVERIES: usize = 64;
// longer slots or leads than this can only be a mistake or mischief
const MAX_SLOT_MS: u16 = 10_000;

/// A recipient-list index this station acknowledges for, and the callsign it stands for.
pub type Ack = (u8, String);

type Accepted = (Announce, Trust, Vec<String>, Vec<Ack>);

/// What a station can say about who sent a delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    Unsigned,
    Verified,
    /// Signed, but the sender's key isn't in this station's roster.
    UnknownKey,
}

impl Trust {
    pub fn name(self) -> &'static str {
        match self {
            Trust::Unsigned => "unsigned",
            Trust::Verified => "verified",
            Trust::UnknownKey => "signed, key not in roster",
        }
    }
}

/// A delivery for us, heard about before any of it decodes.
#[derive(Debug)]
pub struct Announced {
    pub id: u32,
    pub mode: Mode,
    pub precedence: Precedence,
    pub from: String,
    pub mid: String,
    pub k: u32,
    pub trust: Trust,
}

#[derive(Debug)]
pub enum Output {
    Announced(Announced),
    Report { at_ms: u64, frame: Frame },
    Decoded(Decoded),
    Failed { id: u32, why: String },
}

#[derive(Debug)]
pub struct Decoded {
    pub id: u32,
    pub mid: String,
    pub mode: Mode,
    pub precedence: Precedence,
    pub from: String,
    pub trust: Trust,
    /// Local callsigns whose inbox gets the message.
    pub calls: Vec<String>,
    pub b2f: Vec<u8>,
    /// Our (index, callsign) entries in the recipient list, kept so a restarted
    /// receiver can still answer status requests.
    pub acks: Vec<Ack>,
    pub expires_ms: u64,
}

enum State {
    /// Collecting the announcement. One that isn't for us, or doesn't pass the
    /// signature check, is remembered by hash and the wait goes on: a forged
    /// announcement under the same id mustn't lock out the real one.
    Waiting {
        announce: Option<(u16, usize, Assembly)>,
        pending: Vec<(u32, Vec<u8>)>,
        rejected: Vec<[u8; 32]>,
    },
    Receiving {
        announce: Box<Announce>,
        trust: Trust,
        calls: Vec<String>,
        acks: Vec<Ack>,
        payload: Assembly,
    },
    Done {
        acks: Vec<Ack>,
    },
}

fn waiting() -> State {
    State::Waiting {
        announce: None,
        pending: vec![],
        rejected: vec![],
    }
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
    keys: HashMap<String, VerifyingKey>,
    require_signed: bool,
    deliveries: HashMap<u32, Delivery>,
    /// Deliveries decoded lately, so a recording played back later is ignored.
    seen: HashSet<u32>,
}

impl Receiver {
    pub fn new(calls: Vec<String>, groups: Vec<String>) -> Receiver {
        Receiver {
            calls,
            groups,
            keys: HashMap::new(),
            require_signed: false,
            deliveries: HashMap::new(),
            seen: HashSet::new(),
        }
    }

    /// Marks a delivery as already decoded, so it's never taken again.
    pub fn seen(&mut self, id: u32) {
        self.seen.insert(id);
    }

    /// Senders' public keys by callsign, and whether to refuse anything not
    /// verified against them.
    pub fn trusting(
        mut self,
        keys: HashMap<String, VerifyingKey>,
        require_signed: bool,
    ) -> Receiver {
        self.keys = keys;
        self.require_signed = require_signed;
        self
    }

    /// Symbols held and needed for a delivery still being received.
    pub fn progress(&self, id: u32) -> Option<(u32, u32)> {
        match &self.deliveries.get(&id)?.state {
            State::Receiving { payload, .. } => Some((payload.have(), payload.k())),
            _ => None,
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
        self.seen.remove(&id);
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.deliveries.retain(|_, d| d.expires_ms > now_ms);
    }

    pub fn handle(&mut self, frame: Frame, now_ms: u64) -> Vec<Output> {
        let id = frame.id();
        let known = self.deliveries.contains_key(&id);
        if !known && self.seen.contains(&id) {
            return vec![];
        }
        match frame {
            Frame::Announce { .. } | Frame::Symbol { .. } if !known => {
                if !self.make_room() {
                    return vec![];
                }
                let d = Delivery::new(waiting(), now_ms, UNANNOUNCED_TTL_MS);
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
                lead_ms,
                slot_ms,
                polled,
                ..
            } => self.on_status(id, round, (lead_ms, slot_ms), &polled, now_ms),
            Frame::Report { .. } => vec![],
        }
    }

    /// Makes space for one more delivery, giving up deliveries that aren't ours first,
    /// then the oldest one still waiting for its announcement.
    fn make_room(&mut self) -> bool {
        if self.deliveries.len() < MAX_DELIVERIES {
            return true;
        }
        let waiting = |d: &&Delivery, foreign: bool| matches!(&d.state, State::Waiting { rejected, .. } if !foreign || !rejected.is_empty());
        let victim = [true, false].into_iter().find_map(|foreign| {
            self.deliveries
                .iter()
                .filter(|(_, d)| waiting(d, foreign))
                .min_by_key(|(_, d)| d.expires_ms)
                .map(|(id, _)| *id)
        });
        victim.map(|id| self.deliveries.remove(&id)).is_some()
    }

    fn on_announce(&mut self, id: u32, esi: u32, len: u16, symbol: Vec<u8>) -> Vec<Output> {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        let State::Waiting {
            announce, rejected, ..
        } = &mut d.state
        else {
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
        let body = asm.result().expect("just decoded").to_vec();
        *announce = None;
        let hash = codec::hash(&body);
        if rejected.contains(&hash) {
            return vec![];
        }
        let verdict = match Announce::parse(&body) {
            Some(a) => self.accept(id, a),
            None => Err(None),
        };
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        let State::Waiting {
            pending, rejected, ..
        } = &mut d.state
        else {
            unreachable!("still waiting")
        };
        let (a, trust, calls, acks) = match verdict {
            Ok(v) => v,
            Err(why) => {
                // stop collecting symbols for something that isn't ours; if the real
                // announcement turns up, repair rounds make up the difference
                rejected.push(hash);
                pending.clear();
                return why
                    .map(|why| Output::Failed { id, why })
                    .into_iter()
                    .collect();
            }
        };
        let pending = std::mem::take(pending);
        d.ttl_ms = (a.ttl_min as u64 * 60_000).min(MAX_TTL_MS);
        let payload = Assembly::new(a.oti).expect("checked in accept");
        let mut out = vec![Output::Announced(Announced {
            id,
            mode: a.mode,
            precedence: a.precedence,
            from: a.from.clone(),
            mid: a.mid.clone(),
            k: payload.k(),
            trust,
        })];
        d.state = State::Receiving {
            announce: Box::new(a),
            trust,
            calls,
            acks,
            payload,
        };
        for (esi, data) in pending {
            out.extend(self.on_symbol(id, esi, data));
        }
        out
    }

    /// Whether to receive this delivery. A refusal worth telling the operator
    /// about carries a reason.
    fn accept(&self, id: u32, a: Announce) -> Result<Accepted, Option<String>> {
        if a.codec != CODEC_BROTLI_RAPTORQ || Assembly::new(a.oti).is_none() {
            return Err(None);
        }
        // our own transmissions can come back through a shared TNC or a digipeater
        if self.calls.contains(&a.from) {
            return Err(None);
        }
        let (calls, acks) = self.addressed_to_us(&a).ok_or(None)?;
        let trust = match (&a.signature, self.keys.get(&a.from)) {
            (None, _) => Trust::Unsigned,
            (Some(_), Some(key)) if keys::verify(&a, id, key) => Trust::Verified,
            (Some(_), Some(_)) => {
                return Err(Some(format!(
                    "{} claims to be from {}, but the signature doesn't match its roster key",
                    a.mid, a.from
                )));
            }
            (Some(_), None) => Trust::UnknownKey,
        };
        if self.require_signed && trust != Trust::Verified {
            return Err(Some(format!(
                "{} from {} is {}; this station only takes verified messages",
                a.mid,
                a.from,
                trust.name()
            )));
        }
        Ok((a, trust, calls, acks))
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
            State::Waiting {
                pending, rejected, ..
            } => {
                if rejected.is_empty() && pending.len() < MAX_PENDING {
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
            State::Done { .. } => vec![],
        }
    }

    fn finish(&mut self, id: u32) -> Output {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        let State::Receiving {
            announce,
            trust,
            calls,
            acks,
            payload,
        } = &mut d.state
        else {
            unreachable!("finish is only called while receiving")
        };
        let packed = payload.result().expect("decoded");
        let checked = if codec::hash(packed) != announce.hash {
            Err("decoded data doesn't match the announced hash".to_string())
        } else {
            codec::decompress(packed).map_err(|e| format!("decoded but did not decompress: {e}"))
        };
        let checked = checked.and_then(|b2f| match message::check(&b2f) {
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
            precedence: announce.precedence,
            from: announce.from.clone(),
            trust: *trust,
            calls: std::mem::take(calls),
            b2f,
            acks: acks.clone(),
            expires_ms: d.expires_ms,
        };
        d.state = State::Done {
            acks: std::mem::take(acks),
        };
        self.seen.insert(id);
        Output::Decoded(out)
    }

    fn on_status(
        &mut self,
        id: u32,
        round: u8,
        (lead_ms, slot_ms): (u16, u16),
        polled: &[bool],
        now_ms: u64,
    ) -> Vec<Output> {
        let d = self.deliveries.get_mut(&id).expect("delivery exists");
        if d.answered == Some(round) || slot_ms > MAX_SLOT_MS || lead_ms > MAX_SLOT_MS {
            return vec![];
        }
        let (acks, need) = match &d.state {
            State::Receiving { acks, payload, .. } => {
                (acks, Some(payload.need().min(u16::MAX as u32) as u16))
            }
            State::Done { acks } => (acks, None),
            _ => return vec![],
        };
        let out: Vec<Output> = acks
            .iter()
            .filter(|(i, _)| polled.get(*i as usize) == Some(&true))
            .map(|(i, _)| {
                let slot = polled[..*i as usize].iter().filter(|p| **p).count() as u64;
                Output::Report {
                    at_ms: now_ms + lead_ms as u64 + slot * slot_ms as u64,
                    frame: Frame::Report {
                        id,
                        round,
                        index: *i,
                        need,
                    },
                }
            })
            .collect();
        // a request that polled none of us mustn't use up the round
        if !out.is_empty() {
            d.answered = Some(round);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Link;
    use crate::sender::{self, Delivery, Envelope, Plan};
    use ed25519_dalek::SigningKey;

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
        fn tx_done_ms(&mut self) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    fn frames(to: &[&str], mode: Mode, margin: f64) -> (Vec<Frame>, Vec<u8>) {
        signed_frames(to, mode, margin, None)
    }

    fn signed_frames(
        to: &[&str],
        mode: Mode,
        margin: f64,
        key: Option<&ed25519_dalek::SigningKey>,
    ) -> (Vec<Frame>, Vec<u8>) {
        let to: Vec<String> = to.iter().map(|s| s.to_string()).collect();
        let (_, msg) = message::compose("N0NCS", &to, "test", "hello net", 0);
        let env = Envelope {
            from: "N0NCS".into(),
            recipients: to,
            mode,
            precedence: Precedence::default_for(mode),
            ttl_min: 60,
        };
        let d = Delivery::new(77, &msg, env, 256, key).unwrap();
        let plan = Plan {
            margin,
            rounds: 0,
            acks: false,
            repeats: 0,
            repeat_gap_ms: 0,
            slot_ms: 500,
            lead_ms: 0,
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
            lead_ms: 0,
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
    fn emergency_traffic_arrives_marked_emergency() {
        let (f, _) = frames(&["K1ABC"], Mode::Emergency, 0.0);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        let out = feed(&mut rx, &f);
        let Some(Output::Announced(a)) = out.iter().find(|o| matches!(o, Output::Announced(_)))
        else {
            panic!("no announcement");
        };
        assert_eq!(a.precedence, Precedence::Emergency);
        let Some(Output::Decoded(d)) = out.iter().find(|o| matches!(o, Output::Decoded(_))) else {
            panic!("did not decode");
        };
        assert_eq!(
            (d.mode, d.precedence),
            (Mode::Emergency, Precedence::Emergency)
        );
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
        assert!(
            matches!(out.last(), Some(Output::Failed { why, .. }) if why.contains("hash")),
            "{out:?}"
        );
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

    fn decoded(out: Vec<Output>) -> Option<Decoded> {
        out.into_iter().find_map(|o| match o {
            Output::Decoded(d) => Some(d),
            _ => None,
        })
    }

    fn roster_with(key: &SigningKey) -> HashMap<String, VerifyingKey> {
        HashMap::from([("N0NCS".to_string(), key.verifying_key())])
    }

    #[test]
    fn signatures_are_checked_against_the_roster() {
        let ncs = SigningKey::from_bytes(&[1; 32]);
        let impostor = SigningKey::from_bytes(&[2; 32]);
        let calls = || vec!["K1ABC".to_string()];

        let (f, msg) = signed_frames(&["K1ABC"], Mode::Addressed, 0.0, Some(&ncs));
        let mut rx = Receiver::new(calls(), vec![]).trusting(roster_with(&ncs), false);
        let d = decoded(feed(&mut rx, &f)).expect("verified message decodes");
        assert_eq!((d.trust, d.b2f), (Trust::Verified, msg));

        let mut rx = Receiver::new(calls(), vec![]).trusting(HashMap::new(), false);
        let d = decoded(feed(&mut rx, &f)).expect("unknown key still decodes");
        assert_eq!(d.trust, Trust::UnknownKey);

        let (forged, _) = signed_frames(&["K1ABC"], Mode::Addressed, 0.0, Some(&impostor));
        let mut rx = Receiver::new(calls(), vec![]).trusting(roster_with(&ncs), false);
        let out = feed(&mut rx, &forged);
        assert!(matches!(out[..], [Output::Failed { .. }]), "{out:?}");
        assert!(rx.handle(status(1, vec![true]), 0).is_empty());
    }

    #[test]
    fn require_signed_refuses_anything_unverified() {
        let ncs = SigningKey::from_bytes(&[1; 32]);
        let calls = || vec!["K1ABC".to_string()];
        let (unsigned, _) = frames(&["K1ABC"], Mode::Addressed, 0.0);
        let mut rx = Receiver::new(calls(), vec![]).trusting(roster_with(&ncs), true);
        assert!(decoded(feed(&mut rx, &unsigned)).is_none());

        let (signed, _) = signed_frames(&["K1ABC"], Mode::Addressed, 0.0, Some(&ncs));
        let mut rx = Receiver::new(calls(), vec![]).trusting(roster_with(&ncs), true);
        assert_eq!(
            decoded(feed(&mut rx, &signed)).unwrap().trust,
            Trust::Verified
        );
    }

    #[test]
    fn bulletin_survives_a_lost_first_announcement() {
        let (f, msg) = frames(&["NET1"], Mode::Bulletin, 0.5);
        assert!(matches!(f.last(), Some(Frame::Announce { .. })));
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec!["NET1".into()]);
        let d = decoded(feed(&mut rx, &f[1..])).expect("decodes from the closing announcement");
        assert_eq!(d.b2f, msg);
    }

    #[test]
    fn ignores_its_own_deliveries() {
        let (f, _) = frames(&["NET1"], Mode::Bulletin, 0.0);
        let mut rx = Receiver::new(vec!["N0NCS".into()], vec!["NET1".into()]);
        assert!(feed(&mut rx, &f).is_empty());
    }

    #[test]
    fn a_forged_announcement_first_does_not_lock_out_the_real_one() {
        let ncs = SigningKey::from_bytes(&[1; 32]);
        let (real, msg) = signed_frames(&["K1ABC"], Mode::Addressed, 0.0, Some(&ncs));
        // same delivery id, sent first: unsigned, and addressed elsewhere
        let (unsigned, _) = frames(&["K1ABC"], Mode::Addressed, 0.0);
        let (elsewhere, _) = frames(&["W9ZZZ"], Mode::Addressed, 0.0);
        for forged in [&unsigned[0], &elsewhere[0]] {
            let mut rx =
                Receiver::new(vec!["K1ABC".into()], vec![]).trusting(roster_with(&ncs), true);
            rx.handle(real[1].clone(), 0);
            rx.handle(forged.clone(), 0);
            let d = decoded(feed(&mut rx, &real)).expect("the real announcement still counts");
            assert_eq!((d.trust, d.b2f), (Trust::Verified, msg.clone()));
        }
    }

    #[test]
    fn refusals_are_reported_once() {
        let (f, _) = frames(&["K1ABC"], Mode::Addressed, 0.0);
        let ncs = SigningKey::from_bytes(&[1; 32]);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]).trusting(roster_with(&ncs), true);
        let out = feed(&mut rx, &[f[0].clone(), f[0].clone(), f[0].clone()]);
        assert_eq!(out.len(), 1, "{out:?}");
    }

    #[test]
    fn require_signed_refuses_unknown_keys_too() {
        let ncs = SigningKey::from_bytes(&[1; 32]);
        let stranger = SigningKey::from_bytes(&[3; 32]);
        let (f, _) = signed_frames(&["K1ABC"], Mode::Addressed, 0.0, Some(&stranger));
        let other = HashMap::from([("W2XYZ".to_string(), ncs.verifying_key())]);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]).trusting(other, true);
        assert!(decoded(feed(&mut rx, &f)).is_none());
    }

    #[test]
    fn a_replayed_delivery_is_ignored_after_it_expires() {
        let (f, _) = frames(&["K1ABC"], Mode::Addressed, 0.0);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        assert!(decoded(feed(&mut rx, &f)).is_some());
        rx.expire(u64::MAX);
        assert!(feed(&mut rx, &f).is_empty());
    }

    #[test]
    fn a_status_that_polls_nobody_here_does_not_use_up_the_round() {
        let (f, _) = frames(&["K1ABC", "W2XYZ"], Mode::Addressed, 0.0);
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        feed(&mut rx, &f[..1]);
        assert!(rx.handle(status(1, vec![false, true]), 0).is_empty());
        assert_eq!(rx.handle(status(1, vec![true, true]), 0).len(), 1);
    }

    #[test]
    fn ttl_is_capped_at_a_day() {
        let to = vec!["K1ABC".to_string()];
        let (_, msg) = message::compose("N0NCS", &to, "test", "hello net", 0);
        let env = Envelope {
            from: "N0NCS".into(),
            recipients: to,
            mode: Mode::Addressed,
            precedence: Precedence::Routine,
            ttl_min: u16::MAX,
        };
        let d = Delivery::new(77, &msg, env, 256, None).unwrap();
        let plan = Plan {
            margin: 0.0,
            rounds: 0,
            acks: false,
            repeats: 0,
            repeat_gap_ms: 0,
            slot_ms: 500,
            lead_ms: 0,
            guard_ms: 0,
        };
        let mut tape = Tape(vec![]);
        sender::run(&mut tape, &d, &plan, |_| {}).unwrap();
        let mut rx = Receiver::new(vec!["K1ABC".into()], vec![]);
        rx.handle(tape.0[0].clone(), 0);
        rx.expire(MAX_TTL_MS + 1);
        assert!(rx.handle(status(1, vec![true]), MAX_TTL_MS + 1).is_empty());
    }
}
