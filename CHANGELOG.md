# Changelog

All notable changes to netcast are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

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

[Unreleased]: https://github.com/Londopy/netcast/commits/main
