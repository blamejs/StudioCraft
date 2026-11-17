//! View › Scopes: the waveform, parade, histogram or a vectorscope of the program output.
//!
//! While the window is open the compositor sends a small NV12 copy of the program (at most
//! [`craft_scopes::MAX_W`] × [`craft_scopes::MAX_H`], BT.709 limited range) to [`Scopes::feed`].
//! Each interface frame hands the newest copy to the scopes thread when it is idle. The thread
//! computes and paints the chosen scope into buffers it keeps, makes the texture image, and sends
//! the picture back. The compositor thread only draws the copy, and the interface thread only
//! hands the finished image to its texture. The threads share bounded lock-free queues and atomic
//! flags, and the scopes thread parks while it has nothing to do.
//!
//! The copy is drawn from the program with linear filtering and its chroma is at half width and
//! half height, so hard edges in the program show as intermediate levels on the scopes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use craft_scopes::{Computed, Frame, Grid, Histogram, Matrix, Params, Range, ScopeKind, Signal, paint};
use studiocraft_media::queue::{Fifo, Latest};
use studiocraft_media::video::{PixelFormat, PooledFrame, VideoFrame};
use studiocraft_studio::ScopeFeed;

/// Frames the feed holds; an older one is dropped and goes back to the compositor's pool. With the
/// one the scopes thread holds, the scopes never hold more than the compositor's converter keeps
/// for this sink (its capacity plus one).
const FEED_FRAMES: usize = 1;

/// A painted scope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picture {
    pub kind: ScopeKind,
    /// Premultiplied RGBA8, `width` × `height`, row 0 at the top. Empty for the histogram.
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    /// The texture image of `rgba`, made on the scopes thread; the window moves it into its
    /// texture.
    pub image: Option<egui::ColorImage>,
    /// The bins the window draws for [`ScopeKind::Histogram`].
    pub histogram: Option<Histogram>,
    /// The size of the program copy the scope was computed from.
    pub source: (u32, u32),
}

/// What the scopes thread keeps between frames, so a frame of the same size does not allocate.
#[derive(Default)]
pub struct Work {
    signal: Signal,
    computed: Computed,
    scratch: paint::Scratch,
    trace: Vec<u8>,
}

/// The color a waveform or parade trace is drawn in.
fn trace_color(name: &str) -> [f32; 3] {
    match name {
        "R" => [1.0, 0.25, 0.25],
        "G" => [0.25, 1.0, 0.25],
        "B" => [0.35, 0.45, 1.0],
        "Cb" => [0.45, 0.6, 1.0],
        "Cr" => [1.0, 0.45, 0.6],
        _ => [0.9, 0.9, 0.9],
    }
}

/// Compute and paint `kind` from an NV12 program copy into `out`, reusing its buffers and `work`.
pub fn compute(frame: &VideoFrame, kind: ScopeKind, work: &mut Work, out: &mut Picture) -> Result<(), String> {
    if frame.format != PixelFormat::Nv12 || !frame.is_consistent() {
        return Err(format!("the scopes read NV12 frames; got {:?} {} × {}", frame.format, frame.width, frame.height));
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let luma = w.checked_mul(h).ok_or("the frame is too large")?;
    let (y, uv) = frame.data.split_at_checked(luma).ok_or("the frame is shorter than its luma plane")?;
    let f = Frame::Nv12 { width: w, height: h, y, y_stride: w, uv, uv_stride: w, matrix: Matrix::Bt709, range: Range::Limited };
    work.signal.fill(&f).map_err(|e| e.to_string())?;
    let p = Params::default();
    craft_scopes::compute_into(kind, &work.signal, &p, &mut work.computed);
    out.kind = kind;
    out.source = (frame.width, frame.height);
    match &work.computed {
        Computed::Waveform(wf) => {
            let none = Grid::default();
            let mut layers = [(&none, [0.0; 3]); 4];
            for (l, t) in layers.iter_mut().zip(&wf.traces) {
                *l = (t, trace_color(t.name));
            }
            let n = wf.traces.len().min(layers.len());
            let reference = paint::waveform_reference(wf.per_column);
            (out.width, out.height) = paint::grids_into(layers.get(..n).unwrap_or(&[]), reference, 1.0, &mut work.scratch, &mut out.rgba);
            out.histogram = None;
        }
        Computed::Parade(wf) => {
            // The traces side by side, each painted on its own.
            let reference = paint::waveform_reference(wf.per_column);
            let n = wf.traces.len();
            let (tw, th) = wf.traces.first().map_or((0, 0), |t| (t.cols, t.rows));
            let row = tw.saturating_mul(4);
            out.rgba.clear();
            out.rgba.resize(row.saturating_mul(n).saturating_mul(th), 0);
            for (i, t) in wf.traces.iter().enumerate() {
                paint::grids_into(&[(t, trace_color(t.name))], reference, 1.0, &mut work.scratch, &mut work.trace);
                for (r, src) in work.trace.chunks_exact(row.max(1)).take(th).enumerate() {
                    let at = r * row * n + i * row;
                    if let Some(dst) = out.rgba.get_mut(at..at + row) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            (out.width, out.height) = (tw * n, th);
            out.histogram = None;
        }
        Computed::Vectorscope(v) => {
            (out.width, out.height) = paint::vectorscope_into(v, [0.6, 1.0, 0.6], 1.0, true, &mut work.scratch, &mut out.rgba);
            out.histogram = None;
        }
        Computed::Histogram(hist) => {
            out.rgba.clear();
            (out.width, out.height) = (0, 0);
            match &mut out.histogram {
                Some(mine) => mine.clone_from(hist),
                None => out.histogram = Some(hist.clone()),
            }
        }
        _ => return Err(format!("the scopes cannot draw {kind:?}")),
    }
    out.image = (out.width > 0 && out.height > 0 && out.rgba.len() == out.width * out.height * 4)
        .then(|| egui::ColorImage::from_rgba_premultiplied([out.width, out.height], &out.rgba));
    Ok(())
}

struct Job {
    frame: Arc<PooledFrame>,
    kind: ScopeKind,
    /// A picture the window is done with, for its buffers.
    spare: Option<Picture>,
}

/// A finished picture, or why the copy could not be drawn and the picture's buffers to reuse.
type Outcome = Result<Picture, (String, Picture)>;

/// What the window and the scopes thread share.
struct Shared {
    /// The copy to draw next. The window publishes only while the thread is idle.
    job: Latest<Job>,
    /// Finished pictures. At most one is outstanding, since the thread draws one copy at a time.
    pictures: Fifo<Outcome>,
    /// Set by the window when it hands a copy over, cleared by the thread when the picture is out.
    busy: AtomicBool,
    stop: AtomicBool,
}

/// The open Scopes window's feed and thread.
pub struct Scopes {
    feed: Arc<Fifo<Arc<PooledFrame>>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The newest picture, and whether the window has taken its image.
    pub latest: Option<Picture>,
    pub fresh: bool,
    /// The newest error the thread reported.
    pub error: Option<String>,
    spare: Option<Picture>,
    /// Pictures received.
    pub computed: u64,
    /// Program copies taken from the feed but not drawn because a newer one arrived or the thread
    /// was busy.
    skipped: u64,
    /// The texture the window shows the newest picture with.
    pub texture: Option<egui::TextureHandle>,
}

impl Scopes {
    /// Start the scopes thread. Fails when the thread cannot start.
    pub fn start() -> Result<Scopes, String> {
        let shared = Arc::new(Shared { job: Latest::new(), pictures: Fifo::new(2), busy: AtomicBool::new(false), stop: AtomicBool::new(false) });
        let theirs = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("scopes".into())
            .spawn(move || {
                let mut work = Work::default();
                loop {
                    if let Some(job) = theirs.job.take() {
                        let mut out = job.spare.unwrap_or_default();
                        let r = match compute(&job.frame, job.kind, &mut work, &mut out) {
                            Ok(()) => Ok(out),
                            Err(e) => Err((e, out)),
                        };
                        drop(job.frame);
                        theirs.pictures.push(r);
                        theirs.busy.store(false, Ordering::Release);
                    } else if theirs.stop.load(Ordering::Acquire) {
                        break;
                    } else {
                        std::thread::park();
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Scopes {
            feed: Arc::new(Fifo::new(FEED_FRAMES)),
            shared,
            thread: Some(thread),
            latest: None,
            fresh: false,
            error: None,
            spare: None,
            computed: 0,
            skipped: 0,
            texture: None,
        })
    }

    /// The feed to hand the studio: frames go to [`Scopes::feed`], at most the scopes' signal size.
    pub fn feed(&self) -> ScopeFeed {
        ScopeFeed { sink: Arc::clone(&self.feed), max_width: craft_scopes::MAX_W as u32, max_height: craft_scopes::MAX_H as u32 }
    }

    /// Program copies the scopes never drew: dropped from the full feed, replaced by a newer copy,
    /// or arriving while the thread was busy.
    pub fn dropped(&self) -> u64 {
        self.feed.dropped().saturating_add(self.skipped)
    }

    /// Hand the newest program copy to the thread when it is idle, and take any finished picture.
    /// Other copies go back to the compositor's pool.
    pub fn pump(&mut self, kind: ScopeKind) {
        let mut newest = None;
        while let Some(f) = self.feed.pop() {
            if newest.replace(f).is_some() {
                self.skipped += 1;
            }
        }
        if let Some(frame) = newest {
            match &self.thread {
                Some(t) if !self.shared.busy.load(Ordering::Acquire) => {
                    self.shared.busy.store(true, Ordering::Release);
                    self.shared.job.publish(Job { frame, kind, spare: self.spare.take() });
                    t.thread().unpark();
                }
                // The thread is still drawing the last copy; this one is dropped.
                _ => self.skipped += 1,
            }
        }
        while let Some(r) = self.shared.pictures.pop() {
            self.received(r);
        }
    }

    fn received(&mut self, r: Outcome) {
        let spare = match r {
            Ok(p) => {
                self.fresh = true;
                self.error = None;
                self.computed += 1;
                self.latest.replace(p)
            }
            Err((e, buffers)) => {
                self.error = Some(e);
                Some(buffers)
            }
        };
        if self.spare.is_none() {
            self.spare = spare;
        }
    }

    /// Wait for the thread to finish the copy handed over with [`Scopes::pump`] and take its
    /// picture (tests). False when no picture came.
    #[cfg(test)]
    pub fn wait(&mut self) -> bool {
        while self.shared.busy.load(Ordering::Acquire) && self.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            std::thread::yield_now();
        }
        let mut got = false;
        while let Some(r) = self.shared.pictures.pop() {
            self.received(r);
            got = true;
        }
        got
    }

    /// The feed the compositor writes to, for tests that stand in for it.
    #[cfg(test)]
    pub fn feed_queue(&self) -> &Arc<Fifo<Arc<PooledFrame>>> {
        &self.feed
    }
}

impl Drop for Scopes {
    fn drop(&mut self) {
        // The thread ends after the copy it is drawing.
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// The scopes in the order the window lists them.
pub const MENU_ORDER: [ScopeKind; 5] =
    [ScopeKind::Waveform, ScopeKind::Parade, ScopeKind::Histogram, ScopeKind::VectorscopeYuv, ScopeKind::VectorscopeHls];

/// Draw a histogram's R', G', B' and Y' bins as four outlines, scaled to the fullest bin.
pub fn draw_histogram(ui: &mut egui::Ui, h: &Histogram) {
    let width = ui.available_width().max(64.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, width * 0.5), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, egui::Color32::BLACK);
    let peak = [&h.r, &h.g, &h.b, &h.y].iter().flat_map(|c| c.iter()).copied().max().unwrap_or(0).max(1) as f32;
    let colors = [
        egui::Color32::from_rgb(235, 70, 70),
        egui::Color32::from_rgb(70, 220, 70),
        egui::Color32::from_rgb(90, 120, 255),
        egui::Color32::from_rgb(220, 220, 220),
    ];
    for (bins, color) in [&h.r, &h.g, &h.b, &h.y].into_iter().zip(colors) {
        let n = bins.len().max(2) - 1;
        let points: Vec<egui::Pos2> = bins
            .iter()
            .enumerate()
            .map(|(i, &c)| egui::pos2(rect.left() + rect.width() * i as f32 / n as f32, rect.bottom() - rect.height() * c as f32 / peak))
            .collect();
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.0, color)));
    }
}

/// The scope a saved setting names, or the waveform.
pub fn kind_of(setting: &str) -> ScopeKind {
    ScopeKind::from_name(setting).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use studiocraft_media::pool::Pool;
    use studiocraft_media::video::FramePool;

    use super::*;

    /// An NV12 frame: left half 100 % white, right half black, BT.709 limited range.
    fn half_white(pool: &Arc<FramePool>, w: u32, h: u32) -> Arc<PooledFrame> {
        let mut f = pool.take();
        assert!(f.reshape(w, h, PixelFormat::Nv12));
        let (wu, hu) = (w as usize, h as usize);
        for (i, v) in f.data.iter_mut().enumerate() {
            *v = if i < wu * hu { if i % wu < wu / 2 { 235 } else { 16 } } else { 128 };
        }
        Arc::new(f)
    }

    #[test]
    fn every_kind_paints_from_an_nv12_copy_and_reuses_its_buffers() {
        let pool: Arc<FramePool> = Pool::new(2, VideoFrame::default);
        let frame = half_white(&pool, 480, 270);
        let mut work = Work::default();
        for kind in ScopeKind::ALL {
            let mut out = Picture::default();
            compute(&frame, kind, &mut work, &mut out).unwrap();
            assert_eq!((out.kind, out.source), (kind, (480, 270)));
            let before = (out.rgba.as_ptr(), out.rgba.capacity());
            compute(&frame, kind, &mut work, &mut out).unwrap();
            if kind == ScopeKind::Histogram {
                let h = out.histogram.as_ref().unwrap();
                assert_eq!((h.y[255], h.y[0]), (240 * 270, 240 * 270), "half the samples at white and half at black");
                assert!(out.rgba.is_empty());
            } else {
                assert_eq!(out.rgba.len(), out.width * out.height * 4);
                assert!(out.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0), "{kind:?} painted nothing");
                assert_eq!((out.rgba.as_ptr(), out.rgba.capacity()), before, "{kind:?} reallocated");
            }
        }
    }

    #[test]
    fn the_waveform_puts_white_and_black_on_their_rows() {
        let pool: Arc<FramePool> = Pool::new(1, VideoFrame::default);
        let mut out = Picture::default();
        compute(&half_white(&pool, 64, 32), ScopeKind::Waveform, &mut Work::default(), &mut out).unwrap();
        assert_eq!((out.width, out.height), (64, 256));
        let alpha = |x: usize, row_from_top: usize| out.rgba[(row_from_top * 64 + x) * 4 + 3];
        // Row 0 at the top is level 255 (white), the bottom row level 0 (black).
        assert!(alpha(10, 0) > 0 && alpha(10, 255) == 0, "white column");
        assert!(alpha(50, 255) > 0 && alpha(50, 0) == 0, "black column");
    }

    #[test]
    fn the_parade_draws_its_traces_side_by_side() {
        let pool: Arc<FramePool> = Pool::new(1, VideoFrame::default);
        let mut out = Picture::default();
        compute(&half_white(&pool, 64, 32), ScopeKind::Parade, &mut Work::default(), &mut out).unwrap();
        assert_eq!((out.width, out.height), (3 * 64, 256));
        // Each trace's white half has its top row lit, in its own color.
        for (i, ch) in [(0usize, 0usize), (1, 1), (2, 2)] {
            let px = &out.rgba[(i * 64 + 10) * 4..(i * 64 + 10) * 4 + 4];
            assert!(px[3] > 0 && px[ch] == px[0].max(px[1]).max(px[2]), "trace {i}: {px:?}");
        }
    }

    #[test]
    fn a_frame_that_is_not_nv12_is_refused() {
        let mut f = VideoFrame::default();
        assert!(f.reshape(4, 4, PixelFormat::Rgba8));
        let err = compute(&f, ScopeKind::Waveform, &mut Work::default(), &mut Picture::default()).unwrap_err();
        assert!(err.contains("NV12"), "{err}");
        let short = VideoFrame { width: 4, height: 4, format: PixelFormat::Nv12, data: vec![0; 5], ..VideoFrame::default() };
        assert!(compute(&short, ScopeKind::Waveform, &mut Work::default(), &mut Picture::default()).is_err());
    }

    #[test]
    fn the_thread_computes_the_newest_copy_and_hands_buffers_back() {
        let pool: Arc<FramePool> = Pool::new(4, VideoFrame::default);
        let mut s = Scopes::start().unwrap();
        assert_eq!(s.feed().max_width, 480);
        for round in 0..3u64 {
            s.feed_queue().push(half_white(&pool, 16, 8));
            s.feed_queue().push(half_white(&pool, 32, 16));
            s.pump(ScopeKind::VectorscopeYuv);
            assert!(s.wait());
            let p = s.latest.as_ref().unwrap();
            assert_eq!((p.kind, p.source), (ScopeKind::VectorscopeYuv, (32, 16)), "the newest copy");
            assert_eq!(s.computed, round + 1);
            assert_eq!(s.dropped(), round + 1, "the older copy was dropped from the feed");
            assert!(s.fresh && s.error.is_none());
            assert!(p.image.as_ref().is_some_and(|i| i.size == [p.width, p.height]), "the thread made the texture image");
        }
        drop(s);
        assert_eq!(pool.free(), pool.made(), "every frame went back to the pool");
    }

    #[test]
    fn a_copy_that_cannot_be_drawn_hands_its_buffers_back() {
        let pool: Arc<FramePool> = Pool::new(2, VideoFrame::default);
        let mut s = Scopes::start().unwrap();
        let mut f = pool.take();
        assert!(f.reshape(4, 4, PixelFormat::Rgba8));
        s.feed_queue().push(Arc::new(f));
        s.pump(ScopeKind::Waveform);
        assert!(s.wait());
        assert!(s.error.as_deref().is_some_and(|e| e.contains("NV12")));
        assert!(s.latest.is_none() && s.spare.is_some(), "the picture's buffers come back for the next copy");
    }

    #[test]
    fn a_busy_thread_holds_no_queued_copy() {
        let pool: Arc<FramePool> = Pool::new(2, VideoFrame::default);
        let mut s = Scopes::start().unwrap();
        // While the thread draws a copy, a newer one goes straight back to the pool.
        s.shared.busy.store(true, Ordering::Release);
        s.feed_queue().push(half_white(&pool, 16, 8));
        s.pump(ScopeKind::Waveform);
        assert_eq!(s.dropped(), 1);
        assert_eq!(pool.free(), pool.made(), "the scopes hold no copy");
        s.shared.busy.store(false, Ordering::Release);
    }

    #[test]
    fn a_saved_setting_names_a_scope_or_reads_as_the_waveform() {
        assert_eq!(kind_of("parade"), ScopeKind::Parade);
        assert_eq!(kind_of("vectorscopeHls"), ScopeKind::VectorscopeHls);
        assert_eq!(kind_of("nonsense"), ScopeKind::Waveform);
        assert_eq!(kind_of(""), ScopeKind::Waveform);
    }
}
