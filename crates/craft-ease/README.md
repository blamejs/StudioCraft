# craft-ease

Easing curves, keyed tracks, constant-speed motion paths and damped springs for animation in the
Craft apps. The crate depends only on the standard library, with `serde` as an optional feature (on
by default), and builds for `wasm32-unknown-unknown`.

## Contents

Values are `f64` throughout.

- `Easing` maps linear progress `t` in `0..=1` to eased progress. The variants are no ease
  (`Linear`), `Hold`, the Classic slider (−100 to +100), nine named families (Quad, Cubic, Quart,
  Quint, Sine, Back, Circ, Bounce, Elastic) each In, Out and In Out, `CubicBezier` as the CSS
  Easing Functions specification defines `cubic-bezier()`, an editable `CustomEase`, a
  `SampledEase` table and a physical `SpringEase`. `Easing::apply(t)` returns exactly 0 for
  `t <= 0` and NaN, and exactly 1 for `t >= 1`, for every variant; `+∞` gives 1. It does not
  allocate.
- `Easing::from_temporal` builds the cubic Bezier for a segment from the speed and influence of its
  two sides (`TemporalEase`), as keyframe velocity dialogs give them. The influence on each side
  may be from 0.001 to 1, and the two may add up to more than 1. `Easing::temporal` reads them
  back.
- `Track<T>` holds keys of any `Animatable` value (`f32`, `f64`, `Vec2`, `[f32; N]`, `[f64; N]`),
  each with the ease to the next key. A held key uses `Easing::Hold`, and −0 and +0 are one time.
  `Track::sample` is a binary search and does not allocate.
- `MotionPath` is a chain of cubic Bezier segments with an arc-length table per segment, built
  once. Each table cell is measured by Gauss-Legendre quadrature with Newton refinement, and a
  segment whose handles lie on its chord is treated as a straight line. `point_at` and
  `point_at_length` move at constant speed along the path and continue in a straight line past
  either end. `MotionPath::through` sets the handles of a smooth path through a list of anchors,
  each handle a sixth of the line between the anchor's neighbors.
- `Spring` and `SpringState` move a value toward a target. `SpringState::step` applies the exact
  solution of the damped spring equation over the step, so a step costs O(1), any step length is
  stable, and many short steps give the same value as one long one.
  `Spring::from_physical(mass, stiffness, damping)` takes a mass, a stiffness and a damping
  coefficient.

## Use

### Checked and lenient constructors

Every type has a checked constructor that returns `EaseError` for a value that is not finite or is
out of range: `Easing::cubic_bezier`, `Easing::from_temporal`, `Easing::spring`,
`CustomEase::try_new`, `SampledEase::try_new`, `Track::from_keys`, `MotionPath::new`,
`Spring::new` and `Spring::from_physical`. `CustomEase::new` and `SampledEase::new` repair what
they are given instead, and files are read through the same repairs.

### Saved form

The `serde` feature reads and writes `Easing` (spring eases included) and `Track`. `MotionPath`,
`Spring` and `SpringState` are not serialized.

Serialized eases keep their form. Presets are strings such as `"QuadIn"`, and `Classic`,
`CubicBezier`, `Custom` and `Sampled` are one-key objects. `Hold` and `Spring` are the last
variants. A track is read from its fields by name or in order.

## Limits and failures

| Constant | Value |
| --- | --- |
| `MAX_EASE_Y` | 10 (eased values stay within ±10) |
| `MAX_EASE_POINTS` | 64 points in a custom ease |
| `MAX_EASE_SAMPLES` | 1,025 samples in a sampled ease |
| `MAX_TRACK_KEYS` | 100,000 keys in a track |
| `MAX_PATH_SEGMENTS` | 10,000 segments in a motion path |
| `MAX_SPRING_HZ` | 10,000 swings a second |
| `MAX_DAMPING_RATIO` | 1,000,000 |
| `MIN_INFLUENCE` | 0.001 |

- Reading a track refuses a file with more than `MAX_TRACK_KEYS` keys while it reads.
- Reading a file keeps the first 65,536 entries of a custom ease's points or a sampled ease's
  values and skips the rest.
- A sampled ease's name drops control characters, bidirectional overrides and invisible format
  characters such as zero-width spaces. A sampled ease with no name left is named "Sampled ease".
- `CubicBezier` keeps y1 and y2 within ±10 when it is evaluated. Its solver inverts the curve with
  both handles at full reach in closed form, and otherwise stops within 1e-12 of the span in at
  most 64 steps.
- `Track` samples keys at opposite ends of the `f64` range without overflow. It returns
  `EaseError::Overflow` when an overshooting ease carries a value past what its type holds.
- A spring ease that is still moving after 10 seconds has the distance it is still off taken out
  evenly over the ease, so it reaches 1 without a jump. Any other spring ease is within about
  0.1 % of 1 just before it reaches 1.

## Sources

- The named families are the easing equations published by Robert Penner. Back In Out and Elastic
  In Out use the published In Out equations, which scale Back's overshoot by 1.525 and give Elastic
  a longer period.
- `CubicBezier` follows the CSS Easing Functions Level 1 specification.

## License

Licensed under either of MIT or Apache-2.0, at your option.
