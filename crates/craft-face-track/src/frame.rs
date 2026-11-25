//! Camera pictures and the rotated square crops the models look at.
//!
//! A [`Frame`] borrows an RGBA8 picture with any row stride, as cameras deliver them. [`crop`]
//! samples a rotated square [`Roi`] of it straight into a model's input buffer, so no full-size
//! float copy of the picture is ever made.

use crate::{Error, Result};

/// A borrowed RGBA8 picture: `height` rows of `stride` bytes, of which the first `width × 4` are
/// pixels. Alpha is ignored.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    rgba: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl<'a> Frame<'a> {
    /// A picture of `width × height` pixels whose rows start `stride` bytes apart.
    pub fn new(rgba: &'a [u8], width: usize, height: usize, stride: usize) -> Result<Frame<'a>> {
        let row = width.checked_mul(4).ok_or_else(|| Error::Frame("the picture is too wide".into()))?;
        if width == 0 || height == 0 {
            return Err(Error::Frame("the picture is empty".into()));
        }
        if stride < row {
            return Err(Error::Frame(format!("rows of {stride} bytes are shorter than {width} pixels")));
        }
        let need = stride.checked_mul(height - 1).and_then(|n| n.checked_add(row)).ok_or_else(|| Error::Frame("the picture is too large".into()))?;
        if rgba.len() < need {
            return Err(Error::Frame(format!("{} bytes do not hold a {width} × {height} picture", rgba.len())));
        }
        Ok(Frame { rgba, width, height, stride })
    }

    /// A tightly packed picture (rows of `width × 4` bytes).
    pub fn packed(rgba: &'a [u8], width: usize, height: usize) -> Result<Frame<'a>> {
        Frame::new(rgba, width, height, width.saturating_mul(4))
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The red, green and blue of the pixel at `(x, y)`, which must be inside the picture.
    fn rgb(&self, x: usize, y: usize) -> [f32; 3] {
        let at = y * self.stride + x * 4;
        match self.rgba.get(at..at + 3) {
            Some(p) => [p[0] as f32, p[1] as f32, p[2] as f32],
            None => [0.0; 3],
        }
    }
}

/// A rotated square region of a picture: center, side and angle (radians, the direction of the
/// square's x axis in the picture, y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Roi {
    pub center: [f32; 2],
    pub size: f32,
    pub angle: f32,
}

impl Roi {
    /// Picture position of the normalized square position `(u, v)` (0 to 1).
    pub fn to_frame(&self, u: f32, v: f32) -> [f32; 2] {
        let (s, c) = self.angle.sin_cos();
        let (x, y) = ((u - 0.5) * self.size, (v - 0.5) * self.size);
        [self.center[0] + c * x - s * y, self.center[1] + s * x + c * y]
    }

    /// Whether the region can be sampled: a finite center and angle and a side of more than 4 pixels.
    pub fn is_usable(&self) -> bool {
        self.center.iter().all(|v| v.is_finite()) && self.angle.is_finite() && self.size.is_finite() && self.size > 4.0
    }
}

/// What a [`crop`] reads outside the picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    /// Black (the face detector's input).
    Zero,
    /// The nearest edge pixel (the face mesh's input).
    Replicate,
}

/// Sample `roi` into the `n×n` RGB tensor `out` (bilinear), each channel mapped from 0 to 255 onto
/// `lo` to `hi`.
///
/// The square's corners map to the output corners `(0,0)` to `(n,n)`: output pixel `i` is read at
/// `u = i/n`, not at its center, the source position is read as a pixel-center index with no
/// half-pixel shift, and the result is rounded to a whole 0 to 255 value before the range map. This
/// is how the models' own pipeline samples its crops, so the models see the inputs they were
/// trained on. Outside the picture it reads `border`. An `out` shorter than `n·n·3` is left as it
/// is.
pub fn crop(frame: &Frame<'_>, roi: &Roi, n: usize, [lo, hi]: [f32; 2], border: Border, out: &mut [f32]) {
    let Some(len) = n.checked_mul(n).and_then(|v| v.checked_mul(3)) else { return };
    let Some(out) = out.get_mut(..len) else { return };
    let (wi, hi_) = (frame.width as i64, frame.height as i64);
    let px = |x: i64, y: i64| -> [f32; 3] {
        let (x, y) = match border {
            Border::Zero if x < 0 || y < 0 || x >= wi || y >= hi_ => return [0.0; 3],
            Border::Zero => (x, y),
            Border::Replicate => (x.clamp(0, wi - 1), y.clamp(0, hi_ - 1)),
        };
        frame.rgb(x as usize, y as usize)
    };
    let scale = (hi - lo) / 255.0;
    let (s, c) = roi.angle.sin_cos();
    for j in 0..n {
        for i in 0..n {
            let (u, v) = ((i as f32 / n as f32 - 0.5) * roi.size, (j as f32 / n as f32 - 0.5) * roi.size);
            let (x, y) = (roi.center[0] + c * u - s * v, roi.center[1] + s * u + c * v);
            let (x0, y0) = (x.floor(), y.floor());
            let (fx, fy) = (x - x0, y - y0);
            // Saturating casts: NaN becomes 0 and an absurd position reads the border.
            let (x0, y0) = (x0 as i64, y0 as i64);
            let (x1, y1) = (x0.saturating_add(1), y0.saturating_add(1));
            let (a, b, cc, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
            let at = (j * n + i) * 3;
            for k in 0..3 {
                let v = (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (cc[k] * (1.0 - fx) + d[k] * fx) * fy;
                let v = if v.is_finite() { v.round().clamp(0.0, 255.0) } else { 0.0 };
                out[at + k] = lo + v * scale;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4×4 picture whose red channel ramps 0, 16, …, 240 in reading order, with blue 255.
    fn ramp() -> Vec<u8> {
        (0..16).flat_map(|i| [(i * 16) as u8, 0, 255, 255]).collect()
    }

    #[test]
    fn frames_check_their_size() {
        let px = ramp();
        assert!(Frame::packed(&px, 4, 4).is_ok());
        assert!(Frame::packed(&px, 4, 5).is_err());
        assert!(Frame::packed(&px, 0, 4).is_err());
        assert!(Frame::new(&px, 4, 4, 15).is_err());
        assert!(Frame::new(&px, usize::MAX, 1, usize::MAX).is_err());
        assert!(Frame::new(&px, 1, usize::MAX, 4).is_err());
        // A stride wider than the row: the last row needs only its pixels.
        assert!(Frame::new(&px, 2, 2, 32).is_ok());
    }

    #[test]
    fn roi_maps_corners_and_rotates() {
        let r = Roi { center: [10.0, 20.0], size: 4.0, angle: 0.0 };
        assert_eq!(r.to_frame(0.0, 0.0), [8.0, 18.0]);
        assert_eq!(r.to_frame(1.0, 1.0), [12.0, 22.0]);
        // A quarter turn: the square's x axis points down the picture.
        let q = Roi { angle: std::f32::consts::FRAC_PI_2, ..r };
        let p = q.to_frame(1.0, 0.5);
        assert!((p[0] - 10.0).abs() < 1e-5 && (p[1] - 22.0).abs() < 1e-5);
        assert!(Roi { size: 9.0, ..r }.is_usable());
        assert!(!Roi { center: [f32::NAN, 0.0], size: 9.0, angle: 0.0 }.is_usable());
        assert!(!Roi { size: 9.0, angle: f32::INFINITY, ..r }.is_usable());
        // A side of 4 pixels or less is too small to sample.
        assert!(!r.is_usable());
    }

    #[test]
    fn crop_samples_corner_aligned_and_rounds() {
        let px = ramp();
        let f = Frame::packed(&px, 4, 4).unwrap();
        let mut c = vec![0.0; 48];
        crop(&f, &Roi { center: [2.0, 2.0], size: 4.0, angle: 0.0 }, 4, [0.0, 1.0], Border::Zero, &mut c);
        // Cropped 1:1 the ramp comes back: output pixel i is at u = i/n, a source pixel center.
        for i in 0..16 {
            assert!((c[i * 3] - (i * 16) as f32 / 255.0).abs() < 1e-6, "{i}");
            assert!((c[i * 3 + 2] - 1.0).abs() < 1e-6);
        }
        let at = |cx: f32| {
            let mut c = vec![0.0; 48];
            crop(&f, &Roi { center: [cx, 2.0], size: 4.0, angle: 0.0 }, 4, [0.0, 255.0], Border::Zero, &mut c);
            c[0]
        };
        // Half a pixel to the right is halfway between ramp values 0 and 16.
        assert!((at(2.5) - 8.0).abs() < 1e-3, "{}", at(2.5));
        // Rounded to a whole 0-255 value: a third of the way from 0 to 16 is 5.33.
        assert!((at(2.0 + 1.0 / 3.0) - 5.0).abs() < 1e-3, "{}", at(2.0 + 1.0 / 3.0));
    }

    #[test]
    fn crop_reads_rows_by_stride() {
        // The ramp with 8 bytes of padding after each row.
        let px: Vec<u8> = ramp().chunks(16).flat_map(|r| r.iter().copied().chain([7; 8])).collect();
        let f = Frame::new(&px, 4, 4, 24).unwrap();
        let mut c = vec![0.0; 48];
        crop(&f, &Roi { center: [2.0, 2.0], size: 4.0, angle: 0.0 }, 4, [0.0, 255.0], Border::Zero, &mut c);
        for i in 0..16 {
            assert_eq!(c[i * 3], (i * 16) as f32, "{i}");
        }
    }

    #[test]
    fn crop_borders_are_zero_or_replicated() {
        let px = ramp();
        let f = Frame::packed(&px, 4, 4).unwrap();
        let far = Roi { center: [-10.0, -10.0], size: 4.0, angle: 0.0 };
        let mut c = vec![9.0; 12];
        crop(&f, &far, 2, [-1.0, 1.0], Border::Zero, &mut c);
        assert!(c.iter().all(|v| *v == -1.0));
        // Replicated, the top-left pixel (0, 0, 255) fills the crop.
        crop(&f, &far, 2, [0.0, 255.0], Border::Replicate, &mut c);
        for p in c.chunks(3) {
            assert!(p[0].abs() < 1e-3 && p[1].abs() < 1e-3 && (p[2] - 255.0).abs() < 1e-3, "{p:?}");
        }
        let edge = Roi { center: [0.0, 2.0], size: 4.0, angle: 0.0 };
        let (mut z, mut r) = (vec![0.0; 48], vec![0.0; 48]);
        crop(&f, &edge, 4, [0.0, 255.0], Border::Zero, &mut z);
        crop(&f, &edge, 4, [0.0, 255.0], Border::Replicate, &mut r);
        assert!(z[2] == 0.0 && (r[2] - 255.0).abs() < 1e-3, "{} {}", z[2], r[2]);
    }

    #[test]
    fn crop_survives_absurd_regions_and_short_outputs() {
        let px = ramp();
        let f = Frame::packed(&px, 4, 4).unwrap();
        for roi in [Roi { center: [1e30, -1e30], size: 1e30, angle: 1.0 }, Roi { center: [f32::NAN; 2], size: f32::INFINITY, angle: f32::NAN }] {
            for border in [Border::Zero, Border::Replicate] {
                let mut c = vec![0.5; 48];
                crop(&f, &roi, 4, [-1.0, 1.0], border, &mut c);
                assert!(c.iter().all(|v| v.is_finite() && (-1.0..=1.0).contains(v)));
            }
        }
        let mut short = vec![3.0; 47];
        crop(&f, &Roi { center: [2.0, 2.0], size: 4.0, angle: 0.0 }, 4, [0.0, 1.0], Border::Zero, &mut short);
        assert!(short.iter().all(|v| *v == 3.0));
    }
}
