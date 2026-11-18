# craft-flv

FLV tags for H.264 and AAC, for the Craft apps: the file header, `onMetaData`, the AVC and AAC
sequence headers, video and audio tags with composition time, the AMF0 values of script data, and a
tag reader. The same tag bodies are the bodies of RTMP audio, video and data messages, so an RTMP
sender writes them into its chunks without the FLV tag header. It has no dependencies beyond `std`,
contains no `unsafe` code, and builds for `wasm32-unknown-unknown`.

License: MIT OR Apache-2.0.

## What it writes

- `Writer::new` writes the 9-byte version 1 file header with the audio and video flags, and the
  first PreviousTagSize (0).
- `Writer::write` writes one tag: TagType, DataSize, the timestamp in milliseconds with its upper
  byte in TimestampExtended, StreamID 0, the body, and the PreviousTagSize (11 plus the body's
  length).
- `Body` is a tag's body:

  | `Body` | Tag type | Bytes before the data | Data |
  | --- | --- | --- | --- |
  | `AvcSequenceHeader(avcc)` | 9 | `0x17 0x00 0 0 0` | the AVCDecoderConfigurationRecord |
  | `AvcAnnexB { key, composition_time, data }` | 9 | `0x17` or `0x27`, `0x01`, the composition time | each NAL unit with a 4-byte length in place of its start code |
  | `AvcNalus { key, composition_time, data }` | 9 | `0x17` or `0x27`, `0x01`, the composition time | the NAL units, already length-prefixed |
  | `AvcEndOfSequence` | 9 | `0x17 0x02 0 0 0` | nothing |
  | `AacSequenceHeader(asc)` | 8 | `0xAF 0x00` | the AudioSpecificConfig |
  | `AacRaw(frame)` | 8 | `0xAF 0x01` | the raw AAC frame |
  | `Script(data)` | 18 | nothing | an AMF0 name string and value |

  The audio byte `0xAF` is SoundFormat 10 (AAC), SoundRate 3, 16-bit samples and stereo, the values
  the specification gives for every AAC tag; decoders read the sample rate and channels from the
  AudioSpecificConfig.
- `Metadata::encode` writes the `onMetaData` script data: the string `onMetaData` and an ECMA array
  of the properties that are set, in the order of the specification's table. `Metadata::h264_aac`
  fills in a live stream's properties. `Writer::write_metadata` writes it as a tag.
- `amf0` writes and reads numbers, Booleans, strings and long strings, objects, null, undefined,
  ECMA arrays, strict arrays and dates. Its `write_*` functions append to a `Vec<u8>` without
  building a `Value`, for RTMP commands.

## What it reads

`Reader` reads the file header and then one tag per `next_tag` call into a buffer the caller
reuses. Bytes past 9 in a larger header are skipped. `parse_video`, `parse_audio` and
`parse_script` read the bodies; `Nalus` splits a length-prefixed access unit, and `AnnexB` splits
an Annex B one. `parse_video` reads AVC access units with 4-byte NAL unit lengths, and refuses one
whose lengths are 0 or do not cover it exactly. Video of other codecs, video info frames and audio in other formats are returned as
`Other` with their field values.

## Checks and errors

Each tag is checked before any of it is written, and a refused tag leaves the output as it was.
`Writer::write` refuses:

- an `avcC` shorter than 7 bytes, with a configurationVersion other than 1, or with NAL unit
  lengths other than 4 bytes;
- an Annex B access unit that does not start with a start code or holds no NAL unit;
- length-prefixed NAL units whose lengths are 0 or do not cover the data exactly;
- a composition time outside -8 388 608 to 8 388 607 ms (a signed 24-bit field);
- an AudioSpecificConfig shorter than 2 bytes or with audio object type 0;
- an empty AAC frame, or one that starts with an ADTS header;
- script data that does not start with an AMF0 string;
- a body longer than 16 777 215 bytes (`MAX_TAG_DATA`);
- a negative timestamp, or one earlier than the last tag of the same type.

`Reader` refuses a signature other than `FLV`, a version other than 1, set reserved bits, a
DataOffset under 9 or over 65 536 (`MAX_DATA_OFFSET`), a first PreviousTagSize other than 0, a tag type other than 8, 9 or 18, an
encrypted tag (Filter bit set), a StreamID other than 0, and a PreviousTagSize that does not match
its tag. A stream that ends between tags ends the reading (`Ok(None)`); one that ends inside a tag
is `Error::Truncated`. AMF0 reading refuses references, movie clips, typed objects and the other
markers this crate does not read, nesting deeper than 64, more than 1 048 576 values in one call
(`MAX_VALUES`, about 32 MiB of decoded values), strings that are not UTF-8, and a strict array whose
count is larger than the bytes left. AMF0 writing refuses nesting deeper than 64 and more
than 1 048 576 values as well, so everything it writes reads back, and a property name over
65 535 bytes.

## Cost

`Writer::write` allocates nothing. It builds the tag header and the PreviousTagSize on the stack and
writes the body from the caller's slice, so a frame is copied once, into the output. An Annex B
access unit is read twice, once to measure it and once to write it. A tag is written in several
`write_all` calls, so wrap a file or socket in a `BufWriter`. `Reader::next_tag` reads into the
caller's buffer, which stops growing once it holds the largest tag. `Writer::write_metadata`
allocates one buffer for the script data.

`cargo bench -p craft-flv` writes and reads one second of a 6000 kb/s, 60 fps stream with
160 kb/s AAC (60 video tags of about 12 500 bytes and 47 audio tags); docs/performance.md lists the
results.

## Example

```rust
use craft_flv::{Body, Header, Metadata, Writer};

let mut flv = Writer::new(Vec::new(), Header { audio: true, video: true })?;
flv.write_metadata(0, &Metadata::h264_aac(1920, 1080, 60.0, 6000, 160, 48000, true))?;
flv.write(0, &Body::AvcSequenceHeader(&[1, 0x64, 0, 0x28, 0xFF, 0xE0, 0]))?;
flv.write(0, &Body::AacSequenceHeader(&[0x11, 0x90]))?;
flv.write(0, &Body::AvcAnnexB { key: true, composition_time: 0, data: &[0, 0, 0, 1, 0x65, 0x88] })?;
let bytes = flv.into_inner();
assert_eq!(&bytes[..3], b"FLV");
# Ok::<(), craft_flv::Error>(())
```

## Tests

`cargo test -p craft-flv` compares the header, tag header, AVC, AAC and `onMetaData` bytes with the
specification's field tables, writes streams and reads them back tag by tag, and checks every
refusal above. `tests/fuzz.rs` writes random streams of Annex B and length-prefixed access units
and AAC frames and reads back every timestamp, keyframe flag, composition time and NAL unit,
round-trips random AMF0 values, and feeds random bytes to the reader and every parser.

## Specifications

- Video File Format Specification, version 10.1, Annex E, The FLV File Format:
  <https://veovera.org/docs/legacy/video-file-format-v10-1-spec.pdf>
- Action Message Format, AMF 0:
  <https://veovera.org/docs/legacy/amf0-file-format-spec.pdf>
- ISO/IEC 14496-15, for the AVCDecoderConfigurationRecord and length-prefixed NAL units, and
  ITU-T H.264 Annex B, for the byte stream format.
- ISO/IEC 14496-3, for the AudioSpecificConfig.
