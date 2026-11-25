//! The virtual camera: a device that other apps open as a camera and that shows what this app
//! writes to it.
//!
//! On Linux the device is an output node of the `v4l2loopback` kernel module. [`VirtualCamera::open`]
//! sets the node's picture size, frame rate and pixel encoding, and [`VirtualCamera::write`] sends
//! each NV12 frame, converted to that encoding. On other systems [`devices`] is empty and
//! [`VirtualCamera::open`] returns [`PlatformError::Unsupported`].
//!
//! The conversions from NV12 ([`convert`]) are the same on every system.

use studiocraft_media::video::{PixelFormat, VideoFrame};

use crate::PlatformError;

/// A device that other apps can open as a camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualCameraInfo {
    /// The name other apps list it under.
    pub name: String,
    /// The device node, such as `/dev/video10`.
    pub path: String,
}

/// The pixel encodings a virtual camera sends, in the order [`VirtualCamera::open`] offers them
/// to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtualPixel {
    /// Packed 4:2:2 YUV, the encoding most camera apps read.
    Yuyv,
    /// 4:2:0 YUV: a luma plane and an interleaved chroma plane, as the compositor makes it.
    Nv12,
    /// 4:2:0 YUV: a luma plane, then a U plane and a V plane.
    I420,
}

impl VirtualPixel {
    pub const ALL: [VirtualPixel; 3] = [VirtualPixel::Yuyv, VirtualPixel::Nv12, VirtualPixel::I420];

    /// The V4L2 code of the encoding.
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            VirtualPixel::Yuyv => *b"YUYV",
            VirtualPixel::Nv12 => *b"NV12",
            VirtualPixel::I420 => *b"YU12",
        }
    }

    /// Bytes of a `width` x `height` picture, or `None` when a side is odd or the size overflows.
    pub fn frame_bytes(self, width: u32, height: u32) -> Option<usize> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return None;
        }
        let pixels = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?;
        match self {
            VirtualPixel::Yuyv => pixels.checked_mul(2),
            VirtualPixel::Nv12 | VirtualPixel::I420 => pixels.checked_mul(3).map(|n| n / 2),
        }
    }

    /// Bytes of one row of the first plane of a `width` pixels wide picture.
    pub fn row_bytes(self, width: u32) -> Option<u32> {
        match self {
            VirtualPixel::Yuyv => width.checked_mul(2),
            VirtualPixel::Nv12 | VirtualPixel::I420 => Some(width),
        }
    }
}

/// Convert the NV12 `frame` to `to`, into `out` (resized; its memory is reused from one frame to
/// the next). An error when the frame is not a whole NV12 picture with even sides.
pub fn convert(frame: &VideoFrame, to: VirtualPixel, out: &mut Vec<u8>) -> Result<(), PlatformError> {
    let (w, h) = (frame.width, frame.height);
    let size = to.frame_bytes(w, h);
    if frame.format != PixelFormat::Nv12 || !frame.is_consistent() || size.is_none() {
        return Err(PlatformError::Failed(format!("a {w} x {h} {:?} frame cannot be sent to a camera", frame.format)));
    }
    let (Ok(w), Ok(h)) = (usize::try_from(w), usize::try_from(h)) else {
        return Err(PlatformError::Failed("the frame is too large".into()));
    };
    let (luma, chroma) = frame.data.split_at(w * h);
    out.clear();
    match to {
        VirtualPixel::Nv12 => out.extend_from_slice(&frame.data),
        VirtualPixel::I420 => {
            out.extend_from_slice(luma);
            let (pairs, _) = chroma.as_chunks::<2>();
            out.extend(pairs.iter().map(|[u, _]| *u));
            out.extend(pairs.iter().map(|[_, v]| *v));
        }
        VirtualPixel::Yuyv => {
            out.reserve(w * h * 2);
            // Each chroma row serves two picture rows.
            for (rows, uv_row) in luma.chunks_exact(w * 2).zip(chroma.chunks_exact(w)) {
                let (uv, _) = uv_row.as_chunks::<2>();
                for row in rows.chunks_exact(w) {
                    let (ys, _) = row.as_chunks::<2>();
                    for ([y0, y1], [u, v]) in ys.iter().zip(uv) {
                        out.extend_from_slice(&[*y0, *u, *y1, *v]);
                    }
                }
            }
        }
    }
    Ok(())
}

/// The virtual camera devices, in the order of their device nodes.
pub fn devices() -> Vec<VirtualCameraInfo> {
    imp::devices()
}

/// A virtual camera open for writing.
pub struct VirtualCamera {
    inner: imp::Device,
    info: VirtualCameraInfo,
    pixel: VirtualPixel,
    width: u32,
    height: u32,
    buffer: Vec<u8>,
}

impl VirtualCamera {
    /// Open the device at `path`, or the first of [`devices`] when `path` is empty, for
    /// `width` x `height` pictures at `fps_num` / `fps_den` frames a second.
    pub fn open(path: &str, width: u32, height: u32, fps_num: u32, fps_den: u32) -> Result<VirtualCamera, PlatformError> {
        if VirtualPixel::Nv12.frame_bytes(width, height).is_none() {
            return Err(PlatformError::Failed(format!("a virtual camera needs an even width and height, and the output is {width} x {height}")));
        }
        if fps_num == 0 || fps_den == 0 {
            return Err(PlatformError::Failed(format!("the frame rate {fps_num}/{fps_den} is not valid")));
        }
        if !cfg!(target_os = "linux") {
            return Err(PlatformError::Unsupported);
        }
        let path = path.trim();
        let info = if path.is_empty() {
            devices()
                .into_iter()
                .next()
                .ok_or_else(|| PlatformError::NotFound("a virtual camera device (load the v4l2loopback kernel module)".into()))?
        } else {
            VirtualCameraInfo { name: path.to_string(), path: path.to_string() }
        };
        let (inner, pixel, name) = imp::Device::open(&info.path, width, height, fps_num, fps_den)?;
        let info = VirtualCameraInfo { name: name.unwrap_or(info.name), ..info };
        Ok(VirtualCamera { inner, info, pixel, width, height, buffer: Vec::new() })
    }

    pub fn info(&self) -> &VirtualCameraInfo {
        &self.info
    }

    /// The encoding the device took.
    pub fn pixel(&self) -> VirtualPixel {
        self.pixel
    }

    /// Send the NV12 `frame`, and return the bytes written in the device's encoding. `Ok(None)`
    /// when the device had no room for it, so it was skipped.
    pub fn write(&mut self, frame: &VideoFrame) -> Result<Option<usize>, PlatformError> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(PlatformError::Failed(format!(
                "the frame is {} x {} and the virtual camera {} x {}",
                frame.width, frame.height, self.width, self.height
            )));
        }
        let data = if self.pixel == VirtualPixel::Nv12 && frame.is_consistent() && frame.format == PixelFormat::Nv12 {
            &frame.data
        } else {
            convert(frame, self.pixel, &mut self.buffer)?;
            &self.buffer
        };
        Ok(self.inner.write(data)?.then_some(data.len()))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io::Write;

    use v4l::capability::Flags;
    use v4l::format::{Colorspace, FieldOrder, Format, FourCC, Quantization};
    use v4l::fraction::Fraction;
    use v4l::video::Output;
    use v4l::video::output::Parameters;

    use super::{VirtualCameraInfo, VirtualPixel};
    use crate::PlatformError;

    /// The driver name `v4l2loopback` devices report.
    const LOOPBACK_DRIVER: &str = "v4l2 loopback";

    pub(super) fn devices() -> Vec<VirtualCameraInfo> {
        let Ok(entries) = std::fs::read_dir("/dev") else { return Vec::new() };
        let mut nodes: Vec<(u32, String)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let n: u32 = name.strip_prefix("video")?.parse().ok()?;
                Some((n, e.path().to_string_lossy().into_owned()))
            })
            .collect();
        nodes.sort_unstable();
        nodes
            .into_iter()
            .filter_map(|(_, path)| {
                let caps = v4l::Device::with_path(&path).and_then(|d| d.query_caps()).ok()?;
                (caps.driver == LOOPBACK_DRIVER && caps.capabilities.contains(Flags::VIDEO_OUTPUT))
                    .then(|| VirtualCameraInfo { name: caps.card.trim().to_string(), path })
            })
            .collect()
    }

    /// Whether the frame intervals `a_num / a_den` and `b_num / b_den` seconds are the same.
    pub(super) fn same_rate(a_num: u32, a_den: u32, b_num: u32, b_den: u32) -> bool {
        a_den != 0 && b_den != 0 && u64::from(a_num) * u64::from(b_den) == u64::from(b_num) * u64::from(a_den)
    }

    pub(super) struct Device {
        dev: v4l::Device,
        size: usize,
    }

    impl Device {
        /// Open `path` and set its format: the first encoding of [`VirtualPixel::ALL`] the device
        /// takes at this size. Also the device's own name.
        pub(super) fn open(
            path: &str,
            width: u32,
            height: u32,
            fps_num: u32,
            fps_den: u32,
        ) -> Result<(Device, VirtualPixel, Option<String>), PlatformError> {
            let failed = |what: &str, e: std::io::Error| PlatformError::Failed(format!("{path} {what}: {e}"));
            let dev = v4l::Device::with_path(path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => PlatformError::NotFound(path.to_string()),
                _ => failed("could not be opened", e),
            })?;
            let caps = dev.query_caps().map_err(|e| failed("did not describe itself", e))?;
            if caps.driver != LOOPBACK_DRIVER {
                return Err(PlatformError::Failed(format!("{path} is not a v4l2loopback device")));
            }
            if !caps.capabilities.contains(Flags::VIDEO_OUTPUT) {
                return Err(PlatformError::Failed(format!("{path} does not take video; another app may be writing to it")));
            }
            for pixel in VirtualPixel::ALL {
                let mut want = Format::new(width, height, FourCC::new(&pixel.fourcc()));
                want.field_order = FieldOrder::Progressive;
                want.colorspace = Colorspace::Rec709;
                want.quantization = Quantization::LimitedRange;
                let Ok(got) = dev.set_format(&want) else { continue };
                if got.fourcc != want.fourcc || (got.width, got.height) != (width, height) {
                    continue;
                }
                // Rows are written without padding, so a device that pads them is passed over.
                if pixel.row_bytes(width) != Some(got.stride) {
                    continue;
                }
                let Some(size) = pixel.frame_bytes(width, height) else { continue };
                let interval = Parameters::new(Fraction::new(fps_den, fps_num));
                let params = dev.set_params(&interval).map_err(|e| failed("did not take the frame rate", e))?;
                if !same_rate(params.interval.numerator, params.interval.denominator, fps_den, fps_num) {
                    return Err(PlatformError::Failed(format!(
                        "{path} runs at {}/{} seconds a frame and the output at {fps_den}/{fps_num}",
                        params.interval.numerator, params.interval.denominator
                    )));
                }
                let name = caps.card.trim();
                return Ok((Device { dev, size }, pixel, (!name.is_empty()).then(|| name.to_string())));
            }
            Err(PlatformError::Failed(format!("{path} takes none of YUYV, NV12 or YU12 pictures at {width} x {height}")))
        }

        pub(super) fn write(&mut self, data: &[u8]) -> Result<bool, PlatformError> {
            if data.len() != self.size {
                return Err(PlatformError::Failed(format!("a picture of {} bytes was ready and the camera takes {}", data.len(), self.size)));
            }
            match self.dev.write(data) {
                Ok(n) if n == data.len() => Ok(true),
                Ok(n) => Err(PlatformError::Failed(format!("the camera took {n} of {} bytes", data.len()))),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted => Ok(false),
                Err(e) => Err(PlatformError::Failed(format!("the camera could not be written: {e}"))),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{VirtualCameraInfo, VirtualPixel};
    use crate::PlatformError;

    pub(super) fn devices() -> Vec<VirtualCameraInfo> {
        Vec::new()
    }

    pub(super) struct Device;

    impl Device {
        pub(super) fn open(
            _path: &str,
            _width: u32,
            _height: u32,
            _fps_num: u32,
            _fps_den: u32,
        ) -> Result<(Device, VirtualPixel, Option<String>), PlatformError> {
            Err(PlatformError::Unsupported)
        }

        pub(super) fn write(&mut self, _data: &[u8]) -> Result<bool, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 x 2 NV12 frame: luma 0..8, chroma row (U, V) pairs 100,200 and 101,201.
    fn frame() -> VideoFrame {
        let mut f = VideoFrame::default();
        assert!(f.reshape(4, 2, PixelFormat::Nv12));
        f.data = vec![0, 1, 2, 3, 4, 5, 6, 7, 100, 200, 101, 201];
        f
    }

    #[test]
    fn nv12_converts_to_each_encoding() {
        let f = frame();
        let mut out = Vec::new();
        convert(&f, VirtualPixel::Nv12, &mut out).unwrap();
        assert_eq!(out, f.data);
        convert(&f, VirtualPixel::I420, &mut out).unwrap();
        assert_eq!(out, [0, 1, 2, 3, 4, 5, 6, 7, 100, 101, 200, 201]);
        convert(&f, VirtualPixel::Yuyv, &mut out).unwrap();
        // Both rows take the one chroma row.
        assert_eq!(out, [0, 100, 1, 200, 2, 101, 3, 201, 4, 100, 5, 200, 6, 101, 7, 201]);
        for p in VirtualPixel::ALL {
            convert(&f, p, &mut out).unwrap();
            assert_eq!(Some(out.len()), p.frame_bytes(4, 2), "{p:?}");
        }
    }

    #[test]
    fn yuyv_rows_take_their_own_chroma_row() {
        let mut f = VideoFrame::default();
        assert!(f.reshape(2, 4, PixelFormat::Nv12));
        f.data = vec![0, 1, 2, 3, 4, 5, 6, 7, 10, 20, 11, 21];
        let mut out = Vec::new();
        convert(&f, VirtualPixel::Yuyv, &mut out).unwrap();
        assert_eq!(out, [0, 10, 1, 20, 2, 10, 3, 20, 4, 11, 5, 21, 6, 11, 7, 21]);
    }

    #[test]
    fn frames_that_are_not_whole_nv12_pictures_are_refused() {
        let mut out = Vec::new();
        let mut short = frame();
        short.data.pop();
        assert!(convert(&short, VirtualPixel::Yuyv, &mut out).is_err());
        let mut rgba = VideoFrame::default();
        assert!(rgba.reshape(4, 2, PixelFormat::Rgba8));
        assert!(convert(&rgba, VirtualPixel::I420, &mut out).is_err());
        let mut odd = VideoFrame::default();
        assert!(odd.reshape(3, 2, PixelFormat::Nv12));
        assert!(convert(&odd, VirtualPixel::Yuyv, &mut out).is_err());
        assert!(convert(&VideoFrame::default(), VirtualPixel::Nv12, &mut out).is_err());
        assert_eq!(VirtualPixel::Yuyv.frame_bytes(u32::MAX - 1, u32::MAX - 1), None);
    }

    #[test]
    fn a_camera_needs_an_even_size_and_a_frame_rate() {
        assert!(VirtualCamera::open("", 641, 480, 30, 1).is_err());
        assert!(VirtualCamera::open("", 640, 480, 0, 1).is_err());
        assert!(VirtualCamera::open("", 640, 480, 30, 0).is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn other_systems_have_no_virtual_camera_yet() {
        assert!(devices().is_empty());
        assert!(matches!(VirtualCamera::open("x", 640, 480, 30, 1), Err(PlatformError::Unsupported)));
        assert!(matches!(VirtualCamera::open("", 640, 480, 30, 1), Err(PlatformError::Unsupported)));
    }

    #[test]
    fn rows_are_as_wide_as_their_first_plane() {
        assert_eq!(VirtualPixel::Yuyv.row_bytes(640), Some(1280));
        assert_eq!(VirtualPixel::Nv12.row_bytes(640), Some(640));
        assert_eq!(VirtualPixel::I420.row_bytes(640), Some(640));
        assert_eq!(VirtualPixel::Yuyv.row_bytes(u32::MAX), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_device_rate_counts_as_the_same_only_when_it_is() {
        assert!(imp::same_rate(1, 30, 1, 30));
        assert!(imp::same_rate(2, 60, 1, 30));
        assert!(imp::same_rate(1001, 30000, 1001, 30000));
        assert!(!imp::same_rate(1, 30, 1, 60));
        assert!(!imp::same_rate(1001, 30000, 1, 30));
        assert!(!imp::same_rate(0, 0, 0, 0));
        assert!(imp::same_rate(u32::MAX, u32::MAX, 1, 1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_device_is_not_found() {
        let r = VirtualCamera::open("/dev/studiocraft-no-such-camera", 640, 480, 30, 1);
        assert!(matches!(r, Err(PlatformError::NotFound(_))), "{:?}", r.err());
    }
}
