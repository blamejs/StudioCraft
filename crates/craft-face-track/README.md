# craft-face-track

Face tracking on the device for a Craft app: the face detector and 478-point face mesh of the Face
Landmarker model bundle run on a TensorFlow Lite interpreter in pure Rust, the mesh is turned into
a head pose and expression weights, and a tracker follows one face from picture to picture. The
crate depends on no app crate.

| Item | What |
|---|---|
| `Bundle` | the model bundle (`face_landmarker.task`, a zip of stored members, at most `MAX_BUNDLE_BYTES`): the detector, the mesh, and the canonical face with its fit weights, all checked when read |
| `Tracker` | follows one face: `track(frame, time, out)` fills a `TrackFrame` with the smoothed points, the pose and the expressions |
| `TrackerOptions`, `FaceChoice` | which face a search picks (largest, leftmost or rightmost), the smoothing, and when the detector checks a weakly held face |
| `Landmarker` | the two models with their buffers: `detect` finds faces in a square of the picture, `mesh` places the points in a crop |
| `Frame`, `Roi` | a borrowed RGBA8 picture with any row stride, and a rotated square crop of it |
| `HeadPose` | rotation (quaternion and yaw, pitch, roll in degrees), center, scale and fit residual |
| `Expressions`, `Blendshape` | the weights of the 52 common face shapes, 0 to 1 |
| `OneEuro`, `RotationFilter`, `Smoothing` | One Euro filters for values and rotations, and the tracker's smoothing levels |
| `Pool` | the threads trackers run their models on, 1 to 8 |
| `tflite::Model` | the TensorFlow Lite reader and interpreter |

## Models

No model is bundled. `Bundle::open` checks the file's size before reading it. The bundle is the
published `face_landmarker.task` (3,758,596 bytes, SHA-256
`64184e229b263107bc2b804c6625db1341ff2bb731874b0bcc2fe6544e0bc9ff`), under the Apache License,
Version 2.0. The tracker reads three of its members:

| Member | Input | Output |
|---|---|---|
| `face_detector.tflite` | 128 × 128 RGB, −1 to 1 | 896 boxes with six keypoints, and their scores |
| `face_landmarks_detector.tflite` | 256 × 256 RGB, 0 to 1 | 478 points with depth, a presence score and a tongue score |
| `geometry_pipeline_metadata_landmarks.binarypb` | | the canonical face (468 points, centimeters) and 33 weighted fit points |

The bundle's expression model (`face_blendshapes.tflite`) is not used.

The interpreter runs the float operators these models use: `CONV_2D`, `DEPTHWISE_CONV_2D`,
`MAX_POOL_2D`, `ADD`, `PAD`, `CONCATENATION`, `RESHAPE`, `RELU`, `RELU6`, `PRELU`, `LOGISTIC` and
`DEQUANTIZE` of float16 weights. Other operators, quantized tensors and dilated convolutions are a
load error. A model is also refused at load when one run would need more memory, multiply-adds or
values written than fixed limits allow, or when it lists more than 65,536 tensors, 16,384
operators, 131,072 buffers, 256 operator codes, or 64 graph inputs or outputs. Each operator must
have exactly one output and the number of inputs its kind takes; a `CONCATENATION` joins at most
64 parts. Each tensor gets a slot in a `Workspace` sized at load, and
a slot is reused once the tensors read from it are done, so a run does not allocate.

## Tracking

While no face is followed, `track` runs the detector over the whole picture, picks a face, and runs
the mesh twice: in the detector's crop, then in a crop level with the eyes. While a face is
followed, each picture takes one mesh run in the crop the previous mesh framed. The face is lost
when the presence score falls below 0.5, and the same picture is searched again.

While the presence score is below `weak_presence` (default 0.9), the detector also runs, at most
once every `redetect_every` seconds (default 2). A detection whose center lies inside the followed
face frames the next crop. Other detections are ignored, so the tracker does not switch faces.

Times are the caller's, in seconds. A time earlier than the last one resets the filters, and a time
that is not a number is an error. Points are in picture pixels (x right, y down) with depth in the
same scale, smaller nearer the camera.

## Pose and expressions

The pose is a weighted least-squares fit of rotation, scale and translation from the canonical face
to the mesh, over the bundle's fit points. Yaw is positive toward the right of the picture, pitch
positive up, and roll positive counterclockwise as the camera sees it.

Expressions are measured from the mesh after it is carried into the canonical face's frame, against
the same measurements on the canonical face. "Left" is the subject's left, which an unmirrored
camera shows on the right of the picture. The measured shapes are in `Blendshape::MEASURED`: eye
blink and wide, the four eye directions, inner and outer brow up, brow down, jaw open, mouth smile
and frown, pucker, and mouth left and right. `tongueOut` is the mesh's tongue score. The other 26
shapes are 0. The ranges that map each measurement to 0 to 1 are StudioCraft's own values.

## Threads

Every tracker runs on a `Pool`. Trackers that share a pool share its threads, so the pool's size
caps them together. `Pool::single()` runs on the caller's thread.

## Tests

`cargo test -p craft-face-track` runs the unit tests, which build small models and synthetic faces
in memory. `tests/model.rs` and `tests/no_alloc.rs` need the bundle at `$CRAFT_FACE_LANDMARKER`
(read when the test runs, or when it is built, so `--config` with an `env` value works) or at
`target/models/face_landmarker.task`, and print a skip line without it. They compare the models'
outputs with sums from TensorFlow Lite's interpreter on the same input, check the canonical face,
and count allocations. `cargo bench -p craft-face-track --bench tracker` times the models; the
results are in `docs/performance.md`.

## License

MIT OR Apache-2.0. The crate contains code from https://github.com/storytold/effectcraft
(`crates/segment`); see `NOTICE`.
