# craft-rtmp

A blocking RTMP and RTMPS publish client for the Craft apps: the handshake, chunking, the
`connect`, `releaseStream`, `FCPublish`, `createStream`, `publish` and `deleteStream` commands,
acknowledgements, and a timeout on every step. It uses `std::net` with no async runtime, starts no
threads, and contains no `unsafe` code. RTMPS uses rustls with the ring provider, behind the
default `rtmps` feature. The crate is built for native targets only; on `wasm32` it is empty.

License: MIT OR Apache-2.0.

## What it sends

- `Publisher::connect` takes a `Config` (`Config::new(url, key)`), connects to the first four
  addresses the host name resolves to in turn (`MAX_ADDRESSES`), runs the TLS handshake for
  `rtmps://`, and runs the RTMP
  handshake: C0 with version 3, C1 with the time and 1528 random bytes, and C2 echoing S1. It then
  sends Set Chunk Size 4096 and the commands in order:

  | Command | Transaction | Waits for |
  | --- | --- | --- |
  | `connect` with `app`, `type`, `flashVer` and `tcUrl` | 1 | `_result` |
  | `releaseStream` with the key | 2 | nothing |
  | `FCPublish` with the key | 3 | nothing |
  | `createStream` | 4 | `_result` with the message stream ID |
  | `publish` with the key and `live`, on the new stream | 0 | `onStatus` `NetStream.Publish.Start`; other statuses at level `status` are skipped |

  `_error`, or an `onStatus` with level `error`, is `Error::Rejected` with the command, the code
  and the server's description. `Publisher::connect_to` does the same with addresses the caller
  resolved, and TLS still checks the certificate against the URL's host name.
- `Publisher::send_flv` sends a `craft_flv::Checked` body as an audio, video or data message, with
  the FLV prefix bytes and the data as two slices. `Publisher::set_data_frame` sends `@setDataFrame`
  followed by `onMetaData` script data. `Publisher::send` sends raw message parts.
- `Publisher::close` sends `deleteStream` with the stream ID, sends a TLS close_notify for
  `rtmps://`, and shuts down the connection's write side.
- `ChunkWriter::write_message` picks the smallest chunk header the specification allows: type 0
  for a new chunk stream, a new message stream or a timestamp that goes back; type 1 when the
  length or message type changes; type 2 when only the delta changes; and type 3 when the delta
  repeats. It never starts a message with type 3 straight after a type 0 header. A timestamp or
  delta of 0xFFFFFF or more goes in the extended timestamp field, repeated in every continuation
  chunk. Chunk stream IDs 2 to 65 599 use the one-, two- and three-byte basic headers.

## What it reads

- `Publisher::poll` reads what the server sent without blocking, at most about 256 KiB per call
  (`POLL_READ_LIMIT`). `Publisher::send` calls it every 250 ms.
- Window Acknowledgement Size sets how often the client acknowledges: after each window of bytes
  it has received, it sends an Acknowledgement with the byte count (wrapping at 2^32).
- Set Peer Bandwidth sets the client's window: a hard limit replaces it, a soft limit keeps the
  smaller of the two, and a dynamic limit counts as hard after a hard limit or when it is the
  first limit. When the window changes, the client sends Window Acknowledgement Size with it.
- A Ping Request gets a Ping Response with the same timestamp.
- Acknowledgements from the server are counted in `Stats::bytes_acked`. One that would count more
  bytes than the client has sent is ignored.
- `ChunkReader` reads a chunk stream into whole messages and applies Set Chunk Size and Abort.

## Timeouts and errors

- `Config::connect_timeout` (10 s) bounds each TCP connection attempt, so the TCP part of a
  connect takes at most 40 s. The system resolver is not bounded. When no address accepts the
  connection, `Error::Connect` lists each address tried with the error it gave.
- Every timeout must be longer than 0 and at most 24 hours (`MAX_TIMEOUT`).
- `Config::setup_timeout` (15 s) starts once the TCP connection is open and bounds the reads and
  writes of the TLS handshake, the RTMP handshake and the commands up
  to the answer to `publish`. Running out is `Error::Timeout` with the step.
- A write that makes no progress for `Config::stall_timeout` (5 s) is `Error::Stalled`. After it,
  and after any write error, every send is `Error::Broken`, since the connection may hold part of
  a message.
- A server that closes the connection is `Error::Closed`. A TLS failure is `Error::Tls`, whose
  `TlsKind` says whether the certificate's root is not trusted, its name does not match, it has
  expired, the certificate check refused it for another reason, or the server sent a fatal alert.
  The system's certificate check on Windows and macOS reports some refusals without a reason, and
  those are `TlsKind::CertificateNotAccepted`.
- `Endpoint::parse` refuses schemes other than `rtmp` and `rtmps`, a user name or password, an
  empty host, an IPv6 address without brackets, a port of 0 or past 65 535, an empty application
  name, and spaces or control characters. `StreamKey::new` refuses an empty key, one over 65 535
  bytes, and one with a space or control character.
- The reader refuses a chunk that continues a chunk stream with no earlier header, a new message
  header while a message is part way through, a chunk size of 0, a message longer than its limit
  (1 MiB for messages from the server), messages in progress whose lengths add up past that limit,
  and more than 64 chunk streams (`MAX_CHUNK_STREAMS`).
- `send_flv` refuses an Annex B body, and `send` refuses a message over 16 777 215 bytes, without
  breaking the connection.

## The stream key

`StreamKey` prints as `StreamKey(<redacted>)`, and `Endpoint`'s `Debug` output leaves out the
query string. Text from the server that goes into an error has every occurrence of the key replaced
with `<stream key>`, the URL's query string replaced with `<redacted>` wherever no letter, digit
or one of `-_~%` touches it, control characters removed, and at most 256 characters kept.

## Threads and cost

The caller sends from its own thread; the crate holds no locks. `ChunkWriter::write_message`
builds the chunk headers on the stack and writes headers and payload slices in vectored writes of
up to 64 slices, so a message is not copied before the socket and chunking allocates nothing
(`tests/alloc.rs` checks this). Over plain TCP a 12.5 kB video message at the 4096-byte chunk size
is one system call. Over TLS, rustls copies and encrypts each message and flushes it as TLS
records.

`cargo bench -p craft-rtmp` chunks one second of a 6000 kb/s, 60 fps H.264 stream with 160 kb/s
AAC (60 video messages of 12 500 bytes and 47 audio messages of 426 bytes): into `io::sink` at the
4096 and 128-byte chunk sizes, into a `Vec`, and over loopback TCP. On Linux (AMD EPYC 9R14) in a
release build, on 2026-11-18, chunking that second into `io::sink` took 4.509 µs at 4096 bytes and
23.38 µs at 128 bytes, chunking it into a `Vec` at 4096 bytes took 17.80 µs, and sending it over
loopback TCP at 4096 bytes took 1.253 ms, almost all of it in the system calls.

## Example

```rust,no_run
use craft_flv::Body;
use craft_rtmp::{Config, Publisher};

let config = Config::new("rtmp://live.example.com/app", "stream-key")?;
let mut rtmp = Publisher::connect(&config)?;
rtmp.send_flv(0, &Body::AacSequenceHeader(&[0x11, 0x90]).check()?)?;
rtmp.close()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Tests

`cargo test -p craft-rtmp` compares chunks with the specification's examples, checks every header
type, extended timestamps and basic header form, and round-trips random messages at random chunk
sizes read back in random pieces. Against a loopback RTMP server in the crate, it publishes a stream
and checks every command and message the server receives, the acknowledgements, ping answers and
window changes, refusals with the key hidden, a server that never answers, one that closes, one
with another version, a stalled write, refused addresses with each one reported and at most four
tried, and RTMPS with a certificate authority made when the test runs, trusted and untrusted, with
the kind of an untrusted certificate kept.

## Specifications

- Real-Time Messaging Protocol (RTMP) specification 1.0:
  <https://veovera.org/docs/legacy/rtmp-v1-0-spec.pdf>
- Action Message Format, AMF 0:
  <https://veovera.org/docs/legacy/amf0-file-format-spec.pdf>
