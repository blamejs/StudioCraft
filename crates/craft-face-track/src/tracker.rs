//! Following one face from picture to picture.
//!
//! While no face is followed, the tracker searches: it runs the face detector over the whole
//! picture, picks a face by [`FaceChoice`], runs the face mesh in the crop around it, and runs the
//! mesh once more in the crop the first mesh frames, which is level with the eyes. Once a face is
//! followed, each picture takes one mesh run, in the crop the previous mesh framed. A presence
//! score below 0.5 loses the face, and the tracker searches again in the same picture.
//!
//! While the presence score is weak (below [`TrackerOptions::weak_presence`]), the tracker also
//! runs the detector, at most once every [`TrackerOptions::redetect_every`] seconds. A detection
//! that overlaps the followed face frames the next picture's crop; a detection of another face is
//! ignored, so a second person walking past does not take over. The detector is the smaller
//! model, and the mesh still runs once on that picture, so following never stops to search.
//!
//! The tracker is synchronous: [`Tracker::track`] runs on the caller's thread with the models'
//! work spread over the tracker's [`Pool`], and returns. Times come from the caller, in seconds.
//! Once its buffers are warm, tracking does not allocate.

use std::sync::Arc;

use crate::bundle::Bundle;
use crate::expressions::{Blendshape, Expressions, Measures, measure};
use crate::filter::{OneEuro, RotationFilter, Smoothing};
use crate::frame::{Frame, Roi};
use crate::landmarker::{Detection, Landmarker, MESH_POINTS, Mesh};
use crate::pool::Pool;
use crate::pose::{HeadPose, fit};
use crate::{Error, Result};

/// Which face a search picks when it finds several.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FaceChoice {
    /// The largest face (usually the nearest).
    #[default]
    Largest,
    /// The face furthest to the left of the picture.
    Leftmost,
    /// The face furthest to the right of the picture.
    Rightmost,
}

/// How a [`Tracker`] follows a face.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackerOptions {
    pub choice: FaceChoice,
    pub smoothing: Smoothing,
    /// Seconds between detector runs while the presence score is weak.
    pub redetect_every: f64,
    /// The presence score under which the detector runs to check the crop (0.5 to 1).
    pub weak_presence: f32,
}

impl Default for TrackerOptions {
    fn default() -> TrackerOptions {
        TrackerOptions { choice: FaceChoice::Largest, smoothing: Smoothing::Medium, redetect_every: 2.0, weak_presence: 0.9 }
    }
}

impl TrackerOptions {
    fn check(&self) -> Result<()> {
        if !self.redetect_every.is_finite() || self.redetect_every < 0.0 {
            return Err(Error::Model(format!("the re-detection interval is {} seconds; it is 0 or more", self.redetect_every)));
        }
        if !(0.5..=1.0).contains(&self.weak_presence) {
            return Err(Error::Model(format!("the weak presence score is {}; it is 0.5 to 1", self.weak_presence)));
        }
        Ok(())
    }
}

/// What the tracker reports for one picture.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackFrame {
    /// Whether a face is followed in this picture. The other fields hold the last face's values
    /// when it is not.
    pub found: bool,
    /// The face mesh's presence score, 0 to 1.
    pub presence: f32,
    /// The 478 mesh points (smoothed): picture pixels, x right and y down, and depth in the same
    /// pixel scale (smaller is nearer the camera). The first 468 are the face, then the left
    /// iris (center first) and the right iris, as the picture shows them.
    pub points: Vec<[f32; 3]>,
    pub pose: HeadPose,
    pub expressions: Expressions,
    /// The time given with the picture.
    pub time: f64,
}

impl Default for TrackFrame {
    fn default() -> TrackFrame {
        TrackFrame::new()
    }
}

impl TrackFrame {
    /// An empty frame with room for every point.
    pub fn new() -> TrackFrame {
        TrackFrame {
            found: false,
            presence: 0.0,
            points: Vec::with_capacity(MESH_POINTS),
            pose: HeadPose::default(),
            expressions: Expressions::default(),
            time: 0.0,
        }
    }
}

/// The smoothing filters of one followed face.
struct Filters {
    points: Vec<[OneEuro; 3]>,
    rotation: RotationFilter,
    /// Center x, y, z and scale.
    position: [OneEuro; 4],
    expressions: [OneEuro; 52],
}

impl Filters {
    fn new(s: Smoothing) -> Filters {
        Filters {
            points: vec![[OneEuro::new(s.points()); 3]; MESH_POINTS],
            rotation: RotationFilter::new(s.rotation()),
            position: [OneEuro::new(s.position()); 4],
            expressions: [OneEuro::new(s.expressions()); 52],
        }
    }

    fn reset(&mut self) {
        self.points.iter_mut().flatten().for_each(OneEuro::reset);
        self.rotation.reset();
        self.position.iter_mut().for_each(OneEuro::reset);
        self.expressions.iter_mut().for_each(OneEuro::reset);
    }
}

/// Follows one face through a stream of pictures. Give each camera its own tracker; trackers
/// that share a [`Pool`] share its threads.
pub struct Tracker {
    landmarker: Landmarker,
    pool: Pool,
    options: TrackerOptions,
    neutral: Measures,
    /// The crop of the next picture while a face is followed.
    follow: Option<Roi>,
    last_detect: f64,
    last_time: f64,
    mesh: Mesh,
    filters: Filters,
}

impl std::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker").field("following", &self.follow.is_some()).field("options", &self.options).finish()
    }
}

impl Tracker {
    /// A tracker for `bundle`'s models, running on `pool`.
    pub fn new(bundle: Arc<Bundle>, pool: Pool, options: TrackerOptions) -> Result<Tracker> {
        options.check()?;
        let neutral = Measures::neutral(&bundle.canonical)
            .ok_or_else(|| Error::Bundle("the canonical face lacks the points expressions are measured on".into()))?;
        Ok(Tracker {
            landmarker: Landmarker::new(bundle),
            pool,
            options,
            neutral,
            follow: None,
            last_detect: f64::NEG_INFINITY,
            last_time: f64::NEG_INFINITY,
            mesh: Mesh::new(),
            filters: Filters::new(options.smoothing),
        })
    }

    pub fn options(&self) -> TrackerOptions {
        self.options
    }

    /// Change how the tracker follows; new smoothing applies from the next picture.
    pub fn set_options(&mut self, options: TrackerOptions) -> Result<()> {
        options.check()?;
        if options.smoothing != self.options.smoothing {
            let s = options.smoothing;
            self.filters.points.iter_mut().flatten().for_each(|f| f.set_params(s.points()));
            self.filters.rotation.set_params(s.rotation());
            self.filters.position.iter_mut().for_each(|f| f.set_params(s.position()));
            self.filters.expressions.iter_mut().for_each(|f| f.set_params(s.expressions()));
        }
        self.options = options;
        Ok(())
    }

    /// Whether a face is being followed.
    pub fn is_following(&self) -> bool {
        self.follow.is_some()
    }

    /// Stop following: the next picture searches the whole picture.
    pub fn reset(&mut self) {
        self.follow = None;
        self.last_detect = f64::NEG_INFINITY;
        self.filters.reset();
    }

    /// Track the face in `frame`, taken at `time` seconds, into `out`.
    pub fn track(&mut self, frame: &Frame<'_>, time: f64, out: &mut TrackFrame) -> Result<()> {
        out.time = time;
        if !time.is_finite() {
            return Err(Error::Frame(format!("the picture's time is {time}")));
        }
        if time < self.last_time {
            // The clock went back (a new stream): start over.
            self.reset();
        }
        self.last_time = time;
        let mut found = match self.follow {
            Some(roi) => {
                self.landmarker.mesh(frame, &roi, &self.pool, &mut self.mesh)?;
                self.mesh.found()
            }
            None => false,
        };
        if !found {
            if self.follow.take().is_some() {
                self.filters.reset();
            }
            found = self.search(frame, time)?;
        }
        out.found = found;
        out.presence = self.mesh.presence;
        if !found {
            return Ok(());
        }
        self.follow = self.mesh.next_roi();
        if self.mesh.presence < self.options.weak_presence && time - self.last_detect >= self.options.redetect_every {
            self.recheck(frame, time)?;
        }
        self.report(time, out);
        Ok(())
    }

    /// The square covering the whole picture.
    fn whole(frame: &Frame<'_>) -> Roi {
        let (w, h) = (frame.width() as f32, frame.height() as f32);
        Roi { center: [w / 2.0, h / 2.0], size: w.max(h), angle: 0.0 }
    }

    /// Find a face in the whole picture and mesh it; whether one was found.
    fn search(&mut self, frame: &Frame<'_>, time: f64) -> Result<bool> {
        self.last_detect = time;
        let choice = self.options.choice;
        let pick = {
            let dets = self.landmarker.detect(frame, &Tracker::whole(frame), &self.pool)?;
            let key = |d: &Detection| match choice {
                FaceChoice::Largest => d.area(),
                FaceChoice::Leftmost => -d.center()[0],
                FaceChoice::Rightmost => d.center()[0],
            };
            dets.iter().filter(|d| d.roi().is_usable()).max_by(|a, b| key(a).total_cmp(&key(b))).copied()
        };
        let Some(d) = pick else { return Ok(false) };
        self.landmarker.mesh(frame, &d.roi(), &self.pool, &mut self.mesh)?;
        if !self.mesh.found() {
            return Ok(false);
        }
        // The detector's box is rough and not turned with the head: mesh once more in the crop the
        // first mesh frames.
        if let Some(roi) = self.mesh.next_roi() {
            self.landmarker.mesh(frame, &roi, &self.pool, &mut self.mesh)?;
        }
        Ok(self.mesh.found())
    }

    /// Run the detector to check a weakly held face; a detection of the same face frames the next
    /// crop.
    fn recheck(&mut self, frame: &Frame<'_>, time: f64) -> Result<()> {
        self.last_detect = time;
        let held = self.mesh.bounds();
        let held_center = [(held[0] + held[2]) / 2.0, (held[1] + held[3]) / 2.0];
        let dets = self.landmarker.detect(frame, &Tracker::whole(frame), &self.pool)?;
        let same = dets
            .iter()
            .filter(|d| {
                let c = d.center();
                c[0] >= held[0] && c[0] <= held[2] && c[1] >= held[1] && c[1] <= held[3]
            })
            .min_by(|a, b| {
                let da = (a.center()[0] - held_center[0]).hypot(a.center()[1] - held_center[1]);
                let db = (b.center()[0] - held_center[0]).hypot(b.center()[1] - held_center[1]);
                da.total_cmp(&db)
            });
        if let Some(roi) = same.map(Detection::roi).filter(Roi::is_usable) {
            self.follow = Some(roi);
        }
        Ok(())
    }

    /// Fill `out` from the current mesh: the pose, the expressions and the smoothed points.
    fn report(&mut self, time: f64, out: &mut TrackFrame) {
        let bundle = self.landmarker.bundle();
        let (canonical, basis) = (&bundle.canonical, &bundle.basis);
        let points = &self.mesh.points;
        if let Some(f) = fit(canonical, basis, points) {
            let pose = HeadPose::from_fit(&f, canonical, basis, points);
            measure(points, &f, &self.neutral, self.mesh.tongue, &mut out.expressions);
            let fl = &mut self.filters;
            let rotation = fl.rotation.filter(pose.rotation, time);
            let mut p = pose;
            p.rotation = rotation;
            let r = crate::pose::matrix(rotation.map(f64::from));
            let [yaw, pitch, roll] = crate::pose::angles(&r);
            p.yaw = yaw.to_degrees() as f32;
            p.pitch = pitch.to_degrees() as f32;
            p.roll = roll.to_degrees() as f32;
            for (k, v) in p.center.iter_mut().enumerate() {
                if let Some(f) = fl.position.get_mut(k) {
                    *v = f.filter(f64::from(*v), time) as f32;
                }
            }
            if let Some(f) = fl.position.get_mut(3) {
                p.scale = f.filter(f64::from(p.scale), time) as f32;
            }
            out.pose = p;
            for s in Blendshape::ALL {
                if let Some(f) = fl.expressions.get_mut(s.index()) {
                    let w = f.filter(f64::from(out.expressions.get(s)), time) as f32;
                    out.expressions.set(s, w);
                }
            }
        }
        out.points.clear();
        for (p, f) in points.iter().zip(self.filters.points.iter_mut()) {
            out.points.push([
                f[0].filter(f64::from(p[0]), time) as f32,
                f[1].filter(f64::from(p[1]), time) as f32,
                f[2].filter(f64::from(p[2]), time) as f32,
            ]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_checked() {
        assert!(TrackerOptions::default().check().is_ok());
        for bad in [
            TrackerOptions { redetect_every: -1.0, ..TrackerOptions::default() },
            TrackerOptions { redetect_every: f64::NAN, ..TrackerOptions::default() },
            TrackerOptions { weak_presence: 0.2, ..TrackerOptions::default() },
            TrackerOptions { weak_presence: 1.5, ..TrackerOptions::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_whole_picture_is_one_square() {
        let px = vec![0u8; 8 * 4 * 4];
        let f = Frame::packed(&px, 8, 4).unwrap();
        let r = Tracker::whole(&f);
        assert_eq!(r, Roi { center: [4.0, 2.0], size: 8.0, angle: 0.0 });
    }
}
