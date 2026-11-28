//! The virtual camera, started and stopped on its own beside the recording, the stream and the
//! replay buffer. It runs no encoder: the compositor's NV12 frames at the output size go straight
//! to the `vcam` output plug-in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_model::Settings;
use studiocraft_plugin::{DestinationCounts, OutputInstance, OutputJob, OutputStatus, RateControl, VideoFormat};

use crate::{Notice, OutputState, Studio, StudioError};

/// The output plug-in that sends frames to the virtual camera device.
const OUTPUT: &str = "vcam";
/// The video encoder an [`OutputJob`] carries. The virtual camera does not run it.
const CARRIED_ENCODER: &str = "h264";

/// A running virtual camera.
pub(crate) struct VirtualCamera {
    instance: Box<dyn OutputInstance>,
    stats: Arc<EncoderStats>,
    started: Instant,
    target: String,
}

impl Studio {
    pub(crate) fn start_virtual_camera(&mut self) -> Result<(), StudioError> {
        if self.vcam.is_some() {
            return Ok(());
        }
        if self.video.is_none() {
            return Err(StudioError::Output("the virtual camera shows nothing without the graphics device".into()));
        }
        let registry = Arc::clone(self.engine.registry());
        let output = registry.output(OUTPUT)?;
        let encoder = registry.encoder(CARRIED_ENCODER)?;
        let v = self.profile.video.sanitized();
        let stats = Arc::new(EncoderStats::default());
        let job = OutputJob {
            video: VideoFormat { width: v.output_width, height: v.output_height, fps_num: v.fps_num, fps_den: v.fps_den, pixel: PixelFormat::Nv12 },
            sample_rate: SAMPLE_RATE,
            video_encoder: encoder,
            video_encoder_settings: Settings::new(),
            rate: RateControl::Lossless,
            audio_bitrate_kbps: 0,
            audio_streams: Vec::new(),
            ffmpeg: PathBuf::new(),
            stats: Arc::clone(&stats),
            start: self.clock.now(),
            other_encoders: Vec::new(),
            captions: None,
        };
        let instance = output.start(&Settings::new(), job)?;
        let target = instance.target();
        self.vcam = Some(VirtualCamera { instance, stats, started: Instant::now(), target });
        self.update_encode_target();
        Ok(())
    }

    /// Stop the virtual camera. Its thread ends within a few milliseconds, so the stop waits for
    /// it here.
    pub(crate) fn stop_virtual_camera(&mut self) {
        let Some(mut vcam) = self.vcam.take() else { return };
        self.update_encode_target();
        if let Err(e) = vcam.instance.stop() {
            self.notices.push(Notice::new("The virtual camera could not be stopped: {error}", vec![("error", e.to_string())], true));
        }
    }

    pub(crate) fn toggle_virtual_camera(&mut self) -> Result<(), StudioError> {
        if self.vcam.is_some() {
            self.stop_virtual_camera();
            Ok(())
        } else {
            self.start_virtual_camera()
        }
    }

    /// Stop a virtual camera whose device failed, with a notice.
    pub(crate) fn poll_virtual_camera(&mut self) {
        let Some(OutputStatus::Failed(e)) = self.vcam.as_ref().map(|v| v.instance.status()) else { return };
        if let Some(mut vcam) = self.vcam.take() {
            self.update_encode_target();
            // The failure is already known and shown; the stop only ends the thread.
            let _ = vcam.instance.stop();
        }
        self.notices.push(Notice::new("The virtual camera stopped: {error}", vec![("error", e)], true));
    }

    /// The virtual camera's queue of frames, while it runs.
    pub(crate) fn virtual_camera_sink(&self) -> Option<Arc<Fifo<Arc<PooledFrame>>>> {
        self.vcam.as_ref().map(|v| v.instance.video())
    }

    /// What the interface shows about the virtual camera: `target` is the device, and `bytes` the
    /// picture bytes sent so far.
    pub fn virtual_camera(&self) -> Option<OutputState> {
        let v = self.vcam.as_ref()?;
        Some(OutputState {
            active: true,
            stopping: false,
            status: v.instance.status(),
            target: v.target.clone(),
            elapsed: v.started.elapsed(),
            bytes: v.stats.bytes.load(std::sync::atomic::Ordering::Relaxed),
            destinations: DestinationCounts::default(),
        })
    }
}
