# craft-avatar

craft-avatar holds the data and the per-frame math for avatars that follow a face seen by a
camera: the avatar model, the tracking vocabulary, the mapping from tracked face shapes to the
avatar's expressions, calibration, the lost-face and idle motion, and the spring-bone step. It
draws nothing and reads no files. The crate depends only on `craft-ease`, `serde` and
`thiserror`, and builds for `wasm32-unknown-unknown`.

Licensed under MIT OR Apache-2.0.

## Contents

| Item | Description |
|---|---|
| `Model` | A whole avatar: `Node`s in parent-first order, the `Humanoid` map of `HumanBone`s (the 55 bones of the VRM 1.0 humanoid, 15 of them required), `Mesh`es of `Primitive`s with skins and `MorphTarget`s, `Skin`s, `Material`s with optional `Toon` shading, decoded `Texture`s, `Expression`s, `LookAt`, `SpringBones`, first-person flags, the license terms in `Meta`, and reader warnings. |
| `Model::validate` | It checks every index, count and value against `Limits` and names the first problem. The default limits are 1,024 nodes at most 64 deep, 2,000,000 vertices and 2,000,000 triangles, 256 morph targets per mesh, 32,000,000 morph target values (positions and normals together), 256 meshes with 4,096 primitives in all, 256 skins, materials and textures, 4,096 pixels a texture side, 512 MiB of texture data, 256 expressions with 1,024 binds of each kind, 256 spring chains with 2,048 joints in all, 256 colliders and 256 collider groups. |
| `Blendshape`, `Weights`, `TrackFrame` | These are the 52 face shapes avatar rigs use (`eyeBlinkLeft`, `jawOpen` and the rest), one weight per shape from 0 to 1, and one tracked frame with the head's rotation and position. `mirrored` swaps each side's shapes and turns yaw and roll the other way. Weights read and write as a map from shape name to weight. |
| `Expression`, `apply_overrides` | An expression sets morph targets, material colors and texture offsets. `apply_overrides` applies the binary rule and the `block` and `blend` overrides of the mouth, blink and gaze groups, as the VRM 1.0 expressions specification describes them. |
| `Mapping`, `gaze` | A mapping is a list of rules, each an expression, a formula (the average, smallest or largest of up to 8 shapes), a threshold and a gain. `Mapping::standard` is the default table. `bind` resolves the rules against one avatar's expressions. `gaze` gives the direction of sight from the eye shapes, up to 30 degrees each way. |
| `LookAt` | The gaze settings: eye bones or look expressions, and the four range maps of the VRM 1.0 look-at specification. |
| `Calibrator`, `Calibration` | A calibrator averages 1 to 5 seconds of frames into the neutral pose and refuses a neutral with an eye closed (either eye's average blink above 0.5), the mouth open (jaw above 0.3) or fewer than 5 frames with a face. A calibration measures each frame from the neutral. |
| `Presence`, `LostFace`, `WhenLost` | When the face is lost, the pose holds for 0.5 s and eases to the idle pose over 1 s; when a face is found it eases back over 0.25 s. The avatar can also hold its last pose, or show an expression while lost. |
| `Idle` | It makes breathing (one breath in 4 s), blinks (every 2 to 6 s, 0.15 s each, from a seed) and a sway of the head of up to 3 degrees. |
| `SpringSim` | It swings the spring chains with stiffness, drag and gravity, keeps each part at its length and outside sphere and capsule colliders, in fixed steps of 1/60 s, at most 8 a call. |
| `Animator`, `AvatarPose` | It runs the steps once per canvas frame: mirror, calibration, lost-face factor, idle pose, mapping, gaze and overrides, into the head rotation, lean, eye rotations and one weight per expression. |

The settings types `AnimatorSettings`, `Mapping`, `Calibration`, `LostFace`, `WhenLost` and
`IdleMotion` are serde types in camelCase. Error and calibration messages are English, and an
app translates them before showing them.

## Axes and units

Positions are in meters in glTF's axes: right-handed, +Y up, and the avatar faces +Z, so its own
left is +X. A positive yaw turns toward +X, a positive pitch turns down, and angles in settings
and poses are in degrees. Times are seconds on the caller's clock; every step takes the time
as an argument and none reads a clock.

A `Transform` is a translation, a rotation and a scale, and `Transform::then` multiplies scales
per axis. The world transforms it gives are exact while every scale is uniform. Under a
non-uniform scale with a rotated child they are an approximation, and the spring step uses the
same composition.

## Allocation

`Animator::step`, `SpringSim::step`, `BoundMapping::eval`, `apply_overrides` and
`Model::world_into` do not allocate. `tests/no_alloc.rs` runs ten seconds of the pose and spring
steps for 60 expressions and 100 spring joints under a counting allocator.
