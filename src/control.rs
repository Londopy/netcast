use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::codec::MAX_OBJECT;
use crate::frame::{Mode, Precedence};
use crate::receiver::Announced;
use crate::roster::Roster;
use crate::sender::{Progress, State};
use crate::station::{self, Inbox, Outgoing, Station, unix_now};
use crate::{history, message};

const LOG_LINES: usize = 200;
const INCOMING_MAX: usize = 50;
pub const HISTORY_ROWS: usize = 10;
pub const LOG_WIDTH: usize = 300;
const SUBJECT_WIDTH: usize = 120;

/// What the page shows: the listener's view of incoming deliveries, the
/// delivery being sent, and a log. The listener and send threads write to it.
#[derive(Default, Serialize)]
pub struct Board {
    pub tnc: String,
    pub incoming: Vec<Incoming>,
    pub sending: Option<Sending>,
    pub log: VecDeque<String>,
}

pub type Shared = Arc<Mutex<Board>>;

/// Runs `f` on the board. A thread that panicked mid-edit leaves at worst a
/// stale status line, so a poisoned lock is used anyway.
pub fn edit<R>(b: &Shared, f: impl FnOnce(&mut Board) -> R) -> R {
    f(&mut b.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Text that came off the air, made safe to show: no control or direction-changing
/// characters, and at most `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    let bidi = |c: char| matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() || bidi(c) { ' ' } else { c })
        .take(max)
        .collect();
    if s.chars().count() > max {
        out.push_str("...");
    }
    out
}

impl Board {
    pub fn log(&mut self, line: String) {
        self.log.push_back(clean(&line, LOG_WIDTH));
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }
}

#[derive(Clone, Serialize)]
pub struct Incoming {
    pub id: String,
    pub from: String,
    pub mid: String,
    pub mode: &'static str,
    pub precedence: &'static str,
    pub trust: &'static str,
    pub have: u32,
    pub k: u32,
    pub status: &'static str,
    pub subject: Option<String>,
    pub note: Option<String>,
}

fn find(inc: &mut [Incoming], id: u32) -> Option<&mut Incoming> {
    let id = format!("{id:08x}");
    inc.iter_mut().find(|i| i.id == id)
}

pub fn announced(inc: &mut Vec<Incoming>, a: &Announced) {
    if find(inc, a.id).is_some() {
        return;
    }
    inc.insert(
        0,
        Incoming {
            id: format!("{:08x}", a.id),
            from: a.from.clone(),
            mid: a.mid.clone(),
            mode: a.mode.name(),
            precedence: a.precedence.name(),
            trust: a.trust.name(),
            have: 0,
            k: a.k,
            status: "receiving",
            subject: None,
            note: None,
        },
    );
    inc.truncate(INCOMING_MAX);
}

pub fn progress(inc: &mut [Incoming], id: u32, have: u32, k: u32) {
    if let Some(i) = find(inc, id) {
        i.have = have.min(k);
        i.k = k;
        // a fresh start after a bad symbol shows up as receiving again
        i.status = "receiving";
    }
}

pub fn decoded(inc: &mut [Incoming], id: u32, subject: Option<String>, note: Option<String>) {
    if let Some(i) = find(inc, id) {
        i.status = "decoded";
        i.have = i.k;
        i.subject = subject.map(|s| clean(&s, SUBJECT_WIDTH));
        i.note = note.map(|n| clean(&n, LOG_WIDTH));
    }
}

pub fn failed(inc: &mut [Incoming], id: u32, why: &str) {
    if let Some(i) = find(inc, id) {
        i.status = "failed";
        i.note = Some(clean(why, LOG_WIDTH));
    }
}

#[derive(Serialize)]
pub struct Sending {
    pub id: String,
    pub mid: String,
    pub mode: &'static str,
    pub precedence: &'static str,
    pub k: u32,
    pub signed: bool,
    pub recipients: Vec<(String, String)>,
    pub round: u8,
    pub phase: &'static str,
    pub error: Option<String>,
}

/// What the page posts to send a message: either a composed one or a file from
/// the outbox folder.
#[derive(Deserialize)]
pub struct SendForm {
    #[serde(default)]
    pub to: Vec<String>,
    pub mode: String,
    /// Routine, welfare, priority or emergency; the mode's default when left out.
    pub precedence: Option<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    pub file: Option<String>,
    #[serde(default)]
    pub sign: bool,
    #[serde(default)]
    pub ack: bool,
    #[serde(default)]
    pub repeat: u32,
    /// Minutes between bulletin repeats.
    #[serde(default = "default_interval")]
    pub interval_min: u64,
}

fn default_interval() -> u64 {
    10
}

/// Net control's side of a station, whatever screen drives it: the station,
/// what it reports on the board, and the roster and outbox to send from.
pub struct Control {
    pub station: Arc<Station>,
    pub board: Shared,
    pub roster: Option<Roster>,
    pub outbox: Option<Outbox>,
}

/// Why a send didn't start.
#[derive(Debug)]
pub enum Refused {
    Busy,
    Invalid(String),
}

impl Control {
    pub fn new(station: Station, roster: Option<Roster>, outbox: Option<PathBuf>) -> Control {
        Control {
            station: Arc::new(station),
            board: Arc::new(Mutex::new(Board::default())),
            roster,
            outbox: outbox.map(Outbox::new),
        }
    }

    /// Starts the listener on its own thread, reporting to the board.
    pub fn listen(&self, inbox: Inbox) {
        let (s, b) = (self.station.clone(), self.board.clone());
        edit(&b, |b| b.tnc = "connecting to the TNC".into());
        thread::spawn(move || {
            if let Err(e) = s.listen(inbox, Some(b.clone())) {
                edit(&b, |b| b.tnc = format!("not listening: {e:#}"));
            }
        });
    }

    /// Checks the form and starts the delivery on its own thread, unless one
    /// is already on the air.
    pub fn send(&self, form: SendForm) -> Result<(), Refused> {
        let out = self
            .outgoing(form)
            .map_err(|e| Refused::Invalid(format!("{e:#}")))?;
        let busy = edit(&self.board, |b| {
            let busy = b.sending.as_ref().is_some_and(|s| s.phase == "sending");
            if !busy {
                b.sending = Some(Sending {
                    id: String::new(),
                    mid: String::new(),
                    mode: out.mode.name(),
                    precedence: out.precedence.name(),
                    k: 0,
                    signed: out.sign,
                    recipients: out
                        .to
                        .iter()
                        .map(|c| (c.clone(), "pending".into()))
                        .collect(),
                    round: 0,
                    phase: "sending",
                    error: None,
                });
            }
            busy
        });
        if busy {
            return Err(Refused::Busy);
        }
        let (s, b) = (self.station.clone(), self.board.clone());
        thread::spawn(move || run_send(&s, &b, out));
        Ok(())
    }

    /// Recent deliveries sent and received, newest first.
    pub fn history(&self) -> io::Result<(Vec<history::Entry>, Vec<history::Entry>)> {
        let (sent, received) = history::entries(&self.station.dir)?;
        let recent =
            |rows: Vec<history::Entry>| rows.into_iter().rev().take(HISTORY_ROWS).collect();
        Ok((recent(sent), recent(received)))
    }

    fn outgoing(&self, f: SendForm) -> Result<Outgoing> {
        let mode = match f.mode.as_str() {
            "addressed" => Mode::Addressed,
            "bulletin" => Mode::Bulletin,
            "emergency" => Mode::Emergency,
            other => return Err(anyhow!("unknown mode {other:?}")),
        };
        let me = &self.station.call;
        let to = station::recipients(&f.to, self.roster.as_ref(), me)?;
        let b2f = match &f.file {
            Some(name) => self.outbox_message(name)?,
            None => {
                if f.subject.trim().is_empty() {
                    return Err(anyhow!("the message needs a subject"));
                }
                if f.body.len() > MAX_OBJECT {
                    return Err(anyhow!("the message is over 4 MiB"));
                }
                message::compose(&me.to_string(), &to, f.subject.trim(), &f.body, unix_now()).1
            }
        };
        let mut out = Outgoing::new(b2f, to, mode, f.ack)?;
        if let Some(p) = &f.precedence {
            out.precedence =
                Precedence::from_name(p).ok_or_else(|| anyhow!("unknown precedence {p:?}"))?;
        }
        out.sign = f.sign;
        out.repeats = f.repeat.min(10);
        out.interval_s = f.interval_min.clamp(1, 24 * 60) * 60;
        Ok(out)
    }

    fn outbox_message(&self, name: &str) -> Result<Vec<u8>> {
        let outbox = self
            .outbox
            .as_ref()
            .ok_or_else(|| anyhow!("this station has no outbox folder"))?;
        // only plain names of files that are listed, never a path
        if !outbox
            .list()
            .iter()
            .any(|f| f.file == name && f.problem.is_none())
        {
            return Err(anyhow!("{name} is not a usable message in the outbox"));
        }
        Ok(fs::read(outbox.dir.join(name))?)
    }
}

fn run_send(s: &Station, b: &Shared, out: Outgoing) {
    let (signed, acks) = (out.sign, out.acks);
    let result = s.send(
        out,
        |d| {
            edit(b, |b| {
                if let Some(snd) = &mut b.sending {
                    snd.id = format!("{:08x}", d.id);
                    snd.mid = d.announce.mid.clone();
                    snd.k = d.k();
                    snd.signed = signed;
                }
                b.log(format!(
                    "sending {} ({}, {}) to {}",
                    d.announce.mid,
                    d.announce.mode.name(),
                    d.announce.precedence.name(),
                    d.announce.recipients.join(", ")
                ));
            })
        },
        |p| {
            edit(b, |b| {
                let line = match p {
                    Progress::Pass { symbols } => {
                        format!("queued the announcement and {symbols} symbols")
                    }
                    Progress::Status { round, states } => {
                        if let Some(snd) = &mut b.sending {
                            snd.round = round;
                            for ((_, st), s) in snd.recipients.iter_mut().zip(states) {
                                *st = s.to_string();
                            }
                        }
                        format!("status round {round} done")
                    }
                };
                b.log(line);
            })
        },
    );
    edit(b, |b| {
        let line = match &result {
            Ok(states) if acks => {
                let missing = states.iter().filter(|s| **s != State::Decoded).count();
                format!("delivery finished, {missing} not confirmed")
            }
            Ok(_) => "delivery finished".to_string(),
            Err(e) => format!("delivery failed: {e:#}"),
        };
        if let Some(snd) = &mut b.sending {
            // nobody reports back without acks, so all we know is that it went out
            if result.is_ok() && !acks {
                for (_, st) in &mut snd.recipients {
                    *st = "sent".into();
                }
            }
            snd.phase = if result.is_ok() { "done" } else { "failed" };
            snd.error = result.as_ref().err().map(|e| format!("{e:#}"));
        }
        b.log(line);
    });
}

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct OutboxFile {
    pub file: String,
    pub mid: Option<String>,
    pub subject: Option<String>,
    pub problem: Option<String>,
}

/// A file as it was when last checked: its size and time, and the result.
struct Checked {
    len: u64,
    time: Option<SystemTime>,
    file: OutboxFile,
}

/// The outbox folder. Each file is read and checked once, and again only when
/// its size or time changes, since the page asks for the list every second.
pub struct Outbox {
    dir: PathBuf,
    checked: Mutex<HashMap<String, Checked>>,
}

impl Outbox {
    pub fn new(dir: PathBuf) -> Outbox {
        Outbox {
            dir,
            checked: Mutex::new(HashMap::new()),
        }
    }

    /// The .b2f files in the folder, with what's wrong with any that can't go.
    pub fn list(&self) -> Vec<OutboxFile> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return vec![];
        };
        let mut checked = self.checked.lock().unwrap_or_else(|e| e.into_inner());
        let mut files = vec![];
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let Ok(file) = e.file_name().into_string() else {
                continue;
            };
            if !meta.is_file() || !file.to_ascii_lowercase().ends_with(".b2f") {
                continue;
            }
            let (len, time) = (meta.len(), meta.modified().ok());
            let entry = match checked.get(&file) {
                Some(c) if c.len == len && c.time == time => c.file.clone(),
                _ => {
                    let f = check_file(&self.dir, file.clone(), len);
                    // a file that couldn't be read (locked, still syncing) gets
                    // another try on the next poll instead of staying broken
                    if f.problem.is_none() {
                        let c = Checked {
                            len,
                            time,
                            file: f.clone(),
                        };
                        checked.insert(file, c);
                    }
                    f
                }
            };
            files.push(entry);
        }
        files.sort_by(|a, b| a.file.cmp(&b.file));
        files
    }
}

fn check_file(dir: &Path, file: String, len: u64) -> OutboxFile {
    let read = if len as usize > MAX_OBJECT {
        Err("over 4 MiB".to_string())
    } else {
        fs::read(dir.join(&file)).map_err(|e| e.to_string())
    };
    match read.and_then(|b| message::check(&b).map(|mid| (mid, b))) {
        Ok((mid, b)) => OutboxFile {
            file,
            mid: Some(mid),
            subject: message::header(&b, "subject"),
            problem: None,
        },
        Err(why) => OutboxFile {
            file,
            mid: None,
            subject: None,
            problem: Some(why),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::station::{Radio, Transport};

    fn control(outbox: Option<PathBuf>) -> Control {
        let radio = Radio {
            transport: Transport::Kiss {
                addr: "127.0.0.1:1".into(),
                baud: 1200,
                txdelay_ms: 300,
            },
            guard_ms: 500,
        };
        let station = Station::new(
            "N0NCS".parse().unwrap(),
            "NET1".parse().unwrap(),
            radio,
            std::env::temp_dir(),
            None,
        );
        Control::new(station, None, outbox)
    }

    fn form(json: &str) -> SendForm {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_second_send_is_refused_while_one_is_on_the_air() {
        let c = control(None);
        edit(&c.board, |b| {
            b.sending = Some(Sending {
                id: "00000001".into(),
                mid: "FIRST".into(),
                mode: "addressed",
                precedence: "routine",
                k: 3,
                signed: false,
                recipients: vec![("K1ABC".into(), "pending".into())],
                round: 0,
                phase: "sending",
                error: None,
            })
        });
        let r = c.send(form(
            r#"{"to":["K1ABC"],"mode":"addressed","subject":"x","body":"y"}"#,
        ));
        assert!(matches!(r, Err(Refused::Busy)));
        let mid = edit(&c.board, |b| b.sending.as_ref().map(|s| s.mid.clone()));
        assert_eq!(mid.as_deref(), Some("FIRST"));
    }

    #[test]
    fn the_form_picks_a_precedence_or_takes_the_modes() {
        let c = control(None);
        let form = |extra: &str| -> SendForm {
            serde_json::from_str(&format!(
                r#"{{"to":["K1ABC"],"mode":"emergency","subject":"x","body":"y"{extra}}}"#
            ))
            .unwrap()
        };
        let p = |extra| c.outgoing(form(extra)).map(|o| o.precedence);
        assert_eq!(p("").unwrap(), Precedence::Emergency);
        assert_eq!(
            p(r#","precedence":"welfare""#).unwrap(),
            Precedence::Welfare
        );
        assert!(p(r#","precedence":"flash""#).is_err());
    }

    #[test]
    fn outbox_lists_messages_and_what_is_wrong_with_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (mid, good) = message::compose("N0NCS", &["K1ABC".into()], "Sitrep", "all ok", 0);
        fs::write(tmp.path().join("a.b2f"), &good).unwrap();
        fs::write(tmp.path().join("b.b2f"), b"garbage").unwrap();
        fs::write(tmp.path().join("notes.txt"), b"ignored").unwrap();
        let outbox = Outbox::new(tmp.path().to_path_buf());
        let files = outbox.list();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].mid.as_deref(), Some(mid.as_str()));
        assert_eq!(files[0].subject.as_deref(), Some("Sitrep"));
        assert!(files[1].problem.is_some());
        // a file that changes is checked again
        fs::write(tmp.path().join("a.b2f"), b"now broken").unwrap();
        assert!(outbox.list()[0].problem.is_some());
        fs::write(tmp.path().join("a.b2f"), &good).unwrap();
        assert!(outbox.list()[0].problem.is_none());

        let c = control(Some(tmp.path().to_path_buf()));
        assert_eq!(c.outbox_message("a.b2f").unwrap(), good);
        assert!(c.outbox_message("b.b2f").is_err());
        assert!(c.outbox_message("notes.txt").is_err());
    }

    #[test]
    fn text_off_the_air_is_cleaned_and_capped() {
        assert_eq!(clean("Net\u{1b}[2J news\u{202e}", 120), "Net [2J news ");
        let long = "x".repeat(5000);
        assert_eq!(clean(&long, 120).len(), 123);
        let mut inc = vec![];
        let a = Announced {
            id: 1,
            mode: Mode::Bulletin,
            precedence: Precedence::Routine,
            from: "N0NCS".into(),
            mid: "MID1".into(),
            k: 3,
            trust: crate::receiver::Trust::Unsigned,
        };
        announced(&mut inc, &a);
        decoded(&mut inc, 1, Some(long), None);
        assert_eq!(inc[0].subject.as_ref().unwrap().len(), 123);
    }
}
