# craft-transitions

Scene transitions for the Craft apps: Cut, Fade, Fade through color and Swipe. Each one is a WGSL
blend between the old and new scene, with typed and versioned parameters, an ease, the gains of the
old and new audio, and the same blend on the CPU. It depends only on `craft-ease` (and `serde`,
optionally), contains no `unsafe` code, and builds for `wasm32-unknown-unknown`.

License: MIT OR Apache-2.0.

## Transitions

| `Kind` | Id | What it does | Parameters it reads | Default ease |
| --- | --- | --- | --- | --- |
| `Cut` | `cut` | Switches at once and ignores the duration. | none | linear (always) |
| `Fade` | `fade` | Blends the old scene into the new one. | `ease` | linear |
| `FadeThroughColor` | `fade_color` | Fades the old scene to `color` over the first half, then the color to the new scene. | `color`, `ease` | linear |
| `Swipe` | `swipe` | Slides the new scene in over the old one from `edge`. | `edge`, `ease` | in and out |

`Kind::ALL` lists them, `Kind::from_id` finds one by id, and `Kind::name` gives its English name.
`Edge` (`left`, `right`, `top`, `bottom`) and `Ease` (`linear`, `ease_in`, `ease_out`,
`ease_in_out`) have the same `ALL`, `from_id`, `id` and `label`. `Ease::EaseInOut` is the cubic
smoothstep 3t² − 2t³.

## Drawing a transition

`Kind::shader` is the body of

```wgsl
fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32>
```

The host declares `from_tex` and `to_tex` (the old and new scene, `texture_2d<f32>`) and `samp`
(a `sampler`) before the body, samples both scenes at `uv` into `a` and `b`, and calls it with:

- `t`: `Kind::ease(&params).apply(progress)`, clamped to 0 to 1, where `progress` is the fraction
  of the duration that has passed;
- `p`: `Kind::uniform(&params)`.

`Kind::blend` is the same blend for one pixel on the CPU, for tests, thumbnails and machines
without a GPU. `Kind::audio_gains(progress)` gives the old and new audio's gains: Cut gives the new
audio at once, and the others cross-fade linearly.

```rust
use craft_transitions::{Edge, Kind, Params};

let params = Params { edge: Edge::Right, ..Params::default() };
let t = Kind::Swipe.ease(&params).apply(0.5) as f32;
let p = Kind::Swipe.uniform(&params);
let pixel = Kind::Swipe.blend([1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0], [0.75, 0.5], t, p, |_| [0.0, 0.0, 1.0, 1.0]);
```

## Saving parameters

`Params` carries `version`, which is `PARAMS_VERSION` (1) when written. With the `serde` feature
(on by default) it reads and writes as `{"version": 1, "color": [r, g, b, a], "edge": "left",
"ease": "ease_in_out"}`. Missing fields take their defaults and `version` is required. An unknown
edge or ease id is an error. After reading, call `Params::check`, which refuses version 0 and any
version newer than this crate reads.
