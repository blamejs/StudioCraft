# Mechanics

This file lists the timing and tuning values StudioCraft uses, where each one lives in the code,
and where it came from. `mechanics.json` holds the same values for tools. No code compares the two
files, so a change to one must be copied to the other by hand.

## Sources of the values

A value marked **estimated** in the Source column is a common choice for studio software, and it
has not been checked against public documentation or measured in another program. A value with a
named source follows that source. When a value is confirmed from public documentation, add the
page's URL as its source in both files, remove the estimate mark here, and set its `estimated`
field to `false` in `mechanics.json`.

## Video

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Canvas size | 1920 × 1080 | `model::profile::VideoSettings::default` | estimated |
| Output size | 1920 × 1080 | `model::profile::VideoSettings::default` | estimated |
| Frame rate | 30 fps | `model::profile::VideoSettings::default` | estimated |
| Largest canvas side | 8192 px | `render::compositor::MAX_CANVAS` | estimated |
| Compositor texture ring | 3 frames | `render::compositor::RING_LEN` | estimated |
| Largest side of a video filter's output | 8192 px | `plugin::filter::MAX_EFFECT_SIDE` | estimated |
| Largest side of a Linux display or window capture | 8192 px | `platform::portal::MAX_SIDE` | estimated |
| Spare frames a Linux display or window capture reuses | 2 | `platform::screen_frames::SPARE_FRAMES` | estimated |
| Spare frames a camera capture reuses | 2 | `platform::screen_frames::SPARE_FRAMES` | estimated |
| Camera format when none is chosen | The largest picture up to 1920 × 1080 at 30 fps or more | `platform::camera::default_format` | estimated |
| Camera frames in a row that cannot be read before the capture fails | 30 | `platform::camera::BAD_FRAMES` | estimated |
| Cameras whose formats are remembered while they are busy | 32 | `platform::camera::KNOWN_CAMERAS` | estimated |
| Gray formats listed for one Linux camera | 64 | `platform::camera_gray::MOST` | estimated |
| Wait for a Linux gray camera frame before the capture looks for a stop | 500 ms | `platform::camera_gray::WAIT` | estimated |
| Buffers a Linux gray camera stream maps | 4 | `platform::camera_gray::BUFFERS` | estimated |
| Wait for a Linux gray camera frame, for the first frame and after a 500 ms wait ran out, before the capture fails | 5 s | `platform::camera_gray::READ_WAIT` | estimated |
| Camera YUV to RGB conversion | BT.601, studio range (Y 16 to 235) | `platform::camera_frames::rgb` | ITU-R BT.601 |
| Wait before a failed camera is opened again | 2 s | `sources::camera::REOPEN` | estimated |
| How often an open Properties window looks for devices plugged in or out | 3 s | `ui-egui::forms::WATCH` | estimated |
| Time without a new camera frame before the camera counts as lost | 5 s | `sources::camera::STALL` | estimated |
| Camera polling rate when no format is chosen | 60 per second | `sources::camera::DEFAULT_FPS` | estimated |
| Crop and pad edges and area | 0 px each (the whole source) | `sources::filters::CropPad` | estimated |
| Color key similarity and smoothness | 400 and 80 (1 to 1000, thousandths of RGB distance) | `sources::filters::ColorKey` | estimated |
| Color key default key color | #00ff00 | `sources::filters::COLOR_KEY_DEFAULT` | estimated |
| Largest side of an image mask picture | 8192 px | `plugin::filter::MAX_EFFECT_SIDE` | estimated |
| Image mask luma weights (color channel) | 0.2126, 0.7152, 0.0722 (BT.709) | `sources::filters::ImageMask` | estimated |
| Fastest scroll speed | 2000 px/s each way | `sources::filters::SCROLL_MAX_SPEED` | estimated |
| Scroll default limited width and height | 100 px | `sources::filters::SCROLL_DEFAULT_LIMIT` | estimated |
| Sharpen Sharpness | 0.08 (0 to 1) | `sources::filters::SHARPEN_DEFAULT` | estimated |
| Sharpen gain at Sharpness 1 | 4 × the difference from the 8 neighbors' average | `sources::filters::SHARPEN_GAIN` | estimated |

## Transitions

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Default transition | Fade | `model::collection::TransitionConfig::default` | estimated |
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
| Audio sync offset | 0 ms (0 to 5000 ms) | `model::collection::AudioState`, `model::collection::MAX_SYNC_OFFSET_MS` | estimated |
| Monitoring mode | Off | `model::collection::MonitorMode` | estimated |
| Monitoring device | The system's default output | `model::profile::DEFAULT_AUDIO_DEVICE` | estimated |
| Blocks queued for the monitoring device | 4 | `studio::monitor::MONITOR_QUEUE_BLOCKS` | estimated |
| Blocks the monitoring device waits for before it plays | 2 | `studio::monitor::PRIME_BLOCKS` | estimated |
| Linux capture format | 48 000 Hz, 2 channels, 32-bit float | `platform::audio::imp::CAPTURE_FORMAT` | estimated |
| Linux device list wait for the sound server | 2 s | `platform::audio::imp::LIST_TIMEOUT` | estimated |
| Linux capture conversion buffer | 8192 samples | `platform::audio::imp::SCRATCH_SAMPLES` | estimated |
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
| Loudness windows | Momentary 400 ms, short-term 3 s | `craft_audio_dsp::loudness` | ITU-R BS.1770-4 and EBU R 128 |

## Recording and streaming

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Recording quality levels (x264 CRF) | Compact 28, High 23, Pristine 16, Lossless (no CRF: x264 records with `-qp 0`, and the ffmpeg NVENC H.264 and HEVC encoders record with `-tune lossless`; every other encoder, NVENC AV1 included, returns an error for Lossless) | `model::profile::RecordingQuality::quality_level` | estimated |
| Stream video bitrate | 6000 kbps | `model::profile::StreamSettings::default` | estimated |
| Stream audio bitrate | 160 kbps | `model::profile::StreamSettings::default` | estimated |
| Keyframe interval | 2 s | `model::profile::StreamSettings::default` | estimated |
| Stream encoder | H.264 (built in) | `model::profile::StreamSettings::default` | estimated |
| Stream destinations in a profile | at most 16 | `model::profile::MAX_DESTINATIONS` | estimated |
| Stream destination ring | 4 s of the stream | `outputs::stream::RING_SECONDS` | estimated |
| Waits before a stream destination tries again | 2, 4, 8 and 16 s, then every 30 s | `outputs::stream::RETRY_DELAYS` | estimated |
| Failed tries in a row before a stream destination gives up | 10 | `outputs::stream::MAX_TRIES` | estimated |
| Stream connection that counts as lasting | 30 s | `outputs::stream::STABLE_AFTER` | estimated |
| Default encoder | Software x264 | `model::profile::OutputSettings::default` | estimated |
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
| Wait for one hardware encoder test | 5 s | `studio::outputs::PROBE_TIMEOUT` | estimated |
| Wait for the hardware encoder tests of one start together | 10 s | `studio::outputs::PROBE_BUDGET` | estimated |
| Recording file name | `YYYY-MM-DD HH-MM-SS.<ext>` | `studio::outputs::recording_path` | estimated |
| Replay buffer length | 20 s (5 to 3600 s) | `model::profile::ReplaySettings` | estimated |
| Replay buffer memory limit | 512 MB (32 to 16384 MB; the code counts 1024 × 1024 bytes per MB) | `model::profile::ReplaySettings` | estimated |
| Replay buffer encoders | H.264 (built in) at the recording's quality, AAC at 192 kbps. A recording set to Lossless gives the replay buffer High. | `studio::outputs::plan` | estimated |
| Replays saved at once (waiting for the buffer or being written) | 8 | `outputs::replay::MAX_SAVES` | estimated |
| Replay save wait for the buffer's packets | 5 s | `outputs::replay::ANSWER_WAIT` | estimated |
| Saved replay file name | `Replay YYYY-MM-DD HH-MM-SS.mkv` | `studio::outputs::replay_path` | estimated |
| Audio tracks file format | WAV, 32-bit float, 48 kHz, stereo, with a `bext` time reference | `audio_tracks::wav::header` | estimated |
| Audio tracks write buffer | 96 MiB (16 to 1024 MiB) | `model::profile::AudioTracksSettings` | estimated |
| Audio tracks file size before the next part | 1 GiB (1 to 3 GiB) | `model::profile::AudioTracksSettings` | estimated |
| Audio tracks tap queue and pool | 32 and 34 blocks per track | `media::tap::TAP_QUEUE_BLOCKS`, `media::tap::TAP_POOL_BLOCKS` | estimated |
| Audio tracks chunk | 1 MiB | `audio_tracks::collect::CHUNK_BYTES` | estimated |
| Audio tracks header rewrite and sync | every 1 s and every 5 s | `audio_tracks::HEADER_EVERY`, `audio_tracks::SYNC_EVERY` | estimated |
| A source added back continues its file within | 60 s | `audio_tracks::collect::REJOIN_SECONDS` | estimated |
| Audio tracks stop at free space below | 1 GiB or 120 s of the running outputs, whichever is more | `audio_tracks::space::reserve` | estimated |
| Audio tracks low-space warning | the stop point plus 600 s of the tracks and outputs | `audio_tracks::space::warning` | estimated |
| Audio tracks folder | `<recording name> Audio/Audio Files`, or `Stream YYYY-MM-DD HH-MM-SS Audio/Audio Files` for a stream alone | `studio::audio_tracks::take_place` | estimated |
| Audio tracks takes finishing before the next take waits | 2 | `studio::audio_tracks::MAX_FINISHING` | estimated |
| Audio tracks wait for takes when the app closes | 10 s | `studio::audio_tracks::CLOSE_WAIT` | estimated |
| Audio tracks session file | `<recording name> Audio/<recording name>.scraft` (session format version 1) | `audio_tracks::session::path_for` | estimated |
| Markers and dropouts a session lists | 10,000 | `audio_tracks::session::MAX_LOCATIONS` | estimated |
| Markers waiting for the disk thread | 1,024 | `audio_tracks::markers::QUEUED` | estimated |
| Characters kept of a marker's name | 256 | `audio_tracks::markers::MAX_NAME_CHARS` | estimated |
| After a take | Nothing | `model::profile::AfterStop` | estimated |
| Audio editor looked up on the `PATH` | `soundcraft` | `studio::audio_editor::EDITOR_PROGRAM` | estimated |
| Openings of a session or folder at once | 4 | `studio::audio_tracks::MAX_OPENING` | estimated |
| Pause screen countdown | 300 s (0 for none, up to 43,200 s) | `model::profile::PauseSettings` | estimated |
| Pause screen countdown text | `Back in {time}`, then `Back in a moment` at zero (at most 200 characters each) | `model::profile::PauseSettings` | estimated |
| Pause screen at zero | Keep the pause screen | `model::profile::PauseAtZero` | estimated |
| Pause screen countdown format | `m:ss`, and `h:mm:ss` from one hour, with the seconds rounded up | `studio::pause::countdown_text` | estimated |
| Pause screen text | bottom center, inside a margin of 5% of each side, 6% of the canvas height (2 to 15%, and 6 to 1,024 pixels), white with a 4-pixel black outline (0 to 64) | `model::profile::PauseSettings`, `studio::pause::text_position`, `studio::pause::text_px` | estimated |
| Pause screen background | `#101014ff`, with the picture or video fitted inside the canvas | `model::profile::PauseSettings` | estimated |
| Pause screen program audio | muted, with the transition's fade | `model::profile::PauseSettings`, `engine::mixer::MixerCmd::Hold` | estimated |

## Editing

| Value | Setting | Code | Source |
| --- | --- | --- | --- |
| Snap distance | 8 screen points (Alt turns it off) | `ui-egui::preview::SNAP_POINTS` | estimated |
| Undo steps kept | 200 | `engine::history::LIMIT` | estimated |
| Changes merged into one undo step | Same key within 750 ms | `engine::history::COALESCE` | estimated |
| Scene shortcuts | Ctrl+1 to Ctrl+9 | `ui-egui::commands::SCENE_HOTKEYS` | estimated |
| Autosave after a change | 800 ms | `ui-egui::AUTOSAVE` | estimated |
| Dock widths when none is set (the last shown dock takes the rest) | Scenes 300, Sources 340, Audio Mixer 400, Application Audio 360 points | `ui-egui::strip::DEFAULT_WIDTHS` | estimated |
| Undocked dock window | The dock's default width by 200 points to start, at least 200 by 120 points | `ui-egui::StudioApp::dock_windows` | estimated |
| Saved dock layouts | 32 | `ui-egui::store::MAX_DOCK_LAYOUTS` | estimated |
| Status notices shown | 8 s (errors 24 s) | `ui-egui::NOTICE_S` | estimated |
