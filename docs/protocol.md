# netcast protocol, version 1

This is the on-air format netcast uses. It is published here, before any on-air use, so that
anyone can decode a netcast transmission (47 CFR 97.309(a)(4)). Nothing in it is encrypted.

All multi-byte integers are big-endian.

## Transport

Over AX.25 (Direwolf and other KISS TNCs), every netcast frame is the information field of a
UI frame:

- control `0x03`, PID `0xF0`
- destination: the net's group address, for example `NET1`
- source: the transmitting station's callsign, which identifies the station in clear text
  in every frame (97.119)

The largest frame is 256 bytes, so the symbol size is 244 bytes. Receivers listen only to frames
sent to the net's address. Reports from receivers go to the same address.

## Frame header

Every frame starts with these 12 bytes:

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 2 | magic | `NC` (`0x4E 0x43`) |
| 2 | 1 | version | `1` |
| 3 | 1 | type | 1 announce, 2 symbol, 3 status request, 4 report |
| 4 | 4 | delivery ID | random and non-zero, picked by the sender for each delivery |
| 8 | 4 | payload ID | RFC 6330 payload ID: source block number (1 byte, always 0), then ESI (3 bytes) |

The payload ID holds the ESI of the symbol in announce and symbol frames. It is zero in status
requests and reports. A frame with a non-zero source block number is not a netcast v1 frame.

## The message object

1. The sender starts from a Winlink message in B2F form: the header lines, a blank line, the
   body and any attachments, exactly as Pat stores it in a `.b2f` file.
2. It compresses the message with brotli at quality 11 and a 22-bit window. That is codec 1.
3. The compressed bytes are one RaptorQ (RFC 6330) source object: one source block, one
   sub-block, symbol alignment 4, symbol size T = 244 on AX.25.

Netcast caps the message at 4 MiB, before and after compression. The number of source symbols
K = ceil(length / T) must not exceed 56403.

## Type 1: announce

The announcement says what the delivery is. It is sent as its own small RaptorQ object, so a
long recipient list can span several frames and still survive losses.

Frame body, after the header:

| Size | Field |
|---|---|
| 2 | length of the announcement in bytes |
| T_a | one encoding symbol of the announcement object |

The announcement object uses one source block, one sub-block, alignment 4, and symbol size
T_a. T_a is whatever remains of the frame after the length field, and is always a multiple
of 4. A receiver takes T_a from the frame size, and the ESI from the header.

When the announcement fits in one frame, T_a is its length rounded up to a multiple of 4,
and every repeat sends ESI 0. When it needs several frames, the first burst sends the source
symbols, and each later burst sends K_a fresh repair symbols.

Announcement layout:

| Size | Field |
|---|---|
| 1 | codec, 1 = brotli + RaptorQ |
| 12 | RFC 6330 OTI for the message object (transfer length, symbol size, blocks, sub-blocks, alignment) |
| 1 | mode: 1 addressed, 2 bulletin, 3 emergency |
| 2 | TTL in minutes: how long receivers keep this delivery after they last hear it (receivers cap it at 24 hours) |
| 1 + n | MID: a length byte, then n ASCII bytes (1 to 12 letters and digits, not a Windows device name such as `NUL` or `COM1`) |
| 1 | recipient count, 1 to 255 |
| per recipient, 1 + n | a length byte, then the callsign or group name in ASCII |
| 1 | signature length: 0, or 64 for an Ed25519 signature |
| 0 or 64 | signature |

Netcast v1 always sends signature length 0. Receivers skip a 64-byte signature without checking
it. Checking signatures comes in a later version, with the net roster.

The sender sends the announcement before the first symbol of every pass, and again before every
20th symbol frame, so a station that tunes in late still learns what it is hearing.

## Type 2: symbol

The frame body is one encoding symbol of the message object, exactly T bytes. The ESI is in the
header. Source symbols (ESI 0 to K-1) go first, in order. Repair symbols follow, with ESIs from
K upward. As RFC 6330 specifies, a decoder maps a repair ESI to the internal
symbol ID ESI + K' - K, where K' is the extended source block size. The sender never sends the
same ESI twice in a delivery.

Receivers keep symbols that arrive before the announcement has decoded, up to 4096 per
delivery, and use them once it has.

## Type 3: status request

Sent only in addressed mode, or in emergency mode when the sender asks for acknowledgements.

| Size | Field |
|---|---|
| 1 | round number, starting at 1 |
| 2 | slot length in milliseconds |
| 1 | n, the length of the recipient list |
| ceil(n/8) | bitmap of polled recipients, most significant bit first: bit i is entry i of the list |

Each polled station replies in its own slot. Slots are numbered in list order among the polled
stations only. The station in slot s starts transmitting s x slot length after it has received
the request. A station that has already reported `decoded` is not polled again.

A station answers each round once, however many copies of the request it hears, and ignores
slot lengths over 10 seconds.

The sender sets the slot length to TXDELAY + report airtime + a guard (500 ms by default). It
waits for every slot, plus one more guard, after its own transmission ends. When a reply arrives
after the end of its slot, the sender assumes the rest are running just as late, and waits that
much longer, up to the length of the whole slot window.

## Type 4: report

| Size | Field |
|---|---|
| 1 | status: 1 decoded, 2 need |
| 2 | how many more symbols the station needs (0 when decoded) |
| 1 | the station's index in the recipient list |
| 1 | the round of the status request this answers |

The sender ignores a `need` from an earlier round, since it is out of date. It still accepts a
late `decoded`.

`need` is K minus the number of distinct symbols the station holds, and at least 1. RaptorQ
can fail to decode with exactly K symbols, and then 1 more is the honest answer.

A station that hasn't decoded the announcement stays silent, since it doesn't know it is being
polled. A station answers only for its own callsigns, never for a group name it listens to.
A station that has decoded a delivery keeps answering `decoded` until the TTL runs out, even
after a restart, so a lost report never causes extra repair.

## Delivery

1. Initial pass: the announcement, all K source symbols, then ceil(K x margin) repair symbols.
   The default margin is 10% in addressed mode.
2. Status round: poll every recipient that hasn't reported `decoded`. Anyone who doesn't answer
   is marked silent for that round.
3. Repair round: the announcement, then max(need) + 2 fresh repair symbols, where max is taken
   over this round's reports. If nobody reported a need, only the announcement goes out, for
   stations that missed it.
4. Repeat steps 2 and 3, up to the round limit (default 3 repair rounds), or until every
   recipient has reported `decoded`.

Bulletin mode, and emergency mode without acknowledgements, send only the initial pass, with a
50% default margin. The sender can repeat that pass a set number of times. Each repeat is a
full pass of fresh repair symbols, so it helps stations that missed part of the first pass, and
stations that missed all of it.

## Receiving

When the message object decodes, the receiver decompresses it. It checks the result the way
Pat's parser will read it:
- the headers are `Key: value` lines
- `Body:` and every `File:` size is a whole number
- the sections fill the rest of the message exactly
- `Date:`, if present, is `YYYY/MM/DD HH:MM`
- the `Mid:` header matches the announced MID

If any check fails, a bad symbol has probably got in. The receiver throws away what it has and
starts collecting fresh symbols for the same delivery. An announcement that decodes but doesn't
parse is handled the same way.

When the checks pass, the receiver adds `X-Unread: true` to the headers, as Pat does for mail
it receives. Then it writes `<MID>.b2f` into the Pat inbox of every local callsign the delivery
is for:

- a recipient entry that equals a local callsign delivers to that callsign
- a recipient entry that equals a group the station listens to delivers to all of its local
  callsigns

An existing `<MID>.b2f` means the message is a duplicate, and it is left alone.
