# Mechanics

The timing and tuning values StudioCraft uses, where each one lives in the code, and where it came
from. `mechanics.json` holds the same values for tools.

A value marked **estimated** is a common choice for studio software, and it has not been checked
against public documentation or measured in another program. A value with a named source follows
that source. When a value is
confirmed from public documentation, add the page's URL as its source and remove the estimate mark
in both files.

## Video

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Canvas size | 1920 × 1080 | `model::profile::VideoSettings::default` | estimated |
| Output size | 1920 × 1080 | `model::profile::VideoSettings::default` | estimated |
| Frame rate | 30 fps | `model::profile::VideoSettings::default` | estimated |
| Largest canvas side | 8192 px | `render::compositor::MAX_CANVAS` | estimated |
| Compositor texture ring | 3 frames | `render::compositor::RING_LEN` | estimated |
| Largest side of a video filter's output | 8192 px | `plugin::filter::MAX_EFFECT_SIDE` | estimated |
| Crop and pad edges and area | 0 px each (the whole source) | `sources::filters::CropPad` | estimated |

## Transitions

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Default transition | fade | `model::collection::TransitionConfig::default` | estimated |
| Default duration | 300 ms | `model::collection::TransitionConfig::default` | estimated |
| Longest duration | 20 000 ms | `model::collection::MAX_TRANSITION_MS` | estimated |
| Audio fade on a scene change | 300 ms | `engine::mixer` (`set_fade_ms`) | estimated |
| Fade and Fade through color easing | Linear | `render::transitions::FADE_EASING` | estimated |
| Swipe easing | Ease in and out | `render::transitions::SWIPE_EASING` | estimated |

## Audio

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Sample rate | 48 000 Hz | `media::audio::SAMPLE_RATE` | estimated |
| Channels | 2 | `media::audio::CHANNELS` | estimated |
| Block size | 1024 frames | `media::audio::BLOCK_FRAMES` | estimated |
| Blocks queued per output | 4 | `engine::mixer::MAX_QUEUED_BLOCKS` | estimated |
| Meter floor | −60 dB | `media::mix::METER_FLOOR_DB` | estimated |
| Meter yellow zone from | −20 dB | `ui-egui::docks::METER_YELLOW_DB` | estimated |
| Meter red zone from | −9 dB | `ui-egui::docks::METER_RED_DB` | estimated |
| Meter fall rate | 24 dB/s | `media::mix::METER_FALL_DB_PER_S` | estimated |
| Peak hold | 1.5 s | `media::mix::PEAK_HOLD_S` | estimated |
| Noise gate open / close | −26 dB / −32 dB | `sources::filters::NoiseGate` | estimated |
| Noise gate attack / hold / release | 25 / 200 / 150 ms | `sources::filters::NoiseGate` | estimated |
| Compressor threshold / ratio | −18 dB / 3:1 | `sources::filters::Compressor` | estimated |
| Compressor attack / release | 6 / 60 ms | `sources::filters::Compressor` | estimated |
| Limiter ceiling / release | −1 dB / 60 ms | `sources::filters::Limiter` | estimated |
| Limiter look-ahead | 5 ms | `craft_audio_dsp::effects::dynamics::Limiter::LOOKAHEAD_MS` | estimated |
| Equalizer bands | 200 Hz low shelf, 1 kHz peak, 5 kHz high shelf | `sources::filters::Equalizer` | estimated |
| Loudness windows | momentary 400 ms, short-term 3 s | `craft_audio_dsp::loudness` | ITU-R BS.1770-4 and EBU R 128 |

## Recording and streaming

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Recording quality levels (x264 CRF) | Compact 28, High 23, Pristine 16, Lossless | `model::profile::RecordingQuality::quality_level` | estimated |
| Stream video bitrate | 6000 kbps | `model::profile::StreamSettings::default` | estimated |
| Stream audio bitrate | 160 kbps | `model::profile::StreamSettings::default` | estimated |
| Keyframe interval | 2 s | `model::profile::StreamSettings::default` | estimated |
| Default encoder | software x264 | `model::profile::OutputSettings::default` | estimated |
| x264 preset | veryfast | `outputs::encoders::VideoEncoder::schema` | estimated |
| NVENC preset | P5 | `outputs::encoders::VideoEncoder::schema` | estimated |
| H.264 (built in) preset | Speed (of Speed, Balanced and Quality) | `encode::h264::H264::schema` | estimated |
| H.264 (built in) keyframe interval in recordings | 2 s | `encode::h264::RECORDING_KEYFRAME_S` | estimated |
| H.264 (built in) bitrates | 100 to 200,000 kbps | `encode::h264::KBPS` | estimated |
| H.264 (built in) keyframe intervals at a constant bitrate | 1 to 20 s | `encode::h264::KEYFRAME_S` | estimated |
| Largest H.264 picture | 139,264 macroblocks | `encode::h264::MAX_MACROBLOCKS` | ITU-T H.264 Table A-1 (level 6.2) |
| Built-in AAC bitrates | 32 to 512 kbps | `encode::aac::KBPS` | estimated |
| Built-in recording packet queue | 64 MiB and 3 s of media | `outputs::native::QUEUE_BYTES`, `outputs::native::QUEUE_SPAN_NS` | estimated |
| Built-in recording encoder lag before it fails | 10 s | `outputs::native::MAX_LAG_NS` | estimated |
| Recording file name | `YYYY-MM-DD HH-MM-SS.<ext>` | `studio::outputs::recording_path` | estimated |

## Editing

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Snap distance | 8 screen points (Alt turns it off) | `ui-egui::preview::SNAP_POINTS` | estimated |
| Undo steps kept | 200 | `engine::history::LIMIT` | estimated |
| Changes merged into one undo step | same key within 750 ms | `engine::history::COALESCE` | estimated |
| Scene shortcuts | Ctrl+1 to Ctrl+9 | `ui-egui::commands::SCENE_HOTKEYS` | estimated |
| Autosave after a change | 800 ms | `ui-egui::AUTOSAVE` | estimated |
| Status notices shown | 8 s (errors 24 s) | `ui-egui::NOTICE_S` | estimated |
