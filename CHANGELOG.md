# Changelog

All notable changes to netcast are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

## [0.2.0] - 2026-11-19

### Changed

- The on-air format is now version 2, with a precedence in the announcement. 0.1.0 stations
  can't hear 0.2.0 ones or the other way round, so upgrade every station on a net together.
- `netcast upgrade` asks before installing; `--yes` skips the question for scripts. It also
  checks that it can write the program's folder before asking.
- `silent` now means a station the sender never heard from during the delivery.
- The simulator's test messages no longer have random MIDs, so its results repeat exactly.

### Added

- A precedence on every message (routine, welfare, priority or emergency, as on a radiogram),
  set with `send --precedence` or on the web page, and shown in each receiving station's log
  and web page. Emergency mode defaults to emergency.
- The sender's table shows stations that are still receiving, with how many symbols they need.
- A 0.2.0 listener that hears netcast frames of another protocol version says so once.
- `docs/patents.md`: what Qualcomm's IPR declarations for RaptorQ say, and what is still open.

### Fixed

- A station whose reports stopped getting through after round 1 was marked silent, and if it
  was the only one still short, no more repair was sent for it. The sender now keeps it as
  receiving and sizes the next repair from what it probably still misses, for up to two
  missed polls in a row.
- One forged report claiming a huge need could make the sender queue tens of thousands of
  repair symbols. A report now counts for at most the whole message.
## [0.1.0] - 2026-11-19

### Added

- `netcast send`: delivers a B2F message or a plain text body to a list of stations over
  Direwolf's KISS port, with RaptorQ fountain coding, slotted status reports and repair
  rounds sized for the worst receiver.
- `netcast listen`: decodes deliveries for its callsigns and groups, checks them the way Pat
  parses them, writes them into Pat's inbox as unread, and answers status requests, also after
  a restart. It reconnects when the TNC goes away.
- Addressed, bulletin and emergency modes, with repeated passes for bulletins.
- `netcast status`: the sent and received history.
- `netcast sim`: simulates a net and compares channel time against FLAMP-style hole lists and
  one session per station.
- The on-air format, published in `docs/protocol.md`.
- `netcast ui`: a web page on 127.0.0.1 for composing, sending from an outbox folder, picking
  recipients from the roster, and watching each station's state and incoming messages live.
- Net rosters as `.londopy` files, with `netcast roster new`, `add`, `remove` and `show`.
- ardopcf FEC mode as a second transport (`--via ardop`), for HF. Each netcast frame fills
  exactly one ARDOP frame, status rounds are timed from ardopcf's own end-of-transmission
  report, and receivers answer in 4FSK.500.100S.
- Optional Ed25519 signatures: `netcast key`, `send --sign`, and `listen --require-signed`.
  Receivers check signatures against the roster and label each message.
- `netcast upgrade`: installs the newest release after checking it against the release's
  `SHA256SUMS.txt`, and a once-a-day notice when there is one (`NETCAST_NO_UPDATE_CHECK=1` turns
  it off).
- Windows installer, MSIs, portable builds and script installs (the installer, the MSIs and
  `install.ps1` put netcast on PATH), plus Linux binaries for x64 and arm64.
- Portable mode: an empty file named `portable` next to the exe keeps the key and logs there.
- `netcast check`: before a net, checks the state folder, station key, roster, Pat's mailbox
  and the TNC, one line each.
- The web page shows recent sent and received deliveries, and takes the gap between bulletin
  repeats.

[Unreleased]: https://github.com/Londopy/netcast/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Londopy/netcast/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Londopy/netcast/releases/tag/v0.1.0
