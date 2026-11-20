use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Result, anyhow};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::codec::MAX_OBJECT;
use crate::frame::Mode;
use crate::keys;
use crate::message;
use crate::receiver::Trust;
use crate::roster::Roster;
use crate::sender::{Progress, State};
use crate::station::{self, Inbox, Outgoing, Station, unix_now};

const LOG_LINES: usize = 200;
const INCOMING_MAX: usize = 50;
const LOG_WIDTH: usize = 300;
const SUBJECT_WIDTH: usize = 120;
// a message is at most 4 MiB, and a form around it is small
const MAX_REQUEST: usize = MAX_OBJECT + 64 * 1024;

const INDEX: &str = include_str!("../ui/index.html");
const CSS: &str = include_str!("../ui/app.css");
const JS: &str = include_str!("../ui/app.js");
const ICON: &str = include_str!("../ui/favicon.svg");

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

pub fn announced(
    inc: &mut Vec<Incoming>,
    id: u32,
    from: &str,
    mid: &str,
    mode: Mode,
    trust: Trust,
    k: u32,
) {
    if find(inc, id).is_some() {
        return;
    }
    inc.insert(
        0,
        Incoming {
            id: format!("{id:08x}"),
            from: from.into(),
            mid: mid.into(),
            mode: mode.name(),
            trust: trust.name(),
            have: 0,
            k,
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
}

/// One HTTP request, as far as the UI cares.
pub struct Req {
    pub method: String,
    pub path: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    pub token: Option<String>,
    pub body: Vec<u8>,
}

pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

fn reply(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status,
        content_type,
        body: body.into(),
    }
}

fn error(status: u16, msg: impl std::fmt::Display) -> Reply {
    reply(
        status,
        "application/json",
        json!({ "error": msg.to_string() }).to_string(),
    )
}

pub struct Ui {
    station: Arc<Station>,
    board: Shared,
    roster: Option<Roster>,
    outbox: Option<PathBuf>,
    token: String,
    port: u16,
}

impl Ui {
    pub fn new(station: Station, roster: Option<Roster>, outbox: Option<PathBuf>, port: u16) -> Ui {
        let token: [u8; 16] = rand::rng().random();
        Ui {
            station: Arc::new(station),
            board: Arc::new(Mutex::new(Board::default())),
            roster,
            outbox,
            token: keys::hex(&token),
            port,
        }
    }

    /// Starts the listener and serves the page on 127.0.0.1 until the process ends.
    pub fn run(self, inbox: Inbox) -> Result<()> {
        let server = tiny_http::Server::http(("127.0.0.1", self.port))
            .map_err(|e| anyhow!("can't serve on 127.0.0.1:{}: {e}", self.port))?;
        let (s, b) = (self.station.clone(), self.board.clone());
        edit(&b, |b| b.tnc = "connecting to the TNC".into());
        thread::spawn(move || {
            if let Err(e) = s.listen(inbox, Some(b.clone())) {
                edit(&b, |b| b.tnc = format!("not listening: {e:#}"));
            }
        });
        println!("netcast UI on http://127.0.0.1:{}/", self.port);
        let ui = Arc::new(self);
        for request in server.incoming_requests() {
            // one thread each, so a client that stalls mid-request holds up only itself
            let ui = ui.clone();
            thread::spawn(move || ui.answer(request));
        }
        Ok(())
    }

    fn answer(&self, mut request: tiny_http::Request) {
        let resp = match read_request(&mut request) {
            Ok(req) => self.handle(&req),
            Err(e) => error(400, e),
        };
        let headers = [
            ("Content-Type", resp.content_type),
            ("X-Content-Type-Options", "nosniff"),
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
            (
                "Content-Security-Policy",
                "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
            ),
        ];
        let mut r = tiny_http::Response::from_data(resp.body).with_status_code(resp.status);
        for (k, v) in headers {
            let h = tiny_http::Header::from_bytes(k, v).expect("static header is valid");
            r.add_header(h);
        }
        if let Err(e) = request.respond(r) {
            eprintln!("couldn't answer the browser: {e}");
        }
    }

    fn allowed_host(&self, host: &str) -> bool {
        let p = self.port;
        [
            format!("127.0.0.1:{p}"),
            format!("localhost:{p}"),
            format!("[::1]:{p}"),
        ]
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host))
    }

    /// Every request is checked here. The page can make the radio transmit, so a
    /// request has to name this server as its host (which stops DNS rebinding),
    /// and API calls have to come from this page: same origin, carrying the token
    /// that only the served page knows.
    pub fn handle(&self, req: &Req) -> Reply {
        if !req.host.as_deref().is_some_and(|h| self.allowed_host(h)) {
            return error(421, "this server only answers to 127.0.0.1 and localhost");
        }
        let api = req.path.starts_with("/api/");
        if api {
            let origin_ok = req.origin.as_deref().is_none_or(|o| {
                o.strip_prefix("http://")
                    .is_some_and(|h| self.allowed_host(h))
            });
            if !origin_ok || req.token.as_deref() != Some(self.token.as_str()) {
                return error(403, "this request didn't come from the netcast page");
            }
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => reply(
                200,
                "text/html; charset=utf-8",
                INDEX.replace("{{token}}", &self.token),
            ),
            ("GET", "/app.css") => reply(200, "text/css; charset=utf-8", CSS),
            ("GET", "/app.js") => reply(200, "text/javascript; charset=utf-8", JS),
            ("GET", "/favicon.svg") => reply(200, "image/svg+xml", ICON),
            ("GET", "/api/state") => reply(200, "application/json", self.state().to_string()),
            ("POST", "/api/send") => self.send(&req.body),
            (_, "/" | "/app.css" | "/app.js" | "/favicon.svg" | "/api/state" | "/api/send") => {
                error(405, format!("{} isn't allowed here", req.method))
            }
            _ => error(404, format!("nothing at {}", req.path)),
        }
    }

    fn state(&self) -> serde_json::Value {
        let roster = self.roster.as_ref().map(|r| {
            json!({
                "members": r.calls(),
                "groups": std::iter::once(r.net.clone()).chain(r.groups.clone()).collect::<Vec<_>>(),
            })
        });
        let board = edit(&self.board, |b| serde_json::to_value(&*b)).expect("board serializes");
        json!({
            "call": self.station.call.to_string(),
            "net": self.station.net.to_string(),
            "can_sign": self.station.key.is_some(),
            "roster": roster,
            "outbox": self.outbox.as_deref().map(outbox_files),
            "board": board,
        })
    }

    fn send(&self, body: &[u8]) -> Reply {
        let form: SendForm = match serde_json::from_slice(body) {
            Ok(f) => f,
            Err(e) => return error(400, format!("bad form: {e}")),
        };
        let out = match self.outgoing(form) {
            Ok(o) => o,
            Err(e) => return error(400, format!("{e:#}")),
        };
        let busy = edit(&self.board, |b| {
            let busy = b.sending.as_ref().is_some_and(|s| s.phase == "sending");
            if !busy {
                b.sending = Some(Sending {
                    id: String::new(),
                    mid: String::new(),
                    mode: out.mode.name(),
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
            return error(409, "a delivery is already on the air");
        }
        let (s, b) = (self.station.clone(), self.board.clone());
        thread::spawn(move || run_send(&s, &b, out));
        reply(202, "application/json", "{}")
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
        out.sign = f.sign;
        out.repeats = f.repeat.min(10);
        Ok(out)
    }

    fn outbox_message(&self, name: &str) -> Result<Vec<u8>> {
        let dir = self
            .outbox
            .as_deref()
            .ok_or_else(|| anyhow!("this station has no outbox folder"))?;
        // only plain names of files that are listed, never a path
        if !outbox_files(dir)
            .iter()
            .any(|f| f.file == name && f.problem.is_none())
        {
            return Err(anyhow!("{name} is not a usable message in the outbox"));
        }
        Ok(fs::read(dir.join(name))?)
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
                    "sending {} ({}) to {}",
                    d.announce.mid,
                    d.announce.mode.name(),
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

#[derive(Serialize, PartialEq, Debug)]
pub struct OutboxFile {
    pub file: String,
    pub mid: Option<String>,
    pub subject: Option<String>,
    pub problem: Option<String>,
}

/// The .b2f files in the outbox folder, with what's wrong with any that can't go.
pub fn outbox_files(dir: &Path) -> Vec<OutboxFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut files: Vec<OutboxFile> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.to_ascii_lowercase().ends_with(".b2f"))
        .map(|file| {
            let read = fs::metadata(dir.join(&file))
                .map_err(|e| e.to_string())
                .and_then(|m| {
                    if m.len() as usize > MAX_OBJECT {
                        Err("over 4 MiB".into())
                    } else {
                        fs::read(dir.join(&file)).map_err(|e| e.to_string())
                    }
                });
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
        })
        .collect();
    files.sort_by(|a, b| a.file.cmp(&b.file));
    files
}

fn read_request(r: &mut tiny_http::Request) -> Result<Req> {
    let header = |name: &str| {
        r.headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str().to_string())
    };
    let (host, origin, token) = (header("Host"), header("Origin"), header("X-Netcast-Token"));
    let mut body = Vec::new();
    r.as_reader()
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > MAX_REQUEST {
        return Err(anyhow!("request is too big"));
    }
    let path = r.url().split('?').next().unwrap_or("/").to_string();
    Ok(Req {
        method: r.method().as_str().to_string(),
        path,
        host,
        origin,
        token,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::station::{Radio, Transport};

    fn ui(outbox: Option<PathBuf>) -> Ui {
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
        Ui::new(station, None, outbox, 8740)
    }

    fn get(ui: &Ui, path: &str) -> Req {
        Req {
            method: "GET".into(),
            path: path.into(),
            host: Some("127.0.0.1:8740".into()),
            origin: None,
            token: Some(ui.token.clone()),
            body: vec![],
        }
    }

    #[test]
    fn serves_the_page_with_its_token() {
        let u = ui(None);
        let r = u.handle(&Req {
            token: None,
            ..get(&u, "/")
        });
        assert_eq!(r.status, 200);
        let page = String::from_utf8(r.body).unwrap();
        assert!(page.contains(&u.token) && !page.contains("{{token}}"));
        assert_eq!(u.handle(&get(&u, "/api/state")).status, 200);
        assert_eq!(u.handle(&get(&u, "/nope")).status, 404);
    }

    #[test]
    fn refuses_requests_from_anywhere_else() {
        let u = ui(None);
        let rebound = Req {
            host: Some("evil.example:8740".into()),
            ..get(&u, "/")
        };
        assert_eq!(u.handle(&rebound).status, 421);
        let no_host = Req {
            host: None,
            ..get(&u, "/api/state")
        };
        assert_eq!(u.handle(&no_host).status, 421);
        let no_token = Req {
            token: None,
            ..get(&u, "/api/state")
        };
        assert_eq!(u.handle(&no_token).status, 403);
        let wrong_token = Req {
            token: Some("guess".into()),
            ..get(&u, "/api/state")
        };
        assert_eq!(u.handle(&wrong_token).status, 403);
        let cross_site = Req {
            method: "POST".into(),
            origin: Some("https://evil.example".into()),
            body: br#"{"to":["K1ABC"],"mode":"addressed","subject":"x","body":"y"}"#.to_vec(),
            ..get(&u, "/api/send")
        };
        assert_eq!(u.handle(&cross_site).status, 403);
        assert!(edit(&u.board, |b| b.sending.is_none()));
        let localhost = Req {
            origin: Some("http://localhost:8740".into()),
            host: Some("localhost:8740".into()),
            ..get(&u, "/api/state")
        };
        assert_eq!(u.handle(&localhost).status, 200);
    }

    #[test]
    fn send_form_is_checked_before_anything_goes_out() {
        let u = ui(None);
        let post = |body: &str| Req {
            method: "POST".into(),
            body: body.as_bytes().to_vec(),
            ..get(&u, "/api/send")
        };
        let cases = [
            r#"{"to":["K1ABC"],"mode":"addressed","subject":"","body":"hi"}"#,
            r#"{"to":["K1/ABC"],"mode":"addressed","subject":"s","body":"hi"}"#,
            r#"{"to":["K1ABC"],"mode":"loud","subject":"s","body":"hi"}"#,
            r#"{"to":["K1ABC"],"mode":"bulletin","subject":"s","body":"hi","ack":true}"#,
            r#"{"to":["K1ABC"],"mode":"addressed","file":"../secret.b2f"}"#,
            r#"not json"#,
        ];
        for c in cases {
            assert_eq!(u.handle(&post(c)).status, 400, "{c}");
        }
        assert!(edit(&u.board, |b| b.sending.is_none()));
    }

    #[test]
    fn outbox_lists_messages_and_what_is_wrong_with_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (mid, good) = message::compose("N0NCS", &["K1ABC".into()], "Sitrep", "all ok", 0);
        fs::write(tmp.path().join("a.b2f"), &good).unwrap();
        fs::write(tmp.path().join("b.b2f"), b"garbage").unwrap();
        fs::write(tmp.path().join("notes.txt"), b"ignored").unwrap();
        let files = outbox_files(tmp.path());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].mid.as_deref(), Some(mid.as_str()));
        assert_eq!(files[0].subject.as_deref(), Some("Sitrep"));
        assert!(files[1].problem.is_some());

        let u = ui(Some(tmp.path().to_path_buf()));
        assert_eq!(u.outbox_message("a.b2f").unwrap(), good);
        assert!(u.outbox_message("b.b2f").is_err());
        assert!(u.outbox_message("notes.txt").is_err());
    }

    #[test]
    fn text_off_the_air_is_cleaned_and_capped() {
        assert_eq!(clean("Net\u{1b}[2J news\u{202e}", 120), "Net [2J news ");
        let long = "x".repeat(5000);
        assert_eq!(clean(&long, 120).len(), 123);
        let mut inc = vec![];
        announced(
            &mut inc,
            1,
            "N0NCS",
            "MID1",
            Mode::Bulletin,
            Trust::Unsigned,
            3,
        );
        decoded(&mut inc, 1, Some(long), None);
        assert_eq!(inc[0].subject.as_ref().unwrap().len(), 123);
    }
}
