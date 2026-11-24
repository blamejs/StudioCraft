//! Head pose from a face mesh: the rotation, position and scale that best carry the canonical face
//! onto the measured points.
//!
//! The fit is a weighted least-squares similarity transform (rotation, uniform scale and
//! translation) over the points the bundle's geometry weights, solved in closed form with unit
//! quaternions (B. K. P. Horn, "Closed-form solution of absolute orientation using unit
//! quaternions", J. Opt. Soc. Am. A 4(4), 1987). It runs in a fixed number of steps and does not
//! allocate.
//!
//! Angles use the camera's view: a positive yaw turns the face toward the right of the picture
//! (the subject's left), a positive pitch tilts it up and a positive roll turns it counterclockwise
//! as the camera sees it. The angles decompose the rotation as yaw about the vertical axis, then
//! pitch, then roll.

/// A head's pose in one picture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeadPose {
    /// The rotation from the canonical face to the head, as a unit quaternion `[w, x, y, z]` in a
    /// frame with x to the right of the picture, y up and z toward the camera.
    pub rotation: [f32; 4],
    /// Yaw, pitch and roll, in degrees.
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    /// Where the canonical face's origin lands, in picture pixels (x right, y down) with depth in
    /// the mesh's pixel scale (smaller is nearer the camera).
    pub center: [f32; 3],
    /// Picture pixels per centimeter of the canonical face.
    pub scale: f32,
    /// The weighted root-mean-square distance between the fitted face and the points, in pixels.
    pub residual: f32,
}

type V3 = [f64; 3];
type M3 = [[f64; 3]; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn mat_vec(m: &M3, v: V3) -> V3 {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

fn transpose_vec(m: &M3, v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ]
}

/// A picture point (x right, y down, depth smaller nearer) in the fit's frame (y up, z toward the
/// camera).
pub(crate) fn to_fit_frame(p: [f32; 3]) -> V3 {
    [p[0] as f64, -(p[1] as f64), -(p[2] as f64)]
}

/// The rotation matrix of the unit quaternion `[w, x, y, z]`.
pub(crate) fn matrix(q: [f64; 4]) -> M3 {
    let [w, x, y, z] = q;
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

/// Yaw, pitch and roll (radians) of a rotation matrix, as [`HeadPose`] describes them.
pub(crate) fn angles(r: &M3) -> [f64; 3] {
    let b = (-r[1][2]).clamp(-1.0, 1.0).asin();
    let yaw = r[0][2].atan2(r[2][2]);
    let roll = r[1][0].atan2(r[1][1]);
    [yaw, -b, roll]
}

/// The eigenvector of the largest eigenvalue of a symmetric 4×4 matrix (cyclic Jacobi).
fn top_eigenvector(mut a: [[f64; 4]; 4]) -> [f64; 4] {
    // Scaled to a largest entry of 1, so the stop test below is relative to the matrix: weights
    // scaled alike give the same eigenvector.
    let big = a.iter().flatten().fold(0.0f64, |m, v| m.max(v.abs()));
    if big > 0.0 && big.is_finite() {
        a = a.map(|row| row.map(|v| v / big));
    }
    let mut v = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
    for _ in 0..32 {
        let off: f64 = (0..4).flat_map(|i| (0..4).filter(move |&j| j != i).map(move |j| (i, j))).map(|(i, j)| a[i][j] * a[i][j]).sum();
        if off < 1e-24 {
            break;
        }
        for p in 0..3 {
            for q in p + 1..4 {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for row in &mut a {
                    let (akp, akq) = (row[p], row[q]);
                    row[p] = c * akp - s * akq;
                    row[q] = s * akp + c * akq;
                }
                for k in 0..4 {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for row in &mut v {
                    let (vp, vq) = (row[p], row[q]);
                    row[p] = c * vp - s * vq;
                    row[q] = s * vp + c * vq;
                }
            }
        }
    }
    let best = (0..4).max_by(|&i, &j| a[i][i].total_cmp(&a[j][j])).unwrap_or(0);
    [v[0][best], v[1][best], v[2][best], v[3][best]]
}

/// The fitted transform in the fit's frame: `point ≈ scale · R · canonical + t`.
#[derive(Clone, Debug)]
pub(crate) struct Fit {
    pub q: [f64; 4],
    pub r: M3,
    pub scale: f64,
    pub t: V3,
}

impl Fit {
    /// A picture point carried into the canonical face's frame (centimeters).
    pub fn to_canonical(&self, p: [f32; 3]) -> V3 {
        let d = sub(to_fit_frame(p), self.t);
        let v = transpose_vec(&self.r, d);
        let s = if self.scale > 0.0 { 1.0 / self.scale } else { 0.0 };
        [v[0] * s, v[1] * s, v[2] * s]
    }
}

/// Whether the weighted `points` (in the fit's frame through `frame`), less their weighted center
/// `center`, reach out in two independent directions. Points on one line leave the rotation about
/// that line undetermined.
fn spans_a_plane(points: &[[f32; 3]], basis: &[(usize, f32)], center: V3, frame: fn([f32; 3]) -> V3) -> bool {
    let mut m = [[0.0f64; 3]; 3];
    for &(i, w) in basis {
        let Some(p) = points.get(i) else { return false };
        let d = sub(frame(*p), center);
        for a in 0..3 {
            for b in 0..3 {
                m[a][b] += w as f64 * d[a] * d[b];
            }
        }
    }
    let trace = m[0][0] + m[1][1] + m[2][2];
    // The 2×2 principal minors add up to the products of the eigenvalues in pairs, which are all
    // zero for points on a line.
    let minors = m[0][0] * m[1][1] - m[0][1] * m[0][1] + m[0][0] * m[2][2] - m[0][2] * m[0][2] + m[1][1] * m[2][2] - m[1][2] * m[1][2];
    trace > 0.0 && minors > 1e-12 * trace * trace
}

/// The weighted similarity transform carrying `canonical` onto `points` (picture pixels), over the
/// `basis` points and weights. `None` when the points are too few, degenerate or not numbers.
pub(crate) fn fit(canonical: &[[f32; 3]], basis: &[(usize, f32)], points: &[[f32; 3]]) -> Option<Fit> {
    let mut wsum = 0.0f64;
    let (mut cc, mut pc) = ([0.0f64; 3], [0.0f64; 3]);
    for &(i, w) in basis {
        let (c, p) = (canonical.get(i)?, points.get(i)?);
        let (c, p, w) = ([c[0] as f64, c[1] as f64, c[2] as f64], to_fit_frame(*p), w as f64);
        wsum += w;
        for k in 0..3 {
            cc[k] += w * c[k];
            pc[k] += w * p[k];
        }
    }
    if wsum.is_nan() || wsum <= 0.0 {
        return None;
    }
    for k in 0..3 {
        cc[k] /= wsum;
        pc[k] /= wsum;
    }
    // The weighted cross-covariance S[a][b] = Σ w·c_a·p_b and the canonical spread.
    let mut s = [[0.0f64; 3]; 3];
    let mut spread = 0.0f64;
    for &(i, w) in basis {
        let (c, p) = (canonical.get(i)?, points.get(i)?);
        let c = sub([c[0] as f64, c[1] as f64, c[2] as f64], cc);
        let p = sub(to_fit_frame(*p), pc);
        let w = w as f64;
        spread += w * dot(c, c);
        for a in 0..3 {
            for b in 0..3 {
                s[a][b] += w * c[a] * p[b];
            }
        }
    }
    if spread.is_nan() || spread <= 0.0 || s.iter().flatten().any(|v| !v.is_finite()) {
        return None;
    }
    if !spans_a_plane(canonical, basis, cc, |c| [c[0] as f64, c[1] as f64, c[2] as f64]) || !spans_a_plane(points, basis, pc, to_fit_frame) {
        return None;
    }
    let [[sxx, sxy, sxz], [syx, syy, syz], [szx, szy, szz]] = s;
    let n = [
        [sxx + syy + szz, syz - szy, szx - sxz, sxy - syx],
        [syz - szy, sxx - syy - szz, sxy + syx, szx + sxz],
        [szx - sxz, sxy + syx, -sxx + syy - szz, syz + szy],
        [sxy - syx, szx + sxz, syz + szy, -sxx - syy + szz],
    ];
    let q = top_eigenvector(n);
    let len = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    if len.is_nan() || len <= 0.0 {
        return None;
    }
    let mut q = q.map(|v| v / len);
    if q[0] < 0.0 {
        q = q.map(|v| -v);
    }
    let r = matrix(q);
    let mut num = 0.0f64;
    for &(i, w) in basis {
        let (c, p) = (canonical.get(i)?, points.get(i)?);
        let c = sub([c[0] as f64, c[1] as f64, c[2] as f64], cc);
        let p = sub(to_fit_frame(*p), pc);
        num += w as f64 * dot(p, mat_vec(&r, c));
    }
    let scale = num / spread;
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let rc = mat_vec(&r, cc);
    let t = [pc[0] - scale * rc[0], pc[1] - scale * rc[1], pc[2] - scale * rc[2]];
    Some(Fit { q, r, scale, t })
}

impl HeadPose {
    /// The pose of the face whose mesh is `points` (picture pixels with depth), fitted to the
    /// bundle's `canonical` face over its `basis` points. `None` when no fit exists.
    pub fn of(canonical: &[[f32; 3]], basis: &[(usize, f32)], points: &[[f32; 3]]) -> Option<HeadPose> {
        fit(canonical, basis, points).map(|f| HeadPose::from_fit(&f, canonical, basis, points))
    }

    pub(crate) fn from_fit(f: &Fit, canonical: &[[f32; 3]], basis: &[(usize, f32)], points: &[[f32; 3]]) -> HeadPose {
        let [yaw, pitch, roll] = angles(&f.r);
        let (mut err, mut wsum) = (0.0f64, 0.0f64);
        for &(i, w) in basis {
            let (Some(c), Some(p)) = (canonical.get(i), points.get(i)) else { continue };
            let rc = mat_vec(&f.r, [c[0] as f64, c[1] as f64, c[2] as f64]);
            let d = sub(to_fit_frame(*p), [f.scale * rc[0] + f.t[0], f.scale * rc[1] + f.t[1], f.scale * rc[2] + f.t[2]]);
            err += w as f64 * dot(d, d);
            wsum += w as f64;
        }
        HeadPose {
            rotation: f.q.map(|v| v as f32),
            yaw: yaw.to_degrees() as f32,
            pitch: pitch.to_degrees() as f32,
            roll: roll.to_degrees() as f32,
            center: [f.t[0] as f32, -f.t[1] as f32, -f.t[2] as f32],
            scale: f.scale as f32,
            residual: if wsum > 0.0 { (err / wsum).sqrt() as f32 } else { 0.0 },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small face-like point set: an irregular cloud with depth.
    pub(crate) fn cloud() -> Vec<[f32; 3]> {
        (0..40)
            .map(|i| {
                let a = i as f32 * 0.7;
                [6.0 * a.cos(), 8.0 * (a * 1.3).sin(), 2.0 + 3.0 * (a * 0.4).cos()]
            })
            .collect()
    }

    fn rot_y(a: f64) -> M3 {
        let (s, c) = a.sin_cos();
        [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
    }

    fn rot_x(a: f64) -> M3 {
        let (s, c) = a.sin_cos();
        [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
    }

    fn rot_z(a: f64) -> M3 {
        let (s, c) = a.sin_cos();
        [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
    }

    fn mul(a: &M3, b: &M3) -> M3 {
        let mut m = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        m
    }

    /// The canonical points posed by yaw, pitch and roll (degrees), scaled and moved, as picture
    /// points.
    pub(crate) fn posed(canonical: &[[f32; 3]], [yaw, pitch, roll]: [f64; 3], scale: f64, at: [f64; 3]) -> Vec<[f32; 3]> {
        let r = mul(&mul(&rot_y(yaw.to_radians()), &rot_x(-pitch.to_radians())), &rot_z(roll.to_radians()));
        canonical
            .iter()
            .map(|c| {
                let v = mat_vec(&r, [c[0] as f64, c[1] as f64, c[2] as f64]);
                let p = [scale * v[0] + at[0], scale * v[1] + at[1], scale * v[2] + at[2]];
                [p[0] as f32, -p[1] as f32, -p[2] as f32]
            })
            .collect()
    }

    fn basis(n: usize) -> Vec<(usize, f32)> {
        (0..n).map(|i| (i, 0.5 + (i % 3) as f32 * 0.25)).collect()
    }

    #[test]
    fn a_posed_face_is_recovered() {
        let c = cloud();
        for angles in [[0.0, 0.0, 0.0], [25.0, 0.0, 0.0], [0.0, 15.0, 0.0], [0.0, 0.0, -30.0], [-35.0, 20.0, 12.0], [60.0, -40.0, 80.0]] {
            let pts = posed(&c, angles, 12.5, [320.0, -240.0, -40.0]);
            let p = HeadPose::of(&c, &basis(c.len()), &pts).unwrap();
            let got = [p.yaw as f64, p.pitch as f64, p.roll as f64];
            for k in 0..3 {
                assert!((got[k] - angles[k]).abs() < 1e-2, "{angles:?} gave {got:?}");
            }
            assert!((p.scale - 12.5).abs() < 1e-3, "{}", p.scale);
            assert!((p.center[0] - 320.0).abs() < 1e-2 && (p.center[1] - 240.0).abs() < 1e-2 && (p.center[2] - 40.0).abs() < 1e-2, "{:?}", p.center);
            assert!(p.residual < 1e-3, "{}", p.residual);
            let n = p.rotation.iter().map(|v| v * v).sum::<f32>();
            assert!((n - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn signs_follow_the_cameras_view() {
        let c = cloud();
        // The nose (a point in front of the face) moves to the right of the picture for a positive
        // yaw and up the picture for a positive pitch.
        let nose = [[0.0f32, 0.0, 5.0]];
        let right = posed(&nose, [20.0, 0.0, 0.0], 1.0, [0.0; 3]);
        assert!(right[0][0] > 0.0);
        let up = posed(&nose, [0.0, 20.0, 0.0], 1.0, [0.0; 3]);
        assert!(up[0][1] < 0.0, "picture y is down: {:?}", up[0]);
        let pts = posed(&c, [20.0, 10.0, 5.0], 3.0, [0.0; 3]);
        let p = HeadPose::of(&c, &basis(c.len()), &pts).unwrap();
        assert!(p.yaw > 0.0 && p.pitch > 0.0 && p.roll > 0.0);
    }

    #[test]
    fn noise_and_weights() {
        let c = cloud();
        let mut pts = posed(&c, [10.0, -5.0, 3.0], 8.0, [100.0, -100.0, 0.0]);
        // One far outlier with no weight does not move the fit.
        pts[3] = [1e4, -1e4, 5e3];
        let mut b = basis(c.len());
        b[3].1 = 0.0;
        let p = HeadPose::of(&c, &b, &pts).unwrap();
        assert!((p.yaw - 10.0).abs() < 1e-2 && (p.pitch + 5.0).abs() < 1e-2 && (p.roll - 3.0).abs() < 1e-2, "{p:?}");
    }

    #[test]
    fn degenerate_inputs_give_no_pose() {
        let c = cloud();
        let b = basis(c.len());
        assert!(HeadPose::of(&c, &[], &c).is_none());
        assert!(HeadPose::of(&c, &b, &c[..10]).is_none());
        assert!(HeadPose::of(&c, &b, &vec![[1.0, 2.0, 3.0]; c.len()]).is_none());
        let mut nan = c.clone();
        nan[0][0] = f32::NAN;
        assert!(HeadPose::of(&c, &b, &nan).is_none());
        assert!(HeadPose::of(&c, &[(0, 0.0), (1, 0.0)], &c).is_none());
        // A mirrored face still fits a proper rotation (never a reflection).
        let mirrored: Vec<[f32; 3]> = c.iter().map(|p| [-p[0], -p[1], -p[2]]).collect();
        if let Some(p) = HeadPose::of(&c, &b, &mirrored) {
            let r = matrix(p.rotation.map(|v| v as f64));
            let det = r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1]) - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0])
                + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0]);
            assert!((det - 1.0).abs() < 1e-4, "{det}");
        }
    }

    /// Multiplying every weight by one factor, however small, gives the same pose.
    #[test]
    fn scaling_every_weight_alike_keeps_the_pose() {
        let axes = vec![[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
        let pts = posed(&axes, [90.0, 0.0, 0.0], 1.0, [0.0; 3]);
        for w in [1.0f32, 1e-20] {
            let b: Vec<(usize, f32)> = (0..axes.len()).map(|i| (i, w)).collect();
            let p = HeadPose::of(&axes, &b, &pts).unwrap();
            assert!((p.yaw - 90.0).abs() < 1e-2 && (p.scale - 1.0).abs() < 1e-4, "weight {w}: {p:?}");
        }
    }

    /// Weighted points on one line do not fix a rotation about it; three points in a plane do.
    #[test]
    fn points_on_a_line_give_no_pose() {
        let mut c = cloud();
        c[0] = [-1.0, 0.0, 0.0];
        c[1] = [1.0, 0.0, 0.0];
        c[2] = [0.0, 1.0, 0.0];
        let line = [(0, 1.0), (1, 1.0), (5, 0.0)];
        assert!(HeadPose::of(&c, &line, &posed(&c, [0.0; 3], 1.0, [0.0; 3])).is_none());
        let plane = [(0, 1.0), (1, 1.0), (2, 1.0)];
        let p = HeadPose::of(&c, &plane, &posed(&c, [20.0, 0.0, 0.0], 2.0, [0.0; 3])).unwrap();
        assert!((p.yaw - 20.0).abs() < 1e-2 && (p.scale - 2.0).abs() < 1e-3, "{p:?}");
    }

    #[test]
    fn points_return_to_the_canonical_frame() {
        let c = cloud();
        let pts = posed(&c, [30.0, 10.0, -20.0], 5.0, [50.0, -60.0, 10.0]);
        let f = fit(&c, &basis(c.len()), &pts).unwrap();
        for (p, want) in pts.iter().zip(&c) {
            let got = f.to_canonical(*p);
            for k in 0..3 {
                assert!((got[k] - want[k] as f64).abs() < 1e-3, "{got:?} {want:?}");
            }
        }
    }
}
