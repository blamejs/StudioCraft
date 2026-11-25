//! The bundle's two models in a row: the face detector finds faces in a square of the picture,
//! and the face mesh places 478 points with depth on one face.
//!
//! The detector sees a 128×128 crop scaled to −1…1 and its output decodes against 896 anchors
//! (strides 8, 16, 16, 16; two anchors per cell per layer), merged by weighted non-maximum
//! suppression. The mesh sees a 256×256 crop (0…1) of a square 1.5 times the face, turned so the
//! eyes are level, and reports a presence score and a tongue score beside the points. Every
//! buffer is sized when the [`Landmarker`] is made, so neither model allocates when it runs.

use std::sync::Arc;

use crate::bundle::Bundle;
use crate::frame::{Border, Frame, Roi, crop};
use crate::pool::Pool;
use crate::tflite::Workspace;
use crate::{Error, Result};

/// The detector's input side.
pub(crate) const DET_SIZE: usize = 128;
/// The mesh's input side.
pub(crate) const MESH_SIZE: usize = 256;
/// Points of the face mesh: 468 face points and 10 iris points.
pub const MESH_POINTS: usize = 478;
/// Points of the canonical face (the mesh without the iris points).
pub(crate) const CANONICAL_POINTS: usize = 468;
/// The detector's anchors.
pub(crate) const ANCHORS: usize = 896;
/// Detection and presence thresholds.
pub(crate) const MIN_SCORE: f32 = 0.5;
const NMS_IOU: f32 = 0.3;
/// Crops are this much larger than the face.
const ROI_SCALE: f32 = 1.5;
/// Mesh points whose direction levels the crop: the outer eye corners (image left first).
const EYE_LINE: [usize; 2] = [33, 263];

/// A detected face: box `[x0, y0, x1, y1]`, six keypoints (eyes, nose tip, mouth, ears; image
/// left first) and score, in picture pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Detection {
    pub bbox: [f32; 4],
    pub keypoints: [[f32; 2]; 6],
    pub score: f32,
}

impl Detection {
    /// The crop the mesh is computed in for this face.
    pub fn roi(&self) -> Roi {
        roi_of(self.bbox, self.keypoints[0], self.keypoints[1])
    }

    /// The box's center.
    pub fn center(&self) -> [f32; 2] {
        [(self.bbox[0] + self.bbox[2]) / 2.0, (self.bbox[1] + self.bbox[3]) / 2.0]
    }

    /// The box's area.
    pub fn area(&self) -> f32 {
        (self.bbox[2] - self.bbox[0]).max(0.0) * (self.bbox[3] - self.bbox[1]).max(0.0)
    }
}

/// One face's mesh: 478 points in picture pixels (x right, y down) with depth `z` in the same
/// pixel scale (smaller is nearer the camera), the presence score (0 to 1), the tongue score (0 to
/// 1) and the crop it was computed in.
#[derive(Clone, Debug, PartialEq)]
pub struct Mesh {
    pub points: Vec<[f32; 3]>,
    pub presence: f32,
    pub tongue: f32,
    pub roi: Roi,
}

impl Default for Mesh {
    fn default() -> Mesh {
        Mesh::new()
    }
}

impl Mesh {
    /// An empty mesh with room for every point.
    pub fn new() -> Mesh {
        Mesh { points: Vec::with_capacity(MESH_POINTS), presence: 0.0, tongue: 0.0, roi: Roi { center: [0.0; 2], size: 0.0, angle: 0.0 } }
    }

    /// Whether the mesh found a face.
    pub fn found(&self) -> bool {
        self.presence >= MIN_SCORE && self.points.len() == MESH_POINTS
    }

    /// The box around the points' picture positions.
    pub fn bounds(&self) -> [f32; 4] {
        bounds(self.points.iter().map(|p| [p[0], p[1]]))
    }

    /// The crop that frames this face in the next picture.
    pub fn next_roi(&self) -> Option<Roi> {
        let (a, b) = (self.points.get(EYE_LINE[0])?, self.points.get(EYE_LINE[1])?);
        Some(roi_of(self.bounds(), [a[0], a[1]], [b[0], b[1]])).filter(Roi::is_usable)
    }
}

/// SSD anchor centers: per stride group, `fm×fm` cells with two anchors per layer each.
fn anchors() -> Vec<[f32; 2]> {
    let mut out = Vec::with_capacity(ANCHORS);
    for (stride, layers) in [(8usize, 1usize), (16, 3)] {
        let fm = DET_SIZE.div_ceil(stride);
        for y in 0..fm {
            for x in 0..fm {
                for _ in 0..2 * layers {
                    out.push([(x as f32 + 0.5) / fm as f32, (y as f32 + 0.5) / fm as f32]);
                }
            }
        }
    }
    out
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let iw = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = iw * ih;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Weighted non-maximum suppression of `dets` into `out`: each cluster of overlapping detections
/// becomes their score-weighted average, with the best score. `used` is working space. Nothing
/// allocates once the vectors have room for every candidate.
fn weighted_nms(dets: &mut [Detection], used: &mut Vec<bool>, out: &mut Vec<Detection>) {
    out.clear();
    used.clear();
    used.resize(dets.len(), false);
    dets.sort_unstable_by(|a, b| b.score.total_cmp(&a.score));
    for i in 0..dets.len() {
        if used[i] {
            continue;
        }
        let top = dets[i];
        // `top` always joins its own cluster: its overlap with itself is 0 for a degenerate box
        // (NaN or zero area).
        let mut total = 0.0f32;
        let mut avg = Detection { bbox: [0.0; 4], keypoints: [[0.0; 2]; 6], score: top.score };
        for j in i..dets.len() {
            if used[j] || (j != i && iou(&dets[j].bbox, &top.bbox) <= NMS_IOU) {
                continue;
            }
            used[j] = true;
            let d = dets[j];
            total += d.score;
            for k in 0..4 {
                avg.bbox[k] += d.bbox[k] * d.score;
            }
            for (a, p) in avg.keypoints.iter_mut().zip(&d.keypoints) {
                a[0] += p[0] * d.score;
                a[1] += p[1] * d.score;
            }
        }
        if total > 0.0 && total.is_finite() {
            for v in &mut avg.bbox {
                *v /= total;
            }
            for p in &mut avg.keypoints {
                p[0] /= total;
                p[1] /= total;
            }
            out.push(avg);
        } else {
            out.push(top);
        }
    }
}

/// The crop a face's mesh is computed in: `bbox` grown to a square 1.5 times its long side,
/// turned so `from → to` is level.
fn roi_of(bbox: [f32; 4], from: [f32; 2], to: [f32; 2]) -> Roi {
    Roi {
        center: [(bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0],
        size: (bbox[2] - bbox[0]).max(bbox[3] - bbox[1]) * ROI_SCALE,
        angle: (to[1] - from[1]).atan2(to[0] - from[0]),
    }
}

fn bounds(points: impl Iterator<Item = [f32; 2]>) -> [f32; 4] {
    points.fold([f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY], |b, p| {
        [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])]
    })
}

/// The detection of anchor `a` from its 16 regressor values `r`, in the picture pixels of `roi`.
/// `None` when the box or a keypoint is not a finite number: merged with the detections it
/// overlaps, it would make their average not a number too.
fn candidate(r: &[f32], a: [f32; 2], roi: &Roi, score: f32) -> Option<Detection> {
    let s = DET_SIZE as f32;
    let r = r.get(..16)?;
    let (cx, cy, bw, bh) = (r[0] / s + a[0], r[1] / s + a[1], r[2] / s, r[3] / s);
    let to = |u: f32, v: f32| roi.to_frame(u, v);
    let corners =
        [to(cx - bw / 2.0, cy - bh / 2.0), to(cx + bw / 2.0, cy - bh / 2.0), to(cx - bw / 2.0, cy + bh / 2.0), to(cx + bw / 2.0, cy + bh / 2.0)];
    let mut keypoints = [[0.0f32; 2]; 6];
    for (k, p) in keypoints.iter_mut().enumerate() {
        *p = to(r[4 + 2 * k] / s + a[0], r[5 + 2 * k] / s + a[1]);
    }
    // The corners are checked, not only their bounds: `min` and `max` pass over a NaN.
    corners.iter().chain(&keypoints).flatten().all(|v| v.is_finite()).then(|| Detection { bbox: bounds(corners.into_iter()), keypoints, score })
}

pub(crate) fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x.clamp(-100.0, 100.0)).exp())
}

/// The detector and the mesh of one [`Bundle`] with the buffers they run in. One landmarker runs
/// one model at a time; give each tracker its own.
pub struct Landmarker {
    bundle: Arc<Bundle>,
    anchors: Vec<[f32; 2]>,
    det_in: Vec<f32>,
    det_ws: Workspace,
    mesh_in: Vec<f32>,
    mesh_ws: Workspace,
    candidates: Vec<Detection>,
    used: Vec<bool>,
    detections: Vec<Detection>,
}

impl std::fmt::Debug for Landmarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Landmarker").field("workspace_floats", &(self.det_ws.len() + self.mesh_ws.len())).finish()
    }
}

impl Landmarker {
    /// A landmarker for `bundle`'s models, with every buffer allocated.
    pub fn new(bundle: Arc<Bundle>) -> Landmarker {
        let det_ws = bundle.detector.workspace();
        let mesh_ws = bundle.mesh.workspace();
        Landmarker {
            bundle,
            anchors: anchors(),
            det_in: vec![0.0; DET_SIZE * DET_SIZE * 3],
            det_ws,
            mesh_in: vec![0.0; MESH_SIZE * MESH_SIZE * 3],
            mesh_ws,
            candidates: Vec::with_capacity(ANCHORS),
            used: Vec::with_capacity(ANCHORS),
            detections: Vec::with_capacity(ANCHORS),
        }
    }

    /// The bundle the landmarker runs.
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// Faces in the square `roi` of a picture (picture pixels), best first.
    pub fn detect(&mut self, frame: &Frame<'_>, roi: &Roi, pool: &Pool) -> Result<&[Detection]> {
        self.detections.clear();
        if !roi.is_usable() {
            return Ok(&self.detections);
        }
        crop(frame, roi, DET_SIZE, [-1.0, 1.0], Border::Zero, &mut self.det_in);
        let model = &self.bundle.detector;
        model.run_into(&mut self.det_ws, &[&self.det_in], pool)?;
        let (Some(reg), Some(cls)) = (model.output_values(&self.det_ws, 0), model.output_values(&self.det_ws, 1)) else {
            return Err(Error::Model("the face detector gave no output".into()));
        };
        self.candidates.clear();
        for (i, a) in self.anchors.iter().enumerate() {
            let (Some(r), Some(&c)) = (reg.get(i * 16..i * 16 + 16), cls.get(i)) else { break };
            let score = sigmoid(c);
            if score < MIN_SCORE || !score.is_finite() {
                continue;
            }
            if let Some(d) = candidate(r, *a, roi, score) {
                self.candidates.push(d);
            }
        }
        weighted_nms(&mut self.candidates, &mut self.used, &mut self.detections);
        Ok(&self.detections)
    }

    /// The face mesh in `roi`, into `out`.
    pub fn mesh(&mut self, frame: &Frame<'_>, roi: &Roi, pool: &Pool, out: &mut Mesh) -> Result<()> {
        out.points.clear();
        out.presence = 0.0;
        out.tongue = 0.0;
        out.roi = *roi;
        if !roi.is_usable() {
            return Ok(());
        }
        crop(frame, roi, MESH_SIZE, [0.0, 1.0], Border::Replicate, &mut self.mesh_in);
        let model = &self.bundle.mesh;
        model.run_into(&mut self.mesh_ws, &[&self.mesh_in], pool)?;
        let (Some(pts), Some(flag), Some(tongue)) =
            (model.output_values(&self.mesh_ws, 0), model.output_values(&self.mesh_ws, 1), model.output_values(&self.mesh_ws, 2))
        else {
            return Err(Error::Model("the face mesh gave no output".into()));
        };
        let s = MESH_SIZE as f32;
        // Depth is in crop pixels, like x and y: scale it as the crop is scaled.
        let z_scale = roi.size / s;
        for p in pts.as_chunks::<3>().0.iter().take(MESH_POINTS) {
            let [x, y] = roi.to_frame(p[0] / s, p[1] / s);
            out.points.push([x, y, p[2] * z_scale]);
        }
        out.presence = flag.first().copied().map(sigmoid).unwrap_or(0.0);
        // The tongue output is already a probability.
        out.tongue = tongue.first().copied().filter(|v| v.is_finite()).unwrap_or(0.0).clamp(0.0, 1.0);
        if out.points.iter().flatten().any(|v| !v.is_finite()) {
            out.presence = 0.0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_match_the_detector_layout() {
        let a = anchors();
        assert_eq!(a.len(), ANCHORS);
        // Stride 8: 16×16 cells, two anchors each; then stride 16: 8×8 cells, six each.
        assert_eq!(a[0], [0.5 / 16.0, 0.5 / 16.0]);
        assert_eq!(a[1], a[0]);
        assert_eq!(a[2], [1.5 / 16.0, 0.5 / 16.0]);
        assert_eq!(a[512], [0.5 / 8.0, 0.5 / 8.0]);
        assert_eq!(a[517], a[512]);
        assert_eq!(a[518], [1.5 / 8.0, 0.5 / 8.0]);
    }

    fn nms(mut dets: Vec<Detection>) -> Vec<Detection> {
        let (mut used, mut out) = (vec![], vec![]);
        weighted_nms(&mut dets, &mut used, &mut out);
        out
    }

    #[test]
    fn nms_averages_overlapping_detections() {
        let d = |x: f32, s: f32| Detection { bbox: [x, 0.0, x + 10.0, 10.0], keypoints: [[x, 0.0]; 6], score: s };
        let out = nms(vec![d(50.0, 0.6), d(0.0, 0.9), d(1.0, 0.9)]);
        assert_eq!(out.len(), 2);
        assert!((out[0].bbox[0] - 0.5).abs() < 1e-5 && out[0].score == 0.9);
        assert_eq!(out[1].bbox[0], 50.0);
    }

    /// A degenerate box (NaN or zero area) overlaps nothing, not even itself: each still becomes
    /// its own output.
    #[test]
    fn nms_terminates_on_degenerate_boxes() {
        let d = Detection { bbox: [f32::NAN; 4], keypoints: [[f32::NAN; 2]; 6], score: 0.7 };
        assert_eq!(nms(vec![d, d, d]).len(), 3);
        let z = Detection { bbox: [0.0; 4], keypoints: [[0.0; 2]; 6], score: 0.0 };
        assert_eq!(nms(vec![z, z]).len(), 2);
        let n = Detection { bbox: [0.0; 4], keypoints: [[0.0; 2]; 6], score: f32::NAN };
        assert_eq!(nms(vec![n, z, d]).len(), 3);
    }

    /// A candidate with a keypoint that is not a number is dropped, so it does not spoil the
    /// valid detection it overlaps.
    #[test]
    fn candidates_with_bad_geometry_are_dropped() {
        let roi = Roi { center: [64.0, 64.0], size: 128.0, angle: 0.0 };
        let good: Vec<f32> = [0.0, 0.0, 20.0, 20.0].into_iter().chain([1.0; 12]).collect();
        let a = [0.5, 0.5];
        let d = candidate(&good, a, &roi, 0.9).unwrap();
        assert!((d.bbox[0] - 54.0).abs() < 1e-4 && (d.bbox[2] - 74.0).abs() < 1e-4, "{:?}", d.bbox);
        let mut eye = good.clone();
        eye[5] = f32::NAN;
        assert!(candidate(&eye, a, &roi, 0.9).is_none());
        let mut width = good.clone();
        width[2] = f32::INFINITY;
        assert!(candidate(&width, a, &roi, 0.9).is_none());
        assert!(candidate(&good[..15], a, &roi, 0.9).is_none());
    }

    #[test]
    fn a_mesh_frames_the_next_crop_level_with_the_eyes() {
        let mut m = Mesh::new();
        m.points = (0..MESH_POINTS).map(|i| [100.0 + (i % 20) as f32, 50.0 + (i / 20) as f32, 0.0]).collect();
        m.points[33] = [100.0, 60.0, 0.0];
        m.points[263] = [110.0, 70.0, 0.0];
        m.presence = 0.9;
        assert!(m.found());
        let r = m.next_roi().unwrap();
        assert!((r.angle - std::f32::consts::FRAC_PI_4).abs() < 1e-5);
        let b = m.bounds();
        assert_eq!(r.center, [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0]);
        assert!((r.size - 1.5 * (b[3] - b[1]).max(b[2] - b[0])).abs() < 1e-4);
        m.points.truncate(10);
        assert!(!m.found());
        assert!(m.next_roi().is_none());
    }
}
