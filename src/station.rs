use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::RngExt;

use crate::ardop::{self, ArdopLink, FecMode};
use crate::ax25::Addr;
use crate::frame::{Frame, Mode, Precedence, VERSION};
use crate::kiss::KissLink;
use crate::link::{Channel, Link};
use crate::pat::Stored;
use crate::receiver::{Output, Receiver, Trust};
use crate::roster::Roster;
use crate::sender::{self, Delivery, Envelope, Plan, Progress, State};
use crate::ui::{self, Incoming, Shared};
use crate::{history, message, pat};

// a full AX.25 info field
pub const AX25_FRAME_MAX: usize = 256;
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(60);
const STALE_REPORT_MS: u64 = 2_000;
// how long a decoded delivery id is refused if it's heard again, so a recording
// played back later doesn't land as a new message
const REPLAY_WINDOW_S: u64 = 7 * 86400;

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How this station reaches its radio.
#[derive(Clone)]
pub enum Transport {
    /// Direwolf or another KISS TNC, sending AX.25 UI frames.
    Kiss {
        addr: String,
        baud: u32,
        txdelay_ms: u64,
    },
    /// ardopcf in FEC mode. `mode` is the frame type messages go out in.
    Ardop { addr: String, mode: FecMode },
}

#[derive(Clone)]
pub struct Radio {
    pub transport: Transport,
    pub guard_ms: u64,
}

impl Radio {
    /// A link for sending messages, or with `answering` for listening and answering
    /// status requests.
    fn connect(
        &self,
        me: &Addr,
        net: &Addr,
        answering: bool,
        start: Instant,
    ) -> io::Result<Box<dyn Link>> {
        Ok(match &self.transport {
            Transport::Kiss {
                addr,
                baud,
                txdelay_ms,
            } => {
                let ch = Channel::new(*baud, *txdelay_ms);
                Box::new(KissLink::connect(addr, me.clone(), net.clone(), ch, start)?)
            }
            Transport::Ardop { addr, mode } => {
                let mode = if answering { ardop::REPORT_MODE } else { *mode };
                Box::new(ArdopLink::connect(addr, me, mode, start)?)
            }
        })
    }

    fn frame_max(&self) -> usize {
        match &self.transport {
            Transport::Kiss { .. } => AX25_FRAME_MAX,
            Transport::Ardop { mode, .. } => ardop::frame_max(*mode),
        }
    }

    /// Report slot length and the wait before the first slot.
    fn slots(&self) -> (u16, u16) {
        match &self.transport {
            Transport::Kiss {
                baud, txdelay_ms, ..
            } => {
                let ch = Channel::new(*baud, *txdelay_ms);
                (
                    sender::slot_ms(&ch, self.guard_ms),
                    sender::kiss_lead_ms(&ch, self.guard_ms),
                )
            }
            Transport::Ardop { .. } => {
                let slot = (ardop::REPORT_MS + self.guard_ms).min(u16::MAX as u64) as u16;
                (slot, ardop::LEAD_MS)
            }
        }
    }

    fn describe(&self) -> String {
        match &self.transport {
            Transport::Kiss { addr, .. } => format!("the KISS TNC at {addr}"),
            Transport::Ardop { addr, .. } => format!("ardopcf at {addr}"),
        }
    }

    /// ardopcf takes one host connection at a time; Direwolf takes any number.
    fn exclusive(&self) -> bool {
        matches!(self.transport, Transport::Ardop { .. })
    }
}

/// This station: who it is, which net it's on, and where it keeps its files.
pub struct Station {
    pub call: Addr,
    pub net: Addr,
    pub radio: Radio,
    pub dir: PathBuf,
    pub key: Option<SigningKey>,
    // with an ardopcf link, sending and listening take turns on the one connection
    tnc: Mutex<()>,
    yield_tnc: AtomicBool,
}

/// A message ready to send, and how to send it.
pub struct Outgoing {
    pub b2f: Vec<u8>,
    pub to: Vec<String>,
    pub mode: Mode,
    pub precedence: Precedence,
    pub sign: bool,
    pub acks: bool,
    pub margin: f64,
    pub rounds: u32,
    pub ttl_min: u16,
    pub repeats: u32,
    pub interval_s: u64,
}

impl Outgoing {
    /// The defaults for a mode. `ack` asks for acknowledgements on an emergency
    /// message; addressed messages always get them and bulletins never do.
    pub fn new(b2f: Vec<u8>, to: Vec<String>, mode: Mode, ack: bool) -> Result<Outgoing> {
        if ack && mode != Mode::Emergency {
            bail!("acknowledgements are only optional for emergency messages");
        }
        let acks = mode == Mode::Addressed || ack;
        Ok(Outgoing {
            b2f,
            to,
            mode,
            precedence: Precedence::default_for(mode),
            sign: false,
            acks,
            margin: if acks { 0.1 } else { 0.5 },
            rounds: 3,
            ttl_min: 60,
            repeats: 0,
            interval_s: 600,
        })
    }
}

/// Recipient names, upper-cased and checked against the roster. With a roster and
/// no names, every member but this station.
pub fn recipients(to: &[String], roster: Option<&Roster>, me: &Addr) -> Result<Vec<String>> {
    let mut out = vec![];
    for t in to {
        let a: Addr = t.parse().map_err(anyhow::Error::msg)?;
        let name = a.to_string();
        if let Some(r) = roster
            && !r.knows(&name)
        {
            bail!("{name} is not in the roster for {}", r.net);
        }
        if !out.contains(&name) {
            out.push(name);
        }
    }
    if out.is_empty()
        && let Some(r) = roster
    {
        out = r
            .calls()
            .into_iter()
            .filter(|c| *c != me.to_string())
            .collect();
    }
    if out.is_empty() {
        bail!("no recipients: name them, or give a roster to send to every member");
    }
    Ok(out)
}

impl Station {
    pub fn new(
        call: Addr,
        net: Addr,
        radio: Radio,
        dir: PathBuf,
        key: Option<SigningKey>,
    ) -> Station {
        Station {
            call,
            net,
            radio,
            dir,
            key,
            tnc: Mutex::new(()),
            yield_tnc: AtomicBool::new(false),
        }
    }

    /// Connects to the TNC the way a listener would, and lets go again. Says
    /// which TNC answered, or `None` when it wasn't tried: ardopcf hands itself
    /// to the newest connection, so a probe would cut off a running listener.
    pub fn try_tnc(&self) -> io::Result<Option<String>> {
        if self.radio.exclusive() {
            return Ok(None);
        }
        let link = self
            .radio
            .connect(&self.call, &self.net, true, Instant::now())?;
        drop(link);
        Ok(Some(self.radio.describe()))
    }

    /// Takes the TNC from the listener when only one connection is allowed.
    fn claim_tnc(&self) -> Option<MutexGuard<'_, ()>> {
        if !self.radio.exclusive() {
            return None;
        }
        self.yield_tnc.store(true, Ordering::SeqCst);
        let guard = self.tnc.lock().unwrap_or_else(|e| e.into_inner());
        self.yield_tnc.store(false, Ordering::SeqCst);
        Some(guard)
    }

    /// Sends one message, logs the outcome, and returns each recipient's final state.
    /// `started` sees the delivery once it's encoded, before anything goes on the air.
    pub fn send(
        &self,
        out: Outgoing,
        started: impl FnOnce(&Delivery),
        progress: impl FnMut(Progress),
    ) -> Result<Vec<State>> {
        if !(0.0..=10.0).contains(&out.margin) {
            bail!("margin {} is out of range (0 to 10)", out.margin);
        }
        let key = match (out.sign, &self.key) {
            (true, Some(k)) => Some(k),
            (true, None) => bail!("there's no station key to sign with; run `netcast key` first"),
            (false, _) => None,
        };
        let id = rand::rng().random_range(1..=u32::MAX);
        let env = Envelope {
            from: self.call.to_string(),
            recipients: out.to.clone(),
            mode: out.mode,
            precedence: out.precedence,
            ttl_min: out.ttl_min,
        };
        let d = Delivery::new(id, &out.b2f, env, self.radio.frame_max(), key)
            .map_err(anyhow::Error::msg)?;
        let (slot_ms, lead_ms) = self.radio.slots();
        let plan = Plan {
            margin: out.margin,
            rounds: out.rounds,
            acks: out.acks,
            repeats: if out.acks { 0 } else { out.repeats },
            repeat_gap_ms: out.interval_s * 1000,
            slot_ms,
            lead_ms,
            guard_ms: self.radio.guard_ms,
        };
        let _tnc = self.claim_tnc();
        let mut link = self
            .radio
            .connect(&self.call, &self.net, false, Instant::now())?;
        started(&d);
        let states = sender::run(&mut *link, &d, &plan, progress)?;
        let outcome: Vec<(String, String)> = out
            .to
            .iter()
            .zip(&states)
            .map(|(c, s)| {
                let s = if out.acks {
                    s.to_string()
                } else {
                    "sent".into()
                };
                (c.clone(), s)
            })
            .collect();
        history::record_sent(
            &self.dir,
            unix_now(),
            id,
            &d.announce.mid,
            out.mode.name(),
            &outcome,
        )?;
        Ok(states)
    }

    /// Receives for `inbox` until the process ends, reconnecting to the TNC whenever
    /// it goes away. Returns only if it can't start.
    pub fn listen(&self, inbox: Inbox, board: Option<Shared>) -> Result<()> {
        let names: Vec<String> = inbox.calls.iter().map(Addr::to_string).collect();
        let mut rx =
            Receiver::new(names.clone(), inbox.groups).trusting(inbox.keys, inbox.require_signed);
        let now_unix = unix_now();
        for id in history::decoded_since(&self.dir, now_unix.saturating_sub(REPLAY_WINDOW_S))? {
            rx.seen(id);
        }
        for r in history::unexpired(&self.dir, now_unix)? {
            rx.remember(r.id, r.acks, (r.expires_unix - now_unix) * 1000);
        }
        let reply_as = inbox
            .calls
            .first()
            .ok_or_else(|| anyhow!("listening needs at least one callsign"))?
            .clone();
        let mut l = Listener {
            rx,
            due: vec![],
            mailbox: inbox.mailbox,
            dir: self.dir.clone(),
            board,
            other_versions: BTreeSet::new(),
        };
        l.say(format!(
            "listening on {} as {}, writing to {}",
            self.net,
            names.join(", "),
            l.mailbox.display()
        ));
        // one clock for every connection, since the receiver keeps times across them
        let start = Instant::now();
        let mut backoff = RETRY_MIN;
        loop {
            while self.yield_tnc.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(200));
            }
            let held = self
                .radio
                .exclusive()
                .then(|| self.tnc.lock().unwrap_or_else(|e| e.into_inner()));
            match self.radio.connect(&reply_as, &self.net, true, start) {
                Ok(mut link) => {
                    backoff = RETRY_MIN;
                    l.tnc(format!("connected to {}", self.radio.describe()), true);
                    match l.serve(&mut *link, &self.yield_tnc) {
                        Ok(()) => l.tnc("handed the TNC over for sending".into(), true),
                        Err(e) => l.tnc(format!("lost the TNC connection: {e}"), false),
                    }
                }
                Err(e) => {
                    drop(held);
                    l.tnc(
                        format!("{e}; trying again in {} s", backoff.as_secs()),
                        false,
                    );
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }
    }
}

/// What a listening station accepts, and where it puts it.
pub struct Inbox {
    pub calls: Vec<Addr>,
    pub groups: Vec<String>,
    pub mailbox: PathBuf,
    pub keys: HashMap<String, VerifyingKey>,
    pub require_signed: bool,
}

struct Listener {
    rx: Receiver,
    due: Vec<(u64, Frame)>,
    mailbox: PathBuf,
    dir: PathBuf,
    board: Option<Shared>,
    /// Protocol versions heard that this build can't read, each mentioned once.
    other_versions: BTreeSet<u8>,
}

impl Listener {
    fn say(&self, line: String) {
        println!("{line}");
        if let Some(b) = &self.board {
            ui::edit(b, |b| b.log(line));
        }
    }

    fn warn(&self, line: String) {
        eprintln!("{line}");
        if let Some(b) = &self.board {
            ui::edit(b, |b| b.log(line));
        }
    }

    fn tnc(&self, line: String, ok: bool) {
        if ok {
            self.say(line.clone());
        } else {
            self.warn(line.clone());
        }
        if let Some(b) = &self.board {
            ui::edit(b, |b| b.tnc = line);
        }
    }

    fn update(&self, f: impl FnOnce(&mut Vec<Incoming>)) {
        if let Some(b) = &self.board {
            ui::edit(b, |b| f(&mut b.incoming));
        }
    }

    /// Runs until the link fails, or until `stop` asks for the TNC back.
    fn serve(&mut self, link: &mut dyn Link, stop: &AtomicBool) -> io::Result<()> {
        // a report whose slot went by while the TNC was handed over would only
        // land on top of someone else's
        let now = link.now_ms();
        self.due.retain(|(t, _)| *t + STALE_REPORT_MS >= now);
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            // wake at least once a second, so a send waiting for the TNC isn't kept waiting
            let poll = link.now_ms() + 1000;
            let next = self
                .due
                .iter()
                .map(|(t, _)| *t)
                .min()
                .map_or(poll, |t| t.min(poll));
            let heard = link.recv_until(next)?;
            if let Some(bytes) = &heard {
                self.check_version(bytes);
            }
            if let Some(bytes) = heard
                && let Some(f) = Frame::parse(&bytes)
            {
                let now = link.now_ms();
                let id = f.id();
                for out in self.rx.handle(f, now) {
                    self.on_output(out, now);
                }
                if let Some((have, k)) = self.rx.progress(id) {
                    self.update(|inc| ui::progress(inc, id, have, k));
                }
            }
            let now = link.now_ms();
            let (ready, later) = std::mem::take(&mut self.due)
                .into_iter()
                .partition(|(t, _)| *t <= now);
            self.due = later;
            for (_, f) in ready {
                if let Frame::Report { id, need, .. } = &f {
                    match need {
                        None => self.say(format!("delivery {id:08x}: reported decoded")),
                        Some(n) => {
                            self.say(format!("delivery {id:08x}: reported needing {n} more"))
                        }
                    }
                }
                link.send(&f.encode())?;
            }
            self.rx.expire(now);
        }
    }

    /// Says once per version when netcast frames arrive that this build can't read.
    fn check_version(&mut self, bytes: &[u8]) {
        if let Some(v) = Frame::other_version(bytes)
            && self.other_versions.insert(v)
        {
            self.warn(format!(
                "hearing netcast frames in protocol version {v}, but this netcast {} speaks version {VERSION}; the sender and this station need releases that speak the same version",
                env!("CARGO_PKG_VERSION")
            ));
        }
    }

    fn on_output(&mut self, out: Output, now: u64) {
        match out {
            Output::Announced(a) => {
                self.say(format!(
                    "delivery {:08x}: {} from {} ({}, {}, {}), {} symbols",
                    a.id,
                    a.mid,
                    a.from,
                    a.mode.name(),
                    a.precedence.name(),
                    a.trust.name(),
                    a.k
                ));
                self.update(|inc| ui::announced(inc, &a));
            }
            Output::Report { at_ms, frame } => self.due.push((at_ms, frame)),
            Output::Decoded(d) => {
                // a verified message may replace an unverified one squatting on its MID
                let replace = d.trust == Trust::Verified;
                for call in &d.calls {
                    let problem = match pat::deliver(&self.mailbox, call, &d.mid, &d.b2f, replace) {
                        Ok(Stored::New(path)) => {
                            self.say(format!(
                                "decoded {} from {} ({}, {}), wrote {}",
                                d.mid,
                                d.from,
                                d.precedence.name(),
                                d.trust.name(),
                                path.display()
                            ));
                            None
                        }
                        Ok(Stored::Already) => {
                            self.say(format!("decoded {}, already in {call}'s inbox", d.mid));
                            None
                        }
                        Ok(Stored::Clash) => Some(format!(
                            "a different message with MID {} is already in {call}'s inbox, so this one wasn't stored",
                            d.mid
                        )),
                        Err(e) => Some(format!("can't write {} into {call}'s inbox: {e}", d.mid)),
                    };
                    if let Some(why) = problem {
                        // not stored means not decoded: receive it again rather than ack it
                        self.warn(why.clone());
                        self.update(|inc| ui::failed(inc, d.id, &why));
                        self.rx.forget(d.id);
                        return;
                    }
                }
                let note = message::header(&d.b2f, "from").and_then(|claimed| {
                    let call = claimed.split('@').next().unwrap_or("").trim();
                    (!call.eq_ignore_ascii_case(&d.from)).then(|| {
                        format!(
                            "the message says it's from {}, but {} sent it",
                            ui::clean(&claimed, 60),
                            d.from
                        )
                    })
                });
                if let Some(n) = &note {
                    self.say(format!("{}: {n}", d.mid));
                }
                let subject = message::header(&d.b2f, "subject");
                self.update(|inc| ui::decoded(inc, d.id, subject, note));
                let r = history::Received {
                    id: d.id,
                    expires_unix: unix_now() + d.expires_ms.saturating_sub(now) / 1000,
                    acks: d.acks,
                };
                let logged = history::record_received(
                    &self.dir,
                    unix_now(),
                    &d.mid,
                    d.mode.name(),
                    &d.calls,
                    &r,
                );
                if let Err(e) = logged {
                    self.warn(format!(
                        "can't write the received log in {}: {e}",
                        self.dir.display()
                    ));
                }
            }
            Output::Failed { id, why } => {
                self.say(format!("delivery {id:08x}: {why}"));
                self.update(|inc| ui::failed(inc, id, &why));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Addr {
        s.parse().unwrap()
    }

    #[test]
    fn recipients_come_from_the_roster_when_none_are_named() {
        let mut r = Roster::new(&addr("NET1"));
        for c in ["N0NCS", "K1ABC", "W2XYZ"] {
            r.add_member(&addr(c), None).unwrap();
        }
        let me = addr("N0NCS");
        assert_eq!(
            recipients(&[], Some(&r), &me).unwrap(),
            vec!["K1ABC", "W2XYZ"]
        );
        let named = ["w2xyz".to_string(), "NET1".into(), "W2XYZ".into()];
        assert_eq!(
            recipients(&named, Some(&r), &me).unwrap(),
            vec!["W2XYZ", "NET1"]
        );
        let stranger = ["N3OOO".to_string()];
        assert!(recipients(&stranger, Some(&r), &me).is_err());
        assert_eq!(recipients(&stranger, None, &me).unwrap(), vec!["N3OOO"]);
        assert!(recipients(&[], None, &me).is_err());
    }

    #[test]
    fn modes_get_their_defaults() {
        let o = Outgoing::new(vec![], vec![], Mode::Addressed, false).unwrap();
        assert!(o.acks && o.margin == 0.1);
        let o = Outgoing::new(vec![], vec![], Mode::Bulletin, false).unwrap();
        assert!(!o.acks && o.margin == 0.5);
        let o = Outgoing::new(vec![], vec![], Mode::Emergency, true).unwrap();
        assert!(o.acks);
        assert_eq!(o.precedence, Precedence::Emergency);
        let o = Outgoing::new(vec![], vec![], Mode::Addressed, false).unwrap();
        assert_eq!(o.precedence, Precedence::Routine);
        assert!(Outgoing::new(vec![], vec![], Mode::Bulletin, true).is_err());
    }

    #[test]
    fn another_protocol_version_is_mentioned_once() {
        let board = Shared::default();
        let mut l = Listener {
            rx: Receiver::new(vec!["K1ABC".into()], vec![]),
            due: vec![],
            mailbox: PathBuf::new(),
            dir: PathBuf::new(),
            board: Some(board.clone()),
            other_versions: BTreeSet::new(),
        };
        let mut old = Frame::Symbol {
            id: 1,
            esi: 0,
            data: vec![1],
        }
        .encode();
        l.check_version(&old);
        old[2] = VERSION - 1;
        l.check_version(&old);
        l.check_version(&old);
        let said = ui::edit(&board, |b| {
            b.log
                .iter()
                .filter(|l| l.contains("protocol version"))
                .count()
        });
        assert_eq!(said, 1);
    }
}
