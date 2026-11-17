//! 8-bit gray V4L2 cameras. The camera library asks V4L2 for gray as `GRAY`, which V4L2 does not
//! define: its 8-bit gray format is `GREY`. Gray formats are listed and streamed here instead.

use v4l::frameinterval::FrameIntervalEnum;
use v4l::framesize::FrameSizeEnum;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, FourCC, Fraction};

use crate::PlatformError;
use crate::camera::{CameraFormat, CameraPixel, CameraStream};

/// The most gray formats listed for one camera.
const MOST: usize = 64;

/// The highest frame rate listed.
const MAX_FPS: u32 = 1000;

/// How long, in milliseconds, a frame read waits before the capture thread looks at its stop
/// flag again.
const WAIT: i32 = 500;

/// How long a read that hands the last frame's buffer back to the driver may wait for the next
/// frame: the first read, and every read once a wait of [`WAIT`] ran out. Such a read cannot be
/// tried again, so a camera that sends nothing in this time fails the capture, and the source
/// opens it again.
const READ_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// `poll`'s flag for data to read (Linux).
const POLLIN: i16 = 0x001;

/// Mapped buffers the driver fills in turn.
const BUFFERS: u32 = 4;

/// V4L2's 8-bit gray, and the name the camera library uses for it, which some drivers may list.
const CODES: [&[u8; 4]; 2] = [b"GREY", b"GRAY"];

fn failed(what: &str, e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Failed(format!("{what} ({e})"))
}

/// The gray code the device `d` lists, if any.
fn code(d: &Device) -> Option<FourCC> {
    let listed = d.enum_formats().ok()?;
    CODES.iter().map(|c| FourCC::new(c)).find(|c| listed.iter().any(|f| f.fourcc == *c))
}

/// The picture sizes of one V4L2 size entry: a stepwise range gives its smallest and largest.
pub(crate) fn sizes(size: &FrameSizeEnum) -> Vec<(u32, u32)> {
    let mut out = match size {
        FrameSizeEnum::Discrete(d) => vec![(d.width, d.height)],
        FrameSizeEnum::Stepwise(s) => vec![(s.max_width, s.max_height), (s.min_width, s.min_height)],
    };
    out.retain(|&(w, h)| w > 0 && h > 0);
    out.dedup();
    out
}

/// Frames per second for a frame interval in seconds, rounded; `None` for an interval of zero or
/// a rate above [`MAX_FPS`].
pub(crate) fn fps(interval: Fraction) -> Option<u32> {
    if interval.numerator == 0 {
        return None;
    }
    let (n, d) = (u64::from(interval.numerator), u64::from(interval.denominator));
    let fps = d.saturating_add(n / 2) / n;
    u32::try_from(fps).ok().filter(|f| (1..=MAX_FPS).contains(f))
}

/// The frame rates of one V4L2 interval entry: a stepwise range gives its fastest and slowest.
pub(crate) fn rates(interval: &FrameIntervalEnum) -> Vec<u32> {
    let mut out: Vec<u32> = match interval {
        FrameIntervalEnum::Discrete(f) => fps(*f).into_iter().collect(),
        FrameIntervalEnum::Stepwise(s) => [s.min, s.max].into_iter().filter_map(fps).collect(),
    };
    out.dedup();
    out
}

/// The gray formats V4L2 node `index` offers, at most [`MOST`]. Asking does not disturb a camera
/// that is streaming.
pub fn formats(index: usize) -> Vec<CameraFormat> {
    let Ok(d) = Device::new(index) else { return Vec::new() };
    let Some(code) = code(&d) else { return Vec::new() };
    let mut out = Vec::new();
    for size in d.enum_framesizes(code).unwrap_or_default() {
        for (width, height) in sizes(&size.size) {
            for interval in d.enum_frameintervals(code, width, height).unwrap_or_default() {
                for fps in rates(&interval.interval) {
                    out.push(CameraFormat { width, height, fps, pixel: CameraPixel::Gray });
                    if out.len() >= MOST {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Start V4L2 node `index` streaming gray in `format`.
pub fn open(index: usize, format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
    let device = Device::new(index).map_err(|e| failed("the camera could not be opened", e))?;
    let code = code(&device).ok_or_else(|| PlatformError::Failed(format!("the camera does not offer {}", format.label())))?;
    let set = device.set_format(&v4l::Format::new(format.width, format.height, code)).map_err(|e| failed("the camera could not be opened", e))?;
    if set.fourcc != code || set.width != format.width || set.height != format.height {
        return Err(PlatformError::Failed(format!("the camera does not offer {}", format.label())));
    }
    // A driver that cannot set the rate keeps its own.
    let _ = device.set_params(&v4l::video::capture::Parameters::with_fps(format.fps));
    let mut stream = v4l::io::mmap::Stream::with_buffers(&device, v4l::buffer::Type::VideoCapture, BUFFERS)
        .map_err(|e| failed("the camera could not start", e))?;
    stream.set_timeout(READ_WAIT);
    let stride = usize::try_from(set.stride).ok();
    let size = usize::try_from(set.size).unwrap_or(0);
    Ok(Box::new(Stream { stream, started: false, direct: false, format, stride, size, rows: Vec::new() }))
}

struct Stream {
    stream: v4l::io::mmap::Stream<'static>,
    /// The first frame read starts the stream.
    started: bool,
    /// Frames are read without waiting first, since a wait ran out.
    direct: bool,
    format: CameraFormat,
    /// The row stride and picture size in bytes the driver set.
    stride: Option<usize>,
    size: usize,
    /// The rows of a frame that carried padding.
    rows: Vec<u8>,
}

impl CameraStream for Stream {
    fn next(&mut self, frame: &mut dyn FnMut(&[u8], CameraFormat)) -> Result<(), PlatformError> {
        // A read hands the last frame's buffer back to the driver before it waits, and one that
        // runs out of time cannot be tried again: it would hand that buffer back a second time.
        // So frames are waited for here first, for at most `WAIT`. When that wait runs out, the
        // driver may be holding no buffer to fill (a driver that grants a single one), and from
        // then on frames are read directly, each read waiting at most `READ_WAIT`.
        if self.started && !self.direct {
            match self.stream.handle().poll(POLLIN, WAIT) {
                // Nothing arrived in time; the capture thread looks at its stop flag.
                Ok(0) => {
                    self.direct = true;
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
                Err(e) => return Err(failed("the camera stopped", e)),
            }
        }
        let first = !self.started;
        self.started = true;
        let (bytes, meta) = match CaptureStream::next(&mut self.stream) {
            Ok(next) => next,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                let what = if first { "the camera sent no picture" } else { "the camera stopped sending pictures" };
                return Err(PlatformError::Failed(what.into()));
            }
            Err(e) => return Err(failed("the camera stopped", e)),
        };
        let used = usize::try_from(meta.bytesused).unwrap_or(0);
        let size = if used > 0 { used } else { self.size };
        let bytes = bytes.get(..size).filter(|_| size > 0).unwrap_or(bytes);
        if crate::camera_frames::unpad(bytes, self.format, self.stride, false, &mut self.rows) {
            frame(&self.rows, self.format);
        } else {
            frame(bytes, self.format);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use v4l::frameinterval::Stepwise as Rates;
    use v4l::framesize::{Discrete, Stepwise};

    use super::*;

    #[test]
    fn sizes_take_both_ends_of_a_range_and_skip_empty_ones() {
        assert_eq!(sizes(&FrameSizeEnum::Discrete(Discrete { width: 640, height: 480 })), vec![(640, 480)]);
        let range = Stepwise { min_width: 16, max_width: 1280, step_width: 0, min_height: 16, max_height: 720, step_height: 0 };
        assert_eq!(sizes(&FrameSizeEnum::Stepwise(range)), vec![(1280, 720), (16, 16)], "a step of zero is read without stepping");
        assert!(sizes(&FrameSizeEnum::Discrete(Discrete { width: 0, height: 480 })).is_empty());
    }

    #[test]
    fn rates_come_from_frame_intervals() {
        assert_eq!(fps(Fraction::new(1, 30)), Some(30));
        assert_eq!(fps(Fraction::new(1001, 30000)), Some(30), "29.97 rounds to 30");
        assert_eq!(fps(Fraction::new(0, 30)), None, "an interval of zero");
        assert_eq!(fps(Fraction::new(1, 0)), None, "an endless interval");
        assert_eq!(fps(Fraction::new(1, u32::MAX)), None, "too fast");
        let range = Rates { min: Fraction::new(1, 60), max: Fraction::new(1, 5), step: Fraction::new(0, 1) };
        assert_eq!(rates(&FrameIntervalEnum::Stepwise(range)), vec![60, 5]);
        assert_eq!(rates(&FrameIntervalEnum::Discrete(Fraction::new(1, 15))), vec![15]);
    }

    #[test]
    fn a_node_that_is_not_there_lists_no_gray_formats() {
        assert!(formats(usize::MAX).is_empty());
        assert!(open(usize::MAX, CameraFormat { width: 2, height: 2, fps: 30, pixel: CameraPixel::Gray }).is_err());
    }
}
