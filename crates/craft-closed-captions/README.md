# craft-closed-captions

Closed captions in a live H.264 or HEVC stream, for the Craft apps. Text goes in a phrase at a
time and comes out as CEA-608 roll-up captions, one frame at a time, in the SEI message that
platforms and players read embedded captions from. The crate also reads those SEI messages back.
It depends only on `std` and on `craft-captions` for the 608 character sets and control codes,
contains no `unsafe` code, and builds for `wasm32-unknown-unknown`.

License: MIT OR Apache-2.0.

## What it writes

- `RollUp::new(fps_num, fps_den, Config)` makes a writer for a stream's frame rate. `Config` has
  the row count (2, 3 or 4; 3 by default) and the silence after which the captions are erased
  (`clear_after_ms`, 8000 by default, 0 for never). A row count or frame rate that is not valid is
  an `Error`.
- `RollUp::push(text)` adds a phrase on channel 1 of field 1. Words continue the open line and wrap
  at 32 columns; a longer word is broken at the edge. Each line starts with the roll-up command, a
  carriage return and the preamble address code of row 15, column 0, each sent twice, so a decoder
  that starts in the middle of the stream shows text from the next line. A special character goes
  as its two-byte code, sent twice. An extended character goes after a basic stand-in that it
  replaces. `push` returns the number of characters that 608 cannot show, which are left out.
- `RollUp::frame(&mut pairs)` gives the byte pairs for the next frame, with odd parity. Pairs go
  out at the field 1 rate, 30000/1001 a second, at any frame rate: one a frame at 29.97 fps, one
  every other frame at 59.94, and about five every four frames at 24. At most 1800 pairs wait (about a
  minute); text that comes faster drops the oldest lines that have not started to go out, and
  `RollUp::dropped` counts the pairs.
- `sei::cc_data(pairs, &mut out)` writes ATSC A/53 Part 4 `cc_data` with at most 31 pairs:
  `0xC0 | cc_count`, `0xFF`, `0xFC` and the two bytes for each pair, and the marker `0xFF`.
- `sei::sei_nal(codec, pairs, &mut out)` writes the whole SEI NAL unit: the header (H.264 type 6,
  or HEVC prefix SEI type 39), payload type 4 (`user_data_registered_itu_t_t35`, ITU-T H.264
  D.1.6 and H.265 D.2.6) with country code `0xB5`, provider code `0x0031`, the identifier `GA94`
  and user data type `0x03`, then the RBSP trailing bits, with emulation prevention bytes.
- `sei::insert_sei(codec, &mut access_unit, nal)` puts the NAL unit, with a 4-byte length, into a
  length-prefixed access unit before its first picture NAL unit. An access unit without a picture,
  or with a length that runs past its end, is an `InsertError` and is left unchanged.

## What it reads

`sei::read_sei(codec, access_unit)` returns the valid field 1 pairs of every closed-caption SEI
message in a length-prefixed access unit. Other SEI messages, user data registered for another
country or provider, and NAL units that are not SEI are passed over. A length, size or count that
runs past the data ends the reading of that NAL unit without a panic.

## Order of frames

The pairs of one frame belong to that frame's picture, so frames are given in the order they are
shown. A stream without B-frames sends its pictures in that order.

## Not written

CEA-708 service data (caption windows) is not written. The `cc_data` carries the 608 pairs only.

## Tests

`cargo test -p craft-closed-captions` decodes what the writer sends with the CEA-608 decoder of
`craft-captions` (`scc::decode_pairs`), checks the SEI bytes against the payload layout above, and
reads hostile and cut access units.
