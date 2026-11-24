//! The kernels the face models need, on pixel-major feature maps (`[pixels, channels]`
//! row-major, NHWC with a batch of one): dense layers (also 1×1 convolutions), convolutions as a
//! patch matrix times the weights, and depthwise convolutions.
//!
//! Every kernel writes into an output slice the caller owns and allocates nothing, so a model run
//! with buffers sized at load does not allocate. Weights are laid out once at load so every inner
//! loop runs over contiguous channels. Work is split across a [`Pool`] by rows of the output.

use crate::pool::Pool;

/// Output columns a dense-layer block keeps in registers.
const COLS: usize = 8;
/// Input rows a dense-layer block reads together.
const ROWS: usize = 4;
/// Output rows per parallel piece of a dense layer.
const ROW_PIECE: usize = 16;

/// A dense layer (also a 1×1, stride-1 convolution): `y = x·W + b`.
#[derive(Clone, Debug, Default)]
pub struct Linear {
    /// `W` as `[inp × out]` (row `i` holds input channel `i`'s weight for every output).
    pub wt: Vec<f32>,
    pub b: Vec<f32>,
    pub inp: usize,
    pub out: usize,
}

impl Linear {
    /// `y[rows × out] = x[rows × inp]·W + b`. Short slices leave `y` unchanged.
    pub fn forward(&self, x: &[f32], rows: usize, y: &mut [f32], pool: &Pool) {
        dense(x, rows, self.inp, &self.wt, &self.b, self.out, y, pool);
    }
}

/// `y[rows × out] = x[rows × inp]·wt[inp × out] + b`, split across `pool` by rows. Short slices
/// leave `y` unchanged.
pub fn dense(x: &[f32], rows: usize, inp: usize, wt: &[f32], b: &[f32], out: usize, y: &mut [f32], pool: &Pool) {
    let (Some(xn), Some(yn), Some(wn)) = (rows.checked_mul(inp), rows.checked_mul(out), inp.checked_mul(out)) else { return };
    if out == 0 || x.len() < xn || y.len() < yn || wt.len() < wn || b.len() < out {
        return;
    }
    let (x, y, wt, b) = (&x[..xn], &mut y[..yn], &wt[..wn], &b[..out]);
    if inp == 0 {
        for row in y.chunks_exact_mut(out) {
            row.copy_from_slice(b);
        }
        return;
    }
    pool.chunks(y, ROW_PIECE * out, xn.saturating_mul(out), |piece, yc| {
        let r0 = piece * ROW_PIECE;
        let n = yc.len() / out;
        if let Some(xc) = x.get(r0 * inp..(r0 + n) * inp) {
            dense_rows(xc, inp, wt, b, out, yc);
        }
    });
}

/// The dense layer on a few rows: blocks of [`ROWS`] rows, then the rest one at a time.
fn dense_rows(x: &[f32], inp: usize, wt: &[f32], b: &[f32], out: usize, y: &mut [f32]) {
    let mut xs = x.chunks_exact(ROWS * inp);
    let mut ys = y.chunks_exact_mut(ROWS * out);
    for (xb, yb) in (&mut xs).zip(&mut ys) {
        let (x0, rest) = xb.split_at(inp);
        let (x1, rest) = rest.split_at(inp);
        let (x2, x3) = rest.split_at(inp);
        dense_block(&[x0, x1, x2, x3], wt, b, out, yb);
    }
    for (xr, yr) in xs.remainder().chunks_exact(inp).zip(ys.into_remainder().chunks_exact_mut(out)) {
        dense_block(&[xr], wt, b, out, yr);
    }
}

/// `R` rows of the dense layer (`y` holds `R × out`): [`COLS`] outputs of every row at a time
/// in registers, so each weight read serves every row.
fn dense_block<const R: usize>(x: &[&[f32]; R], wt: &[f32], b: &[f32], out: usize, y: &mut [f32]) {
    let mut j = 0;
    while j + COLS <= out {
        let mut acc = [[0.0f32; COLS]; R];
        for r in acc.iter_mut() {
            r.copy_from_slice(&b[j..j + COLS]);
        }
        for (p, wrow) in wt.chunks_exact(out).enumerate() {
            let Some(w) = wrow.get(j..j + COLS).and_then(|w| <&[f32; COLS]>::try_from(w).ok()) else { return };
            for (acc, xr) in acc.iter_mut().zip(x) {
                let a = xr.get(p).copied().unwrap_or(0.0);
                for (o, wv) in acc.iter_mut().zip(w) {
                    *o += a * wv;
                }
            }
        }
        for (r, acc) in acc.iter().enumerate() {
            if let Some(dst) = y.get_mut(r * out + j..r * out + j + COLS) {
                dst.copy_from_slice(acc);
            }
        }
        j += COLS;
    }
    for jj in j..out {
        for (r, xr) in x.iter().enumerate() {
            let mut s = b[jj];
            for (a, wrow) in xr.iter().zip(wt.chunks_exact(out)) {
                s += a * wrow[jj];
            }
            if let Some(v) = y.get_mut(r * out + jj) {
                *v = s;
            }
        }
    }
}

/// The input position that window cell `k` of output position `o` reads (`stride` apart, `pad`
/// zero cells before the input), or `None` when it falls in the padding or past an input of `n`.
/// The position is worked out in a wider type, so a large stride or padding skips the cell
/// instead of overflowing.
pub(crate) fn tap(o: usize, stride: usize, k: usize, pad: usize, n: usize) -> Option<usize> {
    let at = (o as u128).checked_mul(stride as u128)?.checked_add(k as u128)?.checked_sub(pad as u128)?;
    usize::try_from(at).ok().filter(|&i| i < n)
}

/// A `ks×ks` convolution (any stride, one group) on `h×w×inp` → `oh×ow×out`: the patches of every
/// output pixel are gathered into a patch matrix, which a dense layer multiplies by the weights.
#[derive(Clone, Debug, Default)]
pub struct Conv {
    /// `[(ky·ks + kx)·inp + ci] × out`.
    pub wt: Vec<f32>,
    pub b: Vec<f32>,
    pub inp: usize,
    pub out: usize,
    pub ks: usize,
    pub stride: usize,
}

impl Conv {
    /// Floats of patch matrix an `oh×ow` output needs.
    pub fn patch_len(&self, oh: usize, ow: usize) -> Option<usize> {
        oh.checked_mul(ow)?.checked_mul(self.ks)?.checked_mul(self.ks)?.checked_mul(self.inp)
    }

    /// An `oh×ow` output with `pad` (top, left) zero rows and columns before the input; what falls
    /// past the input's far edges is zero too, so asymmetric "same" padding works. `patch` holds
    /// at least [`Conv::patch_len`] floats. Short slices leave `y` unchanged.
    pub fn forward(&self, x: &[f32], [h, w]: [usize; 2], [oh, ow]: [usize; 2], pad: [usize; 2], patch: &mut [f32], y: &mut [f32], pool: &Pool) {
        let kk = self.ks * self.ks * self.inp;
        let Some(need) = self.patch_len(oh, ow) else { return };
        if kk == 0 || self.stride == 0 || patch.len() < need || h.checked_mul(w).and_then(|p| p.checked_mul(self.inp)).is_none_or(|n| x.len() < n) {
            return;
        }
        let patch = &mut patch[..need];
        pool.chunks(patch, ow * kk, need, |oy, row| {
            row.fill(0.0);
            for ox in 0..ow {
                for ky in 0..self.ks {
                    let Some(iy) = tap(oy, self.stride, ky, pad[0], h) else { continue };
                    for kx in 0..self.ks {
                        let Some(ix) = tap(ox, self.stride, kx, pad[1], w) else { continue };
                        let src = (iy * w + ix) * self.inp;
                        let dst = ox * kk + (ky * self.ks + kx) * self.inp;
                        if let (Some(d), Some(s)) = (row.get_mut(dst..dst + self.inp), x.get(src..src + self.inp)) {
                            d.copy_from_slice(s);
                        }
                    }
                }
            }
        });
        dense(patch, oh * ow, kk, &self.wt, &self.b, self.out, y, pool);
    }
}

/// A depthwise `ks×ks` convolution (one filter per channel) on `h×w×c`.
#[derive(Clone, Debug, Default)]
pub struct Depthwise {
    /// `[tap × c]`.
    pub w: Vec<f32>,
    pub b: Vec<f32>,
    pub c: usize,
    pub ks: usize,
    pub stride: usize,
}

impl Depthwise {
    /// An `oh×ow` output with `pad` (top, left) before the input, as [`Conv::forward`]. Short
    /// slices leave `y` unchanged.
    pub fn forward(&self, x: &[f32], [h, w]: [usize; 2], [oh, ow]: [usize; 2], pad: [usize; 2], y: &mut [f32], pool: &Pool) {
        let (c, ks) = (self.c, self.ks);
        let fits = |a: usize, b: usize| a.checked_mul(b).and_then(|n| n.checked_mul(c));
        let (Some(xn), Some(yn)) = (fits(h, w), fits(oh, ow)) else { return };
        if c == 0 || self.stride == 0 || x.len() < xn || y.len() < yn || self.w.len() < ks * ks * c || self.b.len() < c {
            return;
        }
        let work = yn.saturating_mul(ks * ks);
        pool.chunks(&mut y[..yn], ow * c, work, |oy, row| {
            for (ox, out) in row.chunks_exact_mut(c).enumerate() {
                out.copy_from_slice(&self.b[..c]);
                for ky in 0..ks {
                    let Some(iy) = tap(oy, self.stride, ky, pad[0], h) else { continue };
                    for kx in 0..ks {
                        let Some(ix) = tap(ox, self.stride, kx, pad[1], w) else { continue };
                        let at = (iy * w + ix) * c;
                        let tap = (ky * ks + kx) * c;
                        let (Some(src), Some(tap)) = (x.get(at..at + c), self.w.get(tap..tap + c)) else { continue };
                        for ((o, s), k) in out.iter_mut().zip(src).zip(tap) {
                            *o += s * k;
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pools() -> [Pool; 2] {
        [Pool::single(), Pool::new(3).unwrap()]
    }

    #[test]
    fn dense_matches_direct_sums_for_every_block_shape() {
        // Rows 0 to 9 cover full blocks and leftovers; columns 1 to 19 cover full and partial
        // register blocks.
        for pool in pools() {
            for (rows, inp, out) in [(1, 1, 1), (4, 3, 8), (5, 7, 9), (9, 17, 19), (130, 33, 24), (3, 0, 5), (4096, 16, 24)] {
                let x: Vec<f32> = (0..rows * inp).map(|i| ((i * 7) % 13) as f32 - 6.0).collect();
                let wt: Vec<f32> = (0..inp * out).map(|i| ((i * 5) % 11) as f32 - 5.0).collect();
                let b: Vec<f32> = (0..out).map(|i| i as f32 * 0.5).collect();
                let mut y = vec![f32::NAN; rows * out];
                pool.install(|| dense(&x, rows, inp, &wt, &b, out, &mut y, &pool));
                for i in 0..rows {
                    for j in 0..out {
                        let want: f32 = b[j] + (0..inp).map(|p| x[i * inp + p] * wt[p * out + j]).sum::<f32>();
                        assert_eq!(y[i * out + j], want, "{rows}x{inp}x{out} at {i},{j} on {pool:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn convolutions_match_direct_sums() {
        // A 3×3 stride-2 convolution with one row and column of padding on a 5×4×2 input.
        let (h, w, inp, out) = (5, 4, 2, 3);
        let x: Vec<f32> = (0..h * w * inp).map(|i| (i % 7) as f32 * 0.5).collect();
        // `[out × ky × kx × inp]` as a model stores it, laid out as `Conv` wants.
        let ohwi: Vec<f32> = (0..out * 9 * inp).map(|i| ((i % 5) as f32 - 2.0) * 0.25).collect();
        let mut wt = vec![0.0; 9 * inp * out];
        for o in 0..out {
            for p in 0..9 * inp {
                wt[p * out + o] = ohwi[o * 9 * inp + p];
            }
        }
        let b = vec![0.1, -0.2, 0.3];
        let conv = Conv { wt, b: b.clone(), inp, out, ks: 3, stride: 2 };
        let (oh, ow) = (3, 2);
        for pool in pools() {
            let mut patch = vec![f32::NAN; conv.patch_len(oh, ow).unwrap()];
            let mut y = vec![0.0; oh * ow * out];
            pool.install(|| conv.forward(&x, [h, w], [oh, ow], [1, 1], &mut patch, &mut y, &pool));
            for oy in 0..oh {
                for ox in 0..ow {
                    for o in 0..out {
                        let mut s = b[o];
                        for ky in 0..3 {
                            for kx in 0..3 {
                                let (iy, ix) = ((oy * 2 + ky) as isize - 1, (ox * 2 + kx) as isize - 1);
                                if iy >= 0 && ix >= 0 && iy < h as isize && ix < w as isize {
                                    for ci in 0..inp {
                                        s += x[(iy as usize * w + ix as usize) * inp + ci] * ohwi[((o * 3 + ky) * 3 + kx) * inp + ci];
                                    }
                                }
                            }
                        }
                        assert!((y[(oy * ow + ox) * out + o] - s).abs() < 1e-5);
                    }
                }
            }
        }
    }

    #[test]
    fn depthwise_is_a_convolution_per_channel() {
        let (h, w, c) = (6, 5, 3);
        let x: Vec<f32> = (0..h * w * c).map(|i| ((i * 3) % 11) as f32 * 0.25 - 1.0).collect();
        let taps: Vec<f32> = (0..9 * c).map(|i| (i as f32 - 13.0) * 0.1).collect();
        let b = vec![0.5, -0.5, 0.25];
        let d = Depthwise { w: taps.clone(), b: b.clone(), c, ks: 3, stride: 2 };
        let (oh, ow) = (3, 3);
        for pool in pools() {
            let mut y = vec![0.0; oh * ow * c];
            pool.install(|| d.forward(&x, [h, w], [oh, ow], [0, 0], &mut y, &pool));
            for oy in 0..oh {
                for ox in 0..ow {
                    for ch in 0..c {
                        let mut s = b[ch];
                        for ky in 0..3 {
                            for kx in 0..3 {
                                let (iy, ix) = (oy * 2 + ky, ox * 2 + kx);
                                if iy < h && ix < w {
                                    s += x[(iy * w + ix) * c + ch] * taps[(ky * 3 + kx) * c + ch];
                                }
                            }
                        }
                        assert!((y[(oy * ow + ox) * c + ch] - s).abs() < 1e-5);
                    }
                }
            }
        }
    }

    /// A stride and padding near `usize::MAX` skip the window cells they put outside the input,
    /// and the cells inside it still count.
    #[test]
    fn huge_strides_and_padding_skip_cells_outside_the_input() {
        let pool = Pool::single();
        // A 2×2 filter over one pixel of one channel: output column 0 reads nothing, and output
        // column 1 reads the pixel through the filter's first cell only.
        let conv = Conv { wt: vec![1.0, 2.0, 3.0, 4.0], b: vec![0.5], inp: 1, out: 1, ks: 2, stride: usize::MAX };
        let mut patch = vec![0.0; conv.patch_len(1, 2).unwrap()];
        let mut y = vec![0.0; 2];
        conv.forward(&[10.0], [1, 1], [1, 2], [0, usize::MAX], &mut patch, &mut y, &pool);
        assert_eq!(y, [0.5, 10.5]);
        let d = Depthwise { w: vec![1.0, 2.0, 3.0, 4.0], b: vec![0.5], c: 1, ks: 2, stride: usize::MAX };
        let mut y = vec![0.0; 2];
        d.forward(&[10.0], [1, 1], [1, 2], [0, usize::MAX], &mut y, &pool);
        assert_eq!(y, [0.5, 10.5]);
        assert_eq!(tap(1, usize::MAX, 0, usize::MAX, 1), Some(0));
        assert_eq!(tap(1, usize::MAX, 1, usize::MAX, 1), None);
        assert_eq!(tap(0, 1, 0, 1, 4), None);
    }

    #[test]
    fn short_buffers_leave_the_output_alone() {
        let pool = Pool::single();
        let mut y = vec![7.0f32; 4];
        dense(&[1.0], 2, 2, &[1.0; 4], &[0.0; 2], 2, &mut y, &pool);
        assert_eq!(y, [7.0; 4]);
        let conv = Conv { wt: vec![1.0; 9], b: vec![0.0], inp: 1, out: 1, ks: 3, stride: 1 };
        conv.forward(&[1.0; 9], [3, 3], [2, 2], [0, 0], &mut [0.0; 3], &mut y, &pool);
        assert_eq!(y, [7.0; 4]);
        let d = Depthwise { w: vec![1.0; 9], b: vec![0.0], c: 1, ks: 3, stride: 0 };
        d.forward(&[1.0; 9], [3, 3], [2, 2], [0, 0], &mut y, &pool);
        assert_eq!(y, [7.0; 4]);
    }
}
