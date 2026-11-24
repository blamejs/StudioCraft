//! Running TensorFlow Lite models without TensorFlow: a reader for the `.tflite` FlatBuffer
//! (fields as numbered in TensorFlow Lite's published `schema.fbs`, Apache-2.0) and an
//! interpreter for the float operators the face models use: `CONV_2D`, `DEPTHWISE_CONV_2D`,
//! `MAX_POOL_2D`, `ADD`, `PAD`, `CONCATENATION`, `RESHAPE`, `RELU`, `RELU6`, `PRELU`, `LOGISTIC`
//! and `DEQUANTIZE` of float16 weights (folded at load). Weights are laid out once at load for
//! the [`nn`](crate::nn) kernels. Anything else (other operators, quantized tensors, dilation) is a
//! load error, never a panic.
//!
//! A model is refused at load when running it would take more than a fixed budget: memory (each
//! tensor, the buffers of one run, one operator's working space, the weights), work (multiply-adds,
//! values written) or kernel size. Every one is worked out from the static shapes with checked
//! arithmetic, so a small hostile file can neither exhaust memory nor keep a run busy for minutes.
//!
//! Every value a run computes lives in a [`Workspace`] whose buffers are sized at load:
//! [`Model::workspace`] gives each tensor a slot, and a slot is reused once every operator that
//! reads its tensor has run. [`Model::run_into`] then runs the model without allocating.

use crate::nn::{Conv, Depthwise, Linear};
use crate::pool::Pool;
use crate::{Error, Result};

type Res<T> = std::result::Result<T, String>;

// ---------------------------------------------------------------- FlatBuffers

/// A FlatBuffer: bounds-checked little-endian reads.
#[derive(Clone, Copy)]
struct Fb<'a>(&'a [u8]);

/// A table: its position in the buffer.
#[derive(Clone, Copy)]
struct Table(usize);

impl<'a> Fb<'a> {
    fn bytes<const N: usize>(&self, at: usize) -> Res<[u8; N]> {
        self.0.get(at..at.checked_add(N).ok_or("bad offset")?).and_then(|s| s.try_into().ok()).ok_or_else(|| "truncated model".to_string())
    }
    fn u8(&self, at: usize) -> Res<u8> {
        Ok(self.bytes::<1>(at)?[0])
    }
    fn u16(&self, at: usize) -> Res<u16> {
        Ok(u16::from_le_bytes(self.bytes(at)?))
    }
    fn u32(&self, at: usize) -> Res<u32> {
        Ok(u32::from_le_bytes(self.bytes(at)?))
    }
    fn i32(&self, at: usize) -> Res<i32> {
        Ok(i32::from_le_bytes(self.bytes(at)?))
    }
    fn u64(&self, at: usize) -> Res<u64> {
        Ok(u64::from_le_bytes(self.bytes(at)?))
    }
    /// Follow the offset stored at `at`.
    fn deref(&self, at: usize) -> Res<usize> {
        at.checked_add(self.u32(at)? as usize).ok_or_else(|| "bad offset".to_string())
    }
    fn root(&self) -> Res<Table> {
        Ok(Table(self.deref(0)?))
    }
    /// Where field `id` of `t` is stored (absent fields have their default).
    fn field(&self, t: Table, id: usize) -> Res<Option<usize>> {
        let vt = (t.0 as i64 - self.i32(t.0)? as i64).try_into().map_err(|_| "bad vtable")?;
        let vt_len = self.u16(vt)? as usize;
        let slot = 4 + 2 * id;
        if slot + 2 > vt_len {
            return Ok(None);
        }
        let off = self.u16(vt + slot)? as usize;
        Ok((off != 0).then_some(t.0 + off))
    }
    fn int(&self, t: Table, id: usize, default: i32) -> Res<i32> {
        self.field(t, id)?.map_or(Ok(default), |p| self.i32(p))
    }
    fn byte(&self, t: Table, id: usize, default: u8) -> Res<u8> {
        self.field(t, id)?.map_or(Ok(default), |p| self.u8(p))
    }
    fn table(&self, t: Table, id: usize) -> Res<Option<Table>> {
        self.field(t, id)?.map(|p| self.deref(p).map(Table)).transpose()
    }
    /// A vector field: (start of its elements, length).
    fn vector(&self, t: Table, id: usize) -> Res<(usize, usize)> {
        let Some(p) = self.field(t, id)? else { return Ok((0, 0)) };
        let v = self.deref(p)?;
        let n = self.u32(v)? as usize;
        if v.saturating_add(4).saturating_add(n) > self.0.len() {
            return Err("truncated model".into());
        }
        Ok((v + 4, n))
    }
    fn tables(&self, t: Table, id: usize) -> Res<Vec<Table>> {
        let (at, n) = self.vector(t, id)?;
        (0..n).map(|i| self.deref(at + 4 * i).map(Table)).collect()
    }
    fn ints(&self, t: Table, id: usize) -> Res<Vec<i32>> {
        let (at, n) = self.vector(t, id)?;
        (0..n).map(|i| self.i32(at + 4 * i)).collect()
    }
    fn string(&self, t: Table, id: usize) -> Res<String> {
        let (at, n) = self.vector(t, id)?;
        Ok(String::from_utf8_lossy(self.0.get(at..at + n).ok_or("truncated model")?).into_owned())
    }
}

// ---------------------------------------------------------------- the model

/// Operator codes (`BuiltinOperator`).
mod op {
    pub const ADD: i32 = 0;
    pub const CONCATENATION: i32 = 2;
    pub const CONV_2D: i32 = 3;
    pub const DEPTHWISE_CONV_2D: i32 = 4;
    pub const DEQUANTIZE: i32 = 6;
    pub const LOGISTIC: i32 = 14;
    pub const MAX_POOL_2D: i32 = 17;
    pub const RELU: i32 = 19;
    pub const RELU6: i32 = 21;
    pub const RESHAPE: i32 = 22;
    pub const PAD: i32 = 34;
    pub const PRELU: i32 = 54;
}

/// The most values a tensor may hold.
const MAX_TENSOR: usize = 1 << 28;
/// The widest convolution or pooling window side.
const MAX_KERNEL: usize = 32;
/// The most floats one operator may use as working space (a convolution's patch matrix:
/// `oh·ow·kh·kw·cin`).
const MAX_SCRATCH: u64 = 1 << 24;
/// The most multiply-adds (convolutions) and window comparisons (pooling) in one run.
const MAX_MACS: u64 = 1 << 33;
/// The most values all operators together may write in one run.
const MAX_VALUES: u64 = 1 << 30;
/// The most floats a run's buffers may hold in all.
const MAX_LIVE: u64 = 1 << 29;
/// The most dimensions a padded tensor may have.
const MAX_PAD_RANK: usize = 8;
/// Bytes of tensor names and shapes a model may carry in all.
const MAX_META: usize = 1 << 24;
/// The most constant values decoded and laid out at load (a weight shared by several tensors or
/// operators counts once per use).
const MAX_CONSTS: usize = 1 << 27;

/// Tensor types (`TensorType`).
const FLOAT32: u8 = 0;
const FLOAT16: u8 = 1;
const INT32: u8 = 2;

/// A fused activation (`ActivationFunctionType`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    None,
    Relu,
    ReluN1To1,
    Relu6,
    Tanh,
}

impl Act {
    fn from(code: u8) -> Res<Act> {
        Ok(match code {
            0 => Act::None,
            1 => Act::Relu,
            2 => Act::ReluN1To1,
            3 => Act::Relu6,
            4 => Act::Tanh,
            c => return Err(format!("unsupported fused activation {c}")),
        })
    }
    fn apply(self, x: &mut [f32]) {
        let f: fn(f32) -> f32 = match self {
            Act::None => return,
            Act::Relu => |v| v.max(0.0),
            Act::ReluN1To1 => |v| v.clamp(-1.0, 1.0),
            Act::Relu6 => |v| v.clamp(0.0, 6.0),
            Act::Tanh => f32::tanh,
        };
        for v in x {
            *v = f(*v);
        }
    }
}

/// One operator, its weights laid out for the kernels.
#[derive(Clone, Debug)]
enum Op {
    /// A 1×1, stride-1 convolution: a dense layer over the pixels.
    Pointwise(Linear, Act),
    Conv(Conv, [usize; 2], Act),
    Depthwise(Depthwise, [usize; 2], Act),
    /// Kernel, stride, padding before (top, left).
    MaxPool {
        k: [usize; 2],
        stride: [usize; 2],
        pad: [usize; 2],
        act: Act,
    },
    Add(Act),
    /// Constant padding (zeros), per dimension (before, after).
    Pad(Vec<[usize; 2]>),
    Concat {
        axis: usize,
        act: Act,
    },
    Reshape,
    Act(Act),
    Prelu(Vec<f32>),
    Logistic,
}

#[derive(Clone, Debug)]
struct Step {
    op: Op,
    /// Runtime inputs (tensor ids).
    inputs: Vec<usize>,
    output: usize,
}

/// A loaded model, ready to run (batch of one, static shapes).
#[derive(Clone, Debug)]
pub struct Model {
    shapes: Vec<Vec<usize>>,
    /// Values per tensor.
    sizes: Vec<usize>,
    names: Vec<String>,
    /// Constant float tensors (weights folded into the steps are not kept).
    consts: Vec<Option<Vec<f32>>>,
    steps: Vec<Step>,
    inputs: Vec<usize>,
    outputs: Vec<usize>,
    /// The workspace slot of each tensor a run computes or is given.
    slot: Vec<Option<usize>>,
    /// Floats each slot holds.
    slot_len: Vec<usize>,
    /// Floats of working space the largest convolution needs.
    scratch: usize,
}

/// The buffers one run of a [`Model`] uses, sized at load. A workspace belongs to the model that
/// made it; running another model on it is an error.
#[derive(Clone, Debug, Default)]
pub struct Workspace {
    slots: Vec<Vec<f32>>,
    scratch: Vec<f32>,
}

impl Workspace {
    /// Floats the workspace holds in all.
    pub fn len(&self) -> usize {
        self.slots.iter().map(Vec::len).sum::<usize>() + self.scratch.len()
    }

    /// Whether the workspace holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn index(i: i32, n: usize) -> Res<usize> {
    usize::try_from(i).ok().filter(|&i| i < n).ok_or_else(|| format!("bad tensor index {i}"))
}

/// "Same" padding: output size and padding before.
fn same(input: usize, k: usize, stride: usize) -> (usize, usize) {
    let out = input.div_ceil(stride);
    let total = ((out.saturating_sub(1)) * stride + k).saturating_sub(input);
    (out, total / 2)
}

/// Output size and padding before, for `padding` (0 same, 1 valid).
fn window(padding: u8, input: usize, k: usize, stride: usize) -> Res<(usize, usize)> {
    match padding {
        0 => Ok(same(input, k, stride)),
        1 if input >= k => Ok(((input - k) / stride + 1, 0)),
        _ => Err("bad window".into()),
    }
}

/// An IEEE 754 half-precision value as `f32`.
pub(crate) fn half_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as u32, (h & 0x3ff) as u32);
    let bits = match (e, m) {
        (0, 0) => s << 31,
        (0, m) => {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            let mut m = m;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            (s << 31) | (e << 23) | ((m & 0x3ff) << 13)
        }
        (0x1f, m) => (s << 31) | (0xff << 23) | (m << 13),
        (e, m) => (s << 31) | ((e + 127 - 15) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

impl Model {
    /// Read a `.tflite` file.
    pub fn read(bytes: &[u8]) -> Result<Model> {
        read(bytes).map_err(Error::Model)
    }

    /// The buffers a run of this model needs, sized for it.
    pub fn workspace(&self) -> Workspace {
        Workspace { slots: self.slot_len.iter().map(|&n| vec![0.0; n]).collect(), scratch: vec![0.0; self.scratch] }
    }

    /// Floats a workspace of this model holds.
    pub fn workspace_len(&self) -> usize {
        self.slot_len.iter().sum::<usize>() + self.scratch
    }

    /// The shape of input `k`.
    pub fn input_shape(&self, k: usize) -> Option<&[usize]> {
        self.inputs.get(k).and_then(|&i| self.shapes.get(i)).map(Vec::as_slice)
    }

    /// The shape and name of output `k`.
    pub fn output(&self, k: usize) -> Option<(&[usize], &str)> {
        let &i = self.outputs.get(k)?;
        Some((self.shapes.get(i)?.as_slice(), self.names.get(i)?.as_str()))
    }

    /// How many outputs the model has.
    pub fn outputs(&self) -> usize {
        self.outputs.len()
    }

    /// Run the model on its inputs (row-major, NHWC) in `ws`, on `pool`'s threads. Read the
    /// results with [`Model::output_values`]. Does not allocate.
    pub fn run_into(&self, ws: &mut Workspace, inputs: &[&[f32]], pool: &Pool) -> Result<()> {
        if ws.slots.len() != self.slot_len.len()
            || ws.slots.iter().zip(&self.slot_len).any(|(s, &n)| s.len() != n)
            || ws.scratch.len() != self.scratch
        {
            return Err(Error::Model("the workspace belongs to another model".into()));
        }
        pool.install(|| self.run_steps(ws, inputs, pool)).map_err(Error::Model)
    }

    /// Output `k` of the last [`Model::run_into`] on `ws`.
    pub fn output_values<'a>(&'a self, ws: &'a Workspace, k: usize) -> Option<&'a [f32]> {
        let &id = self.outputs.get(k)?;
        let n = *self.sizes.get(id)?;
        match self.slot.get(id).copied().flatten() {
            Some(s) => ws.slots.get(s)?.get(..n),
            None => self.consts.get(id)?.as_deref(),
        }
    }

    /// Run the model once on one thread and copy its outputs out (tests and tools).
    pub fn run(&self, inputs: &[&[f32]]) -> Result<Vec<Vec<f32>>> {
        let mut ws = self.workspace();
        self.run_into(&mut ws, inputs, &Pool::single())?;
        (0..self.outputs.len())
            .map(|k| self.output_values(&ws, k).map(<[f32]>::to_vec).ok_or_else(|| Error::Model("an output was not computed".into())))
            .collect()
    }

    fn run_steps(&self, ws: &mut Workspace, inputs: &[&[f32]], pool: &Pool) -> Res<()> {
        if inputs.len() != self.inputs.len() {
            return Err(format!("the model takes {} inputs, not {}", self.inputs.len(), inputs.len()));
        }
        for (k, (&id, x)) in self.inputs.iter().zip(inputs).enumerate() {
            let n = self.sizes[id];
            if x.len() != n {
                return Err(format!("input {k}: expected {:?}", self.shapes[id]));
            }
            let s = self.slot[id].ok_or("an input has no buffer")?;
            ws.slots.get_mut(s).and_then(|b| b.get_mut(..n)).ok_or("bad workspace")?.copy_from_slice(x);
        }
        for s in &self.steps {
            let n = self.sizes[s.output];
            let out = self.slot[s.output].ok_or("an output has no buffer")?;
            // The output's slot is taken out while the step runs: its inputs are in other slots
            // (planned at load), so they stay readable. `take` leaves an empty vector and does not
            // allocate.
            let mut y = std::mem::take(ws.slots.get_mut(out).ok_or("bad workspace")?);
            let r = match y.get_mut(..n) {
                Some(yv) => self.step(s, &ws.slots, &mut ws.scratch, yv, pool),
                None => Err("bad workspace".into()),
            };
            if let Some(slot) = ws.slots.get_mut(out) {
                *slot = y;
            }
            r?;
        }
        Ok(())
    }

    /// The values of tensor `i` during a run.
    fn value<'a>(&'a self, slots: &'a [Vec<f32>], i: usize) -> Res<&'a [f32]> {
        let n = *self.sizes.get(i).ok_or("bad tensor")?;
        match self.slot.get(i).copied().flatten() {
            Some(s) => slots.get(s).and_then(|b| b.get(..n)).ok_or_else(|| "a tensor's buffer is in use".to_string()),
            None => self.consts.get(i).and_then(Option::as_deref).ok_or_else(|| "a tensor was not computed".to_string()),
        }
    }

    fn step(&self, s: &Step, slots: &[Vec<f32>], scratch: &mut [f32], y: &mut [f32], pool: &Pool) -> Res<()> {
        let x = self.value(slots, s.inputs[0])?;
        let xs = &self.shapes[s.inputs[0]];
        let ys = &self.shapes[s.output];
        let (h, w) = (xs.get(1).copied().unwrap_or(1), xs.get(2).copied().unwrap_or(1));
        let (oh, ow) = (ys.get(1).copied().unwrap_or(1), ys.get(2).copied().unwrap_or(1));
        match &s.op {
            Op::Pointwise(l, a) => {
                l.forward(x, h * w, y, pool);
                a.apply(y);
            }
            Op::Conv(c, pad, a) => {
                c.forward(x, [h, w], [oh, ow], *pad, scratch, y, pool);
                a.apply(y);
            }
            Op::Depthwise(d, pad, a) => {
                d.forward(x, [h, w], [oh, ow], *pad, y, pool);
                a.apply(y);
            }
            Op::MaxPool { k, stride, pad, act } => {
                max_pool(x, [h, w], *xs.last().unwrap_or(&1), [oh, ow], *k, *stride, *pad, y);
                act.apply(y);
            }
            Op::Add(a) => {
                let b = self.value(slots, s.inputs[1])?;
                for ((o, p), q) in y.iter_mut().zip(x).zip(b) {
                    *o = p + q;
                }
                a.apply(y);
            }
            Op::Pad(p) => pad(x, xs, p, y),
            Op::Concat { axis, act } => {
                let outer: usize = ys.get(..*axis).ok_or("CONCATENATION: bad axis")?.iter().product();
                let mut at = 0;
                for o in 0..outer {
                    for &id in &s.inputs {
                        let chunk: usize = self.shapes[id].get(*axis..).ok_or("CONCATENATION: bad axis")?.iter().product();
                        let part = self.value(slots, id)?.get(o * chunk..(o + 1) * chunk).ok_or("CONCATENATION: bad input")?;
                        y.get_mut(at..at + chunk).ok_or("CONCATENATION: bad output")?.copy_from_slice(part);
                        at += chunk;
                    }
                }
                act.apply(y);
            }
            Op::Reshape => y.copy_from_slice(x),
            Op::Act(a) => {
                y.copy_from_slice(x);
                a.apply(y);
            }
            Op::Prelu(alpha) => {
                for (row_y, row_x) in y.chunks_exact_mut(alpha.len()).zip(x.chunks_exact(alpha.len())) {
                    for ((o, &v), &a) in row_y.iter_mut().zip(row_x).zip(alpha) {
                        *o = if v >= 0.0 { v } else { v * a };
                    }
                }
            }
            Op::Logistic => {
                for (o, v) in y.iter_mut().zip(x) {
                    *o = 1.0 / (1.0 + (-v).exp());
                }
            }
        }
        Ok(())
    }

    /// What one run takes, from the static shapes (`None` if a count overflows). `live` is the
    /// workspace's size.
    fn cost(&self) -> Option<Cost> {
        let mut c = Cost::default();
        for s in &self.steps {
            let &x = s.inputs.first()?;
            let (xs, ys) = (self.shapes.get(x)?, self.shapes.get(s.output)?);
            let dim = |sh: &[usize], d: usize| sh.get(d).copied().unwrap_or(1) as u64;
            let (h, w, oh, ow) = (dim(xs, 1), dim(xs, 2), dim(ys, 1), dim(ys, 2));
            let out = *self.sizes.get(s.output)? as u64;
            let macs = match &s.op {
                Op::Pointwise(l, _) => h.checked_mul(w)?.checked_mul(l.inp as u64)?.checked_mul(l.out as u64)?,
                Op::Conv(cv, ..) => {
                    let ks = cv.ks as u64;
                    c.kernel = c.kernel.max(ks);
                    let kk = ks.checked_mul(ks)?.checked_mul(cv.inp as u64)?;
                    c.scratch = c.scratch.max(oh.checked_mul(ow)?.checked_mul(kk)?);
                    oh.checked_mul(ow)?.checked_mul(cv.out as u64)?.checked_mul(kk)?
                }
                Op::Depthwise(d, ..) => {
                    let ks = d.ks as u64;
                    c.kernel = c.kernel.max(ks);
                    oh.checked_mul(ow)?.checked_mul(d.c as u64)?.checked_mul(ks)?.checked_mul(ks)?
                }
                Op::MaxPool { k, .. } => {
                    c.kernel = c.kernel.max(k[0].max(k[1]) as u64);
                    out.checked_mul(k[0] as u64)?.checked_mul(k[1] as u64)?
                }
                _ => 0,
            };
            c.macs = c.macs.checked_add(macs)?;
            c.values = c.values.checked_add(out)?;
        }
        c.live = self.slot_len.iter().try_fold(c.scratch, |a, &n| a.checked_add(n as u64))?;
        Some(c)
    }
}

/// What running a model takes, worked out at load from its static shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Cost {
    /// Multiply-adds (convolutions) and window comparisons (pooling).
    macs: u64,
    /// Values all operators write.
    values: u64,
    /// Floats the workspace holds.
    live: u64,
    /// The largest working space one operator uses, in floats.
    scratch: u64,
    /// The widest window side.
    kernel: u64,
}

fn read(bytes: &[u8]) -> Res<Model> {
    let fb = Fb(bytes);
    let root = fb.root()?;
    let codes: Vec<i32> = fb.tables(root, 1)?.into_iter().map(|c| Ok(fb.int(c, 3, 0)?.max(fb.byte(c, 0, 0)? as i8 as i32))).collect::<Res<_>>()?;
    let buffers = fb.tables(root, 4)?;
    let graphs = fb.tables(root, 2)?;
    let g = *graphs.first().ok_or("the model has no graph")?;
    let tensors = fb.tables(g, 0)?;
    let n = tensors.len();
    let mut shapes = Vec::with_capacity(n);
    let mut sizes = Vec::with_capacity(n);
    let mut names = Vec::with_capacity(n);
    // Constant data: float tensors as f32, int tensors (pads, shapes) as i32, decoded once their
    // total is known to fit the budget.
    let mut floats: Vec<Option<Vec<f32>>> = vec![None; n];
    let mut ints: Vec<Option<Vec<i32>>> = vec![None; n];
    let mut data: Vec<(usize, u8, usize, &[u8])> = vec![];
    let mut consts = 0usize;
    let mut meta = 0usize;
    for (i, t) in tensors.iter().enumerate() {
        let shape: Vec<usize> = fb
            .ints(*t, 0)?
            .into_iter()
            .map(|d| usize::try_from(d).map_err(|_| "dynamic shapes are not supported"))
            .collect::<std::result::Result<_, _>>()?;
        let ty = fb.byte(*t, 1, FLOAT32)?;
        let buf = fb.int(*t, 2, 0)? as u32 as usize;
        let name = fb.string(*t, 3)?;
        // Names and shapes are copied per tensor: bound them in all.
        meta = meta.saturating_add(name.len()).saturating_add(shape.len().saturating_mul(8));
        if meta > MAX_META {
            return Err("the model's tensor metadata is too large".into());
        }
        names.push(name);
        // Every size computed from shapes later stays in range, even a product of some of the
        // dimensions (a zero dimension does not let the others grow without bound).
        let count = shape.iter().try_fold(1usize, |a, &d| a.checked_mul(d)).ok_or("a tensor is too large")?;
        if shape.iter().try_fold(1usize, |a, &d| a.checked_mul(d.max(1))).is_none_or(|c| c > MAX_TENSOR) {
            return Err("a tensor is too large".into());
        }
        if fb.field(*t, 4)?.is_some() && fb.table(*t, 4)?.is_some_and(|q| fb.vector(q, 2).is_ok_and(|v| v.1 > 0)) {
            return Err(format!("{}: quantized tensors are not supported", names[i]));
        }
        if buf > 0 {
            let b = *buffers.get(buf).ok_or("bad buffer index")?;
            let (mut at, mut len) = fb.vector(b, 0)?;
            if len == 0 {
                // Large models keep data after the FlatBuffer: offset and size from its start.
                let (o, s) =
                    (fb.field(b, 1)?.map(|p| fb.u64(p)).transpose()?.unwrap_or(0), fb.field(b, 2)?.map(|p| fb.u64(p)).transpose()?.unwrap_or(0));
                (at, len) = (usize::try_from(o).map_err(|_| "bad buffer")?, usize::try_from(s).map_err(|_| "bad buffer")?);
            }
            if len > 0 {
                data.push((i, ty, count, bytes.get(at..at.checked_add(len).ok_or("bad buffer")?).ok_or("truncated model")?));
                // Several tensors may share one buffer: each is decoded on its own.
                consts = consts.checked_add(count).filter(|&c| c <= MAX_CONSTS).ok_or("the model's weights are too large")?;
            }
        }
        shapes.push(shape);
        sizes.push(count);
    }
    for (i, ty, count, data) in data {
        match ty {
            FLOAT32 if data.len() == 4 * count => floats[i] = Some(data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()),
            FLOAT16 if data.len() == 2 * count => {
                floats[i] = Some(data.as_chunks::<2>().0.iter().map(|c| half_to_f32(u16::from_le_bytes(*c))).collect())
            }
            INT32 if data.len() == 4 * count => ints[i] = Some(data.as_chunks::<4>().0.iter().map(|c| i32::from_le_bytes(*c)).collect()),
            _ => return Err(format!("{}: unsupported tensor data", names[i])),
        }
    }
    // Copies of constants made below (folded DEQUANTIZEs, weights laid out for the kernels).
    let consts = std::cell::Cell::new(consts);
    let spend = |values: usize| -> Res<()> {
        consts.set(consts.get().checked_add(values).filter(|&c| c <= MAX_CONSTS).ok_or("the model's weights are too large")?);
        Ok(())
    };
    let inputs = fb.ints(g, 1)?.into_iter().map(|i| index(i, n)).collect::<Res<Vec<_>>>()?;
    let outputs = fb.ints(g, 2)?.into_iter().map(|i| index(i, n)).collect::<Res<Vec<_>>>()?;
    let mut steps = vec![];
    for o in fb.tables(g, 3)? {
        let code = *codes.get(fb.int(o, 0, 0)? as u32 as usize).ok_or("bad operator code")?;
        // Optional inputs are -1.
        let ins: Vec<Option<usize>> = fb.ints(o, 1)?.into_iter().map(|i| if i < 0 { Ok(None) } else { index(i, n).map(Some) }).collect::<Res<_>>()?;
        let outs = fb.ints(o, 2)?;
        let output = index(*outs.first().ok_or("an operator has no output")?, n)?;
        // Float16 weights: fold their DEQUANTIZE.
        if code == op::DEQUANTIZE
            && let Some(i) = ins.first().copied().flatten()
            && let Some(len) = floats[i].as_ref().map(Vec::len)
        {
            // The folded constant is what the graph outputs and the workspace count: it must be
            // the declared size.
            if sizes[output] != len {
                return Err(format!("{}: DEQUANTIZE output size disagrees with its input", names[output]));
            }
            spend(len)?;
            floats[output] = floats[i].clone();
            continue;
        }
        let opts = fb.table(o, 4)?;
        let opt_int = |id: usize, default: i32| opts.map_or(Ok(default), |t| fb.int(t, id, default));
        let opt_byte = |id: usize, default: u8| opts.map_or(Ok(default), |t| fb.byte(t, id, default));
        let input = |k: usize| ins.get(k).copied().flatten().ok_or_else(|| format!("{}: missing input {k}", names[output]));
        let weights = |k: usize| -> Res<Vec<f32>> {
            let id = input(k)?;
            let w = floats[id].as_ref().ok_or_else(|| format!("{}: input {k} is not constant", names[output]))?;
            spend(w.len())?;
            Ok(w.clone())
        };
        let bias = |count: usize| -> Res<Vec<f32>> {
            spend(count)?;
            match ins.get(2).copied().flatten() {
                Some(id) => floats[id].clone().filter(|b| b.len() == count).ok_or_else(|| format!("{}: bad bias", names[output])),
                None => Ok(vec![0.0; count]),
            }
        };
        let in_shape = |k: usize| -> Res<Vec<usize>> { Ok(shapes[input(k)?].clone()) };
        let same_size = |k: usize| -> Res<()> {
            if sizes[input(k)?] == sizes[output] { Ok(()) } else { Err(format!("{}: the input and output sizes disagree", names[output])) }
        };
        let out_shape = shapes[output].clone();
        let stride = |w: usize, h: usize| -> Res<[usize; 2]> {
            let s = [opt_int(h, 1)?, opt_int(w, 1)?];
            if s.iter().any(|v| *v < 1) {
                return Err("bad stride".into());
            }
            Ok(s.map(|v| v as usize))
        };
        let mut runtime = vec![input(0)?];
        let op = match code {
            op::DEQUANTIZE => {
                same_size(0)?;
                Op::Reshape
            }
            op::CONV_2D | op::DEPTHWISE_CONV_2D => {
                let depthwise = code == op::DEPTHWISE_CONV_2D;
                let (padding, s) = (opt_byte(0, 0)?, stride(1, 2)?);
                let act_id = if depthwise { 4 } else { 3 };
                let dil = if depthwise { [opt_int(6, 1)?, opt_int(5, 1)?] } else { [opt_int(5, 1)?, opt_int(4, 1)?] };
                if dil != [1, 1] {
                    return Err(format!("{}: dilated convolutions are not supported", names[output]));
                }
                let act = Act::from(opt_byte(act_id, 0)?)?;
                let (x, f) = (in_shape(0)?, shapes[input(1)?].clone());
                let (&[1, h, w, cin], &[fo, kh, kw, fi], &[1, oh, ow, cout]) = (&x[..], &f[..], &out_shape[..]) else {
                    return Err(format!("{}: unexpected convolution shapes", names[output]));
                };
                if kh != kw || s[0] != s[1] {
                    return Err(format!("{}: only square kernels and strides are supported", names[output]));
                }
                if kh > MAX_KERNEL {
                    return Err(format!("{}: the convolution kernel is too large", names[output]));
                }
                let (ey, py) = window(padding, h, kh, s[0])?;
                let (ex, px) = window(padding, w, kw, s[1])?;
                if (ey, ex) != (oh, ow) {
                    return Err(format!("{}: output size disagrees with the padding", names[output]));
                }
                let wt = weights(1)?;
                let b = bias(cout)?;
                if depthwise {
                    if fo != 1 || fi != cout || cin != cout {
                        return Err(format!("{}: depth multipliers are not supported", names[output]));
                    }
                    Op::Depthwise(Depthwise { w: wt, b, c: cout, ks: kh, stride: s[0] }, [py, px], act)
                } else {
                    if fo != cout || fi != cin {
                        return Err(format!("{}: unexpected filter shape", names[output]));
                    }
                    // OHWI → [(ky·ks + kx)·inp + ci] × out.
                    let kk = kh * kw * cin;
                    spend(kk * cout)?;
                    let mut t = vec![0.0f32; kk * cout];
                    for o in 0..cout {
                        for p in 0..kk {
                            t[p * cout + o] = *wt.get(o * kk + p).ok_or("bad filter")?;
                        }
                    }
                    if kh == 1 && s[0] == 1 {
                        Op::Pointwise(Linear { wt: t, b, inp: cin, out: cout }, act)
                    } else {
                        Op::Conv(Conv { wt: t, b, inp: cin, out: cout, ks: kh, stride: s[0] }, [py, px], act)
                    }
                }
            }
            op::MAX_POOL_2D => {
                let (padding, s) = (opt_byte(0, 0)?, stride(1, 2)?);
                let k = [opt_int(4, 1)?, opt_int(3, 1)?];
                if k.iter().any(|v| *v < 1) {
                    return Err("bad pool size".into());
                }
                let k = k.map(|v| v as usize);
                if k.iter().any(|&v| v > MAX_KERNEL) {
                    return Err(format!("{}: the pooling window is too large", names[output]));
                }
                let x = in_shape(0)?;
                let (&[1, h, w, c], &[1, oh, ow, oc]) = (&x[..], &out_shape[..]) else { return Err("unexpected pool shapes".into()) };
                if c != oc {
                    return Err(format!("{}: pooling changes the channels", names[output]));
                }
                let (ey, py) = window(padding, h, k[0], s[0])?;
                let (ex, px) = window(padding, w, k[1], s[1])?;
                if (ey, ex) != (oh, ow) {
                    return Err(format!("{}: output size disagrees with the padding", names[output]));
                }
                Op::MaxPool { k, stride: s, pad: [py, px], act: Act::from(opt_byte(5, 0)?)? }
            }
            op::ADD => {
                runtime.push(input(1)?);
                same_size(0)?;
                same_size(1).map_err(|_| format!("{}: broadcasting ADD is not supported", names[output]))?;
                Op::Add(Act::from(opt_byte(0, 0)?)?)
            }
            op::PAD => {
                let p = ints[input(1)?].clone().ok_or("PAD: paddings must be constant")?;
                let pads = p
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| Ok([usize::try_from(c[0]).map_err(|_| "bad pad")?, usize::try_from(c[1]).map_err(|_| "bad pad")?]))
                    .collect::<Res<Vec<_>>>()?;
                // The padded shape is the declared one (so it is as bounded as any tensor).
                let x = in_shape(0)?;
                let padded = x.iter().zip(&pads).map(|(d, p)| d.checked_add(p[0])?.checked_add(p[1])).collect::<Option<Vec<_>>>();
                if pads.len() != x.len() || pads.len() > MAX_PAD_RANK || padded.as_ref() != Some(&out_shape) {
                    return Err(format!("{}: the padding disagrees with the output shape", names[output]));
                }
                Op::Pad(pads)
            }
            op::CONCATENATION => {
                runtime = (0..ins.len()).map(input).collect::<Res<_>>()?;
                let rank = out_shape.len() as i32;
                let a = opt_int(0, 0)?;
                let axis = index(if a < 0 { a.saturating_add(rank) } else { a }, out_shape.len())?;
                // The parts fit the output: same rank, same sizes but along `axis`, where they add up.
                let mut along = 0usize;
                for &id in &runtime {
                    let p = &shapes[id];
                    let fits = p.len() == out_shape.len() && p.iter().zip(&out_shape).enumerate().all(|(d, (a, b))| d == axis || a == b);
                    along = p
                        .get(axis)
                        .filter(|_| fits)
                        .and_then(|&d| along.checked_add(d))
                        .ok_or_else(|| format!("{}: the parts disagree with the output shape", names[output]))?;
                }
                if out_shape.get(axis) != Some(&along) {
                    return Err(format!("{}: the parts disagree with the output shape", names[output]));
                }
                Op::Concat { axis, act: Act::from(opt_byte(1, 0)?)? }
            }
            op::RESHAPE => {
                same_size(0)?;
                Op::Reshape
            }
            op::RELU => {
                same_size(0)?;
                Op::Act(Act::Relu)
            }
            op::RELU6 => {
                same_size(0)?;
                Op::Act(Act::Relu6)
            }
            op::LOGISTIC => {
                same_size(0)?;
                Op::Logistic
            }
            op::PRELU => {
                same_size(0)?;
                let alpha = weights(1)?;
                let c = *out_shape.last().ok_or("PRELU: bad shape")?;
                if alpha.len() != c || c == 0 {
                    return Err(format!("{}: only per-channel PReLU is supported", names[output]));
                }
                Op::Prelu(alpha)
            }
            c => return Err(format!("operator {c} is not supported")),
        };
        steps.push(Step { op, inputs: runtime, output });
    }
    let (slot, slot_len) = plan(&steps, &inputs, &outputs, &sizes, &floats)?;
    let model = Model { shapes, sizes, names, consts: floats, steps, inputs, outputs, slot, slot_len, scratch: 0 };
    let c = model.cost().ok_or("the model is too expensive to run")?;
    let over = [
        (c.kernel > MAX_KERNEL as u64, "a kernel is too large"),
        (c.scratch > MAX_SCRATCH, "an operator's working space is too large"),
        (c.macs > MAX_MACS, "the model needs too many multiply-adds"),
        (c.values > MAX_VALUES, "the model does too much work"),
        (c.live > MAX_LIVE, "the model needs too much memory"),
    ];
    if let Some((_, why)) = over.iter().find(|o| o.0) {
        return Err((*why).to_string());
    }
    let scratch = usize::try_from(c.scratch).map_err(|_| "an operator's working space is too large")?;
    Ok(Model { scratch, ..model })
}

/// Which workspace slot holds each tensor, and each slot's size. Graph inputs get a slot each;
/// each step's output takes a free slot (the smallest that fits, else the largest free one, made
/// larger) or a new one; a slot is freed after the last step that reads its tensor. Graph outputs
/// keep theirs. A step never writes the slot it reads from.
fn plan(steps: &[Step], inputs: &[usize], outputs: &[usize], sizes: &[usize], consts: &[Option<Vec<f32>>]) -> Res<(Vec<Option<usize>>, Vec<usize>)> {
    let n = sizes.len();
    let mut last_use = vec![None; n];
    for (k, s) in steps.iter().enumerate() {
        for &i in &s.inputs {
            last_use[i] = Some(k);
        }
    }
    let mut slot: Vec<Option<usize>> = vec![None; n];
    let mut len: Vec<usize> = vec![];
    let mut free: Vec<usize> = vec![];
    let take = |len: &mut Vec<usize>, free: &mut Vec<usize>, need: usize| -> usize {
        let fit = free.iter().enumerate().filter(|(_, s)| len[**s] >= need).min_by_key(|(_, s)| len[**s]).map(|(k, _)| k);
        let pick = fit.or_else(|| free.iter().enumerate().max_by_key(|(_, s)| len[**s]).map(|(k, _)| k));
        match pick {
            Some(k) => {
                let s = free.swap_remove(k);
                len[s] = len[s].max(need);
                s
            }
            None => {
                len.push(need);
                len.len() - 1
            }
        }
    };
    for &i in inputs {
        if slot[i].is_none() {
            slot[i] = Some(take(&mut len, &mut free, sizes[i]));
        }
    }
    for (k, s) in steps.iter().enumerate() {
        for &i in &s.inputs {
            if slot[i].is_none() && consts[i].is_none() {
                return Err("an operator reads a tensor before it is computed".into());
            }
        }
        if slot[s.output].is_some() || consts[s.output].is_some() {
            return Err("an operator writes a tensor that already has a value".into());
        }
        slot[s.output] = Some(take(&mut len, &mut free, sizes[s.output]));
        for &i in &s.inputs {
            if last_use[i] == Some(k)
                && !outputs.contains(&i)
                && let Some(si) = slot[i]
                && !free.contains(&si)
            {
                free.push(si);
            }
        }
    }
    for &o in outputs {
        if slot[o].is_none() && consts[o].is_none() {
            return Err("an output is never computed".into());
        }
    }
    Ok((slot, len))
}

/// Max pooling over `k` windows into `y`; window cells outside the input are ignored.
fn max_pool(x: &[f32], [h, w]: [usize; 2], c: usize, [oh, ow]: [usize; 2], k: [usize; 2], s: [usize; 2], pad: [usize; 2], y: &mut [f32]) {
    for oy in 0..oh {
        for ox in 0..ow {
            let Some(out) = y.get_mut((oy * ow + ox) * c..(oy * ow + ox + 1) * c) else { return };
            out.fill(f32::NEG_INFINITY);
            for ky in 0..k[0] {
                let Some(iy) = (oy * s[0] + ky).checked_sub(pad[0]).filter(|&v| v < h) else { continue };
                for kx in 0..k[1] {
                    let Some(ix) = (ox * s[1] + kx).checked_sub(pad[1]).filter(|&v| v < w) else { continue };
                    let Some(src) = x.get((iy * w + ix) * c..(iy * w + ix + 1) * c) else { continue };
                    for (o, v) in out.iter_mut().zip(src) {
                        *o = o.max(*v);
                    }
                }
            }
        }
    }
}

/// Zero padding of a row-major tensor of shape `shape` into `y` (sized for the padded shape).
fn pad(x: &[f32], shape: &[usize], pads: &[[usize; 2]], y: &mut [f32]) {
    y.fill(0.0);
    let rank = shape.len().min(pads.len());
    let Some(&inner) = shape.get(rank.wrapping_sub(1)) else { return };
    if inner == 0 {
        return;
    }
    // The padded shape.
    let mut out = [0usize; MAX_PAD_RANK];
    let mut idx = [0usize; MAX_PAD_RANK];
    if rank > out.len() {
        return;
    }
    for d in 0..rank {
        out[d] = shape[d] + pads[d][0] + pads[d][1];
    }
    for src in x.chunks_exact(inner) {
        // `idx` holds the row's position in the outer dimensions.
        let mut at = 0usize;
        for d in 0..rank {
            let i = if d + 1 == rank { pads[d][0] } else { idx[d] + pads[d][0] };
            at = at * out[d] + i;
        }
        if let Some(dst) = y.get_mut(at..at + inner) {
            dst.copy_from_slice(src);
        }
        for d in (0..rank - 1).rev() {
            idx[d] += 1;
            if idx[d] < shape[d] {
                break;
            }
            idx[d] = 0;
        }
    }
}

#[cfg(test)]
mod tests;
