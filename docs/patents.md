# RaptorQ and Qualcomm's IPR declaration

This is a reading of the public record, written down so the question isn't forgotten. It is
not legal advice, and nobody with legal training has reviewed it. Anyone distributing netcast
in a product, or worried about it, should have a patent lawyer read the declarations
themselves.

## What the declarations say

Qualcomm filed IETF IPR disclosure #1958 on 6 February 2013 for RFC 6330 (RaptorQ). It was
updated by #2554 on 19 March 2015, with the same licensing terms (one condition is worded a
little differently; see below) and a shorter patent list (US 7,139,960, US 7,451,377,
US 2009/0158114, US 2011/0299629 and EP 1665539). The longer list in #1958 names dozens of
older US patents and applications.

Both can be read at:
<https://datatracker.ietf.org/ipr/1958/> and <https://datatracker.ietf.org/ipr/2554/>

The licensing text has two cases, depending on the device the implementation runs on:

- A device that also implements a wireless wide-area standard (a cellular handset or base
  station): Qualcomm offers a licence on its usual terms, with no extra royalty on top of what
  it already charges for that device. That is an offer to license, not a free grant.
- Any other device: Qualcomm says it will not assert the listed patents against making, using,
  selling or importing it, but only for the implementation of RFC 6330, and only when the
  device fully implements it.

Two conditions apply to the non-assert:

- Qualcomm keeps the right to assert these patents against anyone who asserts a patent
  against Qualcomm's products (defensive suspension).
- It keeps its rights for anything that doesn't fully implement RFC 6330. #1958 words this two
  ways in two places ("not required to implement" and "does not fully implement"); #2554 uses
  "does not fully implement" in both.

## How netcast relates to it

- netcast runs on PCs and Raspberry Pis, which don't implement a cellular standard, so the
  non-assert is the relevant case.
- It uses the `raptorq` crate unmodified for encoding and decoding, and doesn't change the
  algorithm.
- Each delivery is one RaptorQ object with one source block and one sub-block. That is a
  valid RFC 6330 configuration, but netcast's receivers refuse objects with more than one
  source block, and nothing in netcast sends them. Whether an implementation that only ever
  uses one source block "fully implements" RFC 6330 is exactly the kind of question this note
  can't answer.
- Many of the patents in the #1958 list were filed more than 20 years ago, so some have
  probably expired; their status hasn't been checked one by one.

## If it ever becomes a problem

The announcement carries a codec ID (1 = brotli + RaptorQ). Another fountain code, such as
Wirehair (BSD-3-Clause, with no IETF IPR declaration found), could be added as codec 2. Its
announcements would carry Wirehair's parameters in place of the RaptorQ OTI; the frames, status
rounds and reports would stay as they are.

## Still to do

- Have someone qualified read #1958 and #2554 against how netcast uses RaptorQ, in particular
  the "fully implement" condition and the single-source-block point above.
- Check which of the listed patents are still in force.
- Record the answer here.
