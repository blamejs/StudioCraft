# craft-scopes

Video scope maths for a Craft app's scopes panel: the waveform, parade, histogram and the YUV and
hue, lightness and saturation (HLS) vectorscopes of a frame, as count grids, with numeric summaries
and trace images. The crate depends only on `craft-color` and `serde`, has no user interface (UI),
and builds for `wasm32-unknown-unknown`.

## Contents

| Item | What |
|---|---|
| `Frame` | A borrowed frame: `Rgba8` with a row stride, or `Nv12` (Y' plane and interleaved Cb Cr plane, each with its own stride, plus the matrix and range). |
| `Signal` | A frame's R'G'B' code values, decimated by nearest-sample picking to at most 480 × 270 (`fill`, `fill_max`, `from_rgba8`, `from_rgba_f32_with` with `linear_to_pq` for HDR, `from_fn`). |
| `waveform`, `waveform_into` | RGB, luma, luma and chroma, or luma and chroma with the chroma hidden, as one count grid per trace (columns = signal width, `rows` levels). |
| `parade`, `parade_into` | RGB, YUV, or RGB and luma traces, drawn side by side. |
| `histogram`, `histogram_into` | 256 bins of R', G', B', Y'. Samples below 0 and above 1 are counted separately. |
| `histogram_hdr_into`, `HdrBins` | 256 bins of linear values: black to SDR white sRGB-encoded in the first half, then equal stops above SDR white. |
| `vectorscope_yuv`, `vectorscope_yuv_into` | Cb is plotted to the right and Cr up, over ±0.6. `targets(matrix, amplitude)` gives the six color-bar targets, and `SKIN_TONE_DEG` is 123°. |
| `vectorscope_hls`, `vectorscope_hls_into` | Hue as the angle (red at the matrix's YUV red angle, then yellow, green, cyan, blue, magenta counter-clockwise), and HLS saturation as the radius. |
| `compute`, `compute_into` | One scope by `ScopeKind`, with the waveform and parade traces from `Params::waveform` and `Params::parade`. |
| `summary` | Levels (min, max and mean in percent) per channel and per column range, the densest vectorscope cells with angle and magnitude, and a coarse vectorscope. |
| `paint` | Count grids to premultiplied RGBA8 (log intensity, additive traces, Brightness gain; vectorscope points spread over 3 × 3), with `*_into` forms that take a `Scratch`. |

The settings types are `ScopeKind`, `WaveformType`, `ParadeType`, `ColorSpace` (Automatic,
Rec. 601, Rec. 709 and Rec. 2100 for the BT.601, BT.709 and BT.2020 NCL matrices), `Scale` (8-bit,
Float, HDR), `Brightness` and `Targets` (75 %, 100 %). All of them are serde types in camelCase.
Their `label` strings are English, and an app translates them before showing them. `ScopeKind`,
`Computed`, `Frame` and `FrameError` are `#[non_exhaustive]`.

### Definitions

- Y'CbCr comes from R'G'B' with the matrix's Kr and Kb (ITU-R BT.601-7, BT.709-6, BT.2020-2 NCL),
  with Cb and Cr in −0.5 to 0.5. YC chroma amplitude is √(Cb² + Cr²), plotted at Y' ± C.
- NV12 code values are normalized with the frame's range (limited: Y' 16 to 235, Cb and Cr 16 to
  240; full: 0 to 255) and turned into R'G'B' with its matrix. Each Cb Cr pair covers 2 × 2 pixels.
- Waveform rows span 0 to 1 with clamping (values clamped first) or −0.1 to 1.1 without (values
  outside are dropped). With 256 rows an 8-bit code value `c` lands on row `c`.
- HDR maps linear working values (1.0 = 203 cd/m², ITU-R BT.2408) to SMPTE ST 2084 PQ code values,
  and the axis is 0 to 10 000 cd/m². SDR frames on the HDR scale use BT.1886 (γ 2.4, 100 cd/m²
  white).

## Use

Each `*_into` function writes into a value the caller keeps between frames. After the first frame
of a size, `Signal::fill`, the `*_into` scopes and the `paint::*_into` painters do not allocate.
`summary` allocates its results.

## Limits and failures

- `Frame::validate` (called by `Signal::fill`) returns a `FrameError` for a stride shorter than a
  row, a plane shorter than its rows, or a size that overflows. The signal is then empty.
- A signal holds at most 1920 × 1080 samples whatever size `fill_max` is asked for, and a grid at
  most 1920 columns. `Signal::from_fn` cuts a larger size to 1920 × 1080.
- `Params` clamps `rows` to 2 to 4096 and `vector_size` to 3 to 1023, rounding an even size up to
  the next odd one.
- Grid and vectorscope lookups outside the grid return 0.
- The painters paint nothing for a hand-built grid wider than 1920 columns or taller than 4096
  rows, or a vectorscope larger than 1023.
- Summaries leave NaN and infinite samples out.

## Tests

`src/tests.rs` uses generated frames with known values:

- A flat color lands in exact histogram bins.
- A 256-step ramp fills every bin and puts column `x` on row `x`.
- 75 % color bars put each parade column on its code value and each bar on its vectorscope target
  cell (BT.601, BT.709, BT.2020).
- The 8-bit limited-range BT.709 code values of the 100 % color bars in SMPTE RP 219-1 decode from
  NV12 to their R'G'B' within 1.5 codes.
- Strided and packed RGBA read the same samples.
- Bad strides, short planes and overflowing sizes give errors.
- The `*_into` functions match the allocating ones and keep their buffers across frames.
- Hostile parameters and inconsistent signals stay bounded.
- HDR bins split SDR and stops for any split.

`cargo test --release -p craft-scopes perf -- --ignored --nocapture` decimates a 1920 × 1080 frame
and times each scope on the 480 × 270 signal.

## Sources

- ITU-R BT.601-7, BT.709-6 and BT.2020-2 (non-constant luminance) for the Y'CbCr matrices.
- ITU-R BT.2408 for the HDR reference white, SMPTE ST 2084 for PQ, and ITU-R BT.1886 for SDR frames
  on the HDR scale.
- SMPTE RP 219-1 for the color-bar code values the tests check.

## License

Licensed under either of MIT or Apache-2.0, at your option. `NOTICE` names the code this crate
started from.
