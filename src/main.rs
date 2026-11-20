mod ardop;
mod ax25;
mod codec;
mod frame;
mod history;
mod keys;
mod kiss;
mod link;
mod londopy;
mod message;
mod pat;
mod receiver;
mod roster;
mod sender;
mod sim;
mod station;
mod ui;
mod upgrade;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use ardop::FecMode;
use ax25::Addr;
use frame::{Mode, Precedence};
use roster::Roster;
use sender::{Progress, State};
use station::{AX25_FRAME_MAX, Inbox, Outgoing, Radio, Station, Transport, unix_now};

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
        #[command(flatten)]
        inbox: InboxArgs,
        #[command(flatten)]
        radio: RadioArgs,
    },
    /// Deliver a message to stations on the net.
    Send {
        /// A B2F message (.b2f), or a plain text file to send as the body.
        file: PathBuf,
        /// Recipient callsigns, or a group name for bulletins (comma-separated).
        /// With --roster and no --to, every member.
        #[arg(long, value_delimiter = ',')]
        to: Vec<String>,
        /// Your callsign.
        #[arg(long)]
        from: String,
        /// Subject, needed when the file is plain text.
        #[arg(long)]
        subject: Option<String>,
        #[arg(long, value_enum, default_value_t = ModeArg::Addressed)]
        mode: ModeArg,
        /// How urgent the message is, as on a radiogram [default: emergency in
        /// emergency mode, otherwise routine].
        #[arg(long, value_enum)]
        precedence: Option<PrecedenceArg>,
        /// Extra symbols in each full pass, as a fraction of the message
        /// [default: 0.1 with acknowledgements, 0.5 without].
        #[arg(long)]
        margin: Option<f64>,
        /// Repair rounds, each followed by a status round.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=50))]
        rounds: u32,
        /// Ask for acknowledgements on an emergency message.
        #[arg(long)]
        ack: bool,
        /// Bulletin and emergency: send this many more full passes.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=10))]
        repeat: u32,
        /// Seconds between repeats.
        #[arg(long, default_value_t = 600)]
        interval: u64,
        /// Minutes receivers keep the delivery after they last hear it.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u16).range(1..=1440))]
        ttl: u16,
        /// Sign the announcement with this station's key (see `netcast key`).
        #[arg(long)]
        sign: bool,
        /// The net roster; recipients must be on it.
        #[arg(long)]
        roster: Option<PathBuf>,
        #[command(flatten)]
        radio: RadioArgs,
    },
    /// Run the listener and a web page for sending and watching deliveries.
    Ui {
        #[command(flatten)]
        inbox: InboxArgs,
        /// A folder of .b2f messages the page can send.
        #[arg(long)]
        outbox: Option<PathBuf>,
        /// Port for the page, on 127.0.0.1.
        #[arg(long, default_value_t = 8740, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[command(flatten)]
        radio: RadioArgs,
    },
    /// Check this station is ready for a net: its folder, key, roster, Pat mailbox and TNC.
    Check {
        /// This station's callsign.
        #[arg(long)]
        call: String,
        /// The net roster, if there is one.
        #[arg(long)]
        roster: Option<PathBuf>,
        /// Pat's mailbox folder. Read from `pat env` if not given.
        #[arg(long)]
        mailbox: Option<PathBuf>,
        #[command(flatten)]
        radio: RadioArgs,
    },
    /// Make this station's signing key, or show its public half.
    Key {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Create and edit a net roster (.londopy).
    Roster {
        #[command(subcommand)]
        cmd: RosterCmd,
    },
    /// Install the newest netcast release, after checking it against the release's checksums.
    Upgrade {
        /// Only say whether there's a newer release.
        #[arg(long)]
        check: bool,
    },
    /// Show past deliveries, sent and received.
    Status {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Simulate one net and compare channel time with FLAMP-style and one-session-per-station delivery.
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

#[derive(Subcommand)]
enum RosterCmd {
    /// Start a roster for a net.
    New {
        file: PathBuf,
        /// The net's group address.
        #[arg(long)]
        net: String,
    },
    /// Add a member, or a group name with --group. Adding a member again replaces its key.
    Add {
        file: PathBuf,
        name: String,
        /// The member's public key, as `netcast key` prints it.
        #[arg(long, conflicts_with = "group")]
        key: Option<String>,
        #[arg(long)]
        group: bool,
    },
    /// Take a member or group off the roster.
    Remove { file: PathBuf, name: String },
    /// Print what's on the roster.
    Show { file: PathBuf },
}

#[derive(Args)]
struct InboxArgs {
    /// Local callsigns to receive and acknowledge for (comma-separated).
    #[arg(long = "call", required = true, value_delimiter = ',')]
    calls: Vec<String>,
    /// Extra group names to accept, besides the net's own and the roster's.
    #[arg(long = "group", value_delimiter = ',')]
    groups: Vec<String>,
    /// Pat's mailbox folder. Read from `pat env` if not given.
    #[arg(long)]
    mailbox: Option<PathBuf>,
    /// The net roster: its groups are accepted and its keys checked.
    #[arg(long)]
    roster: Option<PathBuf>,
    /// Only take messages signed by a key on the roster.
    #[arg(long, requires = "roster")]
    require_signed: bool,
}

#[derive(Args)]
struct RadioArgs {
    /// The net's group address; every station on the net uses the same one.
    /// Taken from the roster if not given.
    #[arg(long)]
    net: Option<String>,
    /// Which TNC carries the net.
    #[arg(long, value_enum, default_value_t = Via::Direwolf)]
    via: Via,
    /// Direwolf's KISS TCP port.
    #[arg(long, default_value = "127.0.0.1:8001")]
    kiss: String,
    #[arg(long, default_value_t = 1200, value_parser = baud())]
    baud: u32,
    /// Must match TXDELAY in direwolf.conf, in milliseconds.
    #[arg(long, default_value_t = 300)]
    txdelay: u64,
    /// ardopcf's command port; its data port is the next one up.
    #[arg(long, default_value = "127.0.0.1:8515")]
    ardop: String,
    /// The ardopcf FEC frame type messages go out in.
    #[arg(long, default_value = "4FSK.500.100", value_parser = fec_mode)]
    fecmode: FecMode,
    /// Slack added to each report slot, in milliseconds.
    #[arg(long, default_value_t = 500)]
    guard: u64,
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

fn baud() -> clap::builder::RangedI64ValueParser<u32> {
    clap::value_parser!(u32).range(300..=1_000_000)
}

fn fec_mode(s: &str) -> Result<FecMode, String> {
    ardop::mode(s).ok_or_else(|| {
        let names: Vec<&str> = ardop::MODES.iter().map(|m| m.name).collect();
        format!(
            "not an FEC mode netcast can use; pick one of {}",
            names.join(", ")
        )
    })
}

#[derive(Clone, Copy, ValueEnum)]
enum Via {
    Direwolf,
    Ardop,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Addressed,
    Bulletin,
    Emergency,
}

#[derive(Clone, Copy, ValueEnum)]
enum PrecedenceArg {
    Routine,
    Welfare,
    Priority,
    Emergency,
}

impl From<PrecedenceArg> for Precedence {
    fn from(p: PrecedenceArg) -> Precedence {
        match p {
            PrecedenceArg::Routine => Precedence::Routine,
            PrecedenceArg::Welfare => Precedence::Welfare,
            PrecedenceArg::Priority => Precedence::Priority,
            PrecedenceArg::Emergency => Precedence::Emergency,
        }
    }
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
    match run(Cli::parse()) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("netcast: {e:#}");
            std::process::exit(1);
        }
    }
}

/// False when the command ran but didn't fully succeed, like a station that
/// never confirmed.
fn run(cli: Cli) -> Result<bool> {
    upgrade::clean_up();
    // the long-running commands, where the background check has time to finish
    if matches!(
        cli.cmd,
        Cmd::Listen { .. } | Cmd::Send { .. } | Cmd::Ui { .. }
    ) {
        upgrade::notice(&history::default_dir(), unix_now());
    }
    match cli.cmd {
        Cmd::Upgrade { check } => {
            upgrade::run(check)?;
            Ok(true)
        }
        Cmd::Listen { inbox, radio } => {
            let roster = load_roster(inbox.roster.as_deref())?;
            let call = first_call(&inbox)?;
            let st = station(call, radio, roster.as_ref())?;
            let inbox = inbox_for(inbox, &st, roster.as_ref())?;
            st.listen(inbox, None)?;
            Ok(true)
        }
        Cmd::Send {
            file,
            to,
            from,
            subject,
            mode,
            precedence,
            margin,
            rounds,
            ack,
            repeat,
            interval,
            ttl,
            sign,
            roster,
            radio,
        } => {
            let roster = load_roster(roster.as_deref())?;
            let from: Addr = from.parse().map_err(anyhow::Error::msg)?;
            let st = station(from.clone(), radio, roster.as_ref())?;
            let to = station::recipients(&to, roster.as_ref(), &from)?;
            let b2f = load_message(&file, &from, &to, subject.as_deref())?;
            let mut out = Outgoing::new(b2f, to.clone(), Mode::from(mode), ack)?;
            if let Some(p) = precedence {
                out.precedence = p.into();
            }
            out.margin = margin.unwrap_or(out.margin);
            out.rounds = rounds;
            out.ttl_min = ttl;
            out.repeats = repeat;
            out.interval_s = interval;
            out.sign = sign;
            let acks = out.acks;
            let states = st.send(out, print_start, |p| print_progress(&to, p))?;
            println!("transmission finished");
            let missing = states.iter().filter(|s| **s != State::Decoded).count();
            if acks && missing > 0 {
                eprintln!(
                    "netcast: {missing} of {} stations did not confirm",
                    to.len()
                );
                return Ok(false);
            }
            Ok(true)
        }
        Cmd::Ui {
            inbox,
            outbox,
            port,
            radio,
        } => {
            let roster = load_roster(inbox.roster.as_deref())?;
            let call = first_call(&inbox)?;
            let st = station(call, radio, roster.as_ref())?;
            let inbox = inbox_for(inbox, &st, roster.as_ref())?;
            ui::Ui::new(st, roster, outbox, port).run(inbox)?;
            Ok(true)
        }
        Cmd::Check {
            call,
            roster,
            mailbox,
            radio,
        } => Ok(check(&call, roster.as_deref(), mailbox, radio)),
        Cmd::Key { state_dir } => {
            let dir = state_dir.unwrap_or_else(history::default_dir);
            let existed = keys::load(&dir)?.is_some();
            let key = keys::load_or_create(&dir)?;
            let public = keys::hex(key.verifying_key().as_bytes());
            if !existed {
                println!("made a new key in {}", keys::key_path(&dir).display());
            }
            println!("public key: {public}");
            println!(
                "add it to the net roster with: netcast roster add <roster> <your call> --key {public}"
            );
            Ok(true)
        }
        Cmd::Roster { cmd } => {
            roster_cmd(cmd)?;
            Ok(true)
        }
        Cmd::Status { state_dir } => {
            print!(
                "{}",
                history::report(&state_dir.unwrap_or_else(history::default_dir))?
            );
            Ok(true)
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
                bail!("--margin {margin} is out of range (0 to 10)");
            }
            let cfg = sim::Config {
                losses: spread(&loss, receivers as usize)?,
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
            Ok(true)
        }
    }
}

/// `netcast check`: one line per thing a net needs, true when nothing is wrong.
fn check(call: &str, roster: Option<&Path>, mailbox: Option<PathBuf>, radio: RadioArgs) -> bool {
    let mut ok = true;
    let mut line = |good: Option<bool>, text: String| {
        let tag = match good {
            Some(true) => "ok",
            Some(false) => "problem",
            None => "note",
        };
        ok &= good != Some(false);
        println!("{tag:<8} {text}");
    };
    let call: Addr = match call.parse() {
        Ok(a) => a,
        Err(e) => {
            line(Some(false), e);
            return false;
        }
    };
    let dir = radio.state_dir.clone().unwrap_or_else(history::default_dir);
    let probe = dir.join(".netcast-check");
    let writable = fs::create_dir_all(&dir)
        .and_then(|_| fs::write(&probe, b""))
        .and_then(|_| fs::remove_file(&probe));
    match writable {
        Ok(()) => line(Some(true), format!("state folder {}", dir.display())),
        Err(e) => line(
            Some(false),
            format!("can't write to the state folder {}: {e}", dir.display()),
        ),
    }
    match keys::load(&dir) {
        Ok(Some(k)) => line(
            Some(true),
            format!(
                "station key, public half {}",
                keys::hex(k.verifying_key().as_bytes())
            ),
        ),
        Ok(None) => line(
            None,
            "no station key; run `netcast key` before signing anything".into(),
        ),
        Err(e) => line(Some(false), format!("{e:#}")),
    }
    let mut roster_failed = false;
    let roster = match roster.map(Roster::load).transpose() {
        Ok(Some(r)) => {
            let with_keys = r.members.iter().filter(|m| m.key.is_some()).count();
            let listed = r.members.iter().any(|m| m.call == call.to_string());
            line(
                Some(true),
                format!(
                    "roster for {}: {} members ({with_keys} with keys), {} groups",
                    r.net,
                    r.members.len(),
                    r.groups.len()
                ),
            );
            if !listed {
                line(None, format!("{call} isn't a member of this roster"));
            }
            Some(r)
        }
        Ok(None) => {
            line(None, "no roster given".into());
            None
        }
        Err(e) => {
            line(Some(false), format!("{e:#}"));
            roster_failed = true;
            None
        }
    };
    let mailbox = match mailbox {
        Some(m) => Ok(m),
        None => pat::mailbox(),
    };
    match mailbox {
        Ok(m) if m.is_dir() => line(Some(true), format!("Pat mailbox {}", m.display())),
        Ok(m) => line(
            Some(false),
            format!(
                "Pat's mailbox {} doesn't exist yet; run Pat once first",
                m.display()
            ),
        ),
        Err(e) => line(Some(false), format!("{e}")),
    }
    match station(call, radio, roster.as_ref()) {
        Ok(st) => match st.try_tnc() {
            Ok(Some(tnc)) => line(Some(true), format!("reached {tnc} for net {}", st.net)),
            Ok(None) => line(
                None,
                "ardopcf not tried: it takes one program at a time, and connecting would cut off a running netcast".into(),
            ),
            Err(e) => line(Some(false), format!("{e}")),
        },
        // a roster or key problem above already explains this one
        Err(_) if roster_failed => {}
        Err(e) => line(Some(false), format!("{e:#}")),
    }
    ok
}

fn load_roster(path: Option<&Path>) -> Result<Option<Roster>> {
    path.map(Roster::load).transpose()
}

fn first_call(inbox: &InboxArgs) -> Result<Addr> {
    let first = inbox
        .calls
        .first()
        .ok_or_else(|| anyhow!("give at least one --call"))?;
    first.parse().map_err(anyhow::Error::msg)
}

fn station(call: Addr, radio: RadioArgs, roster: Option<&Roster>) -> Result<Station> {
    let net = match (&radio.net, roster) {
        (Some(n), _) => n.parse().map_err(anyhow::Error::msg)?,
        (None, Some(r)) => r.net.parse().map_err(anyhow::Error::msg)?,
        (None, None) => bail!("give the net's address with --net, or a --roster"),
    };
    let dir = radio.state_dir.unwrap_or_else(history::default_dir);
    // only signing needs the key, so a damaged key file mustn't stop anything else
    let key = keys::load(&dir).unwrap_or_else(|e| {
        eprintln!("netcast: not using the station key: {e:#}");
        None
    });
    let transport = match radio.via {
        Via::Direwolf => Transport::Kiss {
            addr: radio.kiss,
            baud: radio.baud,
            txdelay_ms: radio.txdelay,
        },
        Via::Ardop => Transport::Ardop {
            addr: radio.ardop,
            mode: radio.fecmode,
        },
    };
    let radio = Radio {
        transport,
        guard_ms: radio.guard,
    };
    Ok(Station::new(call, net, radio, dir, key))
}

fn inbox_for(args: InboxArgs, st: &Station, roster: Option<&Roster>) -> Result<Inbox> {
    let calls = args
        .calls
        .iter()
        .map(|c| c.parse())
        .collect::<Result<Vec<Addr>, String>>()
        .map_err(anyhow::Error::msg)?;
    let mut groups = vec![st.net.to_string()];
    for g in &args.groups {
        let a: Addr = g.parse().map_err(anyhow::Error::msg)?;
        groups.push(a.to_string());
    }
    if let Some(r) = roster {
        groups.extend(r.groups.iter().cloned());
    }
    let keys = roster.map(Roster::keys).unwrap_or_default();
    if args.require_signed && keys.is_empty() {
        bail!("--require-signed needs a roster with members' keys in it");
    }
    let mailbox = match args.mailbox {
        Some(m) => m,
        None => pat::mailbox()?,
    };
    Ok(Inbox {
        calls,
        groups,
        mailbox,
        keys,
        require_signed: args.require_signed,
    })
}

fn load_message(file: &Path, from: &Addr, to: &[String], subject: Option<&str>) -> Result<Vec<u8>> {
    let bytes = fs::read(file).with_context(|| format!("can't read {}", file.display()))?;
    let why = match message::check(&bytes) {
        Ok(_) => return Ok(bytes),
        Err(why) => why,
    };
    let Some(subject) = subject else {
        bail!(
            "{} is not a usable B2F message ({why}); pass --subject to send it as a plain text body",
            file.display()
        );
    };
    let body = String::from_utf8(bytes)
        .map_err(|_| anyhow!("{} is neither B2F nor UTF-8 text", file.display()))?;
    Ok(message::compose(&from.to_string(), to, subject, &body, unix_now()).1)
}

fn print_start(d: &sender::Delivery) {
    let signed = if d.announce.signature.is_some() {
        ", signed"
    } else {
        ""
    };
    println!(
        "delivery {:08x}: {} ({}, {}{signed}), {} bytes compressed in {} symbols, to {}",
        d.id,
        d.announce.mid,
        d.announce.mode.name(),
        d.announce.precedence.name(),
        d.announce.oti.transfer_length(),
        d.k(),
        d.announce.recipients.join(", ")
    );
}

fn print_progress(to: &[String], p: Progress) {
    match p {
        Progress::Pass { symbols } => println!("queued the announcement and {symbols} symbols"),
        Progress::Status { round, states } => {
            let list: Vec<String> = to
                .iter()
                .zip(states)
                .map(|(c, s)| format!("{c} {s}"))
                .collect();
            println!("status round {round}: {}", list.join(", "));
        }
    }
}

fn roster_cmd(cmd: RosterCmd) -> Result<()> {
    let addr = |s: &str| -> Result<Addr> { s.parse().map_err(anyhow::Error::msg) };
    match cmd {
        RosterCmd::New { file, net } => {
            if file.exists() {
                bail!("{} already exists", file.display());
            }
            Roster::new(&addr(&net)?).save(&file)?;
            println!("made {}", file.display());
        }
        RosterCmd::Add {
            file,
            name,
            key,
            group,
        } => {
            let mut r = Roster::load(&file)?;
            let name = addr(&name)?;
            if group {
                r.add_group(&name)?;
            } else {
                let key = key.as_deref().map(keys::parse_public).transpose()?;
                r.add_member(&name, key.as_ref())?;
            }
            r.save(&file)?;
            println!("added {name} to {}", file.display());
        }
        RosterCmd::Remove { file, name } => {
            let mut r = Roster::load(&file)?;
            let name = addr(&name)?;
            if !r.remove(&name) {
                bail!("{name} isn't on {}", file.display());
            }
            r.save(&file)?;
            println!("removed {name} from {}", file.display());
        }
        RosterCmd::Show { file } => {
            let r = Roster::load(&file)?;
            println!("net {}", r.net);
            if !r.groups.is_empty() {
                println!("groups {}", r.groups.join(", "));
            }
            for m in &r.members {
                match &m.key {
                    Some(k) => println!("  {:<9}  key {}", m.call, k),
                    None => println!("  {:<9}  no key", m.call),
                }
            }
            if r.members.is_empty() {
                println!("  no members yet");
            }
        }
    }
    Ok(())
}

fn spread(loss: &str, n: usize) -> Result<Vec<f64>> {
    let bad =
        || anyhow!("--loss {loss:?}: give a rate up to 0.9 like 0.2, or a range like 0.05:0.3");
    let parse = |s: &str| {
        s.parse::<f64>()
            .ok()
            .filter(|v| (0.0..=0.9).contains(v))
            .ok_or_else(bad)
    };
    if n == 0 {
        bail!("--receivers must be at least 1");
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
