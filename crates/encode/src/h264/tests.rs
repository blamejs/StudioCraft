use craft_video::h264::decode::{Decoder, Picture};

use super::*;

fn params(width: u32, height: u32, rate: RateControl) -> VideoParams {
    VideoParams { width, height, fps_num: 30, fps_den: 1, rate }
}

/// An NV12 frame: a diagonal luma gradient with a bright box that moves 3 pixels per frame, and a
/// chroma gradient.
fn moving(width: usize, height: usize, n: usize) -> Vec<u8> {
    let mut f = vec![0u8; width * height * 3 / 2];
    let (bx, by) = ((n * 3) % width.saturating_sub(16).max(1), height / 3);
    for y in 0..height {
        for x in 0..width {
            let inside = x >= bx && x < bx + 16 && y >= by && y < by + 16;
            f[y * width + x] = if inside { 220 } else { (16 + (x + y) * 200 / (width + height)) as u8 };
        }
    }
    let uv = &mut f[width * height..];
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            uv[y * width + 2 * x] = (64 + x * 128 / (width / 2)) as u8;
            uv[y * width + 2 * x + 1] = (64 + y * 128 / (height / 2)) as u8;
        }
    }
    f
}

/// Encode `frames` and decode the stream again; the pictures in display order.
fn round_trip(session: &mut dyn VideoSession, frames: &[Vec<u8>]) -> Vec<Picture> {
    let mut packets = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        session.encode(f, i as i64, &mut packets).unwrap();
    }
    session.flush(&mut packets);
    assert_eq!(packets.len(), frames.len(), "one packet per frame");
    let mut dec = Decoder::from_avcc(&session.codec_private()).unwrap();
    let mut pictures = Vec::new();
    for p in &packets {
        pictures.extend(dec.decode(&p.data, p.pts).unwrap());
    }
    pictures.extend(dec.flush());
    pictures.sort_by_key(|p| p.pts);
    pictures
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

fn luma(p: &Picture) -> Vec<u8> {
    (0..p.height as usize).flat_map(|y| p.y[y * p.y_stride..][..p.width as usize].iter().copied()).collect()
}

#[test]
fn a_moving_picture_decodes_at_35_db_or_better() {
    let (w, h) = (320usize, 240usize);
    let frames: Vec<Vec<u8>> = (0..24).map(|n| moving(w, h, n)).collect();
    let settings = Settings::new().with("preset", "quality");
    let mut s = H264.open(&settings, &params(w as u32, h as u32, RateControl::Quality(23))).unwrap();
    let pictures = round_trip(s.as_mut(), &frames);
    assert_eq!(pictures.len(), frames.len());
    for (n, (p, f)) in pictures.iter().zip(&frames).enumerate() {
        assert_eq!((p.width, p.height), (w as u32, h as u32));
        let db = psnr(&luma(p), &f[..w * h]);
        assert!(db >= 35.0, "frame {n}: {db:.1} dB");
    }
}

#[test]
fn a_size_that_is_not_a_multiple_of_16_is_cropped_back() {
    for (w, h) in [(1918u32, 1078u32), (16, 16), (34, 18)] {
        let frames: Vec<Vec<u8>> = (0..2).map(|n| moving(w as usize, h as usize, n)).collect();
        let mut s = H264.open(&Settings::new(), &params(w, h, RateControl::Quality(30))).unwrap();
        let pictures = round_trip(s.as_mut(), &frames);
        assert!(pictures.iter().all(|p| (p.width, p.height) == (w, h)), "{w}×{h}");
    }
}

#[test]
fn sizes_past_the_highest_level_are_refused_with_the_limit() {
    assert!(check_size(8192, 4320).is_ok());
    let Err(e) = H264.open(&Settings::new(), &params(8192, 8192, RateControl::Quality(23))) else { panic!("8192×8192 opened") };
    assert!(e.to_string().contains("139264 macroblocks") && e.to_string().contains("Settings › Video"), "{e}");
    for (w, h) in [(0, 2), (2, 0), (31, 32), (16_386, 16), (16, 16_386)] {
        assert!(check_size(w, h).is_err(), "{w}×{h}");
    }
    let bad_rate = VideoParams { fps_num: 0, ..params(64, 64, RateControl::Quality(23)) };
    assert!(H264.open(&Settings::new(), &bad_rate).is_err());
}

#[test]
fn the_stream_says_bt709_limited_and_a_red_frame_keeps_its_values() {
    // 75% red in BT.709 at limited range (ITU-R BT.709 item 3.2-3.3, scaled to 8 bits).
    let (r, g, b) = (0.75f64, 0.0, 0.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let want = [16.0 + 219.0 * y, 128.0 + 224.0 * (b - y) / 1.8556, 128.0 + 224.0 * (r - y) / 1.5748].map(|v| v.round() as u8);
    assert_eq!(want, [51, 109, 212]);
    let (w, h) = (64usize, 48usize);
    let mut f = vec![want[0]; w * h];
    f.extend(std::iter::repeat_n([want[1], want[2]], w * h / 4).flatten());
    let mut s = H264.open(&Settings::new(), &params(w as u32, h as u32, RateControl::Quality(18))).unwrap();
    let pictures = round_trip(s.as_mut(), &[f.clone(), f]);
    let p = &pictures[0];
    assert_eq!((p.color.primaries, p.color.transfer, p.color.matrix, p.color.full_range), (1, 1, 1, false));
    let near = |plane: &[u8], v: u8| plane.iter().all(|s| s.abs_diff(v) <= 2);
    assert!(near(&p.y, want[0]) && near(&p.u, want[1]) && near(&p.v, want[2]), "{:?} {:?} {:?}", &p.y[..4], &p.u[..4], &p.v[..4]);
}

#[test]
fn bitrate_mode_has_a_fixed_group_of_pictures_and_no_b_frames() {
    let rate = RateControl::Bitrate { kbps: 800, keyframe_s: 1 };
    let c = config(&Settings::new(), &params(64, 48, rate)).unwrap();
    assert_eq!((c.keyint, c.bframes, c.scenecut), (30, 0, false));
    assert_eq!(c.rate, Rate::Cbr { kbps: 800 });
    let mut s = H264Session::new(c).unwrap();
    let mut packets = Vec::new();
    for n in 0..75 {
        s.encode(&moving(64, 48, n * 7), n as i64, &mut packets).unwrap();
    }
    s.flush(&mut packets);
    let keys: Vec<i64> = packets.iter().filter(|p| p.keyframe).map(|p| p.pts).collect();
    assert_eq!(keys, [0, 30, 60]);
    assert!(packets.iter().all(|p| p.pts == p.dts), "frames are coded in display order");
}

#[test]
fn quality_mode_keeps_a_keyframe_every_two_seconds() {
    let c = config(&Settings::new(), &VideoParams { fps_num: 60_000, fps_den: 1001, ..params(64, 48, RateControl::Quality(80)) }).unwrap();
    assert_eq!(c.keyint, 120);
    assert_eq!(c.rate, Rate::Crf(51.0), "the level is clamped to 51");
    assert_eq!(c.format, PacketFormat::LengthPrefixed);
    assert!(c.threads >= 1);
}

#[test]
fn presets_map_an_omitted_one_is_the_fastest_and_others_are_refused() {
    for (value, preset) in [("speed", Preset::Speed), ("balanced", Preset::Balanced), ("quality", Preset::Quality)] {
        let c = config(&Settings::new().with("preset", value), &params(64, 48, RateControl::Quality(23))).unwrap();
        assert_eq!(c.preset, preset, "{value}");
    }
    assert_eq!(config(&Settings::new(), &params(64, 48, RateControl::Quality(23))).unwrap().preset, Preset::Speed);
    for bad in [Settings::new().with("preset", "placebo"), Settings::new().with("preset", ""), Settings::new().with("preset", 3)] {
        let Err(e) = config(&bad, &params(64, 48, RateControl::Quality(23))) else { panic!("{bad:?} was taken") };
        assert!(e.to_string().contains("is not one of speed, balanced, quality"), "{e}");
    }
    let schema = H264.schema();
    assert_eq!(schema.fields.len(), 1);
}

#[test]
fn lossless_is_refused_with_what_to_do() {
    let Err(e) = H264.open(&Settings::new(), &params(64, 48, RateControl::Lossless)) else { panic!("a lossless session opened") };
    assert!(matches!(e, PluginError::Unavailable(_)) && e.to_string().contains("choose another encoder"), "{e}");
}

#[test]
fn a_frame_of_the_wrong_size_is_an_error() {
    let mut s = H264.open(&Settings::new(), &params(64, 48, RateControl::Quality(23))).unwrap();
    let mut out = Vec::new();
    assert!(s.encode(&[0; 10], 0, &mut out).is_err());
    assert!(s.encode(&vec![16; 64 * 48 * 3 / 2], 0, &mut out).is_ok());
}

#[test]
fn chroma_is_split_into_its_two_planes() {
    let (mut u, mut v) = ([0u8; 3], [0u8; 3]);
    split_chroma(&[1, 2, 3, 4, 5, 6], &mut u, &mut v);
    assert_eq!((u, v), ([1, 3, 5], [2, 4, 6]));
    assert_eq!(H264.codec(), Codec::H264);
    assert!(H264.ffmpeg().is_none(), "built in");
}
