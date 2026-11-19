# craft-color-transform

craft-color-transform provides color transforms for the Craft apps: RGB spaces and chromatic
adaptation, white balance by temperature and tint, OkLab and OkLCh, monotone tone curves and 1D
lookup tables, and perceptual quantizer (PQ) tone mapping. The crate depends on `craft-color` and
`serde` only, contains no `unsafe` code, and builds for `wasm32-unknown-unknown`.

## Contents

The crate builds on [`craft-color`](https://github.com/storytold/craft-libs/tree/main/crates/color),
which holds the frame color tags, the Y'CbCr matrices and the sRGB, PQ and HLG transfer functions.
The per-pixel functions take `f32` and do not allocate.

| Item | Description |
| --- | --- |
| `Mat3`, `Xy`, `RgbSpace` | These types hold 3×3 matrices, CIE 1931 chromaticities, and RGB spaces from primaries and a white point. `RgbSpace::to_xyz` is the normalized primary matrix. `RgbSpace::from(craft_color::Primaries)` gives the space of a frame tag. |
| `bradford`, `bradford_xyz` | These return the Bradford chromatic adaptation matrix between two whites. `RgbSpace::to_space` converts between spaces with it. |
| `cct::temp_tint_to_xy`, `cct::xy_to_temp_tint` | These convert a correlated color temperature and tint to a white point, and a white point back to temperature and tint. Tint is Duv × 3000 along the locus normal in CIE 1960 (u, v); positive values move toward green. |
| `cct::wb_matrix`, `cct::white_balance` | These return the RGB matrix that makes a scene lit by one white look lit by another. |
| `oklab` | The module converts to OkLab from linear sRGB with the published matrices, from XYZ, and from any `RgbSpace` (`OkLab::for_space`), and it converts to OkLCh. |
| `curve::MonotoneCurve` | This is a tone curve through points that does not overshoot between them. It saves as its list of points. `knots` gives each point with its tangent for evaluation in a shader. |
| `curve::Lut1` | This is a 1D lookup table over 0..1 with linear interpolation. |
| `tonemap::Eetf` | This is the reference electrical-electrical transfer function (EETF) from a mastering display to a target display, with the black lift, on PQ values or nits. When the target peak is below a third of the source range in PQ, where the Annex's knee would be negative, the knee is 0 and the spline starts with a slope of 3 × maxLum. This rule is the crate's own. |
| `tonemap::ToneMap` | This applies the EETF to linear RGB by max(R, G, B) as a ratio, with 1.0 at the 203-nit reference white. |

`craft-cms` has its own 3×3 matrix and Bradford helpers (`craft_cms::math`) for ICC work in the
D50 connection space. `Mat3` converts to and from their row-major `[[f64; 3]; 3]` with `From`.

## Limits and failures

- `Xy::to_xyz` rejects a chromaticity with `y` at or below 0, or with a coordinate that is not
  finite. `bradford` then returns the identity, and so does `RgbSpace::to_xyz` for such a white or
  for collinear primaries.
- Temperatures are clamped to 1000 K to 100,000 K, and NaN reads as 6500 K. A tint that is not
  finite reads as 0.
- `MonotoneCurve` keeps at most 1024 points and drops points with a coordinate that is not finite
  or is beyond ±1e6. Reading a saved curve runs the same constructor and its checks. The saved form
  is one list of points, so a file cannot give the curve mismatched arrays.
- `Lut1::from_fn` and `MonotoneCurve::to_lut` build at most 65,536 entries.
- Luminances are clamped to 0 to 10,000 nits. A target peak that is NaN or not above 0 gives an
  EETF and a `ToneMap` that pass values through.

## Tests

`cargo test -p craft-color-transform` checks the code against published values: the sRGB matrix of
IEC 61966-2-1, the BT.709 and BT.2020 luminance weights, the BT.709 to BT.2020 matrix of
ITU-R BT.2087, the D65 to D50 Bradford matrix as Lindbloom's chromatic adaptation page prints it,
illuminant A and D65 from CIE 15:2004 (temperature, Duv, CIE 1960 coordinates), Ottosson's XYZ to
OkLab table, the OkLab value CSS Color Module Level 4 gives for `#7654CD`, the PQ levels of
BT.2408, and EETF values computed from the BT.2408 Annex 5 equations by a separate implementation.

## Sources

| Item | Source of the formula |
| --- | --- |
| `Mat3`, `Xy`, `RgbSpace` | SMPTE RP 177; primaries from ITU-R BT.709, BT.601, BT.2020 and SMPTE EG 432-1 |
| `bradford`, `bradford_xyz` | The Bradford cone response matrix |
| `cct::temp_tint_to_xy`, `cct::xy_to_temp_tint` | Kim et al., "Design of Advanced Color Temperature Control System for HDTV Applications", Journal of the Korean Physical Society 41 (2002) |
| `cct::wb_matrix`, `cct::white_balance` | Bradford adaptation |
| `oklab` | B. Ottosson, "A perceptual color space for image processing" (2020) |
| `curve::MonotoneCurve` | Fritsch and Carlson, "Monotone Piecewise Cubic Interpolation", SIAM J. Numer. Anal. 17 (1980) |
| `tonemap::Eetf` | Report ITU-R BT.2408-8, Annex 5 |
| `tonemap::ToneMap` | Report ITU-R BT.2408-8, Annex 5 (the maxRGB option) |

## License

The crate is licensed under MIT OR Apache-2.0, at your option.
