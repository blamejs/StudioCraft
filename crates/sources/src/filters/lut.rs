//! The Apply LUT filter: a color lookup table from a `.cube` or `.png` file.
//!
//! A 3D table of `n` x `n` x `n` colors is uploaded as one picture `n * n` pixels wide and
//! `2 * n` high: the slice for blue index `b` starts at x = `b * n`, red runs along x within the
//! slice and green along y. Each value is stored as 16 bits, its high byte in the top `n` rows
//! and its low byte in the same place in the bottom `n` rows. The shader blends the four nearest
//! colors in each of the two nearest slices in both halves, so it never reads across a slice's
//! edge, and joins the two blends into one value.

use std::io::Read;
use std::path::Path;

use studiocraft_model::{Field, FieldKind, PathMode, Settings, SettingsSchema};
use studiocraft_plugin::{EffectImage, Filter, FilterInstance, FilterTarget, MAX_EFFECT_SIDE, Plugin, PluginError, VIDEO_EFFECT_PARAMS, VideoEffect};

use super::{Params, checked, checked_text, float_field};
use crate::image::{load_within, open_file};

/// The largest 3D table, in colors along each side: its picture is `n * n` pixels wide, which
/// must fit in [`MAX_EFFECT_SIDE`].
pub const LUT_MAX_SIZE: u32 = 90;
/// The number of colors along each side of the 3D table a 1D table becomes, at most.
pub const LUT_1D_LATTICE: u32 = 64;
/// The largest 1D table, in entries.
pub const LUT_MAX_1D_SIZE: u32 = 65536;
/// The largest `.cube` file read, in bytes.
pub const LUT_MAX_FILE: u64 = 64 * 1024 * 1024;

const _: () = assert!(LUT_MAX_SIZE * LUT_MAX_SIZE <= MAX_EFFECT_SIDE, "the largest table's picture must fit in a filter image");

/// The file extensions the file dialog offers.
const LUT_EXTENSIONS: [&str; 2] = ["cube", "png"];

/// A 3D color table: `size`³ colors with red changing fastest, then green, then blue, and the
/// input range each channel's index covers.
#[derive(Clone, Debug, PartialEq)]
pub struct Lut {
    pub size: u32,
    pub colors: Vec<[f32; 3]>,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
}

impl Lut {
    /// The color at red, green and blue indices `r`, `g`, `b`.
    fn at(&self, r: u32, g: u32, b: u32) -> [f32; 3] {
        let n = self.size as usize;
        let i = (b as usize * n + g as usize) * n + r as usize;
        self.colors.get(i).copied().unwrap_or([0.0; 3])
    }

    /// The range the table's picture stores for each channel, as its minimum and its width:
    /// from the channel's lowest value to its highest, widened to take in 0 to 1. A width is not
    /// a number when the channel's values span more than the largest `f32`.
    pub fn output_range(&self) -> ([f32; 3], [f32; 3]) {
        let (mut lo, mut hi) = ([0.0f32; 3], [1.0f32; 3]);
        for color in &self.colors {
            for c in 0..3 {
                (lo[c], hi[c]) = (lo[c].min(color[c]), hi[c].max(color[c]));
            }
        }
        (lo, [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]])
    }

    /// The table as the picture the shader reads (see the module comment), each value stored
    /// as its place in [`Lut::output_range`] rounded to 16 bits; `None` when the size is outside
    /// 2 to [`LUT_MAX_SIZE`], the table does not hold `size`³ colors, or the output range's
    /// width is not a finite number.
    pub fn picture(&self) -> Option<EffectImage> {
        let n = self.size;
        let (lo, span) = self.output_range();
        if !(2..=LUT_MAX_SIZE).contains(&n) || self.colors.len() != (n * n * n) as usize || !span.iter().all(|w| w.is_finite()) {
            return None;
        }
        let word = |c: usize, v: f32| (((v - lo[c]) / span[c]).clamp(0.0, 1.0) * 65535.0).round() as u16;
        let half = (n * n * n * 4) as usize;
        let (mut high, mut low) = (Vec::with_capacity(half * 2), Vec::with_capacity(half));
        for g in 0..n {
            for b in 0..n {
                for r in 0..n {
                    let [vr, vg, vb] = self.at(r, g, b);
                    let [cr, cg, cb] = [word(0, vr), word(1, vg), word(2, vb)];
                    high.extend_from_slice(&[(cr >> 8) as u8, (cg >> 8) as u8, (cb >> 8) as u8, 255]);
                    low.extend_from_slice(&[cr as u8, cg as u8, cb as u8, 255]);
                }
            }
        }
        high.append(&mut low);
        EffectImage::new(n * n, 2 * n, high)
    }
}

fn bad(line: usize, what: impl std::fmt::Display) -> String {
    format!("line {line}: {what}")
}

/// Three finite numbers.
fn triple(words: &[&str], line: usize) -> Result<[f32; 3], String> {
    let [a, b, c] = words else { return Err(bad(line, "expected three numbers")) };
    let num = |w: &str| w.parse::<f32>().ok().filter(|v| v.is_finite()).ok_or_else(|| bad(line, format!("`{w}` is not a number")));
    Ok([num(a)?, num(b)?, num(c)?])
}

fn size_of(words: &[&str], line: usize, max: u32) -> Result<u32, String> {
    let [w] = words else { return Err(bad(line, "expected one size")) };
    let n = w.parse::<u32>().map_err(|_| bad(line, format!("`{w}` is not a whole number")))?;
    if !(2..=max).contains(&n) {
        return Err(bad(line, format!("a size of {n} is not supported; it must be from 2 to {max}")));
    }
    Ok(n)
}

/// Read a `.cube` table: `LUT_3D_SIZE` or `LUT_1D_SIZE`, optional `DOMAIN_MIN` and
/// `DOMAIN_MAX` (or `LUT_3D_INPUT_RANGE` and `LUT_1D_INPUT_RANGE`), then one red, green, blue
/// line per entry. Lines starting with `#` and other keywords, `TITLE` among them, are skipped.
/// A keyword that sets the size or the input range may appear only once. A 1D table becomes a
/// 3D table of up to [`LUT_1D_LATTICE`] colors a side.
pub fn parse_cube(text: &str) -> Result<Lut, String> {
    const CHANNELS: [&str; 3] = ["red", "green", "blue"];
    let (mut size3, mut size1) = (None, None);
    let (mut lo, mut hi) = ([0.0f32; 3], [1.0f32; 3]);
    // The lines that set the input range's minimum and maximum.
    let (mut lo_line, mut hi_line) = (None::<usize>, None::<usize>);
    let mut rows: Vec<[f32; 3]> = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        // No line the table needs has more than four words, so a fifth only marks the line as
        // too long, and a line of any length costs no allocation.
        let mut buf = [""; 5];
        let mut count = 0;
        for (slot, w) in buf.iter_mut().zip(raw.split_whitespace()) {
            *slot = w;
            count += 1;
        }
        let words = buf.get(..count).unwrap_or(&[]);
        let Some((&head, rest)) = words.split_first() else { continue };
        if head.starts_with('#') {
            continue;
        }
        // A spelling that reads as a number which is not finite, such as `nan` or `inf`, is a
        // color line, and `triple` refuses it.
        let numeric = head.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+' || c == '.') || head.parse::<f32>().is_ok();
        if numeric {
            let want = match (size3, size1) {
                (Some(n), None) => u64::from(n).pow(3),
                (None, Some(n)) => u64::from(n),
                _ => return Err(bad(line, "a color comes before LUT_3D_SIZE or LUT_1D_SIZE")),
            };
            if rows.len() as u64 >= want {
                return Err(bad(line, format!("the table has more than its {want} entries")));
            }
            rows.push(triple(words, line)?);
            continue;
        }
        if !rows.is_empty() {
            return Err(bad(line, format!("`{head}` comes after the table's colors")));
        }
        let once = |given: bool| if given { Err(bad(line, format!("`{head}` appears more than once"))) } else { Ok(()) };
        let range_again = |prev: Option<usize>| match prev {
            Some(p) => Err(bad(line, format!("`{head}` gives the input range again after line {p}"))),
            None => Ok(()),
        };
        match head {
            "LUT_3D_SIZE" => {
                once(size3.is_some())?;
                size3 = Some(size_of(rest, line, LUT_MAX_SIZE)?);
            }
            "LUT_1D_SIZE" => {
                once(size1.is_some())?;
                size1 = Some(size_of(rest, line, LUT_MAX_1D_SIZE)?);
            }
            "DOMAIN_MIN" => {
                range_again(lo_line)?;
                (lo, lo_line) = (triple(rest, line)?, Some(line));
            }
            "DOMAIN_MAX" => {
                range_again(hi_line)?;
                (hi, hi_line) = (triple(rest, line)?, Some(line));
            }
            "LUT_3D_INPUT_RANGE" | "LUT_1D_INPUT_RANGE" => {
                range_again(lo_line.or(hi_line))?;
                let [a, b] = rest else { return Err(bad(line, "expected two numbers")) };
                let [a, b, _] = triple(&[*a, *b, "0"], line)?;
                (lo, hi, lo_line, hi_line) = ([a; 3], [b; 3], Some(line), Some(line));
            }
            _ => {}
        }
        if size3.is_some() && size1.is_some() {
            return Err(bad(line, "the file holds both a 1D and a 3D table; only one is supported"));
        }
    }
    // A fault in the input range is reported at the later of the lines that set it; the
    // default range of 0 to 1 has none.
    let at = lo_line.max(hi_line).map_or_else(String::new, |l| format!("line {l}: "));
    if let Some(c) = (0..3).find(|&c| hi[c] <= lo[c]) {
        let channel = CHANNELS[c];
        return Err(format!("{at}the input range's {channel} maximum ({}) must be above its minimum ({})", hi[c], lo[c]));
    }
    // The shader scales each channel by 1 over the range's width, which must be a number.
    if let Some(c) = (0..3).find(|&c| {
        let width = hi[c] - lo[c];
        !width.is_finite() || !(1.0 / width).is_finite()
    }) {
        let channel = CHANNELS[c];
        return Err(format!("{at}the {channel} input range from {} to {} is too narrow or too wide", lo[c], hi[c]));
    }
    let (want, n) = match (size3, size1) {
        (Some(n), None) => (u64::from(n).pow(3), n),
        (None, Some(n)) => (u64::from(n), n),
        _ => return Err("the file has no LUT_3D_SIZE or LUT_1D_SIZE line".into()),
    };
    if rows.len() as u64 != want {
        return Err(format!("the table has {} of its {want} entries", rows.len()));
    }
    let lut = if size3.is_some() { Lut { size: n, colors: rows, domain_min: lo, domain_max: hi } } else { lattice_from_1d(&rows, lo, hi) };
    // The shader reads each value back as its place in its channel's output range, whose width
    // must be a number.
    let (out_lo, span) = lut.output_range();
    if let Some(c) = (0..3).find(|&c| !span[c].is_finite()) {
        let out_hi = lut.colors.iter().fold(1.0f32, |hi, v| hi.max(v[c]));
        let channel = CHANNELS[c];
        return Err(format!("the table's {channel} values from {} to {out_hi} span too wide a range", out_lo[c]));
    }
    Ok(lut)
}

/// The 3D table that applies the 1D table `rows` to each channel on its own.
fn lattice_from_1d(rows: &[[f32; 3]], lo: [f32; 3], hi: [f32; 3]) -> Lut {
    let len = rows.len().max(1);
    let size = (len as u32).clamp(2, LUT_1D_LATTICE);
    // The 1D table's value for channel `c` at lattice index `k`, read between its entries in
    // `f64`, where the difference of two finite `f32` values is finite.
    let curve = |c: usize, k: u32| {
        let pos = f64::from(k) / f64::from(size - 1) * (len - 1) as f64;
        let i = (pos.floor() as usize).min(len - 1);
        let j = (i + 1).min(len - 1);
        let f = pos - i as f64;
        let (a, b) = (rows.get(i).map_or(0.0, |r| f64::from(r[c])), rows.get(j).map_or(0.0, |r| f64::from(r[c])));
        (a + (b - a) * f) as f32
    };
    let mut colors = Vec::with_capacity((size * size * size) as usize);
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                colors.push([curve(0, r), curve(1, g), curve(2, b)]);
            }
        }
    }
    Lut { size, colors, domain_min: lo, domain_max: hi }
}

/// Read a table from a picture laid out as one row of `n` tiles of `n` x `n` pixels (`n * n`
/// wide and `n` high), or as a square grid of `t` x `t` tiles of `n` x `n` pixels with
/// `n = t * t`. Within a tile red runs left to right and green top to bottom; blue picks the
/// tile, left to right and then top to bottom.
pub fn lut_from_picture(width: u32, height: u32, rgba: &[u8]) -> Result<Lut, String> {
    let unsupported = || {
        format!(
            "a {width} x {height} picture is not a color table; use a row of n tiles of n x n pixels, or a square grid of tiles, with at most {LUT_MAX_SIZE} colors a side"
        )
    };
    let (n, per_row) = if height >= 2 && u64::from(width) == u64::from(height) * u64::from(height) {
        (height, height)
    } else if width == height {
        // The side is t³ for a grid of t x t tiles.
        let t = (2..=16u32).find(|t| t * t * t == width).ok_or_else(unsupported)?;
        (t * t, t)
    } else {
        return Err(unsupported());
    };
    if n > LUT_MAX_SIZE {
        return Err(unsupported());
    }
    let mut colors = Vec::with_capacity((n * n * n) as usize);
    for b in 0..n {
        let (tx, ty) = ((b % per_row) * n, (b / per_row) * n);
        for g in 0..n {
            for r in 0..n {
                let i = ((u64::from(ty + g) * u64::from(width) + u64::from(tx + r)) * 4) as usize;
                let px = rgba.get(i..i + 3).ok_or_else(unsupported)?;
                colors.push([f32::from(px[0]) / 255.0, f32::from(px[1]) / 255.0, f32::from(px[2]) / 255.0]);
            }
        }
    }
    Ok(Lut { size: n, colors, domain_min: [0.0; 3], domain_max: [1.0; 3] })
}

/// Read the table in `path`: a picture for a `.png` file, otherwise a `.cube` table. Either is
/// refused before it is read when the path is not a file (a named pipe among them) or the file
/// is larger than [`LUT_MAX_FILE`], and a picture with more pixels than the largest table's
/// (`LUT_MAX_SIZE`³) is refused before it is decoded.
pub fn load_lut(path: &str) -> Result<Lut, PluginError> {
    let p = Path::new(path);
    let failed = |e: String| PluginError::Failed(format!("{path}: {e}"));
    let is_png = p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("png"));
    if is_png {
        let (w, h, rgba) = load_within(p, LUT_MAX_FILE, u64::from(LUT_MAX_SIZE).pow(3))?;
        return lut_from_picture(w, h, &rgba).map_err(failed);
    }
    let io = |e: std::io::Error| failed(e.to_string());
    let file = open_file(p).map_err(io)?;
    let too_large = || failed(format!("the file is larger than {} MB", LUT_MAX_FILE / (1024 * 1024)));
    if file.metadata().map_err(io)?.len() > LUT_MAX_FILE {
        return Err(too_large());
    }
    // The file can grow after the size check; the read stops one byte past the cap.
    let mut bytes = Vec::new();
    file.take(LUT_MAX_FILE + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > LUT_MAX_FILE {
        return Err(too_large());
    }
    let text = String::from_utf8(bytes).map_err(|_| failed("the file is not text".into()))?;
    parse_cube(&text).map_err(failed)
}

const APPLY_LUT: &str = r"
fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // p0.x: colors along each side of the table, p0.y: the amount from 0 to 1. p1.xyz: the
    // input range's minimum, extra_params()[0].xyz: 1 over its width. extra_params()[1].xyz and
    // extra_params()[2].xyz: each channel's minimum and width of the range the picture stores.
    let a = color.a;
    if a <= 0.0 {
        return color;
    }
    let n = max(p0.x, 2.0);
    let rgb = clamp(color.rgb / a, vec3<f32>(0.0), vec3<f32>(1.0));
    let c = clamp((rgb - p1.xyz) * extra_params()[0].xyz, vec3<f32>(0.0), vec3<f32>(1.0)) * (n - 1.0);
    let b0 = floor(c.b);
    let b1 = min(b0 + 1.0, n - 1.0);
    // The high bytes are in the picture's top half and the low bytes in its bottom half.
    let y = (c.g + 0.5) / (2.0 * n);
    let x0 = (b0 * n + c.r + 0.5) / (n * n);
    let x1 = (b1 * n + c.r + 0.5) / (n * n);
    let high = mix(image_at(vec2<f32>(x0, y)).rgb, image_at(vec2<f32>(x1, y)).rgb, c.b - b0);
    let low = mix(image_at(vec2<f32>(x0, y + 0.5)).rgb, image_at(vec2<f32>(x1, y + 0.5)).rgb, c.b - b0);
    let stored = (high * 65280.0 + low * 255.0) / 65535.0;
    // The table's values are read back and blended before the result is clipped to 0..1.
    let looked = stored * extra_params()[2].xyz + extra_params()[1].xyz;
    return vec4<f32>(clamp(mix(rgb, looked, p0.y), vec3<f32>(0.0), vec3<f32>(1.0)) * a, a);
}
";

/// Changes a source's colors through a color lookup table read from a `.cube` or `.png` file.
///
/// Amount blends the original colors (0%) with the table's (100%). The table is read when the
/// filter starts or its file changes; with no file the filter leaves the source as it is.
/// Transparency is kept.
pub struct ApplyLut;

impl Plugin for ApplyLut {
    fn id(&self) -> &'static str {
        "apply_lut"
    }
    fn name(&self) -> &'static str {
        "Apply LUT"
    }
    fn schema(&self) -> SettingsSchema {
        let extensions = LUT_EXTENSIONS.map(String::from).to_vec();
        SettingsSchema::new(vec![
            Field::new("file", "LUT file", FieldKind::Path { mode: PathMode::OpenFile, filter_name: "Color lookup tables".into(), extensions }, ""),
            float_field("amount", "Amount (%)", 0.0, 100.0, 1.0, 100.0),
        ])
    }
}

impl Filter for ApplyLut {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut f = LutInstance { file: String::new(), table: None, params: [0.0; VIDEO_EFFECT_PARAMS] };
        f.update(settings)?;
        Ok(Box::new(f))
    }
}

/// A table as the shader reads it.
#[derive(Clone)]
struct Table {
    image: EffectImage,
    size: u32,
    domain_min: [f32; 3],
    domain_max: [f32; 3],
    /// Each channel's minimum and width of the range `image` stores.
    out_min: [f32; 3],
    out_span: [f32; 3],
}

struct LutInstance {
    /// The file `table` was read from.
    file: String,
    table: Option<Table>,
    params: Params,
}

impl FilterInstance for LutInstance {
    /// A rejected update, a file that cannot be read included, leaves the filter as it was.
    fn update(&mut self, s: &Settings) -> Result<(), PluginError> {
        let file = checked_text(s, "file")?;
        let amount = checked(s, "amount", 100.0, 0.0, 100.0, false)?;
        let table = if file == self.file {
            self.table.clone()
        } else if file.trim().is_empty() {
            None
        } else {
            let lut = load_lut(file)?;
            let image = lut.picture().ok_or_else(|| PluginError::Failed(format!("{file}: the table is too large")))?;
            let (out_min, out_span) = lut.output_range();
            Some(Table { image, size: lut.size, domain_min: lut.domain_min, domain_max: lut.domain_max, out_min, out_span })
        };
        let mut p = [0.0; VIDEO_EFFECT_PARAMS];
        p[1] = (amount / 100.0) as f32;
        if let Some(t) = &table {
            p[0] = t.size as f32;
            p[4..7].copy_from_slice(&t.domain_min);
            for c in 0..3 {
                p[8 + c] = 1.0 / (t.domain_max[c] - t.domain_min[c]);
            }
            p[12..15].copy_from_slice(&t.out_min);
            p[16..19].copy_from_slice(&t.out_span);
        }
        (self.file, self.table, self.params) = (file.to_string(), table, p);
        Ok(())
    }
    fn video_effect(&self) -> Option<VideoEffect> {
        let table = self.table.as_ref()?;
        Some(VideoEffect { image: Some(table.image.clone()), ..VideoEffect::new("apply_lut", APPLY_LUT, self.params) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity table of `n` colors a side, as `.cube` text.
    fn identity_cube(n: u32) -> String {
        let mut s = format!("TITLE \"identity\"\n# a comment\nLUT_3D_SIZE {n}\n");
        let v = |i: u32| f64::from(i) / f64::from(n - 1);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    s.push_str(&format!("{} {} {}\n", v(r), v(g), v(b)));
                }
            }
        }
        s
    }

    #[test]
    fn a_cube_table_is_read_with_red_changing_fastest() {
        let text = "LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n\
            0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n0.5 0.25 0.125\n";
        let lut = parse_cube(text).unwrap();
        assert_eq!(lut.size, 2);
        assert_eq!(lut.at(1, 0, 0), [1.0, 0.0, 0.0]);
        assert_eq!(lut.at(0, 1, 0), [0.0, 1.0, 0.0]);
        assert_eq!(lut.at(0, 0, 1), [0.0, 0.0, 1.0]);
        assert_eq!(lut.at(1, 1, 1), [0.5, 0.25, 0.125]);
        assert_eq!(parse_cube(&identity_cube(17)).unwrap().colors.len(), 17 * 17 * 17);
        // A file saved with a byte order mark reads the same.
        assert_eq!(parse_cube(&format!("\u{feff}{text}")).unwrap(), lut);
        assert_eq!(parse_cube(&text.replace('\n', "\r\n")).unwrap(), lut);
    }

    #[test]
    fn a_cube_input_range_is_read_in_both_spellings() {
        let body = "0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        let lut = parse_cube(&format!("DOMAIN_MIN 0 0.1 0.2\nDOMAIN_MAX 2 3 4\nLUT_3D_SIZE 2\n{body}")).unwrap();
        assert_eq!((lut.domain_min, lut.domain_max), ([0.0, 0.1, 0.2], [2.0, 3.0, 4.0]));
        let lut = parse_cube(&format!("LUT_3D_SIZE 2\nLUT_3D_INPUT_RANGE -0.5 1.5\n{body}")).unwrap();
        assert_eq!((lut.domain_min, lut.domain_max), ([-0.5; 3], [1.5; 3]));
        // A narrow range is kept as it is; one whose width is not a number is refused.
        let lut = parse_cube(&format!("LUT_3D_SIZE 2\nDOMAIN_MAX 0.0000001 1 1\n{body}")).unwrap();
        assert_eq!(lut.domain_max[0], 1e-7);
        for range in ["DOMAIN_MIN -3e38 0 0\nDOMAIN_MAX 3e38 1 1", "DOMAIN_MIN 0 0 0\nDOMAIN_MAX 1e-45 1 1"] {
            let err = parse_cube(&format!("LUT_3D_SIZE 2\n{range}\n{body}")).unwrap_err();
            assert!(err.contains("too narrow or too wide"), "{range}: {err}");
        }
    }

    #[test]
    fn a_1d_cube_table_becomes_a_3d_table_that_maps_each_channel() {
        // Each channel squared, sampled at 0, 0.5 and 1.
        let lut = parse_cube("LUT_1D_SIZE 3\n0 0 0\n0.25 0.25 0.25\n1 1 1\n").unwrap();
        assert_eq!(lut.size, 3);
        assert_eq!(lut.at(1, 0, 2), [0.25, 0.0, 1.0]);
        assert_eq!(lut.at(2, 1, 0), [1.0, 0.25, 0.0]);
        // A long table is read between its entries at a lattice of at most 64 colors.
        let mut long = format!("LUT_1D_SIZE {}\n", 1000);
        for i in 0..1000 {
            let v = f64::from(i) / 999.0;
            long.push_str(&format!("{v} {v} {v}\n"));
        }
        let lut = parse_cube(&long).unwrap();
        assert_eq!(lut.size, LUT_1D_LATTICE);
        let [r, g, b] = lut.at(21, 42, 63);
        assert!((r - 21.0 / 63.0).abs() < 1e-3 && (g - 42.0 / 63.0).abs() < 1e-3 && (b - 1.0).abs() < 1e-6, "{r} {g} {b}");
    }

    #[test]
    fn a_cube_table_with_a_fault_is_refused_with_its_line() {
        let eight = "0 0 0\n".repeat(8);
        let cases: [(String, &str); 14] = [
            (String::new(), "no LUT_3D_SIZE"),
            ("TITLE \"x\"\n".into(), "no LUT_3D_SIZE"),
            ("0 0 0\n".into(), "line 1: a color comes before"),
            ("LUT_3D_SIZE 1\n".into(), "line 1: a size of 1"),
            (format!("LUT_3D_SIZE {}\n", LUT_MAX_SIZE + 1), "line 1: a size of 91"),
            ("LUT_3D_SIZE 4294967296\n".into(), "not a whole number"),
            ("LUT_3D_SIZE two\n".into(), "not a whole number"),
            ("LUT_3D_SIZE 2\n0 0 0\n".into(), "has 1 of its 8 entries"),
            (format!("LUT_3D_SIZE 2\n{eight}0 0 0\n"), "line 10: the table has more than its 8 entries"),
            ("LUT_3D_SIZE 2\n0 0\n".into(), "line 2: expected three numbers"),
            ("LUT_3D_SIZE 2\n0 nan 0\n".into(), "line 2: `nan` is not a number"),
            ("LUT_3D_SIZE 2\n0 1e999 0\n".into(), "line 2: `1e999` is not a number"),
            (format!("LUT_3D_SIZE 2\nDOMAIN_MAX 1 0 1\n{eight}"), "line 2: the input range's green maximum (0) must be above its minimum (0)"),
            ("LUT_3D_SIZE 2\nLUT_1D_SIZE 2\n".into(), "line 2: the file holds both"),
        ];
        for (text, want) in cases {
            let err = parse_cube(&text).unwrap_err();
            assert!(err.contains(want), "{text:?}: {err}");
        }
        let err = parse_cube(&format!("LUT_3D_SIZE 2\n0 0 0\nDOMAIN_MIN 0 0 0\n{eight}")).unwrap_err();
        assert!(err.contains("line 3: `DOMAIN_MIN` comes after"), "{err}");
        // A line of a million numbers is refused as a line with more than three.
        let long = format!("LUT_3D_SIZE 2\n{}\n", "0 ".repeat(1_000_000));
        assert!(parse_cube(&long).unwrap_err().contains("line 2: expected three numbers"));
        let long = format!("LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0 0\n{eight}");
        assert!(parse_cube(&long).unwrap_err().contains("line 2: expected three numbers"));
        // A long title is skipped.
        assert!(parse_cube(&format!("TITLE \"{}\"\nLUT_3D_SIZE 2\n{eight}", "word ".repeat(1000))).is_ok());
    }

    #[test]
    fn the_table_picture_puts_each_blue_slice_beside_the_last() {
        let lut = parse_cube(&identity_cube(3)).unwrap();
        let image = lut.picture().unwrap();
        assert_eq!((image.width(), image.height()), (9, 6));
        let px = |x: usize, y: usize| {
            let i = (y * 9 + x) * 4;
            image.rgba()[i..i + 4].to_vec()
        };
        // x = b * 3 + r, y = g for the high bytes and 3 + g for the low bytes.
        assert_eq!(px(0, 0), [0, 0, 0, 255]);
        assert_eq!(px(2, 0), [255, 0, 0, 255]);
        assert_eq!(px(0, 2), [0, 255, 0, 255]);
        assert_eq!(px(3, 0), [0, 0, 128, 255]);
        assert_eq!(px(8, 2), [255, 255, 255, 255]);
        assert_eq!((px(3, 3), px(8, 5)), (vec![0, 0, 0, 255], vec![255, 255, 255, 255]));
        // 0.1 is stored as 6554 of 65535: a high byte of 25 and a low byte of 154.
        let tenth = Lut { size: 2, colors: vec![[0.1; 3]; 8], domain_min: [0.0; 3], domain_max: [1.0; 3] }.picture().unwrap();
        assert_eq!((&tenth.rgba()[..4], &tenth.rgba()[32..36]), (&[25, 25, 25, 255][..], &[154, 154, 154, 255][..]));
        // One large value leaves the others close: 0.5 in a range of 0 to 100 reads back within
        // 0.001.
        let mut colors = vec![[0.5; 3]; 8];
        colors[7] = [100.0; 3];
        let outlier = Lut { size: 2, colors, domain_min: [0.0; 3], domain_max: [1.0; 3] };
        let (lo, span) = outlier.output_range();
        let rgba = outlier.picture().unwrap().rgba().to_vec();
        let back = (f32::from(rgba[0]) * 256.0 + f32::from(rgba[32])) / 65535.0 * span[0] + lo[0];
        assert!((back - 0.5).abs() < 0.001, "{back}");
        // Each channel has its own range: a large blue value leaves a red ramp of 0 to 1 whole.
        let mut colors: Vec<[f32; 3]> = (0..8).map(|i| [(i % 2) as f32, 0.0, 0.0]).collect();
        colors[7][2] = 1e6;
        let blue = Lut { size: 2, colors, domain_min: [0.0; 3], domain_max: [1.0; 3] };
        assert_eq!(blue.output_range(), ([0.0; 3], [1.0, 1.0, 1e6]));
        assert_eq!(blue.picture().unwrap().rgba()[4..8], [255, 0, 0, 255]);
        // Colors outside 0..1 are stored as their place in their channel's output range, here
        // -1 to 1 for red and 0 to 2 for green.
        let wide = Lut { size: 2, colors: vec![[-1.0, 2.0, 0.5]; 8], domain_min: [0.0; 3], domain_max: [1.0; 3] };
        assert_eq!(wide.output_range(), ([-1.0, 0.0, 0.0], [2.0, 2.0, 1.0]));
        assert_eq!(wide.picture().unwrap().rgba()[..4], [0, 255, 128, 255]);
        assert_eq!(lut.output_range(), ([0.0; 3], [1.0; 3]));
        let largest = parse_cube(&identity_cube(LUT_MAX_SIZE)).unwrap().picture().unwrap();
        assert_eq!((largest.width(), largest.height()), (LUT_MAX_SIZE * LUT_MAX_SIZE, 2 * LUT_MAX_SIZE));
    }

    /// The identity table of `t * t` colors a side, as a square grid of tiles.
    fn grid(t: u32) -> (u32, Vec<u8>) {
        let n = t * t;
        let side = n * t;
        let mut rgba = vec![0u8; (side * side * 4) as usize];
        let v = |i: u32| (f64::from(i) * 255.0 / f64::from(n - 1)).round() as u8;
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let (x, y) = ((b % t) * n + r, (b / t) * n + g);
                    let i = ((y * side + x) * 4) as usize;
                    rgba[i..i + 4].copy_from_slice(&[v(r), v(g), v(b), 255]);
                }
            }
        }
        (side, rgba)
    }

    #[test]
    fn a_picture_table_is_read_from_a_row_or_a_grid_of_tiles() {
        let (side, rgba) = grid(4);
        assert_eq!(side, 64);
        let lut = lut_from_picture(side, side, &rgba).unwrap();
        assert_eq!(lut.size, 16);
        assert_eq!(lut.at(15, 0, 0), [1.0, 0.0, 0.0]);
        assert_eq!(lut.at(0, 15, 0), [0.0, 1.0, 0.0]);
        assert_eq!(lut.at(0, 0, 15), [0.0, 0.0, 1.0]);
        assert_eq!(lut.at(5, 10, 6), [85.0 / 255.0, 170.0 / 255.0, 102.0 / 255.0]);
        // The row layout is the top half of the picture the shader reads, so it reads back
        // unchanged.
        let row = parse_cube(&identity_cube(5)).unwrap().picture().unwrap();
        let top = &row.rgba()[..row.rgba().len() / 2];
        let back = lut_from_picture(row.width(), row.height() / 2, top).unwrap();
        assert_eq!(&back.picture().unwrap().rgba()[..top.len()], top);
        for (w, h) in [(10, 3), (65, 65), (1, 1), (0, 0), (512 * 512, 512)] {
            assert!(lut_from_picture(w, h, &[0; 16]).is_err(), "{w} x {h}");
        }
        assert!(lut_from_picture(64, 64, &[0; 16]).is_err(), "too few bytes");
    }

    #[test]
    fn the_filter_reads_its_settings_and_its_file_once() {
        let dir = std::env::temp_dir().join(format!("studiocraft-lut-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("look.cube");
        std::fs::write(&file, format!("DOMAIN_MIN 0.1 0.2 0.3\nDOMAIN_MAX 0.9 0.8 0.7\n{}", identity_cube(3))).unwrap();
        let file = file.to_string_lossy().into_owned();
        assert!(ApplyLut.create(&Settings::new()).unwrap().video_effect().is_none());
        assert!(ApplyLut.create(&ApplyLut.schema().defaults()).unwrap().video_effect().is_none());
        let s = Settings::new().with("file", file.as_str()).with("amount", 25.0);
        let mut f = ApplyLut.create(&s).unwrap();
        let e = f.video_effect().unwrap();
        assert_eq!(e.params[..2], [3.0, 0.25]);
        assert_eq!((&e.params[12..15], &e.params[16..19]), (&[0.0; 3][..], &[1.0; 3][..]));
        assert_eq!(e.params[4..7], [0.1, 0.2, 0.3]);
        assert_eq!(e.params[8..11], [1.0 / (0.9f32 - 0.1), 1.0 / (0.8f32 - 0.2), 1.0 / (0.7f32 - 0.3)]);
        let image = e.image.clone().unwrap();
        assert_eq!((image.width(), image.height()), (9, 6));
        // Amount changes without reading the file again, even after it is gone.
        std::fs::remove_file(&file).unwrap();
        f.update(&s.clone().with("amount", 50.0)).unwrap();
        let e = f.video_effect().unwrap();
        assert_eq!((e.image, e.params[1]), (Some(image), 0.5));
        f.update(&Settings::new().with("file", "")).unwrap();
        assert!(f.video_effect().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_filter_refuses_bad_settings_and_files_and_keeps_the_last_good_ones() {
        use serde_json::{Value, json};
        let dir = std::env::temp_dir().join(format!("studiocraft-lut-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        std::fs::write(path("good.cube"), identity_cube(2)).unwrap();
        std::fs::write(path("short.cube"), "LUT_3D_SIZE 2\n0 0 0\n").unwrap();
        std::fs::write(path("binary.cube"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(path("empty.cube"), "").unwrap();
        std::fs::write(path("broken.png"), b"not a picture").unwrap();
        ::image::RgbaImage::new(10, 3).save(path("odd.png")).unwrap();
        let bad: [(&str, Value); 10] = [
            ("file", json!(5)),
            ("file", json!(path("missing.cube"))),
            ("file", json!(path("short.cube"))),
            ("file", json!(path("binary.cube"))),
            ("file", json!(path("empty.cube"))),
            ("file", json!(path("broken.png"))),
            ("file", json!(path("odd.png"))),
            ("file", json!(dir.to_string_lossy())),
            ("amount", json!(101)),
            ("amount", Value::from(f64::NAN)),
        ];
        let good = Settings::new().with("file", path("good.cube")).with("amount", 40.0);
        for (key, v) in bad {
            let s = good.clone().with(key, v.clone());
            assert!(ApplyLut.create(&s).is_err(), "{key} = {v}");
            let mut f = ApplyLut.create(&good).unwrap();
            let before = f.video_effect();
            assert!(f.update(&s).is_err(), "{key} = {v}");
            assert_eq!(f.video_effect(), before, "{key} = {v}: the last good settings stay");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lut_file_larger_than_the_caps_is_refused_before_it_is_read() {
        let dir = std::env::temp_dir().join(format!("studiocraft-lut-large-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("large.cube");
        let f = std::fs::File::create(&file).unwrap();
        f.set_len(LUT_MAX_FILE + 1).unwrap();
        drop(f);
        let err = load_lut(&file.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("larger than 64 MB"), "{err}");
        // A picture with more pixels than the largest table is refused before its layout is
        // checked, which needs it decoded.
        let big = dir.join("big.png");
        ::image::RgbaImage::new(1000, 1000).save(&big).unwrap();
        let err = load_lut(&big.to_string_lossy()).unwrap_err().to_string();
        assert!(err.contains("1000000 pixels exceed max 729000"), "{err}");
        let fits = dir.join("fits.png");
        ::image::RgbaImage::new(729, 729).save(&fits).unwrap();
        assert_eq!(load_lut(&fits.to_string_lossy()).unwrap().size, 81);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_finite_first_number_is_refused_with_its_line() {
        let seven = "0 0 0\n".repeat(7);
        for first in ["nan", "NaN", "inf", "-inf", "+Infinity"] {
            let err = parse_cube(&format!("LUT_3D_SIZE 2\n{first} 0 0\n{seven}0 0 0\n")).unwrap_err();
            assert!(err.contains(&format!("line 2: `{first}` is not a number")), "{first}: {err}");
        }
    }

    #[test]
    fn a_repeated_keyword_is_refused_with_its_line() {
        let eight = "0 0 0\n".repeat(8);
        let cases = [
            ("LUT_3D_SIZE 2\nLUT_3D_SIZE 2\n", "line 2: `LUT_3D_SIZE` appears more than once"),
            ("LUT_1D_SIZE 2\nLUT_1D_SIZE 2\n", "line 2: `LUT_1D_SIZE` appears more than once"),
            ("LUT_3D_SIZE 2\nDOMAIN_MAX 1 1 1\nDOMAIN_MAX 0.5 1 1\n", "line 3: `DOMAIN_MAX` gives the input range again after line 2"),
            ("DOMAIN_MIN 0 0 0\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\n", "line 3: `DOMAIN_MIN` gives the input range again after line 1"),
            ("LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nLUT_3D_INPUT_RANGE 0 1\n", "line 3: `LUT_3D_INPUT_RANGE` gives the input range again after line 2"),
            ("LUT_3D_SIZE 2\nLUT_3D_INPUT_RANGE 0 1\nDOMAIN_MAX 1 1 1\n", "line 3: `DOMAIN_MAX` gives the input range again after line 2"),
        ];
        for (head, want) in cases {
            let text = if head.contains("1D") { format!("{head}0 0 0\n0 0 0\n") } else { format!("{head}{eight}") };
            let err = parse_cube(&text).unwrap_err();
            assert!(err.contains(want), "{head:?}: {err}");
        }
    }

    #[test]
    fn a_range_fault_names_the_line_and_the_channel() {
        let eight = "0 0 0\n".repeat(8);
        let err = parse_cube(&format!("LUT_3D_SIZE 2\nDOMAIN_MAX 1 0 1\n{eight}")).unwrap_err();
        assert!(err.contains("line 2: the input range's green maximum (0) must be above its minimum (0)"), "{err}");
        let err = parse_cube(&format!("DOMAIN_MIN 0 0 0\nLUT_3D_SIZE 2\nDOMAIN_MAX 1 1 1e-45\n{eight}")).unwrap_err();
        assert!(err.contains("line 3: the blue input range from 0 to 0.000000000000000000000000000000000000000000001 is too"), "{err}");
    }

    #[test]
    fn a_1d_table_with_extreme_values_never_holds_a_value_that_is_not_a_number() {
        // Values whose span is past the largest number are refused instead of loading a table
        // that holds a value that is not a number.
        let err = parse_cube("LUT_1D_SIZE 2\n-3e38 0 0\n3e38 1 1\n").unwrap_err();
        assert!(err.contains("too wide a range"), "{err}");
        // Values within reach keep their ends.
        let lut = parse_cube("LUT_1D_SIZE 2\n-1e30 0 0\n1e30 1 1\n").unwrap();
        assert_eq!((lut.at(0, 0, 0)[0], lut.at(lut.size - 1, 0, 0)[0]), (-1e30, 1e30));
    }

    #[test]
    fn table_values_outside_0_to_1_are_kept_until_after_they_are_blended() {
        let dir = std::env::temp_dir().join(format!("studiocraft-lut-wide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Red runs from -1 to 1 along the red index; green and blue are the identity.
        let file = dir.join("wide.cube");
        std::fs::write(&file, "LUT_3D_SIZE 2\n-1 0 0\n1 0 0\n-1 1 0\n1 1 0\n-1 0 1\n1 0 1\n-1 1 1\n1 1 1\n").unwrap();
        let s = Settings::new().with("file", &*file.to_string_lossy());
        let e = ApplyLut.create(&s).unwrap().video_effect().unwrap();
        let image = e.image.clone().unwrap();
        // The value the shader reads back for a stored red byte: its place in the stored range.
        let red = |x: usize| f32::from(image.rgba()[x * 4]) / 255.0 * e.params[16] + e.params[12];
        let (low, high) = (red(0), red(1));
        assert!((low + 1.0).abs() < 0.01 && (high - 1.0).abs() < 0.01, "{low} {high}");
        assert!(((low + high) / 2.0).abs() < 0.01, "the middle of the red ramp is about 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A named pipe at `path` that a thread writes `data` into once a reader opens it.
    #[cfg(unix)]
    fn pipe_with(path: &std::path::Path, data: &'static [u8]) -> std::thread::JoinHandle<()> {
        let status = std::process::Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success());
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            use std::io::Write;
            if let Ok(mut w) = std::fs::OpenOptions::new().write(true).open(&path) {
                let _ = w.write_all(data);
            }
        })
    }

    /// Hold `path` open for reading until `writer` has opened it, written and closed it.
    #[cfg(unix)]
    fn release_writer(path: &std::path::Path, writer: std::thread::JoinHandle<()>) {
        use std::os::unix::fs::OpenOptionsExt;
        let reader = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).unwrap();
        writer.join().unwrap();
        drop(reader);
    }

    #[cfg(unix)]
    #[test]
    fn a_named_pipe_is_refused_without_waiting_for_a_writer() {
        let dir = std::env::temp_dir().join(format!("studiocraft-lut-pipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["pipe.cube", "pipe.png"] {
            let path = dir.join(name);
            let writer = pipe_with(&path, b"LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n");
            let result = load_lut(&path.to_string_lossy());
            release_writer(&path, writer);
            let err = result.unwrap_err().to_string();
            assert!(err.contains("is not a file"), "{name}: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
