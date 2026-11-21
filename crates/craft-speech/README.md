# craft-speech

Speech-to-text on the device for a Craft app: a recognizer trait, the catalog of Whisper models
with a verified downloader, Whisper inference in pure Rust on the CPU, and live captions that cut
a stream of audio into phrases. The crate depends on no app crate.

| Item | What |
|---|---|
| `Transcriber` | the trait a recognizer implements: mono 16 kHz samples in, a `Transcript` out, with a progress callback that can stop it |
| `Transcript`, `Word` | the language, the model's id and the words, each with its text and `[start, end)` in 16 kHz samples; `normalize` sorts the words and removes overlaps |
| `FixedTranscriber` | a recognizer that answers with a fixed transcript, for tests and demonstrations |
| `load` | the recognizer for a catalog model in a models folder, after every file is checked against its size and SHA-256 |
| `models` | the catalog (`whisper-tiny`, `whisper-base`, `whisper-small`), each file pinned to a revision with its size and SHA-256; `installed` hashes every file, `present` checks sizes only, `missing_bytes`, and `download` (feature `download`) |
| `whisper::Whisper` | Whisper inference on candle (feature `whisper`): 30-second windows, language detection, timestamp decoding and word times from cross-attention alignment |
| `live::LiveCaptioner` | phrases from a stream: a partial result while a phrase is spoken, and a final result with word times when a pause ends it |
| `resample::Downsampler` | any rate and channel count to mono 16 kHz, with the same output however the input is cut |
| `mel`, `vad` | the log-mel front end, and word bounds tightened against the audio's level |
| `word_error_rate` | the word error rate between a reference and a hypothesis |

## Features

| Feature | Adds |
|---|---|
| `whisper` | `whisper::Whisper` and a working `load`, through `candle-core` and `candle-nn` |
| `download` | `models::download` over HTTPS through `ureq` with rustls and the system's certificate checks |

Without `whisper`, `load` returns `SpeechError::Unavailable`. Without `download`,
`models::download` returns `SpeechError::Unavailable`; `models::fetch_files` takes the bytes from
a function instead, which the tests use.

## Models

No model is bundled. `download` writes `<models dir>/<id>/` and needs the model's full size free.
Each file streams into `<name>.part`. The download fails, and deletes the part file, when the
server's length header differs from the pinned size, when the server sends more than the pinned
size, when the SHA-256 differs, or when the progress callback returns `false`. A file that passes
is renamed into place. Files already present with the pinned size and SHA-256 are not fetched
again.

| Model | Download | Notes |
|---|---|---|
| `whisper-tiny` | 146 MiB | the fastest |
| `whisper-base` | 280 MiB | the default (`models::DEFAULT_MODEL`) |
| `whisper-small` | 925 MiB | the most accurate, about 4 times slower than base |

The weights are OpenAI's, under the MIT License; the catalog records each model's license, author
and source page.

## Live captions

`LiveCaptioner::push` takes mono 16 kHz audio in pieces of any size and returns
`LiveEvent::Partial` and `LiveEvent::Final` events in order. The defaults are in `LiveConfig`:

| Setting | Default |
|---|---|
| pause that ends a phrase | 600 ms |
| longest phrase | 12 s (at most 28 s) |
| partial result interval | 1 s, or twice the last transcription's time when that is longer |
| audio kept before the first voiced frame | 200 ms |
| least voice for a phrase | 200 ms |
| voice level | 12 dB above the followed noise floor, and at least -50 dBFS |

The transcriber runs on the caller's thread. Setting the stop flag passed to `LiveCaptioner::new`
ends a running transcription with `SpeechError::Canceled`. `gap` ends the open phrase when audio
was lost, and `flush` ends it when the audio stops. `finalized_until` is the sample before which
every phrase has been given as final.

## Tests

`cargo test -p craft-speech --all-features` runs the unit tests, which use synthetic audio and
fixed transcripts and reach no network. `tests/whisper_model.rs` transcribes real speech when
`whisper-tiny` is in `$CRAFT_SPEECH_MODELS_DIR` or `target/models` and samples are in
`target/fixtures/speech`; otherwise it prints SKIPPED.

## License

MIT OR Apache-2.0. The crate contains code from https://github.com/storytold/filmcraft
(`crates/speech`); see `NOTICE`.
