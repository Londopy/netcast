mod ax25;
mod codec;
mod frame;
mod history;
mod kiss;
mod link;
mod message;
mod pat;
mod receiver;
mod sender;
mod sim;

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use rand::RngExt;

use ax25::Addr;
use frame::{Frame, Mode};
use kiss::KissLink;
use link::{Channel, Link};
use receiver::{Output, Receiver};
use sender::{Delivery, Plan, Progress, State};

// a full AX.25 info field
const AX25_FRAME_MAX: usize = 256;

#[derive(Parser)]
#[command(
    version,
    about = "Send a Winlink message once and have every station on the net decode it"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Receive deliveries and write them into Pat's inbox.
    Listen {
        /// Local callsigns to receive and acknowledge for (comma-separated).
        #[arg(long = "call", required = true, value_delimiter = ',')]
        calls: Vec<String>,
        /// Extra group names to accept, besides the net's own.
        #[arg(long = "group", value_delimiter = ',')]
        groups: Vec<String>,
        /// Pat's mailbox folder. Read from `pat env` if not given.
        #[arg(long)]
        mailbox: Option<PathBuf>,
        #[command(flatten)]
        radio: Radio,
    },
    /// Deliver a message to stations on the net.
    Send {
        /// A B2F message (.b2f), or a plain text file to send as the body.
        file: PathBuf,
        /// Recipient callsigns, or a group name for bulletins (comma-separated).
        #[arg(long, required = true, value_delimiter = ',')]
        to: Vec<String>,
        /// Your callsign.
        #[arg(long)]
        from: String,
        /// Subject, needed when the file is plain text.
        #[arg(long)]
        subject: Option<String>,
        #[arg(long, value_enum, default_value_t = Via::Direwolf)]
        via: Via,
        #[arg(long, value_enum, default_value_t = ModeArg::Addressed)]
        mode: ModeArg,
        /// Extra symbols in each full pass, as a fraction of the message
        /// [default: 0.1 addressed, 0.5 bulletin and emergency].
        #[arg(long)]
        margin: Option<f64>,
        /// Repair rounds, each followed by a status round.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=50))]
        rounds: u32,
        /// Ask for acknowledgements on an emergency message.
        #[arg(long)]
        ack: bool,
        /// Bulletin and emergency: send this many more full passes.
        #[arg(long, default_value_t = 0)]
        repeat: u32,
        /// Seconds between repeats.
        #[arg(long, default_value_t = 600)]
        interval: u64,
        /// Minutes receivers keep the delivery after they last hear it.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u16).range(1..=1440))]
        ttl: u16,
        #[command(flatten)]
        radio: Radio,
    },
    /// Show past deliveries, sent and received.
    Status {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Simulate one net and compare airtime with FLAMP-style and one-session-per-station delivery.
    Sim {
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=255))]
        receivers: u32,
        /// Loss per station: one rate, or LOW:HIGH spread evenly across stations.
        #[arg(long, default_value = "0.05:0.3")]
        loss: String,
        /// Message body size in bytes, before compression.
        #[arg(long, default_value_t = 4000)]
        bytes: usize,
        #[arg(long, default_value_t = 1200, value_parser = baud())]
        baud: u32,
        #[arg(long, default_value_t = 300)]
        txdelay: u64,
        #[arg(long, default_value_t = 0.1)]
        margin: f64,
        /// Repair rounds before netcast gives up; high by default so every method
        /// is compared at completion.
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(0..=50))]
        rounds: u32,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
        trials: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
}

#[derive(Args)]
struct Radio {
    /// The net's group address; every station on the net uses the same one.
    #[arg(long)]
    net: String,
    /// Direwolf's KISS TCP port.
    #[arg(long, default_value = "127.0.0.1:8001")]
    kiss: String,
    #[arg(long, default_value_t = 1200, value_parser = baud())]
    baud: u32,
    /// Must match TXDELAY in direwolf.conf, in milliseconds.
    #[arg(long, default_value_t = 300)]
    txdelay: u64,
    /// Slack added to each report slot, in milliseconds.
    #[arg(long, default_value_t = 500)]
    guard: u64,
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

fn baud() -> clap::builder::RangedI64ValueParser<u32> {
    clap::value_parser!(u32).range(300..=1_000_000)
}

#[derive(Clone, Copy, ValueEnum)]
enum Via {
    Direwolf,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Addressed,
    Bulletin,
    Emergency,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Mode {
        match m {
            ModeArg::Addressed => Mode::Addressed,
            ModeArg::Bulletin => Mode::Bulletin,
            ModeArg::Emergency => Mode::Emergency,
        }
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("netcast: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.cmd {
        Cmd::Listen {
            calls,
            groups,
            mailbox,
            radio,
        } => listen(calls, groups, mailbox, radio),
        Cmd::Send {
            file,
            to,
            from,
            subject,
            via: Via::Direwolf,
            mode,
            margin,
            rounds,
            ack,
            repeat,
            interval,
            ttl,
            radio,
        } => {
            let mode = Mode::from(mode);
            if ack && mode != Mode::Emergency {
                return Err("--ack only applies to emergency mode".into());
            }
            let acks = mode == Mode::Addressed || ack;
            let default_margin = if mode == Mode::Addressed { 0.1 } else { 0.5 };
            let opts = SendOpts {
                mode,
                ttl,
                margin: margin.unwrap_or(default_margin),
                rounds,
                acks,
                repeats: if acks { 0 } else { repeat },
                interval_s: interval,
            };
            send(&file, &to, &from, subject.as_deref(), &opts, radio)
        }
        Cmd::Status { state_dir } => {
            print!(
                "{}",
                history::report(&state_dir.unwrap_or_else(history::default_dir))?
            );
            Ok(())
        }
        Cmd::Sim {
            receivers,
            loss,
            bytes,
            baud,
            txdelay,
            margin,
            rounds,
            trials,
            seed,
        } => {
            if !(0.0..=10.0).contains(&margin) {
                return Err(format!("--margin {margin} is out of range (0 to 10)").into());
            }
            let losses = spread(&loss, receivers as usize)?;
            let cfg = sim::Config {
                losses,
                body_bytes: bytes,
                baud,
                txdelay_ms: txdelay,
                frame_max: AX25_FRAME_MAX,
                margin,
                rounds,
                guard_ms: 500,
                trials,
                seed,
            };
            print_sim(&cfg, &loss, &sim::simulate(&cfg));
            Ok(())
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn addrs(list: &[String]) -> Result<Vec<Addr>, String> {
    list.iter().map(|s| s.parse()).collect()
}

struct SendOpts {
    mode: Mode,
    ttl: u16,
    margin: f64,
    rounds: u32,
    acks: bool,
    repeats: u32,
    interval_s: u64,
}

fn load_message(
    file: &Path,
    from: &Addr,
    to: &[String],
    subject: Option<&str>,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let bytes = fs::read(file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
    let why = match message::check(&bytes) {
        Ok(_) => return Ok(bytes),
        Err(why) => why,
    };
    let Some(subject) = subject else {
        return Err(format!(
            "{} is not a usable B2F message ({why}); pass --subject to send it as a plain text body",
            file.display()
        )
        .into());
    };
    let body = String::from_utf8(bytes)
        .map_err(|_| format!("{} is neither B2F nor UTF-8 text", file.display()))?;
    Ok(message::compose(&from.to_string(), to, subject, &body, unix_now()).1)
}

fn send(
    file: &Path,
    to: &[String],
    from: &str,
    subject: Option<&str>,
    o: &SendOpts,
    radio: Radio,
) -> Result<(), Box<dyn Error>> {
    let from: Addr = from.parse()?;
    let net: Addr = radio.net.parse()?;
    let to: Vec<String> = addrs(to)?.iter().map(Addr::to_string).collect();
    if !(0.0..=10.0).contains(&o.margin) {
        return Err(format!("--margin {} is out of range (0 to 10)", o.margin).into());
    }
    let msg = load_message(file, &from, &to, subject)?;
    let id = rand::rng().random_range(1..=u32::MAX);
    let d = Delivery::new(id, &msg, to.clone(), o.mode, o.ttl, AX25_FRAME_MAX)?;

    let ch = Channel::new(radio.baud, radio.txdelay);
    let plan = Plan {
        margin: o.margin,
        rounds: o.rounds,
        acks: o.acks,
        repeats: o.repeats,
        repeat_gap_ms: o.interval_s * 1000,
        slot_ms: sender::slot_ms(&ch, radio.guard),
        guard_ms: radio.guard,
    };
    let mut link = KissLink::connect(&radio.kiss, from, net, ch, Instant::now())?;
    println!(
        "delivery {id:08x}: {} ({}), {} bytes compressed in {} symbols, to {}",
        d.announce.mid,
        o.mode.name(),
        d.announce.oti.transfer_length(),
        d.k(),
        to.join(", ")
    );
    let states = sender::run(&mut link, &d, &plan, |p| match p {
        Progress::Pass { symbols } => println!("queued the announcement and {symbols} symbols"),
        Progress::Status { round, states } => {
            let list: Vec<String> = to
                .iter()
                .zip(states)
                .map(|(c, s)| format!("{c} {s}"))
                .collect();
            println!("status round {round}: {}", list.join(", "));
        }
    })?;
    println!("transmission finished");

    let outcome: Vec<(String, String)> = if o.acks {
        to.iter()
            .cloned()
            .zip(states.iter().map(|s| s.to_string()))
            .collect()
    } else {
        to.iter().map(|c| (c.clone(), "sent".to_string())).collect()
    };
    let dir = radio.state_dir.unwrap_or_else(history::default_dir);
    history::record_sent(
        &dir,
        unix_now(),
        id,
        &d.announce.mid,
        o.mode.name(),
        &outcome,
    )?;
    let missing = states.iter().filter(|s| **s != State::Decoded).count();
    if o.acks && missing > 0 {
        return Err(format!("{missing} of {} stations did not confirm", to.len()).into());
    }
    Ok(())
}

fn listen(
    calls: Vec<String>,
    groups: Vec<String>,
    mailbox: Option<PathBuf>,
    radio: Radio,
) -> Result<(), Box<dyn Error>> {
    let calls = addrs(&calls)?;
    let net: Addr = radio.net.parse()?;
    let mut groups: Vec<String> = addrs(&groups)?.iter().map(Addr::to_string).collect();
    groups.push(net.to_string());
    let mailbox = match mailbox {
        Some(m) => m,
        None => pat::mailbox()?,
    };
    let dir = radio.state_dir.unwrap_or_else(history::default_dir);
    let names: Vec<String> = calls.iter().map(Addr::to_string).collect();
    let mut rx = Receiver::new(names.clone(), groups);
    let now_unix = unix_now();
    for r in history::unexpired(&dir, now_unix)? {
        rx.remember(r.id, r.acks, (r.expires_unix - now_unix) * 1000);
    }
    println!(
        "listening on {} as {}, writing to {}",
        net,
        names.join(", "),
        mailbox.display()
    );
    let mut station = Listener {
        rx,
        due: vec![],
        mailbox,
        dir,
    };
    // one clock for every connection, since the receiver keeps times across them
    let start = Instant::now();
    let mut backoff = RETRY_MIN;
    loop {
        let ch = Channel::new(radio.baud, radio.txdelay);
        match KissLink::connect(&radio.kiss, calls[0].clone(), net.clone(), ch, start) {
            Ok(mut link) => {
                backoff = RETRY_MIN;
                println!("connected to the TNC at {}", radio.kiss);
                if let Err(e) = station.serve(&mut link) {
                    eprintln!("lost the KISS connection: {e}");
                }
            }
            Err(e) => {
                eprintln!("{e}; trying again in {} s", backoff.as_secs());
                thread::sleep(backoff);
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
}

const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(60);

struct Listener {
    rx: Receiver,
    due: Vec<(u64, Frame)>,
    mailbox: PathBuf,
    dir: PathBuf,
}

impl Listener {
    /// Runs until the link fails.
    fn serve(&mut self, link: &mut KissLink) -> io::Result<()> {
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
                for out in self.rx.handle(f, now) {
                    self.on_output(out, now);
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
                        None => println!("delivery {id:08x}: reported decoded"),
                        Some(n) => println!("delivery {id:08x}: reported need {n}"),
                    }
                }
                link.send(&f.encode())?;
            }
            self.rx.expire(now);
        }
    }

    fn on_output(&mut self, out: Output, now: u64) {
        match out {
            Output::Announced { id, mode, mid, k } => {
                println!("delivery {id:08x}: {mid} ({}), {k} symbols", mode.name());
            }
            Output::Report { at_ms, frame } => self.due.push((at_ms, frame)),
            Output::Decoded(d) => {
                for call in &d.calls {
                    match pat::deliver(&self.mailbox, call, &d.mid, &d.b2f) {
                        Ok(Some(path)) => println!("decoded {}, wrote {}", d.mid, path.display()),
                        Ok(None) => println!("decoded {}, already in {call}'s inbox", d.mid),
                        Err(e) => {
                            // not stored means not decoded: receive it again rather than ack it
                            eprintln!("can't write {} into {call}'s inbox: {e}", d.mid);
                            self.rx.forget(d.id);
                            return;
                        }
                    }
                }
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
                    eprintln!(
                        "can't write the received log in {}: {e}",
                        self.dir.display()
                    );
                }
            }
            Output::Failed { id, why } => println!("delivery {id:08x}: {why}"),
        }
    }
}
fn spread(loss: &str, n: usize) -> Result<Vec<f64>, String> {
    let bad =
        || format!("--loss {loss:?}: give a rate up to 0.9 like 0.2, or a range like 0.05:0.3");
    let parse = |s: &str| {
        s.parse::<f64>()
            .ok()
            .filter(|v| (0.0..=0.9).contains(v))
            .ok_or_else(bad)
    };
    if n == 0 {
        return Err("--receivers must be at least 1".into());
    }
    let (lo, hi) = match loss.split_once(':') {
        Some((a, b)) => (parse(a)?, parse(b)?),
        None => (parse(loss)?, parse(loss)?),
    };
    Ok((0..n)
        .map(|i| {
            if n == 1 {
                lo
            } else {
                lo + (hi - lo) * i as f64 / (n - 1) as f64
            }
        })
        .collect())
}

fn print_sim(cfg: &sim::Config, loss: &str, out: &sim::Outcome) {
    let n = cfg.losses.len();
    println!(
        "{n} receivers, loss {loss}, {}-byte body ({} bytes compressed, {} symbols), {} baud, txdelay {} ms, {} trials",
        cfg.body_bytes, out.compressed, out.k, cfg.baud, cfg.txdelay_ms, cfg.trials
    );
    println!();
    println!("{:<28}{:>10}{:>10}", "method", "time", "complete");
    let rows = [
        ("netcast".to_string(), &out.netcast),
        ("FLAMP-style hole lists".to_string(), &out.flamp),
        (format!("{n} separate sessions"), &out.sessions),
    ];
    for (name, s) in rows {
        println!(
            "{:<28}{:>9.0}s{:>10.1}",
            name,
            s.channel_ms / 1000.0,
            s.complete
        );
    }
}
