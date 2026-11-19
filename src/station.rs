use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::RngExt;

use crate::ax25::Addr;
use crate::frame::{Frame, Mode};
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
// how long a decoded delivery id is refused if it's heard again, so a recording
// played back later doesn't land as a new message
const REPLAY_WINDOW_S: u64 = 7 * 86400;

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How to reach the radio, and its timing.
#[derive(Clone)]
pub struct Radio {
    pub kiss: String,
    pub baud: u32,
    pub txdelay_ms: u64,
    pub guard_ms: u64,
}

impl Radio {
    fn channel(&self) -> Channel {
        Channel::new(self.baud, self.txdelay_ms)
    }
}

/// This station: who it is, which net it's on, and where it keeps its files.
pub struct Station {
    pub call: Addr,
    pub net: Addr,
    pub radio: Radio,
    pub dir: PathBuf,
    pub key: Option<SigningKey>,
}

/// A message ready to send, and how to send it.
pub struct Outgoing {
    pub b2f: Vec<u8>,
    pub to: Vec<String>,
    pub mode: Mode,
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
            ttl_min: out.ttl_min,
        };
        let d =
            Delivery::new(id, &out.b2f, env, AX25_FRAME_MAX, key).map_err(anyhow::Error::msg)?;
        let ch = self.radio.channel();
        let plan = Plan {
            margin: out.margin,
            rounds: out.rounds,
            acks: out.acks,
            repeats: if out.acks { 0 } else { out.repeats },
            repeat_gap_ms: out.interval_s * 1000,
            slot_ms: sender::slot_ms(&ch, self.radio.guard_ms),
            guard_ms: self.radio.guard_ms,
        };
        let mut link = KissLink::connect(
            &self.radio.kiss,
            self.call.clone(),
            self.net.clone(),
            ch,
            Instant::now(),
        )?;
        started(&d);
        let states = sender::run(&mut link, &d, &plan, progress)?;
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
            let ch = self.radio.channel();
            match KissLink::connect(
                &self.radio.kiss,
                reply_as.clone(),
                self.net.clone(),
                ch,
                start,
            ) {
                Ok(mut link) => {
                    backoff = RETRY_MIN;
                    l.tnc(format!("connected to the TNC at {}", self.radio.kiss), true);
                    if let Err(e) = l.serve(&mut link) {
                        l.tnc(format!("lost the KISS connection: {e}"), false);
                    }
                }
                Err(e) => {
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

    /// Runs until the link fails.
    fn serve(&mut self, link: &mut KissLink) -> std::io::Result<()> {
        loop {
            let next = self
                .due
                .iter()
                .map(|(t, _)| *t)
                .min()
                .unwrap_or(link.now_ms() + 1000);
            if let Some(bytes) = link.recv_until(next)?
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
                        Some(n) => self.say(format!("delivery {id:08x}: reported need {n}")),
                    }
                }
                link.send(&f.encode())?;
            }
            self.rx.expire(now);
        }
    }

    fn on_output(&mut self, out: Output, now: u64) {
        match out {
            Output::Announced {
                id,
                mode,
                from,
                mid,
                k,
                trust,
            } => {
                self.say(format!(
                    "delivery {id:08x}: {mid} from {from} ({}, {}), {k} symbols",
                    mode.name(),
                    trust.name()
                ));
                self.update(|inc| ui::announced(inc, id, &from, &mid, mode, trust, k));
            }
            Output::Report { at_ms, frame } => self.due.push((at_ms, frame)),
            Output::Decoded(d) => {
                // a verified message may replace an unverified one squatting on its MID
                let replace = d.trust == Trust::Verified;
                for call in &d.calls {
                    let problem = match pat::deliver(&self.mailbox, call, &d.mid, &d.b2f, replace) {
                        Ok(Stored::New(path)) => {
                            self.say(format!(
                                "decoded {} from {} ({}), wrote {}",
                                d.mid,
                                d.from,
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
        assert!(Outgoing::new(vec![], vec![], Mode::Bulletin, true).is_err());
    }
}
