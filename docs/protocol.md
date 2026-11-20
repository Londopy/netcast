# netcast protocol, version 2

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

The largest frame is 256 bytes, so the symbol size is 244 bytes: what's left after the 12-byte
header, which is already a multiple of 4. (The design notes netcast started from said 240,
leaving room that nothing uses; 244 sends the same message in about 1.6% fewer frames.) Receivers
listen only to frames sent to the net's address. Reports from receivers go to the same address.

Over ARDOP (ardopcf in FEC mode), each netcast frame travels in one ARDOP FEC data frame:

| Size | Field |
|---|---|
| 2 | length of the netcast frame |
| n | the netcast frame |
| rest | filler up to the data size of the FEC frame type |

ardopcf cuts the data it's given into frames by byte count alone, so every netcast frame is
filled out to exactly one FEC frame, and a lost FEC frame is one lost netcast frame. The filler
isn't zeros: on a test channel, frames padded with long runs of one byte value failed to decode
much more often. Receivers read the length and ignore the rest.

The frame type for messages is the sender's choice; netcast takes any FEC type of at least 32
data bytes. The symbol size is the frame's data size minus 14 (the length and the 12-byte
header), rounded down to a multiple of 4: 48 bytes for 4FSK.500.100, for example. Receivers
always answer status requests in 4FSK.500.100S, a robust 32-byte type, so the sender knows how
long an answer takes.

ARDOP frames carry no callsign. Every station runs ardopcf with `FECID TRUE`, so each
transmission includes an ARDOP ID frame naming the station (97.119). ardopcf sends that ID
frame after the first data frame of a transmission only, so at the end of a delivery the sender
asks for one more (`SENDID`), and the delivery closes with the station's ID. A receiver's report
is a single frame, so its ID frame comes right after it.

## Frame header

Every frame starts with these 12 bytes:

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 2 | magic | `NC` (`0x4E 0x43`) |
| 2 | 1 | version | `2` |
| 3 | 1 | type | 1 announce, 2 symbol, 3 status request, 4 report |
| 4 | 4 | delivery ID | random and non-zero, picked by the sender for each delivery |
| 8 | 4 | payload ID | RFC 6330 payload ID: source block number (1 byte, always 0), then ESI (3 bytes) |

The payload ID holds the ESI of the symbol in announce and symbol frames. It is zero in status
requests and reports. A frame with a non-zero source block number is not a netcast frame.

Version 2 added the precedence byte to the announcement; netcast 0.1.0 spoke version 1. A
station ignores frames of any other version. From 0.2.0 on it also says once that it is
hearing them, so a station that upgraded can tell why it decodes nothing from one that
hasn't. A 0.1.0 station says nothing, and a sender sees it as silent.

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
| 32 | SHA-256 of the message object (the compressed message) |
| 1 | mode: 1 addressed, 2 bulletin, 3 emergency |
| 1 | precedence, as on an ARRL radiogram: 0 routine, 1 welfare, 2 priority, 3 emergency. Emergency mode defaults to emergency, the others to routine |
| 2 | TTL in minutes: how long receivers keep this delivery after they last hear it (receivers cap it at 24 hours) |
| 1 + n | sender's callsign: a length byte, then n ASCII bytes |
| 1 + n | MID: a length byte, then n ASCII bytes (1 to 12 letters and digits, not a Windows device name such as `NUL` or `COM1`) |
| 1 | recipient count, 1 to 255 |
| per recipient, 1 + n | a length byte, then the callsign or group name |
| 1 | signature length: 0, or 64 for an Ed25519 signature |
| 0 or 64 | signature |

Callsigns and group names are 1 to 9 characters: upper-case ASCII letters, digits and
hyphens. Receivers drop an announcement with anything else in them, or with bytes left over
at the end.

The sender sends the announcement before the first symbol of every pass, and again before every
20th symbol frame, so a station that tunes in late still learns what it is hearing. Without
status rounds (bulletins, and emergency messages without acknowledgements) it also sends it
after the last symbol of each pass. A station that missed the first copy still has the symbols
it heard, and decodes as soon as this one arrives.

### Signatures

The signature is optional. It is an Ed25519 signature (RFC 8032) over these bytes:

1. the 19 ASCII characters `netcast announce v1`, then a zero byte (20 bytes in all)
2. the delivery ID, 4 bytes
3. the announcement, from the codec byte through the last recipient: everything except the
   signature length and the signature

The prefix still says v1 in protocol version 2, on purpose: it names the signature scheme, which
didn't change. A version 1 announcement can't be read as a version 2 one, because the bytes
after the mode would put a length of 45 or more where the sender's callsign starts, and
callsigns are at most 9 characters.

The announcement carries the hash of the message, so the signature covers the message as well.
The delivery ID is part of what's signed, so a signed announcement can't be replayed under a
different delivery. A signature hides nothing: the announcement is readable by anyone, as before.

A receiver looks up the sender's callsign in its net roster:

- **No signature:** the delivery is `unsigned`.
- **The roster has a key for the sender and the signature checks out:** `verified`.
- **The roster has a key and the signature doesn't check out:** the receiver drops the delivery.
- **The roster has no key for the sender:** `signed, key not in roster`.

A station run with `--require-signed` takes only `verified` deliveries and stays silent for the
rest. A receiver also ignores deliveries whose sender is one of its own callsigns.

An announcement a receiver doesn't take, because it isn't addressed to the station or fails the
signature rules, doesn't settle the delivery. The receiver notes it and keeps listening, so an
announcement forged under a real delivery's ID can't shut out the real one. It does stop
keeping that delivery's symbols until an announcement it takes arrives.

Once a delivery has decoded, the receiver refuses its ID for 7 days, so a recording played back
later doesn't land again as a new message.

What a signature doesn't cover:

- The B2F `From:` header. The signature says which station sent the delivery; the message
  inside can still claim to be from someone else, for example when relaying. Receivers note
  the difference when the two don't match.
- Status requests and reports. Anyone on frequency can send one, so a forged report can make
  net control believe a station decoded when it didn't. Treat the status table as a guide, not
  as a receipt.

Known answer, for checking an implementation. Signing key seed: 32 bytes of `0x01`, delivery ID
`0x01020304`, and an addressed, routine announcement from `N0NCS` to `K1ABC`, MID `ABC123`, TTL 60, the
hash 32 bytes of `0x01`, and an OTI for 1000 bytes in symbols of 244:

```text
public key 8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c
body       0100000003e80000f401000104010101010101010101010101010101010101010101010101
           01010101010101010100003c054e304e43530641424331323301054b31414243
signature  4d2da94f31b4bd5248f514d3d912a6b1080c006fc53058949e7ec8be7871ce62b6e1d3b7
           68fab14100495edbed200a2bc8fdba89842a95a4118167c6d2cf6f0f
```

The lines are wrapped for reading; join them without spaces.

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
| 2 | lead time in milliseconds: how long after hearing the request slot 0 starts |
| 2 | slot length in milliseconds |
| 1 | n, the length of the recipient list |
| ceil(n/8) | bitmap of polled recipients, most significant bit first: bit i is entry i of the list |

Each polled station replies in its own slot. Slots are numbered in list order among the polled
stations only. The station in slot s starts transmitting lead + s x slot length after it has
received the request. A station that has already reported `decoded` is not polled again.

A station answers each round once, however many copies of the request it hears, and ignores
lead times and slot lengths over 10 seconds.

The lead time leaves room for anything the sender's TNC may still send after the request, and
for the sender's radio to get back on receive. Over AX.25 it is TXDELAY plus the guard, since a
station decodes the request while the sender's TNC is still sending its closing flags. Over
ARDOP it is 2300 ms: when the request is the first frame of a transmission, ardopcf follows it
with a 1.8-second ID frame, and the first slot starts after that.

The sender sets the slot length to the report's airtime plus a guard (500 ms by default). Over
AX.25 the airtime is TXDELAY plus the frame. Over ARDOP it is 4000 ms, measured on ardopcf
1.0.4.1.3: one 4FSK.500.100S frame, the gap, and the ID frame after it. Once its own
transmission ends (over ARDOP, when ardopcf reports it has finished), the sender waits out the
lead time, every slot, and one more guard. If every station has answered sooner, it still waits
one lead time before sending again, so it doesn't start on top of the last station's ID frame.
When a reply arrives after the end of its slot, the sender assumes the rest are running just as late, and waits that
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
after a restart, so once its answer gets through the sender stops allowing for it.

## Delivery

1. Initial pass: the announcement, all K source symbols, then ceil(K x margin) repair symbols.
   The default margin is 10% in addressed mode.
2. Status round: poll every recipient that hasn't reported `decoded`. A station the sender has
   never heard from during the delivery is silent. One that answered an earlier round but not
   this one stays receiving.
3. Repair round: the announcement, then max(short) + 2 fresh repair symbols. A station's short
   is the need from its latest report. When this round's report is missing, it is the last
   need less the share of the repair sent since that the station has been hearing:
   (K - need) / symbols sent, as of its last report. After two missed polls in a row, a
   station no longer counts, until it answers again. A need is taken as between 1 and K,
   whatever the report says. If nobody counts, only the announcement goes out, for stations
   that missed it.
4. Repeat steps 2 and 3, up to the round limit (default 3 repair rounds), or until every
   recipient has reported `decoded`.

Bulletin mode, and emergency mode without acknowledgements, send only the initial pass, with a
50% default margin. The sender can repeat that pass a set number of times. Each repeat is a
full pass of fresh repair symbols, so it helps stations that missed part of the first pass, and
stations that missed all of it.

## Receiving

When the message object decodes, the receiver first checks it against the SHA-256 in the
announcement. Then it decompresses it, and checks the result the way Pat's parser will read it:
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
