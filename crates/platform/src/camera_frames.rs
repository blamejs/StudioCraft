//! Camera frames into RGBA or BGRA pictures: the packed YUV, NV12, gray and RGB layouts cameras
//! send, and MJPEG. Each conversion writes into a frame it is given, reusing that frame's buffer.
//!
//! YUV is read as 8-bit studio range (16 to 235 for luma) with the BT.601 matrix.

use studiocraft_media::video::{MAX_FRAME_SIDE, PixelFormat, VideoFrame};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::camera::{CameraFormat, CameraPixel};

/// Converts the frames of one camera, keeping a buffer for MJPEG frames that need one.
#[derive(Default)]
pub struct Converter {
    scratch: Vec<u8>,
}

impl Converter {
    /// Convert one camera frame of `format` in `bytes` into `frame`. Returns `false` when the
    /// bytes do not hold a whole picture of that format; `frame` may then hold part of one.
    pub fn convert(&mut self, bytes: &[u8], format: CameraFormat, frame: &mut VideoFrame) -> bool {
        let (w, h) = (format.width, format.height);
        match format.pixel {
            CameraPixel::Mjpeg if has_huffman_tables(bytes) => mjpeg(bytes, frame),
            CameraPixel::Mjpeg => {
                // An APP0 `AVI1` segment marks the frame as MJPEG, whose decoder then uses the
                // standard tables.
                let Some(rest) = bytes.get(2..) else { return false };
                self.scratch.clear();
                self.scratch.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x07, b'A', b'V', b'I', b'1', 0]);
                self.scratch.extend_from_slice(rest);
                mjpeg(&self.scratch, frame)
            }
            CameraPixel::Yuyv => yuyv(bytes, w, h, frame),
            CameraPixel::Nv12 => nv12(bytes, w, h, frame),
            CameraPixel::Gray => packed(bytes, w, h, 1, PixelFormat::Rgba8, frame),
            CameraPixel::Rgb => packed(bytes, w, h, 3, PixelFormat::Rgba8, frame),
            CameraPixel::Bgr => packed(bytes, w, h, 3, PixelFormat::Bgra8, frame),
        }
    }
}

/// Whether a JPEG picture defines Huffman tables before its scan. A picture whose markers cannot
/// be followed counts as having them, and its decoding then fails as it would.
fn has_huffman_tables(jpeg: &[u8]) -> bool {
    let mut i = 2usize;
    while let (Some(&0xff), Some(&marker), Some(&hi), Some(&lo)) = (jpeg.get(i), jpeg.get(i + 1), jpeg.get(i + 2), jpeg.get(i + 3)) {
        match marker {
            0xc4 => return true,
            0xda => return false,
            // Fill bytes before a marker.
            0xff => i += 1,
            _ => i = i.saturating_add(2).saturating_add(usize::from(u16::from_be_bytes([hi, lo]))),
        }
    }
    true
}

/// Copy `height` rows of `row` bytes, `stride` bytes apart in `bytes`, into `out` without their
/// padding and top row first. `bottom_up` rows are stored last row first. Returns `false` when the
/// rows do not fit in `bytes`.
#[cfg_attr(not(any(windows, target_os = "macos", target_os = "linux")), allow(dead_code))]
pub fn tight_rows(bytes: &[u8], row: usize, stride: usize, height: usize, bottom_up: bool, out: &mut Vec<u8>) -> bool {
    let Some(need) = height.checked_sub(1).and_then(|h| h.checked_mul(stride)).and_then(|n| n.checked_add(row)) else { return false };
    if row == 0 || stride < row || bytes.len() < need {
        return false;
    }
    out.clear();
    for y in 0..height {
        let src = if bottom_up { height - 1 - y } else { y };
        let Some(r) = bytes.get(src * stride..src * stride + row) else { return false };
        out.extend_from_slice(r);
    }
    true
}

/// When an uncompressed frame's rows carry padding at their ends, or (`bottom_up`) are stored
/// last row first, copy them into `out` tight and top row first and return `true`. The stride
/// is `stride` when the system reports it, otherwise the frame's length over its row count (an
/// NV12 frame has 1.5 rows of bytes per picture row, its chroma plane right after its luma
/// plane). Returns `false` when `bytes` can be read as it is, or cannot be read at all.
#[cfg_attr(not(any(windows, target_os = "macos", target_os = "linux")), allow(dead_code))]
pub fn unpad(bytes: &[u8], format: CameraFormat, stride: Option<usize>, bottom_up: bool, out: &mut Vec<u8>) -> bool {
    let (Ok(w), Ok(h)) = (usize::try_from(format.width), usize::try_from(format.height)) else { return false };
    let (per_pixel, rows) = match format.pixel {
        CameraPixel::Mjpeg => return false,
        CameraPixel::Gray => (1, h),
        CameraPixel::Yuyv => (2, h),
        CameraPixel::Rgb | CameraPixel::Bgr => (3, h),
        CameraPixel::Nv12 => (1, h.saturating_add(h / 2)),
    };
    let Some(row) = w.checked_mul(per_pixel) else { return false };
    if rows == 0 || row == 0 {
        return false;
    }
    let stride = match stride {
        Some(s) => s,
        None if bytes.len().is_multiple_of(rows) => bytes.len() / rows,
        None => return false,
    };
    let tight = row.checked_mul(rows).is_some_and(|n| n == bytes.len());
    if stride < row || (stride == row && tight && !bottom_up) {
        return false;
    }
    tight_rows(bytes, row, stride, rows, bottom_up, out)
}

/// `width * height * per_pixel` as a byte count, when it fits.
fn area(width: u32, height: u32, per_pixel: usize) -> Option<usize> {
    usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?.checked_mul(per_pixel)
}

/// One pixel from studio-range Y, Cb and Cr.
fn rgb(y: u8, u: u8, v: u8) -> [u8; 4] {
    let c = 298 * (i32::from(y) - 16);
    let (d, e) = (i32::from(u) - 128, i32::from(v) - 128);
    let clip = |x: i32| u8::try_from(((x + 128) >> 8).clamp(0, 255)).unwrap_or(0);
    [clip(c + 409 * e), clip(c - 100 * d - 208 * e), clip(c + 516 * d), 255]
}

/// Packed 4:2:2, two pixels in each 4 bytes: Y0 U Y1 V.
fn yuyv(bytes: &[u8], width: u32, height: u32, frame: &mut VideoFrame) -> bool {
    let Some(need) = area(width, height, 2) else { return false };
    if !width.is_multiple_of(2) || bytes.len() < need || !frame.reshape(width, height, PixelFormat::Rgba8) {
        return false;
    }
    for (out, &[y0, u, y1, v]) in frame.data.as_chunks_mut::<8>().0.iter_mut().zip(bytes.as_chunks::<4>().0) {
        let ([a, b], _) = out.as_chunks_mut::<4>() else { return false };
        *a = rgb(y0, u, v);
        *b = rgb(y1, u, v);
    }
    true
}

/// A luma plane followed by interleaved Cb Cr at half width and half height.
fn nv12(bytes: &[u8], width: u32, height: u32, frame: &mut VideoFrame) -> bool {
    let (Some(luma), Ok(w)) = (area(width, height, 1), usize::try_from(width)) else { return false };
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) || bytes.len() < luma + luma / 2 || !frame.reshape(width, height, PixelFormat::Rgba8) {
        return false;
    }
    let (ys, uv) = bytes.split_at(luma);
    for (row, (out, ys)) in frame.data.chunks_exact_mut(w * 4).zip(ys.chunks_exact(w)).enumerate() {
        let Some(uv) = uv.get(row / 2 * w..row / 2 * w + w) else { return false };
        for ((out, &[y0, y1]), &[u, v]) in out.as_chunks_mut::<8>().0.iter_mut().zip(ys.as_chunks::<2>().0).zip(uv.as_chunks::<2>().0) {
            let ([a, b], _) = out.as_chunks_mut::<4>() else { return false };
            *a = rgb(y0, u, v);
            *b = rgb(y1, u, v);
        }
    }
    true
}

/// Gray (1 byte) or RGB / BGR (3 bytes) pixels, made opaque.
fn packed(bytes: &[u8], width: u32, height: u32, per_pixel: usize, format: PixelFormat, frame: &mut VideoFrame) -> bool {
    let Some(need) = area(width, height, per_pixel) else { return false };
    if bytes.len() < need || !frame.reshape(width, height, format) {
        return false;
    }
    let out = frame.data.as_chunks_mut::<4>().0.iter_mut();
    if per_pixel == 1 {
        out.zip(bytes).for_each(|(o, &g)| *o = [g, g, g, 255]);
    } else {
        out.zip(bytes.as_chunks::<3>().0).for_each(|(o, &[a, b, c])| *o = [a, b, c, 255]);
    }
    true
}

/// A JPEG picture, at the size its own header gives. Frames without Huffman tables use the
/// standard ones, as MJPEG cameras expect.
fn mjpeg(bytes: &[u8], frame: &mut VideoFrame) -> bool {
    let side = usize::try_from(MAX_FRAME_SIDE).unwrap_or(usize::MAX);
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(side).set_max_height(side);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    if decoder.decode_headers().is_err() {
        return false;
    }
    let Some((w, h)) = decoder.dimensions() else { return false };
    let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else { return false };
    frame.reshape(w, h, PixelFormat::Rgba8) && decoder.decode_into(&mut frame.data).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(width: u32, height: u32, pixel: CameraPixel) -> CameraFormat {
        CameraFormat { width, height, fps: 30, pixel }
    }

    fn convert(bytes: &[u8], format: CameraFormat, frame: &mut VideoFrame) -> bool {
        Converter::default().convert(bytes, format, frame)
    }

    #[test]
    fn padded_frames_are_unpadded_by_the_stride_their_length_gives() {
        let yuyv = CameraFormat { width: 2, height: 2, fps: 30, pixel: CameraPixel::Yuyv };
        let mut out = Vec::new();
        // Rows of 4 bytes padded to 6.
        let padded = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        assert!(unpad(&padded, yuyv, None, false, &mut out));
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!unpad(&padded[..8], yuyv, None, false, &mut out), "a tight frame is read as it is");
        assert!(!unpad(&padded[..7], yuyv, None, false, &mut out), "a frame of no whole row count is left to the converter");
        // A reported stride wins over the length, which can include space after the picture.
        let mut trailing = padded.to_vec();
        trailing.extend([9; 5]);
        assert!(unpad(&trailing, yuyv, Some(6), false, &mut out));
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(unpad(&[1, 2, 3, 4, 5, 6, 7, 8, 9], yuyv, Some(4), false, &mut out), "tight rows with space after them");
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        // NV12 at 2 x 2: two luma rows and one chroma row, each padded to 3 bytes.
        let nv12 = CameraFormat { pixel: CameraPixel::Nv12, ..yuyv };
        assert!(unpad(&[1, 2, 0, 3, 4, 0, 5, 6, 0], nv12, None, false, &mut out));
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
        // A tight bottom-up BGR frame is only flipped.
        let bgr = CameraFormat { width: 1, height: 2, fps: 30, pixel: CameraPixel::Bgr };
        assert!(unpad(&[4, 5, 6, 1, 2, 3], bgr, None, true, &mut out));
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
        let mjpeg = CameraFormat { pixel: CameraPixel::Mjpeg, ..yuyv };
        assert!(!unpad(&padded, mjpeg, None, false, &mut out), "compressed frames are left alone");
    }

    #[test]
    fn padded_and_bottom_up_rows_are_made_tight_and_top_down() {
        // Two rows of 3 bytes, each padded to 4; the last row first.
        let bytes = [4, 5, 6, 0, 1, 2, 3, 0];
        let mut out = Vec::new();
        assert!(tight_rows(&bytes, 3, 4, 2, true, &mut out));
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
        assert!(tight_rows(&bytes, 3, 4, 2, false, &mut out));
        assert_eq!(out, [4, 5, 6, 1, 2, 3]);
        let kept = out.capacity();
        assert!(tight_rows(&bytes, 3, 4, 2, false, &mut out));
        assert_eq!(out.capacity(), kept, "the buffer is reused");
        // The last row needs no padding; anything shorter, a stride under a row, or no rows is refused.
        assert!(tight_rows(&bytes[..7], 3, 4, 2, true, &mut out));
        assert!(!tight_rows(&bytes[..6], 3, 4, 2, true, &mut out));
        assert!(!tight_rows(&bytes, 3, 2, 2, true, &mut out));
        assert!(!tight_rows(&bytes, 3, 4, 0, true, &mut out));
        assert!(!tight_rows(&bytes, 3, usize::MAX, 3, true, &mut out));
    }

    #[test]
    fn studio_range_black_white_and_red() {
        assert_eq!(rgb(16, 128, 128), [0, 0, 0, 255]);
        assert_eq!(rgb(235, 128, 128), [255, 255, 255, 255]);
        // BT.601 red: Y 81, Cb 90, Cr 240.
        let [r, g, b, _] = rgb(81, 90, 240);
        assert!(r >= 254 && g <= 1 && b <= 1, "{r} {g} {b}");
        // Values past the studio range clip instead of wrapping.
        assert_eq!(rgb(255, 255, 255)[0], 255);
        assert_eq!(rgb(0, 0, 0)[2], 0);
    }

    #[test]
    fn yuyv_shares_chroma_between_two_pixels() {
        let mut f = VideoFrame::default();
        assert!(convert(&[16, 128, 235, 128, 235, 128, 16, 128], fmt(2, 2, CameraPixel::Yuyv), &mut f));
        assert_eq!((f.width, f.height, f.format), (2, 2, PixelFormat::Rgba8));
        assert_eq!(&f.data[..8], &[0, 0, 0, 255, 255, 255, 255, 255]);
        assert_eq!(&f.data[8..], &[255, 255, 255, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn nv12_reads_one_chroma_pair_per_two_by_two_block() {
        // 2 x 2 luma, then Cb Cr for the block: red.
        let mut f = VideoFrame::default();
        assert!(convert(&[81, 81, 81, 81, 90, 240], fmt(2, 2, CameraPixel::Nv12), &mut f));
        for px in f.data.as_chunks::<4>().0 {
            assert!(px[0] >= 254 && px[1] <= 1 && px[2] <= 1 && px[3] == 255, "{px:?}");
        }
    }

    #[test]
    fn gray_and_rgb_are_made_opaque() {
        let mut f = VideoFrame::default();
        assert!(convert(&[7, 9], fmt(2, 1, CameraPixel::Gray), &mut f));
        assert_eq!(f.data, [7, 7, 7, 255, 9, 9, 9, 255]);
        assert!(convert(&[1, 2, 3, 4, 5, 6], fmt(2, 1, CameraPixel::Rgb), &mut f));
        assert_eq!((f.format, f.data.as_slice()), (PixelFormat::Rgba8, [1, 2, 3, 255, 4, 5, 6, 255].as_slice()));
        assert!(convert(&[1, 2, 3, 4, 5, 6], fmt(2, 1, CameraPixel::Bgr), &mut f));
        assert_eq!(f.format, PixelFormat::Bgra8);
    }

    #[test]
    fn short_odd_and_oversized_frames_are_refused() {
        let mut f = VideoFrame::default();
        assert!(!convert(&[0; 7], fmt(2, 2, CameraPixel::Yuyv), &mut f));
        assert!(!convert(&[0; 6], fmt(3, 1, CameraPixel::Yuyv), &mut f), "4:2:2 needs an even width");
        assert!(!convert(&[0; 5], fmt(2, 2, CameraPixel::Nv12), &mut f));
        assert!(!convert(&[0; 9], fmt(2, 3, CameraPixel::Nv12), &mut f), "4:2:0 needs an even height");
        assert!(!convert(&[0; 5], fmt(2, 1, CameraPixel::Rgb), &mut f));
        assert!(!convert(&[], fmt(0, 0, CameraPixel::Gray), &mut f));
        assert!(!convert(&[0; 64], fmt(u32::MAX, u32::MAX, CameraPixel::Gray), &mut f));
        assert!(!convert(&[0; 64], fmt(MAX_FRAME_SIDE + 2, 1, CameraPixel::Gray), &mut f));
    }

    #[test]
    fn a_longer_buffer_than_the_picture_is_read_up_to_the_picture() {
        let mut f = VideoFrame::default();
        assert!(convert(&[7, 9, 99, 99], fmt(2, 1, CameraPixel::Gray), &mut f));
        assert_eq!(f.data, [7, 7, 7, 255, 9, 9, 9, 255]);
    }

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let rgb: Vec<u8> = (0..usize::from(width) * usize::from(height)).flat_map(|_| [200u8, 30, 30]).collect();
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90).encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb).unwrap();
        out
    }

    #[test]
    fn mjpeg_frames_are_decoded_at_their_own_size() {
        let mut f = VideoFrame::default();
        // The stream's format said 640 x 480; the picture's header says 16 x 8.
        assert!(convert(&jpeg(16, 8), fmt(640, 480, CameraPixel::Mjpeg), &mut f));
        assert_eq!((f.width, f.height, f.format), (16, 8, PixelFormat::Rgba8));
        let px = &f.data[..4];
        assert!(px[0] > 180 && px[1] < 60 && px[2] < 60 && px[3] == 255, "{px:?}");
    }

    /// A JPEG with its Huffman table segments taken out, as MJPEG cameras send.
    fn without_huffman_tables(jpeg: &[u8]) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        let mut i = 2;
        while i + 4 <= jpeg.len() {
            let marker = jpeg[i + 1];
            if marker == 0xda {
                out.extend_from_slice(&jpeg[i..]);
                break;
            }
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if marker != 0xc4 {
                out.extend_from_slice(&jpeg[i..i + 2 + len]);
            }
            i += 2 + len;
        }
        out
    }

    #[test]
    fn mjpeg_frames_without_huffman_tables_use_the_standard_ones() {
        let stripped = without_huffman_tables(&jpeg(16, 16));
        assert!(!stripped.windows(2).any(|m| m == [0xff, 0xc4]), "no table left");
        let mut f = VideoFrame::default();
        let mut c = Converter::default();
        assert!(c.convert(&stripped, fmt(16, 16, CameraPixel::Mjpeg), &mut f));
        assert!(f.data[0] > 180, "{:?}", &f.data[..4]);
        // The buffer is kept for the next such frame.
        let kept = c.scratch.capacity();
        assert!(c.convert(&stripped, fmt(16, 16, CameraPixel::Mjpeg), &mut f));
        assert_eq!(c.scratch.capacity(), kept);
    }

    #[test]
    fn huffman_tables_are_found_before_the_scan_only() {
        assert!(has_huffman_tables(&jpeg(8, 8)));
        assert!(!has_huffman_tables(&without_huffman_tables(&jpeg(8, 8))));
        // Markers that cannot be followed: the decoder decides.
        assert!(has_huffman_tables(&[0xff, 0xd8, 0x00, 0x01]));
        assert!(has_huffman_tables(&[0xff, 0xd8, 0xff, 0xe0, 0xff, 0xff]));
        assert!(has_huffman_tables(&[]));
    }

    #[test]
    fn broken_mjpeg_is_refused() {
        let mut f = VideoFrame::default();
        let good = jpeg(16, 8);
        assert!(!convert(&good[..good.len() / 3], fmt(16, 8, CameraPixel::Mjpeg), &mut f));
        assert!(!convert(&[0xff, 0xd8, 0xff], fmt(16, 8, CameraPixel::Mjpeg), &mut f));
        assert!(!convert(&[], fmt(16, 8, CameraPixel::Mjpeg), &mut f));
    }
}
