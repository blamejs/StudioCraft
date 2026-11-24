use craft_captions::scc::decode_pairs;
use craft_captions::time::FrameRate;

use super::sei::{Codec, InsertError, insert_sei, read_sei, sei_nal};
use super::*;

const NTSC: (u32, u32) = (30_000, 1001);

fn roll(cfg: Config) -> RollUp {
    RollUp::new(NTSC.0, NTSC.1, cfg).unwrap()
}

/// Run `frames` frames from frame `from`, returning each pair with its frame.
fn run(r: &mut RollUp, from: i64, frames: i64) -> Vec<(i64, u8, u8)> {
    let mut out = Vec::new();
    let mut pairs = Vec::new();
    for f in from..from + frames {
        r.frame(&mut pairs);
        out.extend(pairs.iter().map(|p| (f, p[0], p[1])));
    }
    out
}

/// What an independent 608 decoder shows at the end.
fn shown(pairs: &[(i64, u8, u8)]) -> String {
    decode_pairs(pairs.iter().copied(), FrameRate::FPS_29_97).last().map(|c| c.text.clone()).unwrap_or_default()
}

#[test]
fn a_phrase_reads_back_through_a_608_decoder() {
    let mut r = roll(Config::default());
    assert_eq!(r.push("Hello there, world"), 0);
    let pairs = run(&mut r, 0, 60);
    assert_eq!(shown(&pairs), "Hello there, world");
    assert_eq!(r.pending(), 0);
}

#[test]
fn every_pair_has_odd_parity() {
    let mut r = roll(Config::default());
    r.push("Odd parity on every byte, É ♪ and ü too");
    for (_, a, b) in run(&mut r, 0, 120) {
        assert_eq!(a.count_ones() % 2, 1, "{a:#x}");
        assert_eq!(b.count_ones() % 2, 1, "{b:#x}");
    }
}

#[test]
fn phrases_continue_the_line_and_wrap_at_32_columns() {
    let mut r = roll(Config::default());
    r.push("the quick brown fox jumps");
    r.push("over the lazy dog");
    let text = shown(&run(&mut r, 0, 200));
    assert_eq!(text, "the quick brown fox jumps over\nthe lazy dog");
    for line in text.lines() {
        assert!(line.chars().count() <= COLUMNS, "{line}");
    }
}

#[test]
fn a_word_longer_than_a_row_is_broken_at_the_edge() {
    let mut r = roll(Config::default());
    let long = "x".repeat(40);
    r.push(&long);
    assert_eq!(shown(&run(&mut r, 0, 200)), format!("{}\n{}", "x".repeat(32), "x".repeat(8)));
}

#[test]
fn only_the_set_rows_stay_up() {
    let mut r = roll(Config { rows: 2, ..Config::default() });
    for w in ["one", "two", "three"] {
        r.push(&format!("{w} {}", "y".repeat(30)));
    }
    let text = shown(&run(&mut r, 0, 400));
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.ends_with(&"y".repeat(30)), "{text}");
    assert!(!text.contains("one"), "{text}");
}

#[test]
fn extended_and_special_characters_show_and_others_are_counted() {
    let mut r = roll(Config::default());
    assert_eq!(r.push("Éa ♪ 漢x"), 1);
    assert_eq!(shown(&run(&mut r, 0, 60)), "Éa ♪ x");
}

#[test]
fn a_viewer_who_joins_mid_stream_sees_the_next_line() {
    let mut r = roll(Config::default());
    r.push(&format!("{} second line", "a".repeat(30)));
    let pairs = run(&mut r, 0, 100);
    // Leave out the first line's start and its first characters.
    let late: Vec<_> = pairs.into_iter().skip(10).collect();
    assert!(shown(&late).ends_with("second line"), "{}", shown(&late));
}

#[test]
fn field_1_pairs_go_out_at_30000_over_1001_a_second() {
    for (num, den, frames) in [(60, 1, 600), (24, 1, 240), (30_000, 1001, 300), (10, 1, 100), (50, 1, 500)] {
        let mut r = RollUp::new(num, den, Config { clear_after_ms: 0, ..Config::default() }).unwrap();
        r.push(&"word ".repeat(400));
        let sent = run(&mut r, 0, frames).len() as f64;
        let seconds = frames as f64 * f64::from(den) / f64::from(num);
        let expected = seconds * 30_000.0 / 1001.0;
        assert!((sent - expected).abs() <= 1.0, "{num}/{den}: {sent} pairs, {expected} expected");
    }
}

#[test]
fn an_empty_or_unshowable_phrase_sends_nothing_and_keeps_the_silence() {
    let mut r = roll(Config { clear_after_ms: 1000, ..Config::default() });
    assert_eq!((r.push(""), r.push("   "), r.push("漢字")), (0, 0, 2));
    assert_eq!(r.pending(), 0);
    assert!(run(&mut r, 0, 100).is_empty(), "nothing was shown, so nothing is erased");
    r.push("hi");
    let mut pairs = run(&mut r, 100, 20);
    // A phrase with nothing to show does not put the erase off.
    r.push("漢");
    pairs.extend(run(&mut r, 120, 20));
    let cues = decode_pairs(pairs.iter().copied(), FrameRate::FPS_29_97);
    assert_eq!(cues.last().map(|c| c.text.as_str()), Some("hi"));
    assert!(cues.last().unwrap().end.0 < FrameRate::FPS_29_97.tick_of(140).0);
}

#[test]
fn a_frame_without_text_carries_nothing() {
    let mut r = roll(Config::default());
    let mut pairs = vec![[1, 2]];
    r.frame(&mut pairs);
    assert!(pairs.is_empty());
}

#[test]
fn text_after_a_silence_goes_out_at_the_field_1_rate() {
    for (num, den, most) in [(30_000, 1001, 1), (60, 1, 1), (10, 1, 3)] {
        let mut r = RollUp::new(num, den, Config { clear_after_ms: 0, ..Config::default() }).unwrap();
        assert!(run(&mut r, 0, 31).is_empty());
        r.push("hi");
        let mut pairs = Vec::new();
        r.frame(&mut pairs);
        assert!(pairs.len() <= most, "{num}/{den}: {} pairs after the silence", pairs.len());
    }
}

#[test]
fn captions_are_erased_after_a_silence() {
    let mut r = roll(Config { clear_after_ms: 1000, ..Config::default() });
    r.push("gone soon");
    let mut pairs = run(&mut r, 0, 30);
    assert_eq!(shown(&pairs), "gone soon");
    pairs.extend(run(&mut r, 30, 60));
    let cues = decode_pairs(pairs.iter().copied(), FrameRate::FPS_29_97);
    let last = cues.last().unwrap();
    assert_eq!(last.text, "gone soon");
    // The cue ended when the erase came, about 1 s after the text went out.
    assert!(last.end.0 < FrameRate::FPS_29_97.tick_of(60).0, "{:?}", last.end);
    // Erased once, the captions are not erased again while nothing is said.
    assert!(run(&mut r, 90, 300).is_empty());
    // Text after the erase starts again on a fresh line.
    r.push("back again");
    pairs.extend(run(&mut r, 390, 60));
    assert_eq!(shown(&pairs), "back again");
}

#[test]
fn text_faster_than_the_stream_keeps_the_newest_and_stays_bounded() {
    let mut r = roll(Config::default());
    for i in 0..20_000 {
        r.push(&format!("w{i}"));
        assert!(r.pending() <= MAX_PENDING + LINE_START + COLUMNS, "{}", r.pending());
    }
    assert!(r.dropped() > 0);
    let text = shown(&run(&mut r, 0, 4000));
    assert!(text.ends_with("w19999"), "{text}");
}

#[test]
fn bad_settings_are_errors() {
    assert_eq!(RollUp::new(30, 1, Config { rows: 5, ..Config::default() }).unwrap_err(), Error::Rows(5));
    assert_eq!(RollUp::new(30, 1, Config { rows: 1, ..Config::default() }).unwrap_err(), Error::Rows(1));
    assert_eq!(RollUp::new(0, 1, Config::default()).unwrap_err(), Error::FrameRate { num: 0, den: 1 });
    assert_eq!(RollUp::new(30, 0, Config::default()).unwrap_err(), Error::FrameRate { num: 30, den: 0 });
}

/// A length-prefixed access unit of `nals`.
fn au(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

const PAIRS: [[u8; 2]; 3] = [[0x94, 0x26], [0x94, 0x26], [0xc8, 0xe9]];

#[test]
fn an_h264_sei_holds_the_atsc_closed_caption_payload() {
    let mut nal = Vec::new();
    sei_nal(Codec::H264, &PAIRS, &mut nal);
    let expected =
        [0x06, 4, 20, 0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xC3, 0xFF, 0xFC, 0x94, 0x26, 0xFC, 0x94, 0x26, 0xFC, 0xc8, 0xe9, 0xFF, 0x80];
    assert_eq!(nal, expected);
}

#[test]
fn the_sei_goes_before_the_first_picture_and_reads_back() {
    for (codec, aud, sps, slice) in [
        (Codec::H264, vec![0x09, 0xf0], vec![0x67, 1, 2, 3], vec![0x65, 0x88, 0x84]),
        (Codec::Hevc, vec![35 << 1, 1, 0x50], vec![33 << 1, 1, 9], vec![19 << 1, 1, 0xaf]),
    ] {
        let mut nal = Vec::new();
        sei_nal(codec, &PAIRS, &mut nal);
        let mut unit = au(&[&aud, &sps, &slice]);
        insert_sei(codec, &mut unit, &nal).unwrap();
        assert_eq!(unit, au(&[&aud, &sps, &nal, &slice]), "{codec:?}");
        assert_eq!(read_sei(codec, &unit), PAIRS.to_vec(), "{codec:?}");
    }
}

#[test]
fn a_bad_access_unit_is_left_as_it_was() {
    let mut nal = Vec::new();
    sei_nal(Codec::H264, &PAIRS, &mut nal);
    let mut no_picture = au(&[&[0x67, 1], &[0x68, 2]]);
    let before = no_picture.clone();
    assert_eq!(insert_sei(Codec::H264, &mut no_picture, &nal), Err(InsertError::NoPicture));
    assert_eq!(no_picture, before);
    let mut cut = au(&[&[0x65, 1, 2, 3]]);
    cut.truncate(6);
    let before = cut.clone();
    assert_eq!(insert_sei(Codec::H264, &mut cut, &nal), Err(InsertError::Truncated));
    assert_eq!(cut, before);
    let mut short = vec![0, 0];
    assert_eq!(insert_sei(Codec::H264, &mut short, &nal), Err(InsertError::Truncated));
    assert_eq!(insert_sei(Codec::H264, &mut Vec::new(), &nal), Err(InsertError::NoPicture));
}

#[test]
fn zero_runs_get_emulation_prevention_and_lose_it_on_reading() {
    for raw in [vec![0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4], vec![0; 9], vec![0, 0, 3, 0, 0], vec![1, 2, 3]] {
        let (mut out, mut zeros) = (Vec::new(), 0);
        for &b in &raw {
            sei::escaped(&mut out, &mut zeros, b);
        }
        // 00 00 03 is the prevention byte itself; 00 00 00, 01 and 02 must not appear.
        assert!(out.windows(3).all(|w| !(w[0] == 0 && w[1] == 0 && w[2] <= 2)), "{out:x?}");
        assert!(out.windows(4).all(|w| !(w[0] == 0 && w[1] == 0 && w[2] == 3 && w[3] > 3)), "{out:x?}");
        assert_eq!(sei::unescape(&out), raw);
    }
    // Pairs of zero bytes read back through a whole NAL unit.
    let pairs = [[0, 0], [0, 0], [0, 1]];
    let mut nal = Vec::new();
    sei_nal(Codec::H264, &pairs, &mut nal);
    assert_eq!(read_sei(Codec::H264, &au(&[&nal, &[0x65, 1]])), pairs.to_vec());
}

#[test]
fn more_than_31_pairs_are_cut_to_the_largest_count() {
    let pairs = vec![[0x80, 0x80]; 40];
    let mut nal = Vec::new();
    sei_nal(Codec::H264, &pairs, &mut nal);
    assert_eq!(read_sei(Codec::H264, &au(&[&nal])).len(), MAX_PAIRS_PER_FRAME);
}

#[test]
fn hostile_access_units_are_read_without_a_panic() {
    let mut nal = Vec::new();
    sei_nal(Codec::H264, &PAIRS, &mut nal);
    let good = au(&[&nal, &[0x65, 1]]);
    for cut in 0..good.len() {
        let _ = read_sei(Codec::H264, &good[..cut]);
        let _ = read_sei(Codec::Hevc, &good[..cut]);
    }
    // A count larger than the triplets that follow, and payload sizes past the end.
    let lying = [0x06, 4, 10, 0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xDF, 0xFF, 0x80];
    assert!(read_sei(Codec::H264, &au(&[&lying])).is_empty());
    let huge = [0x06, 4, 0xFF, 0xFF, 0xFF, 0x10, 0x80];
    assert!(read_sei(Codec::H264, &au(&[&huge])).is_empty());
    assert!(read_sei(Codec::H264, &[0xFF; 9]).is_empty());
}

#[test]
fn other_sei_messages_and_other_user_data_are_passed_over() {
    // A recovery point message first, then user data registered for another country.
    let other = [0x06, 6, 1, 0x84, 4, 14, 0xB4, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xC1, 0xFF, 0xFC, 0x94, 0x26, 0xFF, 0x80];
    assert!(read_sei(Codec::H264, &au(&[&other])).is_empty());
    let mut nal = Vec::new();
    sei_nal(Codec::H264, &PAIRS, &mut nal);
    assert_eq!(read_sei(Codec::H264, &au(&[&other, &nal])), PAIRS.to_vec());
}

#[test]
fn a_roll_up_stream_reads_back_from_its_access_units() {
    // Both sides: the writer's pairs go through SEI into access units, and come back out to the
    // independent decoder.
    let mut r = roll(Config::default());
    r.push("round trip through the stream");
    let (mut frames, mut pairs, mut nal) = (Vec::new(), Vec::new(), Vec::new());
    for f in 0..60 {
        r.frame(&mut pairs);
        let mut unit = au(&[&[0x65, 0x88]]);
        if !pairs.is_empty() {
            nal.clear();
            sei_nal(Codec::H264, &pairs, &mut nal);
            insert_sei(Codec::H264, &mut unit, &nal).unwrap();
        }
        frames.extend(read_sei(Codec::H264, &unit).into_iter().map(|p| (f, p[0], p[1])));
    }
    assert_eq!(shown(&frames), "round trip through the stream");
}
