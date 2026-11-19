# netcast

[![CI](https://github.com/Londopy/netcast/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/netcast/actions/workflows/ci.yml)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/netcast/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/netcast)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)

**Send a bulletin to every station on the net in one go, instead of once per station.**

Net control sends a Winlink message once over packet radio, and every addressed station decodes
it straight into its Pat inbox. The message is fountain coded (RaptorQ), so stations that miss
frames don't need those exact frames again. One round of fresh repair symbols, sized for the
station that missed the most, fills everyone's gaps at once.

```text
> netcast send sitrep.txt --subject "Sitrep" --to K1ABC,W2XYZ,N3OOO --from N0NCS --net NET1 --baud 9600 --txdelay 50
delivery 459d1845: J7CNWF489H0L (addressed), 12849 bytes compressed in 53 symbols, to K1ABC, W2XYZ, N3OOO
queued the announcement and 59 symbols
status round 1: K1ABC silent, W2XYZ need 15, N3OOO silent
queued the announcement and 17 symbols
status round 2: K1ABC decoded, W2XYZ decoded, N3OOO silent
queued the announcement and 0 symbols
status round 3: K1ABC decoded, W2XYZ decoded, N3OOO silent
queued the announcement and 0 symbols
status round 4: K1ABC decoded, W2XYZ decoded, N3OOO silent
transmission finished
netcast: 1 of 3 stations did not confirm
```

That run went through a test relay that dropped 20% of frames. W2XYZ was 15 symbols short
after the first pass, and the 17 fresh ones that followed were enough. K1ABC had decoded from
the first pass, but its answer to round 1 was lost. N3OOO was never on the air.

## Why

Winlink is point to point, so a bulletin for 20 stations means 20 sessions or 20 pickups.
FLAMP sends once, but every station reports its own missing blocks and the sender resends the
union of all of them, which grows with every station on the net. With a fountain code, any K
symbols rebuild the message, whichever ones they are. The sender only needs to know the largest
number any one station is short.

`netcast sim` models a net and compares the channel time for the three approaches: report
slots, turnarounds and keying up are all included. Twenty stations, losing between 5% and 30%
of frames each, at 1200 baud with 300 ms TXDELAY:

| Message | netcast | FLAMP-style hole lists | 20 separate sessions |
|---|---|---|---|
| 4 kB body (2.6 kB compressed, 11 symbols) | 92 s | 96 s | 767 s |
| 20 kB body (12 kB compressed, 51 symbols) | 215 s | 345 s | 3243 s |
| 4 kB body, every station at 40% loss | 165 s | 189 s | 1382 s |

Every method got the message to all 20 stations in every trial. On short messages the status
slots cost about as much as the data, so netcast and FLAMP come out close. The gap opens as
messages get longer or the channel gets worse. The baselines are generous: the FLAMP model only
gives slots to stations that are still missing blocks, and the session model leaves out retry
timers. Run `netcast sim --help` to try other nets.

## Features

- One transmission of the message for any number of stations, plus repair sized for the worst
  receiver
- Net control sees each station's state: decoded, how many symbols it still needs, or silent
- Stations that tune in late keep what they hear and decode once the next announcement arrives
- Decoded messages land in Pat's inbox as ordinary `.b2f` files, so they show up in Pat with no
  change to Pat
- Addressed, bulletin and emergency modes
- A web page on the station for composing, picking recipients from the roster, and watching
  each station's state live
- A net roster file that net control shares ahead of time, with members, groups and keys
- Optional Ed25519 signatures, so stations can tell a message really came from net control
- Works over Direwolf's KISS port today, using plain AX.25 UI frames at 1200 or 9600 baud
- The on-air format is published in [docs/protocol.md](docs/protocol.md)

## Install

There are no releases yet. Build it from source (below). The Windows installer and portable
builds will come with the first release.

## Usage

Every station on the net runs Direwolf, plus netcast pointed at Direwolf's KISS port (8001 by
default). Everyone uses the same net address. Set `--txdelay` to match `TXDELAY` in
`direwolf.conf`, and `--baud` to the modem speed.

On each receiving station:

```text
netcast listen --call K1ABC --net NET1
```

The listener finds Pat's mailbox by running `pat env`. Use `--mailbox` to point it somewhere
else. List several callsigns with `--call K1ABC,K1ABC-7`, and use `--group` to accept extra
group names besides the net's own.

On net control:

```text
netcast send message.b2f --to K1ABC,W2XYZ --from N0NCS --net NET1
netcast send bulletin.txt --subject "Net bulletin" --to K1ABC,W2XYZ --from N0NCS --net NET1
netcast send bulletin.txt --subject "Weekly bulletin" --to NET1 --mode bulletin --repeat 2 --interval 900 --from N0NCS --net NET1
```

A `.b2f` file goes out as it is. Anything else is sent as a plain text body, and needs a
`--subject`. Messages you compose in Pat go out through the CMS, so don't point netcast at Pat's
`out` folder: write the message to its own file.

`send` exits with status 1 when an addressed station hasn't confirmed by the last round.
`netcast status` shows what this station has sent and received. The listener reconnects by
itself when Direwolf restarts, and decoded messages show up in Pat as unread.

| Option | Default | What it does |
|---|---|---|
| `--mode` | `addressed` | `addressed`, `bulletin` or `emergency` |
| `--margin` | 0.1 with acks, 0.5 without | extra repair symbols in each full pass, as a fraction of K |
| `--rounds` | 3 | repair rounds, each followed by a status round |
| `--ack` | off | ask for acknowledgements on an emergency message |
| `--repeat`, `--interval` | 0, 600 s | extra full passes for bulletins, and the gap between them |
| `--ttl` | 60 min | how long receivers keep a delivery after they last hear it, up to 24 hours |
| `--kiss` | `127.0.0.1:8001` | Direwolf's KISS TCP port |
| `--guard` | 500 ms | slack added to each report slot |
| `--sign` | off | sign the announcement with this station's key |
| `--roster` | none | the net roster: recipients must be on it, and `--to` defaults to every member |

Each station's Direwolf waits for a clear channel before it answers, which can push its reply
past its slot. The sender allows for replies that run late, but on a busy channel a station can
still be marked silent for a round. If that keeps happening, raise `--guard`.

## The net roster

Net control keeps a roster of the net and shares it with every station before the net. It is
a `.londopy` file, the format all Londopy apps share: a zip with a small manifest and a
`roster.toml` inside.

```text
netcast roster new net1.londopy --net NET1
netcast roster add net1.londopy N0NCS --key <the public key `netcast key` printed>
netcast roster add net1.londopy K1ABC
netcast roster add net1.londopy ARES --group
netcast roster show net1.londopy
```

With `--roster`, a listener also accepts the roster's group names, and checks signatures
against its keys. `send --roster` refuses names that aren't on it, and sends to every member
when you don't give `--to`. With a roster, `--net` can be left out.

## Signing

Signing is optional and off by default. A signed message is still readable by anyone on the
air: the signature only proves who sent it. We found no FCC ruling on signatures in amateur
traffic. A signature doesn't hide the meaning of a message, which is what 97.113(a)(4) is about.

```text
netcast key
```

This makes the station's key pair, keeps the private half in netcast's folder
(`%LOCALAPPDATA%\netcast\station.key` on Windows), and prints the public half for the roster.
Then add `--sign` to `send`, or tick the box on the web page. If you point `--state-dir`
somewhere else, make sure only you can read that folder.

Receivers mark each message `verified`, `unsigned`, or `signed, key not in roster`. A message
whose signature doesn't match the roster key for its sender is dropped. Start a listener with
`--require-signed` to take only verified messages.

`verified` means a station on the roster sent the delivery. It doesn't vouch for the `From:`
line inside the message (the listener notes when the two differ), and it doesn't cover the
status reports, which anyone on frequency could fake. The exact rules are in
[docs/protocol.md](docs/protocol.md).

## The web page

```text
netcast ui --call N0NCS --roster net1.londopy --outbox C:\netcast\outbox
```

Then open <http://127.0.0.1:8740/>. The page runs the listener too, so it shows what's
coming in as well as what you're sending:

- write a message, or pick a `.b2f` from the `--outbox` folder
- choose recipients from the roster
- watch each station's state through the status rounds
- see incoming messages with their progress and whether they're verified

The page only answers on 127.0.0.1. It can make the radio transmit, so it refuses requests
that don't name it as the host, and every request that sends must carry a token that only the
page itself knows. Another website open in the same browser can't send through it.

## Which mode to use

| Traffic | Mode | Acknowledged |
|---|---|---|
| Messages for named stations | `addressed` | yes; this is two-way exchange under 97.111(a)(1) |
| Information bulletins for amateurs | `bulletin` | no; 97.111(b)(6) |
| Emergency traffic | `emergency` | only with `--ack`; 97.111(b)(4) |

Bulletins have to be of direct interest to the amateur service and meant only for amateurs, and
keeping them that way is the operator's job. Netcast never encrypts anything. Every frame
carries the sender's callsign in its AX.25 source address, in clear text.

## How it works

The message is compressed with brotli and split into K symbols of 244 bytes, one per UI frame.
Net control sends an announcement (the MID, the recipient list, and the RaptorQ parameters),
then all K source symbols, then 10% extra repair symbols. Then it polls the stations: each one
answers in its own time slot with `decoded` or `need n`. The next pass is max(n) + 2 repair
symbols that have never been sent before. Each of them helps every station that is still short,
whatever that station missed. After a few rounds every station has decoded, or the stations
that heard nothing at all are marked silent.

Netcast never lets more than about 40 frames queue up in Direwolf, because Direwolf throws
away UI frames once 100 are waiting.

The byte-level format is in [docs/protocol.md](docs/protocol.md).

## Building from source

```text
git clone https://github.com/Londopy/netcast
cd netcast
cargo build --release
cargo test
```

## Not there yet

- ardopcf FEC mode, and `ofm`'s connectionless port
- Opening `.londopy` rosters by double-clicking them in Explorer, which comes with the installer
- Winlink Express inboxes, since Express keeps its own message store

## Patents

RaptorQ is covered by Qualcomm's IETF IPR declaration #1958, which includes a non-assert for
implementations that fully implement RFC 6330. Netcast uses the `raptorq` crate unmodified, and
the codec ID in each announcement leaves room for a different fountain code later. This is not
legal advice.

## Layout

```text
src/
  main.rs       the CLI
  frame.rs      frame and announcement encoding
  codec.rs      brotli and RaptorQ
  sender.rs     a delivery: passes, status rounds, repair
  receiver.rs   per-delivery receive state and reports
  ax25.rs       AX.25 UI frames
  kiss.rs       KISS over TCP
  link.rs       the radio link interface and the airtime model
  sim.rs        the net simulator and the FLAMP and session baselines
  pat.rs        Pat mailbox lookup and inbox writes
  message.rs    B2F parsing and composing
  history.rs    the sent and received logs
  station.rs    sending and listening, shared by the CLI and the web page
  ui.rs         the web page's server and API
  keys.rs       the station key and announcement signatures
  roster.rs     the net roster
  londopy.rs    reading and writing .londopy files
ui/             the web page: HTML, CSS and JS, built into the binary
docs/
  protocol.md   the on-air format
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line
under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) with any change.

## License

Apache-2.0. See [LICENSE](LICENSE).
