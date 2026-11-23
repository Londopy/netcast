use std::io::Read;
use std::sync::Arc;
use std::thread;

use anyhow::{Result, anyhow};
use rand::RngExt;
use serde_json::json;

use crate::codec::MAX_OBJECT;
use crate::control::{Control, LOG_WIDTH, Outbox, Refused, SendForm, clean, edit};
use crate::keys;
use crate::station::Inbox;
use crate::{history, message};

// a message is at most 4 MiB, and a form around it is small
const MAX_REQUEST: usize = MAX_OBJECT + 64 * 1024;

const INDEX: &str = include_str!("../ui/index.html");
const CSS: &str = include_str!("../ui/app.css");
const JS: &str = include_str!("../ui/app.js");
const ICON: &str = include_str!("../ui/favicon.svg");

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
    control: Control,
    token: String,
    port: u16,
}

impl Ui {
    pub fn new(control: Control, port: u16) -> Ui {
        let token: [u8; 16] = rand::rng().random();
        Ui {
            control,
            token: keys::hex(&token),
            port,
        }
    }

    /// Starts the listener and serves the page on 127.0.0.1 until the process ends.
    pub fn run(self, inbox: Inbox) -> Result<()> {
        let server = tiny_http::Server::http(("127.0.0.1", self.port))
            .map_err(|e| anyhow!("can't serve on 127.0.0.1:{}: {e}", self.port))?;
        self.control.listen(inbox);
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
        let c = &self.control;
        let roster = c.roster.as_ref().map(|r| {
            json!({
                "members": r.calls(),
                "groups": std::iter::once(r.net.clone()).chain(r.groups.clone()).collect::<Vec<_>>(),
            })
        });
        let board = edit(&c.board, |b| serde_json::to_value(&*b)).expect("board serializes");
        let recent = |rows: Vec<history::Entry>| -> Vec<serde_json::Value> {
            rows.iter()
                .map(|e| {
                    json!({
                        "when": format!("{} UTC", message::date(e.unix)),
                        "id": e.id, "mid": e.mid, "mode": e.mode, "who": clean(&e.who, LOG_WIDTH),
                    })
                })
                .collect()
        };
        let history = c
            .history()
            .map(|(sent, received)| json!({ "sent": recent(sent), "received": recent(received) }))
            .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        json!({
            "call": c.station.call.to_string(),
            "net": c.station.net.to_string(),
            "can_sign": c.station.key.is_some(),
            "roster": roster,
            "outbox": c.outbox.as_ref().map(Outbox::list),
            "history": history,
            "board": board,
        })
    }

    fn send(&self, body: &[u8]) -> Reply {
        let form: SendForm = match serde_json::from_slice(body) {
            Ok(f) => f,
            Err(e) => return error(400, format!("bad form: {e}")),
        };
        match self.control.send(form) {
            Ok(()) => reply(202, "application/json", "{}"),
            Err(Refused::Busy) => error(409, "a delivery is already on the air"),
            Err(Refused::Invalid(why)) => error(400, why),
        }
    }
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
    use crate::station::{Radio, Station, Transport};
    use std::path::PathBuf;

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
        Ui::new(Control::new(station, None, outbox), 8740)
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
        assert!(edit(&u.control.board, |b| b.sending.is_none()));
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
        assert!(edit(&u.control.board, |b| b.sending.is_none()));
    }
}
