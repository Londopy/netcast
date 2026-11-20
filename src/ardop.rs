use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::ax25::Addr;
use crate::link::Link;

/// An ardopcf FEC frame type and how many data bytes one frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FecMode {
    pub name: &'static str,
    pub bytes: usize,
}

const fn m(name: &'static str, bytes: usize) -> FecMode {
    FecMode { name, bytes }
}

/// Data bytes per frame, from ardopcf's frame table (FrameInfo in ARDOPC.c). The
/// 16-byte modes are left out: a netcast frame doesn't fit in one.
pub const MODES: &[FecMode] = &[
    m("4FSK.500.100S", 32),
    m("4FSK.500.100", 64),
    m("4PSK.200.100", 64),
    m("8PSK.200.100", 108),
    m("16QAM.200.100", 128),
    m("4PSK.500.100", 128),
    m("8PSK.500.100", 216),
    m("16QAM.500.100", 256),
    m("4PSK.1000.100", 256),
    m("8PSK.1000.100", 432),
    m("16QAM.1000.100", 512),
    m("4PSK.2000.100", 512),
    m("8PSK.2000.100", 864),
    m("16QAM.2000.100", 1024),
    m("4FSK.2000.600S", 200),
    m("4FSK.2000.600", 600),
];

pub fn mode(name: &str) -> Option<FecMode> {
    MODES
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(name))
        .copied()
}

/// Receivers answer status requests in this mode, so the sender knows how long
/// an answer takes whatever mode the message itself went out in.
pub const REPORT_MODE: FecMode = m("4FSK.500.100S", 32);
// measured on ardopcf 1.0.4.1.3 with its default leader: one 4FSK.500.100S frame,
// the gap, and the ID frame that follows it
pub const REPORT_MS: u64 = 4_000;
// the ID frame ardopcf sends after a transmission's last data frame (1.8 s measured)
// plus slack, so nobody answers on top of it
pub const LEAD_MS: u16 = 2_300;

// each netcast frame travels as a 2-byte length, the frame, then filler up to the
// mode's frame size, so one ARDOP frame always holds exactly one netcast frame
const LEN: usize = 2;

pub fn frame_max(mode: FecMode) -> usize {
    mode.bytes - LEN
}

pub fn wrap(mode: FecMode, frame: &[u8]) -> Vec<u8> {
    assert!(
        frame.len() <= frame_max(mode),
        "frame too big for {}",
        mode.name
    );
    let mut b = vec![0u8; mode.bytes];
    b[..LEN].copy_from_slice(&(frame.len() as u16).to_be_bytes());
    b[LEN..LEN + frame.len()].copy_from_slice(frame);
    // varied filler, not zeros: a run of one byte value keeps 4FSK on one tone, and
    // over the virtual cable frames padded with zeros failed to decode far more often
    let mut x: u32 = 0x9e37_79b9;
    for p in &mut b[LEN + frame.len()..] {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *p = x as u8;
    }
    b
}

pub fn unwrap(block: &[u8]) -> Option<&[u8]> {
    let n = u16::from_be_bytes(block.get(..LEN)?.try_into().ok()?) as usize;
    block.get(LEN..LEN + n)
}

// frames we let wait in ardopcf's buffer, which drops data silently when it's full
const PACE_FRAMES: usize = 4;
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// a frame takes at most a few seconds on the air, so this long without the TNC
// taking any data or finishing means it's stuck
const STALL_MS: u64 = 60_000;

enum Event {
    Line(String),
    Data([u8; 3], Vec<u8>),
    Closed(String),
}

fn read_lines(mut s: TcpStream, tx: Sender<Event>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = match s.read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(Event::Closed("ardopcf closed the command port".into()));
                return;
            }
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Event::Closed(format!("ardopcf command port: {e}")));
                return;
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(i) = buf.iter().position(|b| *b == b'\r') {
            let line = String::from_utf8_lossy(&buf[..i]).trim().to_string();
            buf.drain(..=i);
            if !line.is_empty() && tx.send(Event::Line(line)).is_err() {
                return;
            }
        }
    }
}

fn read_blocks(mut s: TcpStream, tx: Sender<Event>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match s.read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(Event::Closed("ardopcf closed the data port".into()));
                return;
            }
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Event::Closed(format!("ardopcf data port: {e}")));
                return;
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        // each block: 2-byte length (tag included), 3-byte tag, data
        while buf.len() >= 2 {
            let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
            if buf.len() < 2 + len {
                break;
            }
            if len >= 3 {
                let tag = [buf[2], buf[3], buf[4]];
                let ev = Event::Data(tag, buf[5..2 + len].to_vec());
                if tx.send(ev).is_err() {
                    return;
                }
            }
            buf.drain(..2 + len);
        }
    }
}

/// Netcast over ardopcf in FEC mode: the command port and, one above it, the data port.
pub struct ArdopLink {
    cmd: TcpStream,
    data: TcpStream,
    events: Receiver<Event>,
    inbox: VecDeque<Vec<u8>>,
    mode: FecMode,
    queued: usize,
    // a BUFFER line has arrived since the last data block went out
    got_buffer: bool,
    sending: bool,
    start: Instant,
}

fn connect_any(addrs: &[SocketAddr], port: u16) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address to connect to");
    for a in addrs {
        match TcpStream::connect_timeout(&SocketAddr::new(a.ip(), port), CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

impl ArdopLink {
    /// Connects to ardopcf's command port at `addr` and sets it up for FEC in `mode`.
    /// `start` is the zero of the link's clock, as for `KissLink`.
    pub fn connect(
        addr: &str,
        mycall: &Addr,
        mode: FecMode,
        start: Instant,
    ) -> io::Result<ArdopLink> {
        let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
        let port = addrs.first().map(|a| a.port()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("{addr} did not resolve"))
        })?;
        let reach = |p: u16| {
            connect_any(&addrs, p).map_err(|e| {
                io::Error::new(e.kind(), format!("can't reach ardopcf on port {p}: {e}"))
            })
        };
        let data_port = port.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ardopcf's data port is the command port + 1, so the command port can't be 65535",
            )
        })?;
        let cmd = reach(port)?;
        let data = reach(data_port)?;
        cmd.set_write_timeout(Some(REPLY_TIMEOUT))?;
        data.set_write_timeout(Some(REPLY_TIMEOUT))?;
        let (tx, events) = mpsc::channel();
        let (c, d, tx2) = (cmd.try_clone()?, data.try_clone()?, tx.clone());
        thread::spawn(move || read_lines(c, tx));
        thread::spawn(move || read_blocks(d, tx2));
        let mut link = ArdopLink {
            cmd,
            data,
            events,
            inbox: VecDeque::new(),
            mode,
            queued: 0,
            got_buffer: false,
            sending: false,
            start,
        };
        link.command("INITIALIZE", "INITIALIZE")?;
        link.command("PROTOCOLMODE FEC", "PROTOCOLMODE now")?;
        link.command(&format!("MYCALL {mycall}"), "MYCALL now")?;
        link.command("FECREPEATS 0", "FECREPEATS now")?;
        // ardopcf sends an ID frame with every transmission, which keeps every
        // station identified (97.119) without netcast doing anything
        link.command("FECID TRUE", "FECID now")?;
        if mode.name.contains(".600") {
            link.command("USE600MODES TRUE", "USE600MODES now")?;
        }
        link.command(&format!("FECMODE {}", mode.name), "FECMODE now")?;
        Ok(link)
    }

    fn take(&mut self, ev: Event) -> io::Result<Option<String>> {
        match ev {
            Event::Line(line) => {
                let mut words = line.split_whitespace();
                match (words.next(), words.next()) {
                    (Some("BUFFER"), Some(n)) => {
                        self.queued = n.parse().unwrap_or(self.queued);
                        self.got_buffer = true;
                    }
                    (Some("NEWSTATE"), Some("FECSEND")) => self.sending = true,
                    (Some("NEWSTATE"), Some(_)) => self.sending = false,
                    _ => {}
                }
                Ok(Some(line))
            }
            Event::Data(tag, data) => {
                // ERR frames failed to decode, and IDF is a station ID; only FEC is ours
                if &tag == b"FEC"
                    && let Some(frame) = unwrap(&data)
                {
                    self.inbox.push_back(frame.to_vec());
                }
                Ok(None)
            }
            Event::Closed(why) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, why)),
        }
    }

    /// Handles events until `done` says so, or until `deadline_ms`.
    /// Returns false on the deadline.
    fn pump_until(
        &mut self,
        deadline_ms: u64,
        mut done: impl FnMut(&mut Self, Option<&str>) -> bool,
    ) -> io::Result<bool> {
        if done(self, None) {
            return Ok(true);
        }
        loop {
            let now = self.now_ms();
            if now >= deadline_ms {
                return Ok(false);
            }
            match self
                .events
                .recv_timeout(Duration::from_millis(deadline_ms - now))
            {
                Ok(ev) => {
                    let line = self.take(ev)?;
                    if done(self, line.as_deref()) {
                        return Ok(true);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "lost the connection to ardopcf",
                    ));
                }
            }
        }
    }

    fn wait_for(&mut self, what: &str, prefix: &str) -> io::Result<()> {
        let deadline = self.now_ms() + REPLY_TIMEOUT.as_millis() as u64;
        let mut fault = None;
        let answered = self.pump_until(deadline, |_, line| {
            let Some(line) = line else { return false };
            if line.to_ascii_uppercase().starts_with("FAULT") {
                fault = Some(line.to_string());
                return true;
            }
            line.to_ascii_uppercase()
                .starts_with(&prefix.to_ascii_uppercase())
        })?;
        match (answered, fault) {
            (_, Some(f)) => Err(io::Error::other(format!("ardopcf refused {what}: {f}"))),
            (true, None) => Ok(()),
            (false, None) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ardopcf didn't answer {what}"),
            )),
        }
    }

    fn command(&mut self, line: &str, reply: &str) -> io::Result<()> {
        self.cmd.write_all(format!("{line}\r").as_bytes())?;
        self.wait_for(line, reply)
    }

    /// Waits until ardopcf has room for another frame, so its buffer never overflows.
    fn wait_for_room(&mut self) -> io::Result<()> {
        let limit = PACE_FRAMES * self.mode.bytes;
        loop {
            let before = self.queued;
            let deadline = self.now_ms() + STALL_MS;
            let ok = self.pump_until(deadline, |l, _| {
                l.queued + l.mode.bytes <= limit || l.queued < before
            })?;
            if !ok {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ardopcf stopped taking data off its buffer",
                ));
            }
            if self.queued + self.mode.bytes <= limit {
                return Ok(());
            }
        }
    }
}

impl Drop for ArdopLink {
    fn drop(&mut self) {
        // ends the reader threads
        let _ = self.cmd.shutdown(Shutdown::Both);
        let _ = self.data.shutdown(Shutdown::Both);
    }
}

impl Link for ArdopLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        self.wait_for_room()?;
        let block = wrap(self.mode, frame);
        let mut out = (block.len() as u16).to_be_bytes().to_vec();
        self.got_buffer = false;
        out.extend_from_slice(&block);
        self.data.write_all(&out)?;
        // ardopcf reads commands before data, so FECSEND has to wait until it has
        // the data: an idle TNC refuses FECSEND on an empty buffer. While it's
        // sending, any BUFFER line will do, and FECSEND just adds to the transmission.
        let idle = !self.sending;
        let need = block.len();
        let deadline = self.now_ms() + REPLY_TIMEOUT.as_millis() as u64;
        let ok = self.pump_until(deadline, |l, _| (!idle || l.queued >= need) && l.got_buffer)?;
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ardopcf didn't take the data",
            ));
        }
        self.command("FECSEND TRUE", "FECSEND now")?;
        self.sending = true;
        Ok(())
    }

    fn sign_off(&mut self) -> io::Result<()> {
        // ardopcf only sends its ID after the first frame of a transmission, so
        // one more at the end leaves the delivery identified at its close (97.119)
        self.command("SENDID", "SENDID")?;
        let deadline = self.now_ms() + 10_000;
        let mut keyed = false;
        let ok = self.pump_until(deadline, |_, line| {
            match line {
                Some("PTT TRUE") => keyed = true,
                Some("PTT FALSE") => return keyed,
                _ => {}
            }
            false
        })?;
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ardopcf didn't send its ID",
            ));
        }
        Ok(())
    }

    fn recv_until(&mut self, deadline_ms: u64) -> io::Result<Option<Vec<u8>>> {
        self.pump_until(deadline_ms, |l, _| !l.inbox.is_empty())?;
        Ok(self.inbox.pop_front())
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Waits for ardopcf to finish sending everything, its closing ID frame included.
    fn tx_done_ms(&mut self) -> io::Result<u64> {
        loop {
            let before = self.queued;
            let deadline = self.now_ms() + STALL_MS;
            let ok = self.pump_until(deadline, |l, _| {
                (!l.sending && l.queued == 0) || l.queued < before
            })?;
            if !ok {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ardopcf never finished sending",
                ));
            }
            if !self.sending && self.queued == 0 {
                return Ok(self.now_ms());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_fill_exactly_one_ardop_frame() {
        let mode = mode("4fsk.500.100").unwrap();
        assert_eq!(mode.bytes, 64);
        let w = wrap(mode, b"hello");
        assert_eq!(w.len(), 64);
        assert_eq!(&w[..7], b"\x00\x05hello");
        assert_eq!(unwrap(&w), Some(&b"hello"[..]));
        // a short or damaged block can't claim more than it has
        assert_eq!(unwrap(&[0, 9, 1, 2]), None);
        assert_eq!(unwrap(&[0]), None);
    }

    #[test]
    fn every_mode_carries_a_report_and_a_symbol() {
        let report = crate::frame::Frame::Report {
            id: 1,
            round: 1,
            index: 0,
            need: Some(3),
        }
        .encode();
        assert!(report.len() <= frame_max(REPORT_MODE));
        for m in MODES {
            // symbol frames need room for the header and at least 4 bytes of symbol
            assert!(frame_max(*m) >= crate::frame::HEADER_LEN + 4, "{}", m.name);
        }
        assert!(mode("4FSK.200.50S").is_none());
    }

    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Just enough of ardopcf for the link: it answers commands the way the real
    /// one did over the virtual cable, cuts its buffer into frames by byte count,
    /// "sends" each after a short delay, and lets a test inject received blocks.
    struct FakeTnc {
        port: u16,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        most_queued: Arc<Mutex<usize>>,
        data_out: Arc<Mutex<Option<TcpStream>>>,
    }

    fn bind_pair() -> (TcpListener, TcpListener) {
        loop {
            let cmd = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = cmd.local_addr().unwrap().port();
            if let Ok(data) = TcpListener::bind(("127.0.0.1", port + 1)) {
                return (cmd, data);
            }
        }
    }

    impl FakeTnc {
        fn start(frame_bytes: usize, refuse: Option<&'static str>) -> FakeTnc {
            let (cmd_l, data_l) = bind_pair();
            let port = cmd_l.local_addr().unwrap().port();
            let sent = Arc::new(Mutex::new(vec![]));
            let most_queued = Arc::new(Mutex::new(0));
            let data_out = Arc::new(Mutex::new(None));
            let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
            let (s, mq, d_out, buf) = (
                sent.clone(),
                most_queued.clone(),
                data_out.clone(),
                buffer.clone(),
            );
            thread::spawn(move || {
                let (cmd, _) = cmd_l.accept().unwrap();
                let (data, _) = data_l.accept().unwrap();
                *d_out.lock().unwrap() = Some(data.try_clone().unwrap());
                let out = Arc::new(Mutex::new(cmd.try_clone().unwrap()));
                let say = {
                    let out = out.clone();
                    move |line: &str| {
                        let _ = out
                            .lock()
                            .unwrap()
                            .write_all(format!("{line}\r").as_bytes());
                    }
                };
                let say2 = say.clone();
                let (buf2, mq2) = (buf.clone(), mq.clone());
                thread::spawn(move || {
                    let mut data = data;
                    let mut len = [0u8; 2];
                    while data.read_exact(&mut len).is_ok() {
                        let mut block = vec![0u8; u16::from_be_bytes(len) as usize];
                        data.read_exact(&mut block).unwrap();
                        let mut b = buf2.lock().unwrap();
                        b.extend_from_slice(&block);
                        let mut most = mq2.lock().unwrap();
                        *most = (*most).max(b.len());
                        say2(&format!("BUFFER {}", b.len()));
                    }
                });
                let sending = Arc::new(Mutex::new(false));
                let mut reader = cmd;
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                while reader.read_exact(&mut byte).is_ok() {
                    if byte[0] != b'\r' {
                        line.push(byte[0]);
                        continue;
                    }
                    let text = String::from_utf8(std::mem::take(&mut line)).unwrap();
                    let word = text.split_whitespace().next().unwrap_or("").to_string();
                    let arg = text.split_whitespace().nth(1).unwrap_or("").to_string();
                    if Some(word.as_str()) == refuse {
                        say(&format!("FAULT {word}"));
                        continue;
                    }
                    match word.as_str() {
                        "INITIALIZE" => {
                            say("BUFFER 0");
                            say("INITIALIZE");
                        }
                        "FECSEND" => {
                            say("NEWSTATE FECSEND");
                            say("FECSEND now TRUE");
                            if *sending.lock().unwrap() {
                                continue;
                            }
                            *sending.lock().unwrap() = true;
                            let (buf, s, say, sending) =
                                (buf.clone(), s.clone(), say.clone(), sending.clone());
                            thread::spawn(move || {
                                loop {
                                    thread::sleep(Duration::from_millis(20));
                                    let mut b = buf.lock().unwrap();
                                    if b.is_empty() {
                                        *sending.lock().unwrap() = false;
                                        say("NEWSTATE DISC");
                                        return;
                                    }
                                    let n = frame_bytes.min(b.len());
                                    s.lock().unwrap().push(b.drain(..n).collect());
                                    say(&format!("BUFFER {}", b.len()));
                                }
                            });
                        }
                        _ => say(&format!("{word} now {arg}")),
                    }
                }
            });
            FakeTnc {
                port,
                sent,
                most_queued,
                data_out,
            }
        }

        fn deliver(&self, tag: &[u8; 3], data: &[u8]) {
            let mut block = ((data.len() + 3) as u16).to_be_bytes().to_vec();
            block.extend_from_slice(tag);
            block.extend_from_slice(data);
            let mut out = self.data_out.lock().unwrap();
            out.as_mut().unwrap().write_all(&block).unwrap();
        }
    }

    fn connect(tnc: &FakeTnc, mode: FecMode) -> io::Result<ArdopLink> {
        let call: Addr = "N0NCS".parse().unwrap();
        ArdopLink::connect(
            &format!("127.0.0.1:{}", tnc.port),
            &call,
            mode,
            Instant::now(),
        )
    }

    #[test]
    fn each_frame_goes_out_in_its_own_ardop_frame() {
        let mode = mode("4FSK.500.100").unwrap();
        let tnc = FakeTnc::start(mode.bytes, None);
        let mut link = connect(&tnc, mode).unwrap();
        let frames: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i; 10 + i as usize]).collect();
        for f in &frames {
            link.send(f).unwrap();
        }
        link.tx_done_ms().unwrap();
        let sent = tnc.sent.lock().unwrap();
        assert_eq!(sent.len(), frames.len());
        for (block, f) in sent.iter().zip(&frames) {
            assert_eq!(block.len(), mode.bytes);
            assert_eq!(unwrap(block), Some(&f[..]));
        }
        assert!(*tnc.most_queued.lock().unwrap() <= PACE_FRAMES * mode.bytes);
    }

    #[test]
    fn only_good_fec_frames_come_through() {
        let mode = REPORT_MODE;
        let tnc = FakeTnc::start(mode.bytes, None);
        let mut link = connect(&tnc, mode).unwrap();
        tnc.deliver(b"IDF", b" N0NCS:[EM00]");
        tnc.deliver(b"ERR", &wrap(mode, b"garbled"));
        tnc.deliver(b"FEC", &wrap(mode, b"report"));
        let deadline = link.now_ms() + 2000;
        assert_eq!(link.recv_until(deadline).unwrap(), Some(b"report".to_vec()));
        assert_eq!(link.recv_until(link.now_ms() + 200).unwrap(), None);
    }

    #[test]
    fn a_refused_command_is_a_clear_error() {
        let tnc = FakeTnc::start(64, Some("FECMODE"));
        let err = connect(&tnc, mode("4FSK.500.100").unwrap()).err().unwrap();
        assert!(err.to_string().contains("refused FECMODE"), "{err}");
    }
}
