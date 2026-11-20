//! [`Info`](crate::Info) from the encoders' own packet types.
//!
//! - `craft_video::h264::Packet`, from the built-in H.264 encoder, with the `h264` feature.
//! - `craft_hwenc_native::Packet`, from the hardware encoders, with the `hwenc-native` feature.
//!
//! Each conversion reads the packet's timestamps and keyframe flag and leaves its bytes where they
//! are, so the caller then hands the packet's buffer to [`crate::PacketPool::share`]:
//!
//! ```
//! # #[cfg(feature = "h264")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use craft_packet::{Info, PacketPool};
//! use craft_video::h264::{FrameType, Packet};
//!
//! let mut pool = PacketPool::new(4, 1024)?;
//! let mut packet = Packet { data: vec![0, 0, 0, 1, 0x65], pts: 3000, dts: 0, keyframe: true, frame_type: FrameType::Idr, qp: 26 };
//! let info = Info::try_from(&packet)?;
//! let shared = pool.share(&mut packet.data, info)?;
//! assert_eq!((shared.pts(), shared.dts(), shared.keyframe()), (3000, 0, true));
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "h264"))]
//! # fn main() {}
//! ```

use std::fmt;

#[cfg(any(feature = "h264", feature = "hwenc-native"))]
use crate::packet::Info;

/// Why an encoder's packet gives no [`Info`](crate::Info).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvertError {
    /// The packet holds no bytes.
    Empty,
    /// The packet's decoding time is later than its presentation time.
    DecodedAfterShown {
        /// The packet's presentation time.
        pts: i64,
        /// The packet's decoding time.
        dts: i64,
    },
    /// The keyframe flag does not match the picture type: a keyframe that is not an IDR picture,
    /// or an IDR picture that is not marked as a keyframe.
    KeyframeMismatch,
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::Empty => f.write_str("the encoded packet holds no bytes"),
            ConvertError::DecodedAfterShown { pts, dts } => {
                write!(f, "the encoded packet is decoded at {dts}, after it is presented at {pts}")
            }
            ConvertError::KeyframeMismatch => f.write_str("the encoded packet's keyframe flag does not match its picture type"),
        }
    }
}

impl std::error::Error for ConvertError {}

/// Checks a video packet's bytes and timestamps, and makes its [`Info`].
#[cfg(any(feature = "h264", feature = "hwenc-native"))]
fn video(data: &[u8], pts: i64, dts: i64, keyframe: bool) -> Result<Info, ConvertError> {
    if data.is_empty() {
        return Err(ConvertError::Empty);
    }
    if dts > pts {
        return Err(ConvertError::DecodedAfterShown { pts, dts });
    }
    Ok(Info::video(pts, dts, keyframe))
}

/// A picture from the built-in H.264 encoder. The packet's `pts` and `dts` are kept as they are, in
/// the encoder's time base. Its keyframe flag must match its picture type: only an IDR picture is a
/// keyframe.
#[cfg(feature = "h264")]
impl TryFrom<&craft_video::h264::Packet> for Info {
    type Error = ConvertError;

    fn try_from(p: &craft_video::h264::Packet) -> Result<Self, Self::Error> {
        if p.keyframe != (p.frame_type == craft_video::h264::FrameType::Idr) {
            return Err(ConvertError::KeyframeMismatch);
        }
        video(&p.data, p.pts, p.dts, p.keyframe)
    }
}

/// A picture from a hardware encoder. These encoders do not reorder pictures, so the packet's
/// `pts` is also its `dts`.
#[cfg(feature = "hwenc-native")]
impl TryFrom<&craft_hwenc_native::Packet> for Info {
    type Error = ConvertError;

    fn try_from(p: &craft_hwenc_native::Packet) -> Result<Self, Self::Error> {
        video(&p.data, p.pts, p.pts, p.keyframe)
    }
}
