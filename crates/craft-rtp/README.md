# craft-rtp

RTP and RTCP for receiving media, for the Craft apps: RTP packets and RTCP reports read and
written, the generic NACK and the picture loss indication, a jitter buffer that takes the time as
an argument, the mapping from RTP timestamps to the sender's wall clock, and depacketizers that
rebuild H.264, VP8, Opus and AAC frames. It opens no sockets and starts no threads: the caller
reads the datagrams and passes them in. It has no dependencies beyond `std`, contains no `unsafe`
code, and builds for `wasm32-unknown-unknown`.

License: MIT OR Apache-2.0.

## RTP packets

`Packet::parse` reads a packet into fields that borrow the datagram: the marker bit, the payload
type, the sequence number, the timestamp, the SSRC, the contributing sources, the header extension
(its profile value and data) and the payload without its padding. `Packet::write` appends a packet
to a `Vec<u8>`. `is_rtcp` tells an RTCP packet from an RTP packet on a port that carries both, by
the second byte (192 to 223, RFC 5761 section 4).

## RTCP

`rtcp::Compound` reads the packets of a compound RTCP datagram one at a time:

| Item | Packet type | Read |
| --- | --- | --- |
| `SenderReport` | 200 | the sender information and its report blocks |
| `ReceiverReport` | 201 | the reporter's SSRC and its report blocks |
| `Bye` | 203 | the sources and the reason |
| `Nack` | 205, FMT 1 | the sender and media SSRCs and the sequence numbers asked for |
| `Pli` | 206, FMT 1 | the sender and media SSRCs |
| `Other` | any other | the type, the count field and the body |

The datagram need not start with a report, so reduced-size RTCP (RFC 5506) is read too.
`write_sender_report`, `write_receiver_report`, `write_bye`, `write_nack` and `write_pli` append
one packet each. `write_nack` packs the sequence numbers into entries of a packet id and a bitmask
of the 16 that follow it.

`rtcp::TimeMap` maps an RTP timestamp to nanoseconds on the sender's wall clock (NTP time) from
the stream's sender reports. A report that differs from the mapping by more than 1 s
(`rtcp::MAX_JUMP`) starts the mapping again, as after the sender's clock is set. Timestamps up to
2^31 units before or after the last report map, about 6.6 hours at 90 kHz.

## Jitter buffer

`JitterBuffer` holds the packets of one stream and releases them in sequence order:

- `push(packet, now)` stores a copy of the payload, in a buffer it reuses, and returns `Stored`,
  `Duplicate`, `Late` (its number was already released or given up on), `Stray` or `Restarted`.
  A sequence number more than 3000 ahead or 100 behind the newest (`MAX_DROPOUT` and
  `MAX_MISORDER`) is stray. When the next packet follows a stray one in sequence, the sender has
  started again: the buffer drops what it holds and starts from that packet (RFC 3550 A.1).
- `pop(now)` gives the next packet once its delay has passed since it arrived, or `Gap { missing }`
  when packets did not arrive in time or were dropped. A missing packet is waited for until the
  delay has passed since the first packet after it arrived. `next_wake` gives the time of the
  next event.
- `nacks(out, max)` lists the missing sequence numbers that have not been listed before, so each
  one is asked for once.
- `report_block(ssrc, last_sr, now)` fills a receiver report block: the fraction lost since the
  last call, the cumulative loss, the extended highest sequence number, the jitter, and the last
  sender report's time and the delay since it arrived (RFC 3550 A.3).

The delay is 60 ms at first. Once 16 packets have been measured it is four times the inter-arrival
jitter estimate (RFC 3550 section 6.4.1, kept as in A.8), between 40 and 150 ms. The buffer holds
at most 2 s between its oldest and newest packet and 8 MiB of payload, and gives up on the oldest
packets beyond either limit, so a caller that stops popping does not make it grow. It holds at most
4096 sequence numbers (`SLOTS`) from the oldest it waits for. Every call takes the time as a
`Duration` since a fixed point the caller picks; a time earlier than one passed before counts as
the latest.

## Depacketizers

`Depacketizer::push(timestamp, marker, payload, emit)` takes the payloads in sequence order and
calls `emit` with each frame it completes. `lost()` takes a gap from the jitter buffer.

| `Codec` | Reads | Frame |
| --- | --- | --- |
| `H264` | single NAL units, STAP-A and FU-A (RFC 6184 packetization modes 0 and 1) | an Annex B access unit with 4-byte start codes; `key` when it holds an IDR picture |
| `Vp8` | the payload descriptor with its PictureID, TL0PICIDX, TID and KEYIDX fields (RFC 7741) | the frame's partitions joined; `key` for a key frame |
| `Opus` | one Opus packet per payload (RFC 7587) | the payload, without a copy |
| `Aac(AacConfig)` | `mpeg4-generic` AU-headers with the size and index fields (RFC 3640), several units per packet or one unit in fragments; `AacConfig::HBR` and `AacConfig::LBR` | one raw access unit; the second and later units of a packet get the packet's timestamp plus 1024 samples (`frame_samples`) each |

A video frame ends at the packet with the marker bit, or when a packet with another timestamp
arrives. A video frame with any part lost or unreadable is dropped, and video then waits for the
next key frame; `wants_key_frame` is `true` until it comes, for a picture loss indication. Video
starts by waiting for a key frame. A frame over 8 MiB (`MAX_FRAME`, or the limit passed to
`with_max_frame`) is dropped.

## Checks and errors

Every length is checked before it is used, and a packet that does not read is an error, never a
panic. `Packet::parse` refuses a version other than 2, a contributing source list or extension
that runs past the end, a padding count of 0, and padding longer than the bytes after the header.
`Packet::write` refuses a payload type over 127, more than 15 contributing sources, and extension
data that is not whole 32-bit words. `Compound` refuses a version other than 2, a length past the
end of the datagram, padding on any packet but the last, report blocks, a BYE reason or a feedback
message that runs past the packet, and a generic NACK without entries, and ends after the first
error. The writers refuse more than 31 report blocks or BYE sources, a reason over 255 bytes and
an empty NACK.

The depacketizers refuse an empty payload, a set forbidden bit, a reserved NAL unit type, the
interleaved H.264 modes (STAP-B, MTAP and FU-B), a FU-A fragment with both its start and end bits
set, an STAP-A unit of size 0 or one that runs past the packet, a VP8 descriptor or PictureID that
runs past the payload, AU-headers that are not a whole number of headers, an AAC unit of size 0,
units that do not fill their packet exactly, a unit longer than its packet that has the marker
bit (so no fragment follows), interleaved AAC units, and AAC fragments that do not continue the
unit before them. A refused payload drops the frame it belongs to, and the next
payload is read as usual.

## Cost

Reading a packet or a report allocates nothing. `push` copies the payload once into a buffer from
a list of up to 64 it reuses, and `pop` lends the payload until the next call. A depacketizer
copies each payload once into its frame buffer, which keeps the capacity of the largest frame.

## Tests

`cargo test -p craft-rtp` compares the RTP header, sender report, NACK and PLI bytes with the
specifications' layouts, joins STAP-A, FU-A, VP8 partitions and AAC fragments, drops an access unit
whose middle fragment is lost and waits for the next key frame, wraps sequence numbers, checks the
jitter estimate against RFC 3550's formula with a 1 kHz test clock, waits for and gives up on a
missing packet, lists it for a NACK once, and checks the byte, span and slot limits, a sender that
starts again, and every refusal above. `tests/fuzz.rs` writes random RTP packets and RTCP reports
and NACKs and reads them back, sends random access units through an RFC 6184 packetizer of its own
and the depacketizer, plays random orders with repeats and holes through the jitter buffer, and
feeds random bytes to every reader.

## Specifications

- RTP and RTCP: <https://www.rfc-editor.org/rfc/rfc3550>
- RTP and RTCP on one port: <https://www.rfc-editor.org/rfc/rfc5761>
- Reduced-size RTCP: <https://www.rfc-editor.org/rfc/rfc5506>
- Generic NACK and picture loss indication: <https://www.rfc-editor.org/rfc/rfc4585>
- H.264 payload format: <https://www.rfc-editor.org/rfc/rfc6184>
- VP8 payload format: <https://www.rfc-editor.org/rfc/rfc7741>
- Opus payload format: <https://www.rfc-editor.org/rfc/rfc7587>
- MPEG-4 elementary streams (AAC): <https://www.rfc-editor.org/rfc/rfc3640>
