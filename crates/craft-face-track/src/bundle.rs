//! The Face Landmarker model bundle: a zip archive of stored (uncompressed) members holding the
//! two models the tracker runs and the face geometry it fits poses to.
//!
//! | Member | What the tracker takes from it |
//! | --- | --- |
//! | `face_detector.tflite` | the face detector, 128 × 128 RGB in, 896 boxes with six keypoints out |
//! | `face_landmarks_detector.tflite` | the face mesh, 256 × 256 RGB in, 478 points with depth, a presence score and a tongue score out |
//! | `geometry_pipeline_metadata_landmarks.binarypb` | the canonical face (468 points, in centimeters) and the weights of the points a pose fit uses |
//!
//! The bundle's other members are not read. Every count and offset in the archive and in the
//! geometry message is checked before it is used.

use std::io::Read;
use std::path::Path;

use crate::landmarker::{CANONICAL_POINTS, DET_SIZE, MESH_POINTS, MESH_SIZE};
use crate::tflite::Model;
use crate::{Error, Result};

/// The largest bundle file read (the published one is 3,758,596 bytes).
pub const MAX_BUNDLE_BYTES: u64 = 64 << 20;
/// The most members the archive may list.
const MAX_MEMBERS: usize = 64;

pub const DETECTOR: &str = "face_detector.tflite";
pub const MESH: &str = "face_landmarks_detector.tflite";
pub const GEOMETRY: &str = "geometry_pipeline_metadata_landmarks.binarypb";

/// The models and face geometry of a Face Landmarker bundle, read once and shared by every
/// tracker (`Arc<Bundle>`).
#[derive(Clone, Debug)]
pub struct Bundle {
    pub(crate) detector: Model,
    pub(crate) mesh: Model,
    /// The canonical face's 468 points, in centimeters: x to the face's left as the camera sees it
    /// (image right), y up, z toward the camera.
    pub(crate) canonical: Vec<[f32; 3]>,
    /// The points a pose fit uses and their weights.
    pub(crate) basis: Vec<(usize, f32)>,
}

impl Bundle {
    /// Read the bundle file at `path`, after checking its size. The read stops one byte past
    /// [`MAX_BUNDLE_BYTES`], so a file that grows after the check is refused without being read
    /// whole.
    pub fn open(path: &Path) -> Result<Bundle> {
        let fail = |e: std::io::Error| Error::Bundle(format!("{}: {e}", path.display()));
        let too_big =
            |len: u64| Error::Bundle(format!("{} is {len} bytes, more than the {MAX_BUNDLE_BYTES} a model bundle may have", path.display()));
        let file = std::fs::File::open(path).map_err(fail)?;
        let len = file.metadata().map_err(fail)?.len();
        if len > MAX_BUNDLE_BYTES {
            return Err(too_big(len));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        file.take(MAX_BUNDLE_BYTES + 1).read_to_end(&mut bytes).map_err(fail)?;
        if bytes.len() as u64 > MAX_BUNDLE_BYTES {
            return Err(too_big(bytes.len() as u64));
        }
        Bundle::from_task(&bytes)
    }

    /// Read a bundle from its bytes.
    pub fn from_task(bytes: &[u8]) -> Result<Bundle> {
        if bytes.len() as u64 > MAX_BUNDLE_BYTES {
            return Err(Error::Bundle(format!("the bundle is {} bytes, more than the {MAX_BUNDLE_BYTES} a model bundle may have", bytes.len())));
        }
        let detector = Model::read(stored_member(bytes, DETECTOR)?)?;
        let mesh = Model::read(stored_member(bytes, MESH)?)?;
        let anchors = crate::landmarker::ANCHORS;
        let count = |m: &Model, k: usize| m.output(k).map(|o| o.0.iter().product::<usize>());
        // Each model takes the one picture the tracker gives it.
        if detector.inputs() != 1
            || detector.input_shape(0) != Some(&[1, DET_SIZE, DET_SIZE, 3][..])
            || detector.outputs() != 2
            || detector.output(0).map(|o| o.0) != Some(&[1, anchors, 16][..])
            || count(&detector, 1) != Some(anchors)
        {
            return Err(Error::Bundle(format!("{DETECTOR} is not the face detector this tracker runs")));
        }
        if mesh.inputs() != 1
            || mesh.input_shape(0) != Some(&[1, MESH_SIZE, MESH_SIZE, 3][..])
            || mesh.outputs() != 3
            || count(&mesh, 0) != Some(MESH_POINTS * 3)
            || count(&mesh, 1) != Some(1)
            || count(&mesh, 2) != Some(1)
        {
            return Err(Error::Bundle(format!("{MESH} is not the face mesh this tracker runs")));
        }
        let (canonical, basis) = geometry(stored_member(bytes, GEOMETRY)?).map_err(|e| Error::Bundle(format!("{GEOMETRY}: {e}")))?;
        Ok(Bundle { detector, mesh, canonical, basis })
    }

    /// The face detector.
    pub fn detector(&self) -> &Model {
        &self.detector
    }

    /// The face mesh.
    pub fn mesh(&self) -> &Model {
        &self.mesh
    }

    /// The canonical face's points, in centimeters.
    pub fn canonical(&self) -> &[[f32; 3]] {
        &self.canonical
    }

    /// The points a pose fit uses, with their weights.
    pub fn basis(&self) -> &[(usize, f32)] {
        &self.basis
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize)
}

fn u32_at(b: &[u8], at: usize) -> Option<usize> {
    usize::try_from(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?)).ok()
}

/// The bytes of the stored member `name` of a zip archive.
pub(crate) fn stored_member<'a>(zip: &'a [u8], name: &str) -> Result<&'a [u8]> {
    stored(zip, name).map_err(|e| Error::Bundle(e.to_string()))
}

fn stored<'a>(zip: &'a [u8], name: &str) -> std::result::Result<&'a [u8], String> {
    const EOCD: u32 = 0x0605_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const LOCAL: u32 = 0x0403_4b50;
    // The end record is in the last 22 bytes plus a comment of at most 65,535 bytes. A comment
    // may hold the record's signature too: the record is the candidate whose comment ends the
    // file and whose directory ends before it.
    let start = zip.len().saturating_sub(22 + 0xffff);
    let is_end = |p: usize| {
        u32_at(zip, p) == Some(EOCD as usize)
            && u16_at(zip, p + 20).is_some_and(|c| p + 22 + c == zip.len())
            && u32_at(zip, p + 12).zip(u32_at(zip, p + 16)).is_some_and(|(size, at)| at.checked_add(size).is_some_and(|e| e <= p))
    };
    let end = (start..zip.len().saturating_sub(21)).rev().find(|&p| is_end(p)).ok_or("the bundle is not a zip archive")?;
    let count = u16_at(zip, end + 10).ok_or("truncated archive")?;
    let mut at = u32_at(zip, end + 16).ok_or("truncated archive")?;
    if count > MAX_MEMBERS {
        return Err("the archive lists too many members".into());
    }
    for _ in 0..count {
        if u32_at(zip, at) != Some(CENTRAL as usize) {
            return Err("the archive's directory is damaged".into());
        }
        let field = |o: usize| u16_at(zip, at.saturating_add(o)).ok_or("truncated archive");
        let (method, name_len, extra_len, comment_len) = (field(10)?, field(28)?, field(30)?, field(32)?);
        let size = u32_at(zip, at.saturating_add(20)).ok_or("truncated archive")?;
        let raw = u32_at(zip, at.saturating_add(24)).ok_or("truncated archive")?;
        let local = u32_at(zip, at.saturating_add(42)).ok_or("truncated archive")?;
        let base = at.saturating_add(46);
        let entry = zip.get(base..base.saturating_add(name_len)).ok_or("truncated archive")?;
        if entry == name.as_bytes() {
            if method != 0 || size != raw {
                return Err(format!("{name} is compressed; the bundle's members are stored"));
            }
            if u32_at(zip, local) != Some(LOCAL as usize) {
                return Err(format!("{name}: the archive's entry is damaged"));
            }
            let skip = u16_at(zip, local.saturating_add(26)).ok_or("truncated archive")?
                + u16_at(zip, local.saturating_add(28)).ok_or("truncated archive")?;
            let data = local.saturating_add(30).saturating_add(skip);
            return zip.get(data..data.checked_add(size).ok_or("truncated archive")?).ok_or_else(|| format!("{name} is cut short"));
        }
        at = base.saturating_add(name_len).saturating_add(extra_len).saturating_add(comment_len);
    }
    Err(format!("the bundle has no {name}"))
}

/// One protobuf field: its number and its value.
enum Pb<'a> {
    Varint(u64),
    Fixed32(u32),
    Fixed64,
    Bytes(&'a [u8]),
}

fn varint(b: &[u8], at: &mut usize) -> std::result::Result<u64, &'static str> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*at).ok_or("truncated message")?;
        *at += 1;
        v |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Ok(v);
        }
    }
    Err("a number is too long")
}

/// The fields of a protobuf message, in order.
fn fields(b: &[u8]) -> impl Iterator<Item = std::result::Result<(u64, Pb<'_>), &'static str>> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        if at >= b.len() {
            return None;
        }
        let r = (|| {
            let key = varint(b, &mut at)?;
            let v = match key & 7 {
                0 => Pb::Varint(varint(b, &mut at)?),
                1 => {
                    b.get(at..at + 8).ok_or("truncated message")?;
                    at += 8;
                    Pb::Fixed64
                }
                2 => {
                    let n = usize::try_from(varint(b, &mut at)?).map_err(|_| "bad length")?;
                    let s = b.get(at..at.checked_add(n).ok_or("bad length")?).ok_or("truncated message")?;
                    at += n;
                    Pb::Bytes(s)
                }
                5 => {
                    let s: [u8; 4] = b.get(at..at + 4).and_then(|s| s.try_into().ok()).ok_or("truncated message")?;
                    at += 4;
                    Pb::Fixed32(u32::from_le_bytes(s))
                }
                _ => return Err("unsupported field type"),
            };
            Ok((key >> 3, v))
        })();
        if r.is_err() {
            // Stop after the first error.
            at = b.len();
        }
        Some(r)
    })
}

/// The canonical face and the pose fit's point weights from the geometry message: field 1 is the
/// canonical mesh (field 1 its vertex type, 0 for position plus texture coordinate; field 3 its
/// vertex values, five per point), field 2 repeats a point and its weight (fields 1 and 2).
fn geometry(b: &[u8]) -> std::result::Result<(Vec<[f32; 3]>, Vec<(usize, f32)>), &'static str> {
    // Five values per point: x, y, z and a texture coordinate.
    const PER_POINT: usize = 5;
    let mut values: Vec<f32> = Vec::with_capacity(CANONICAL_POINTS * PER_POINT);
    let mut basis = vec![];
    let mut push = |v: f32| -> std::result::Result<(), &'static str> {
        if values.len() >= CANONICAL_POINTS * PER_POINT {
            return Err("the canonical face has too many points");
        }
        values.push(v);
        Ok(())
    };
    let mut meshes = 0;
    for f in fields(b) {
        match f? {
            (1, Pb::Bytes(mesh)) => {
                meshes += 1;
                if meshes > 1 {
                    return Err("more than one canonical face");
                }
                for g in fields(mesh) {
                    match g? {
                        (1, Pb::Varint(t)) if t != 0 => return Err("the canonical face's points are not in the expected layout"),
                        (3, Pb::Fixed32(v)) => push(f32::from_bits(v))?,
                        (3, Pb::Bytes(packed)) => {
                            if packed.len() % 4 != 0 {
                                return Err("bad point values");
                            }
                            for c in packed.as_chunks::<4>().0 {
                                push(f32::from_le_bytes(*c))?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            (2, Pb::Bytes(entry)) => {
                if basis.len() >= CANONICAL_POINTS {
                    return Err("too many fit points");
                }
                let (mut id, mut weight) = (0u64, 0f32);
                for g in fields(entry) {
                    match g? {
                        (1, Pb::Varint(v)) => id = v,
                        (2, Pb::Fixed32(v)) => weight = f32::from_bits(v),
                        _ => {}
                    }
                }
                let id = usize::try_from(id).ok().filter(|&i| i < CANONICAL_POINTS).ok_or("a fit point is not on the canonical face")?;
                if !weight.is_finite() || weight < 0.0 {
                    return Err("a fit weight is not a number of zero or more");
                }
                basis.push((id, weight));
            }
            (_, Pb::Fixed64 | Pb::Varint(_) | Pb::Fixed32(_) | Pb::Bytes(_)) => {}
        }
    }
    if values.len() != CANONICAL_POINTS * PER_POINT {
        return Err("the canonical face does not have 468 points");
    }
    let canonical: Vec<[f32; 3]> = values.as_chunks::<PER_POINT>().0.iter().map(|p| [p[0], p[1], p[2]]).collect();
    if canonical.iter().flatten().any(|v| !v.is_finite()) {
        return Err("a canonical point is not a number");
    }
    if basis.iter().map(|b| b.1).sum::<f32>() <= 0.0 {
        return Err("the fit has no weighted points");
    }
    Ok((canonical, basis))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip archive of stored members.
    pub(crate) fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = vec![];
        let mut dir = vec![];
        for (name, data) in files {
            let at = out.len() as u32;
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend(0u32.to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(*data);
            dir.extend(0x0201_4b50u32.to_le_bytes());
            dir.extend([20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            dir.extend(0u32.to_le_bytes());
            dir.extend((data.len() as u32).to_le_bytes());
            dir.extend((data.len() as u32).to_le_bytes());
            dir.extend((name.len() as u16).to_le_bytes());
            dir.extend([0; 12]);
            dir.extend(at.to_le_bytes());
            dir.extend(name.as_bytes());
        }
        let dir_at = out.len() as u32;
        out.extend(&dir);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((dir.len() as u32).to_le_bytes());
        out.extend(dir_at.to_le_bytes());
        out.extend([0; 2]);
        out
    }

    fn key(field: u64, ty: u64) -> Vec<u8> {
        pb_varint((field << 3) | ty)
    }

    fn pb_varint(mut v: u64) -> Vec<u8> {
        let mut out = vec![];
        loop {
            let c = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(c);
                return out;
            }
            out.push(c | 0x80);
        }
    }

    /// A geometry message: `points` canonical points (values unpacked or packed) and the fit
    /// points.
    pub(crate) fn geometry_message(points: &[[f32; 3]], packed: bool, basis: &[(u64, f32)]) -> Vec<u8> {
        let mut mesh = [key(1, 0), pb_varint(0)].concat();
        let values: Vec<f32> = points.iter().flat_map(|p| [p[0], p[1], p[2], 0.5, 0.5]).collect();
        if packed {
            let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            mesh.extend(key(3, 2));
            mesh.extend(pb_varint(bytes.len() as u64));
            mesh.extend(bytes);
        } else {
            for v in values {
                mesh.extend(key(3, 5));
                mesh.extend(v.to_le_bytes());
            }
        }
        let mut out = key(1, 2);
        out.extend(pb_varint(mesh.len() as u64));
        out.extend(mesh);
        for &(id, w) in basis {
            let entry = [key(1, 0), pb_varint(id), key(2, 5), w.to_le_bytes().to_vec()].concat();
            out.extend(key(2, 2));
            out.extend(pb_varint(entry.len() as u64));
            out.extend(entry);
        }
        out.extend(key(3, 0));
        out.extend(pb_varint(1));
        out
    }

    fn points() -> Vec<[f32; 3]> {
        (0..CANONICAL_POINTS).map(|i| [i as f32, -(i as f32), 0.5]).collect()
    }

    #[test]
    fn stored_members_are_found_by_name() {
        let z = zip(&[("a.txt", b"alpha"), ("b.bin", b"\x00\x01\x02")]);
        assert_eq!(stored_member(&z, "a.txt").unwrap(), b"alpha");
        assert_eq!(stored_member(&z, "b.bin").unwrap(), b"\x00\x01\x02");
        assert!(stored_member(&z, "c").is_err());
        assert!(stored_member(b"not a zip", "a.txt").is_err());
        // Every prefix of the archive is an error or the member, never a panic.
        for n in 0..z.len() {
            let _ = stored_member(&z[..n], "b.bin");
        }
    }

    #[test]
    fn compressed_members_are_refused() {
        let mut z = zip(&[("a.txt", b"alpha")]);
        // The directory entry's method: deflate.
        let dir = z.windows(4).position(|w| w == 0x0201_4b50u32.to_le_bytes()).unwrap();
        z[dir + 10] = 8;
        let e = stored_member(&z, "a.txt").unwrap_err().to_string();
        assert!(e.contains("compressed"), "{e}");
    }

    #[test]
    fn geometry_reads_points_and_weights_packed_or_not() {
        for packed in [false, true] {
            let (c, b) = geometry(&geometry_message(&points(), packed, &[(4, 0.5), (33, 0.25)])).unwrap();
            assert_eq!(c.len(), CANONICAL_POINTS);
            assert_eq!(c[7], [7.0, -7.0, 0.5]);
            assert_eq!(b, vec![(4, 0.5), (33, 0.25)]);
        }
    }

    #[test]
    fn bad_geometry_is_refused() {
        let short = &points()[..467];
        assert!(geometry(&geometry_message(short, true, &[(4, 1.0)])).is_err());
        assert!(geometry(&geometry_message(&points(), true, &[(468, 1.0)])).is_err());
        assert!(geometry(&geometry_message(&points(), true, &[(4, -1.0)])).is_err());
        assert!(geometry(&geometry_message(&points(), true, &[(4, f32::NAN)])).is_err());
        assert!(geometry(&geometry_message(&points(), true, &[])).is_err());
        let mut nan = points();
        nan[3][1] = f32::INFINITY;
        assert!(geometry(&geometry_message(&nan, false, &[(4, 1.0)])).is_err());
        let msg = geometry_message(&points(), false, &[(4, 1.0)]);
        for n in 0..msg.len() {
            let _ = geometry(&msg[..n]);
        }
    }

    #[test]
    fn a_bundle_without_its_models_is_an_error() {
        let z = zip(&[(GEOMETRY, &geometry_message(&points(), true, &[(4, 1.0)]))]);
        let e = Bundle::from_task(&z).unwrap_err().to_string();
        assert!(e.contains(DETECTOR), "{e}");
    }

    /// A comment that holds the end record's signature does not hide the real record.
    #[test]
    fn an_end_signature_in_the_comment_is_skipped() {
        let mut z = zip(&[("a.txt", b"alpha")]);
        // The comment: a record signature whose own comment would run past the end of the file.
        let mut comment = 0x0605_4b50u32.to_le_bytes().to_vec();
        comment.extend([0; 16]);
        comment.extend(100u16.to_le_bytes());
        let n = z.len();
        z[n - 2..].copy_from_slice(&(comment.len() as u16).to_le_bytes());
        z.extend(&comment);
        assert_eq!(stored_member(&z, "a.txt").unwrap(), b"alpha");
    }

    /// A detector that takes a second input is not the detector the tracker runs: the tracker
    /// gives it one picture.
    #[test]
    fn models_take_one_input() {
        use crate::tflite::tests::model;
        let (d, a) = (DET_SIZE as i32, crate::landmarker::ANCHORS as i32);
        let detector =
            model(vec![], vec![(vec![1, d, d, 3], 0, None), (vec![1, a, 16], 0, None), (vec![1, a, 1], 0, None)], vec![], vec![0, 1, 2], vec![1, 2]);
        // Both models are read before either is checked.
        let z = zip(&[(DETECTOR, &detector), (MESH, &detector)]);
        let e = Bundle::from_task(&z).unwrap_err().to_string();
        assert!(e.contains("is not the face detector"), "{e}");
    }

    #[test]
    fn opening_checks_the_size_first() {
        let dir = std::env::temp_dir().join(format!("craft-face-track-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.task");
        let f = std::fs::File::create(&p).unwrap();
        // A sparse file larger than the limit: reading it would take its whole size.
        f.set_len(MAX_BUNDLE_BYTES + 1).unwrap();
        drop(f);
        let e = Bundle::open(&p).unwrap_err().to_string();
        assert!(e.contains("more than"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(Bundle::open(&dir.join("missing.task")).is_err());
    }
}
