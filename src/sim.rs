use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::io;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use crate::frame::{Frame, HEADER_LEN, Mode, Precedence};
use crate::link::{Channel, Link};
use crate::message;
use crate::receiver::{Output, Receiver};
use crate::sender::{self, Delivery, Envelope, Plan};

// SABM/UA, the B2F proposal and its answer, DISC/UA: each one its own keyed-up frame
const SESSION_SETUP_FRAMES: u32 = 6;
const SESSION_WINDOW: usize = 7;
const SMALL_FRAME: usize = 20;
const FLAMP_MAX_ROUNDS: u32 = 50;

pub struct Config {
    pub losses: Vec<f64>,
    pub body_bytes: usize,
    pub baud: u32,
    pub txdelay_ms: u64,
    pub frame_max: usize,
    pub margin: f64,
    pub rounds: u32,
    pub guard_ms: u64,
    pub trials: u32,
    pub seed: u64,
}

#[derive(Default)]
pub struct Score {
    /// Mean time per trial from first key-up until the channel is free again,
    /// with every station's transmissions and the waits between them.
    pub channel_ms: f64,
    /// Mean number of stations that ended up with the whole message.
    pub complete: f64,
}

pub struct Outcome {
    pub compressed: usize,
    pub k: u32,
    pub netcast: Score,
    pub flamp: Score,
    pub sessions: Score,
}

struct Station {
    rx: Receiver,
    loss: f64,
    rng: StdRng,
    deaf_until_ms: u64,
    message: Option<Vec<u8>>,
}

/// A simulated channel: the sender's frames reach each station with that station's
/// loss rate, and reports come back the same way.
struct SimLink {
    now: u64,
    channel: Channel,
    stations: Vec<Station>,
    inbox: BinaryHeap<Reverse<(u64, u64, Vec<u8>)>>,
    seq: u64,
    symbol_esis: Vec<u32>,
    /// Extra delay on every report, beyond what the airtime model expects.
    report_delay_ms: u64,
}

impl SimLink {
    fn new(stations: Vec<Station>, baud: u32, txdelay_ms: u64) -> SimLink {
        SimLink {
            now: 0,
            channel: Channel::new(baud, txdelay_ms),
            stations,
            inbox: BinaryHeap::new(),
            seq: 0,
            symbol_esis: vec![],
            report_delay_ms: 0,
        }
    }
}

impl Link for SimLink {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let end = self.channel.transmit(self.now, bytes.len());
        let frame = Frame::parse(bytes).expect("sender frames parse");
        if let Frame::Symbol { esi, .. } = frame {
            self.symbol_esis.push(esi);
        }
        for st in &mut self.stations {
            if end < st.deaf_until_ms || st.rng.random_bool(st.loss) {
                continue;
            }
            for out in st.rx.handle(frame.clone(), end) {
                match out {
                    Output::Report { at_ms, frame } => {
                        let b = frame.encode();
                        let ms = self.channel.burst_ms(b.len());
                        if !st.rng.random_bool(st.loss) {
                            self.seq += 1;
                            self.inbox.push(Reverse((
                                at_ms + ms + self.report_delay_ms,
                                self.seq,
                                b,
                            )));
                        }
                    }
                    Output::Decoded(d) => st.message = Some(d.b2f),
                    Output::Announced { .. } | Output::Failed { .. } => {}
                }
            }
        }
        Ok(())
    }

    fn recv_until(&mut self, deadline_ms: u64) -> io::Result<Option<Vec<u8>>> {
        if let Some(Reverse((t, _, _))) = self.inbox.peek()
            && *t <= deadline_ms
        {
            let Reverse((t, _, b)) = self.inbox.pop().expect("peeked");
            self.now = self.now.max(t);
            return Ok(Some(b));
        }
        self.now = self.now.max(deadline_ms);
        Ok(None)
    }

    fn now_ms(&self) -> u64 {
        self.now
    }

    fn tx_done_ms(&mut self) -> io::Result<u64> {
        Ok(self.channel.busy_until())
    }
}

fn station(i: usize, loss: f64, seed: u64) -> Station {
    Station {
        rx: Receiver::new(vec![call(i)], vec![]),
        loss,
        rng: StdRng::seed_from_u64(seed.wrapping_mul(1000).wrapping_add(i as u64)),
        deaf_until_ms: 0,
        message: None,
    }
}

fn call(i: usize) -> String {
    format!("N{i}SIM")
}

fn test_message(body_bytes: usize, n: usize, seed: u64) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let body: String = (0..body_bytes)
        .map(|i| {
            if i % 60 == 59 {
                '\n'
            } else {
                (b'A' + rng.random_range(0..26)) as char
            }
        })
        .collect();
    let to: Vec<String> = (0..n).map(call).collect();
    message::compose("N0NCS", &to, "Simulated bulletin", &body, 0).1
}

fn envelope(n: usize) -> Envelope {
    Envelope {
        from: "N0NCS".into(),
        recipients: (0..n).map(call).collect(),
        mode: Mode::Addressed,
        precedence: Precedence::Routine,
        ttl_min: 60,
    }
}

fn plan(cfg: &Config, ch: &Channel) -> Plan {
    Plan {
        margin: cfg.margin,
        rounds: cfg.rounds,
        acks: true,
        repeats: 0,
        repeat_gap_ms: 0,
        slot_ms: sender::slot_ms(ch, cfg.guard_ms),
        lead_ms: sender::kiss_lead_ms(ch, cfg.guard_ms),
        guard_ms: cfg.guard_ms,
    }
}

pub fn simulate(cfg: &Config) -> Outcome {
    let n = cfg.losses.len();
    let mut out = Outcome {
        compressed: 0,
        k: 0,
        netcast: Score::default(),
        flamp: Score::default(),
        sessions: Score::default(),
    };
    for trial in 0..cfg.trials {
        let seed = cfg.seed + trial as u64;
        let msg = test_message(cfg.body_bytes, n, seed);
        let d = Delivery::new(seed as u32 | 1, &msg, envelope(n), cfg.frame_max, None)
            .expect("simulated message fits");
        out.compressed = d.announce.oti.transfer_length() as usize;
        out.k = d.k();

        let stations = cfg
            .losses
            .iter()
            .enumerate()
            .map(|(i, l)| station(i, *l, seed))
            .collect();
        let mut link = SimLink::new(stations, cfg.baud, cfg.txdelay_ms);
        let p = plan(cfg, &link.channel);
        sender::run(&mut link, &d, &p, |_| {}).expect("simulated link can't fail");
        let complete = link
            .stations
            .iter()
            .filter(|s| s.message.as_deref() == Some(&msg[..]))
            .count();
        out.netcast.channel_ms += link.now_ms() as f64;
        out.netcast.complete += complete as f64;

        let mut rng = StdRng::seed_from_u64(seed ^ 0x5eed);
        let symbol_frame = HEADER_LEN + d.announce.oti.symbol_size() as usize;
        let ch = Channel::new(cfg.baud, cfg.txdelay_ms);
        let (ms, done) = flamp(
            d.k(),
            symbol_frame,
            &cfg.losses,
            &ch,
            cfg.guard_ms,
            &mut rng,
        );
        out.flamp.channel_ms += ms as f64;
        out.flamp.complete += done as f64;
        let (ms, done) = sessions(d.k(), symbol_frame, &cfg.losses, &ch, &mut rng);
        out.sessions.channel_ms += ms as f64;
        out.sessions.complete += done as f64;
    }
    for s in [&mut out.netcast, &mut out.flamp, &mut out.sessions] {
        s.channel_ms /= cfg.trials as f64;
        s.complete /= cfg.trials as f64;
    }
    out
}

/// Plain blocks with hole lists: after each pass every incomplete station reports its
/// missing block numbers in its own slot, and the sender resends the union of what it heard.
fn flamp(
    k: u32,
    frame: usize,
    losses: &[f64],
    ch: &Channel,
    guard_ms: u64,
    rng: &mut StdRng,
) -> (u64, usize) {
    let mut missing: Vec<BTreeSet<u32>> = losses.iter().map(|_| (0..k).collect()).collect();
    let mut to_send: BTreeSet<u32> = (0..k).collect();
    let mut airtime = 0;
    for _ in 0..FLAMP_MAX_ROUNDS {
        // a header block goes out with every pass, like netcast's announcement
        airtime += ch.burst_ms(frame) + ch.frame_ms(frame) * to_send.len() as u64;
        for (m, loss) in missing.iter_mut().zip(losses) {
            m.retain(|b| !to_send.contains(b) || rng.random_bool(*loss));
        }
        if missing.iter().all(|m| m.is_empty()) {
            break;
        }
        // the poll, then the same wait before the first reply slot as netcast's
        airtime += ch.burst_ms(SMALL_FRAME) + sender::kiss_lead_ms(ch, guard_ms) as u64;
        to_send.clear();
        for (m, loss) in missing.iter().zip(losses).filter(|(m, _)| !m.is_empty()) {
            // two bytes per missing block number, split over full frames
            let bytes = 16 + 2 * m.len();
            let full = (bytes / frame) as u64;
            let tail = bytes % frame;
            airtime += guard_ms + ch.txdelay_ms() + full * ch.frame_ms(frame);
            if tail > 0 {
                airtime += ch.frame_ms(tail);
            }
            if !rng.random_bool(*loss) {
                to_send.extend(m.iter().copied());
            }
        }
    }
    (airtime, missing.iter().filter(|m| m.is_empty()).count())
}

/// One connected session per station, AX.25 style: windows of 7 frames, each answered
/// by an ack listing what arrived. A lost ack means the whole window goes again.
/// Retry timers are left out, which flatters this baseline.
fn sessions(k: u32, frame: usize, losses: &[f64], ch: &Channel, rng: &mut StdRng) -> (u64, usize) {
    let mut airtime = 0;
    for loss in losses {
        airtime += SESSION_SETUP_FRAMES as u64 * ch.burst_ms(SMALL_FRAME);
        let mut queue: Vec<u32> = (0..k).collect();
        while !queue.is_empty() {
            let window: Vec<u32> = queue.iter().take(SESSION_WINDOW).copied().collect();
            airtime += ch.txdelay_ms() + ch.frame_ms(frame) * window.len() as u64;
            let got: Vec<u32> = window
                .iter()
                .copied()
                .filter(|_| !rng.random_bool(*loss))
                .collect();
            airtime += ch.burst_ms(SMALL_FRAME);
            if !rng.random_bool(*loss) {
                queue.retain(|b| !got.contains(b));
            }
        }
    }
    (airtime, losses.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sender::State;

    fn config(losses: Vec<f64>) -> Config {
        Config {
            losses,
            body_bytes: 4000,
            baud: 1200,
            txdelay_ms: 300,
            frame_max: 256,
            margin: 0.1,
            rounds: 10,
            guard_ms: 500,
            trials: 1,
            seed: 7,
        }
    }

    fn spread(n: usize, lo: f64, hi: f64) -> Vec<f64> {
        (0..n)
            .map(|i| lo + (hi - lo) * i as f64 / (n - 1) as f64)
            .collect()
    }

    fn run(stations: Vec<Station>, msg: &[u8], cfg: &Config) -> (SimLink, Vec<State>) {
        let d = Delivery::new(42, msg, envelope(stations.len()), cfg.frame_max, None).unwrap();
        let mut link = SimLink::new(stations, cfg.baud, cfg.txdelay_ms);
        let p = plan(cfg, &link.channel);
        let states = sender::run(&mut link, &d, &p, |_| {}).unwrap();
        (link, states)
    }

    #[test]
    fn replies_that_run_late_are_still_heard() {
        let cfg = config(vec![0.0; 6]);
        let msg = test_message(2000, 6, 6);
        let stations = (0..6).map(|i| station(i, 0.0, 6)).collect();
        let d = Delivery::new(42, &msg, envelope(6), cfg.frame_max, None).unwrap();
        let mut link = SimLink::new(stations, cfg.baud, cfg.txdelay_ms);
        // every reply a full second behind the model: more than the 500 ms guard
        link.report_delay_ms = 1500;
        let p = plan(&cfg, &link.channel);
        let mut rounds = 0;
        let states = sender::run(&mut link, &d, &p, |p| {
            if let sender::Progress::Status { .. } = p {
                rounds += 1;
            }
        })
        .unwrap();
        assert_eq!(states, vec![State::Decoded; 6]);
        assert_eq!(rounds, 1);
    }

    #[test]
    fn the_reply_window_counts_the_lead_time() {
        let cfg = config(vec![0.0]);
        let msg = test_message(1000, 1, 8);
        let d = Delivery::new(42, &msg, envelope(1), cfg.frame_max, None).unwrap();
        let mut link = SimLink::new(vec![station(0, 0.0, 8)], cfg.baud, cfg.txdelay_ms);
        // a lead much longer than slot and guard together, as over ARDOP
        let p = Plan {
            lead_ms: 3000,
            ..plan(&cfg, &link.channel)
        };
        let mut rounds = 0;
        let states = sender::run(&mut link, &d, &p, |p| {
            if let sender::Progress::Status { .. } = p {
                rounds += 1;
            }
        })
        .unwrap();
        assert_eq!((states, rounds), (vec![State::Decoded], 1));
    }

    #[test]
    fn every_station_decodes_at_up_to_forty_percent_loss() {
        for seed in 0..5 {
            let cfg = config(spread(10, 0.0, 0.4));
            let msg = test_message(cfg.body_bytes, 10, seed);
            let stations = cfg
                .losses
                .iter()
                .enumerate()
                .map(|(i, l)| station(i, *l, seed))
                .collect();
            let (link, states) = run(stations, &msg, &cfg);
            for (i, st) in link.stations.iter().enumerate() {
                assert_eq!(
                    st.message.as_deref(),
                    Some(&msg[..]),
                    "station {i}, seed {seed}"
                );
            }
            assert!(
                states.iter().all(|s| *s == State::Decoded),
                "seed {seed}: {states:?}"
            );
        }
    }

    #[test]
    fn symbols_are_never_sent_twice() {
        let cfg = config(spread(8, 0.1, 0.35));
        let msg = test_message(6000, 8, 1);
        let stations = cfg
            .losses
            .iter()
            .enumerate()
            .map(|(i, l)| station(i, *l, 1))
            .collect();
        let (link, _) = run(stations, &msg, &cfg);
        let unique: BTreeSet<_> = link.symbol_esis.iter().collect();
        assert!(
            link.symbol_esis.len() > 30,
            "expected repair rounds to have run"
        );
        assert_eq!(unique.len(), link.symbol_esis.len());
    }

    #[test]
    fn station_that_misses_the_first_announcement_still_decodes() {
        let cfg = config(vec![0.0, 0.0]);
        let msg = test_message(8000, 2, 3);
        let mut stations: Vec<Station> = (0..2).map(|i| station(i, 0.0, 3)).collect();
        // tunes in partway through the first pass
        stations[1].deaf_until_ms = 6_000;
        let (link, states) = run(stations, &msg, &cfg);
        assert_eq!(link.stations[1].message.as_deref(), Some(&msg[..]));
        assert_eq!(states, vec![State::Decoded, State::Decoded]);
    }

    #[test]
    fn long_recipient_list_spans_several_announce_frames() {
        let cfg = config(vec![0.2; 60]);
        let msg = test_message(3000, 60, 5);
        let stations = cfg
            .losses
            .iter()
            .enumerate()
            .map(|(i, l)| station(i, *l, 5))
            .collect();
        let d = Delivery::new(9, &msg, envelope(60), cfg.frame_max, None).unwrap();
        assert!(d.announce.to_bytes().len() > cfg.frame_max);
        let (link, states) = run(stations, &msg, &cfg);
        assert!(
            link.stations
                .iter()
                .all(|s| s.message.as_deref() == Some(&msg[..]))
        );
        assert!(states.iter().all(|s| *s == State::Decoded));
    }

    #[test]
    fn station_that_hears_nothing_ends_silent() {
        let cfg = config(vec![0.0, 1.0]);
        let msg = test_message(2000, 2, 4);
        let stations = cfg
            .losses
            .iter()
            .enumerate()
            .map(|(i, l)| station(i, *l, 4))
            .collect();
        let (_, states) = run(stations, &msg, &cfg);
        assert_eq!(states, vec![State::Decoded, State::Silent]);
    }

    #[test]
    fn repair_follows_the_worst_station_not_the_sum() {
        // a long message, where hole lists grow with every station's losses
        let mut cfg = config(spread(20, 0.05, 0.3));
        cfg.body_bytes = 20_000;
        cfg.trials = 5;
        let out = simulate(&cfg);
        assert_eq!(out.netcast.complete, 20.0);
        assert!(
            out.netcast.channel_ms * 1.3 < out.flamp.channel_ms,
            "netcast {} flamp {}",
            out.netcast.channel_ms,
            out.flamp.channel_ms
        );
        assert!(out.netcast.channel_ms * 5.0 < out.sessions.channel_ms);
    }
}
