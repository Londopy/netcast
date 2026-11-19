use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::ax25::{self, Addr};
use crate::link::{Channel, Link};

const FEND: u8 = 0xC0;
const FESC: u8 = 0xDB;
const TFEND: u8 = 0xDC;
const TFESC: u8 = 0xDD;
// data frame on TNC port 0
const CMD_DATA: u8 = 0x00;
const MAX_FRAME: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
// frames we let queue up in the TNC ahead of the air
const PACE_FRAMES: u64 = 40;

pub fn wrap(data: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, CMD_DATA];
    for &b in data {
        match b {
            FEND => out.extend_from_slice(&[FESC, TFEND]),
            FESC => out.extend_from_slice(&[FESC, TFESC]),
            _ => out.push(b),
        }
    }
    out.push(FEND);
    out
}

/// Pulls KISS data frames out of a byte stream that can split anywhere.
#[derive(Default)]
pub struct Unwrapper {
    buf: Vec<u8>,
    esc: bool,
}

impl Unwrapper {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = vec![];
        for &b in bytes {
            match (self.esc, b) {
                (_, FEND) => {
                    let f = std::mem::take(&mut self.buf);
                    if f.len() > 1 && f[0] == CMD_DATA {
                        frames.push(f[1..].to_vec());
                    }
                    self.esc = false;
                }
                (false, FESC) => self.esc = true,
                (true, TFEND) => self.push_byte(FEND),
                (true, TFESC) => self.push_byte(FESC),
                (true, other) => self.push_byte(other),
                (false, other) => self.push_byte(other),
            }
        }
        frames
    }

    fn push_byte(&mut self, b: u8) {
        self.esc = false;
        // a frame this long is garbage, so drop it rather than grow forever
        if self.buf.len() < MAX_FRAME {
            self.buf.push(b);
        }
    }
}

/// Netcast over a KISS TCP port, as UI frames from `me` to the net's group address.
pub struct KissLink {
    stream: TcpStream,
    unwrap: Unwrapper,
    inbox: VecDeque<Vec<u8>>,
    me: Addr,
    net: Addr,
    channel: Channel,
    start: Instant,
}

impl KissLink {
    /// `start` is the zero of this link's clock; pass the same one when reconnecting
    /// so times kept elsewhere stay valid.
    pub fn connect(
        addr: &str,
        me: Addr,
        net: Addr,
        channel: Channel,
        start: Instant,
    ) -> io::Result<KissLink> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} did not resolve"));
        for target in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&target, TIMEOUT) {
                Ok(stream) => {
                    stream.set_write_timeout(Some(TIMEOUT))?;
                    return Ok(KissLink {
                        stream,
                        unwrap: Unwrapper::default(),
                        inbox: VecDeque::new(),
                        me,
                        net,
                        channel,
                        start,
                    });
                }
                Err(e) => last = e,
            }
        }
        Err(io::Error::new(
            last.kind(),
            format!("can't reach the KISS port at {addr}: {last}"),
        ))
    }

    fn take(&mut self, bytes: &[u8]) {
        for f in self.unwrap.push(bytes) {
            if let Some((dest, _, info)) = ax25::parse_ui(&f)
                && dest == self.net
            {
                self.inbox.push_back(info.to_vec());
            }
        }
    }

    /// Reads whatever arrives until `until_ms`, or until a frame is waiting.
    fn pump(&mut self, until_ms: u64) -> io::Result<()> {
        let mut buf = [0u8; 2048];
        while self.inbox.is_empty() {
            let now = self.now_ms();
            if now >= until_ms {
                break;
            }
            self.stream
                .set_read_timeout(Some(Duration::from_millis(until_ms - now)))?;
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the TNC closed the KISS connection",
                    ));
                }
                Ok(n) => self.take(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Link for KissLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        // Direwolf throws away UI frames once 100 are queued, so never get near that
        let limit = PACE_FRAMES * self.channel.frame_ms(frame.len());
        loop {
            let now = self.now_ms();
            let backlog = self.channel.busy_until().saturating_sub(now);
            if backlog <= limit {
                break;
            }
            self.pump(now + backlog - limit)?;
            if !self.inbox.is_empty() {
                // keep anything heard meanwhile for recv_until, and wait on the clock
                std::thread::sleep(Duration::from_millis(backlog - limit));
            }
        }
        self.stream
            .write_all(&wrap(&ax25::ui(&self.net, &self.me, frame)))?;
        let now = self.now_ms();
        self.channel.transmit(now, frame.len());
        Ok(())
    }

    fn recv_until(&mut self, deadline_ms: u64) -> io::Result<Option<Vec<u8>>> {
        self.pump(deadline_ms)?;
        Ok(self.inbox.pop_front())
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn tx_done_ms(&self) -> u64 {
        self.channel.busy_until()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn escapes_and_unescapes() {
        let data = vec![1, FEND, 2, FESC, 3];
        let wrapped = wrap(&data);
        assert_eq!(
            wrapped,
            vec![FEND, 0, 1, FESC, TFEND, 2, FESC, TFESC, 3, FEND]
        );
        let mut u = Unwrapper::default();
        // fed one byte at a time, as a TCP stream may split it
        let mut got = vec![];
        for b in &wrapped {
            got.extend(u.push(&[*b]));
        }
        assert_eq!(got, vec![data]);
    }

    #[test]
    fn skips_non_data_kiss_frames() {
        let mut u = Unwrapper::default();
        let mut stream = vec![FEND, 0x01, 50, FEND];
        stream.extend(wrap(b"ok"));
        assert_eq!(u.push(&stream), vec![b"ok".to_vec()]);
    }

    #[test]
    fn talks_to_a_kiss_tnc_over_tcp() {
        let tnc = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tnc.local_addr().unwrap().port();
        let net: Addr = "NET1".parse().unwrap();
        let other: Addr = "W2XYZ".parse().unwrap();
        let reply = [
            wrap(&ax25::ui(
                &"QST".parse().unwrap(),
                &other,
                b"not for this net",
            )),
            wrap(&ax25::ui(&net, &other, b"report")),
        ]
        .concat();
        let fake = thread::spawn(move || {
            let (mut s, _) = tnc.accept().unwrap();
            let mut got = vec![0u8; 512];
            let n = s.read(&mut got).unwrap();
            // send the reply in two pieces to exercise reassembly
            s.write_all(&reply[..5]).unwrap();
            s.flush().unwrap();
            thread::sleep(Duration::from_millis(50));
            s.write_all(&reply[5..]).unwrap();
            got.truncate(n);
            got
        });

        let me: Addr = "K1ABC".parse().unwrap();
        let mut link = KissLink::connect(
            &format!("127.0.0.1:{port}"),
            me.clone(),
            net.clone(),
            Channel::new(1200, 300),
            Instant::now(),
        )
        .unwrap();
        link.send(b"hello").unwrap();
        assert!(link.tx_done_ms() >= 300);
        let deadline = link.now_ms() + 2000;
        assert_eq!(link.recv_until(deadline).unwrap(), Some(b"report".to_vec()));

        let sent = fake.join().unwrap();
        let mut u = Unwrapper::default();
        let frames = u.push(&sent);
        assert_eq!(ax25::parse_ui(&frames[0]), Some((net, me, &b"hello"[..])));
    }

    #[test]
    fn keeps_the_tnc_queue_short() {
        let tnc = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tnc.local_addr().unwrap().port();
        let sink = thread::spawn(move || {
            let (mut s, _) = tnc.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            while s.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
        });
        let ch = Channel::new(96_000, 0);
        let frame_ms = ch.frame_ms(256);
        let mut link = KissLink::connect(
            &format!("127.0.0.1:{port}"),
            "K1ABC".parse().unwrap(),
            "NET1".parse().unwrap(),
            ch,
            Instant::now(),
        )
        .unwrap();
        for _ in 0..60 {
            link.send(&[0u8; 256]).unwrap();
            let queued = link.tx_done_ms().saturating_sub(link.now_ms());
            assert!(queued <= (PACE_FRAMES + 1) * frame_ms, "{queued} ms queued");
        }
        // 60 frames can't all have gone straight into the queue
        assert!(link.now_ms() >= (60 - PACE_FRAMES - 1) * frame_ms);
        drop(link);
        sink.join().unwrap();
    }
}
