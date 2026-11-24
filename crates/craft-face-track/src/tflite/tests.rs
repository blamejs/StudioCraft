use super::*;

#[test]
fn same_padding_matches_tensorflow() {
    // TensorFlow pads the extra row after: 128 → 64 with a 3×3 stride-2 window pads 0 before.
    assert_eq!(same(128, 3, 2), (64, 0));
    assert_eq!(same(128, 5, 2), (64, 1));
    assert_eq!(same(16, 3, 1), (16, 1));
    assert_eq!(same(7, 2, 2), (4, 0));
    assert_eq!(window(1, 4, 2, 2), Ok((2, 0)));
    assert!(window(1, 1, 2, 2).is_err());
}

#[test]
fn asymmetric_conv_padding() {
    // A 3×3 stride-2 "same" conv on 4×4: pads nothing before, one after.
    let pool = Pool::single();
    let x: Vec<f32> = (0..16).map(|v| v as f32).collect();
    let ones = vec![1.0f32; 9];
    let c = Conv { wt: ones.clone(), b: vec![0.0], inp: 1, out: 1, ks: 3, stride: 2 };
    let mut patch = vec![0.0; c.patch_len(2, 2).unwrap()];
    let mut y = vec![0.0; 4];
    c.forward(&x, [4, 4], [2, 2], [0, 0], &mut patch, &mut y, &pool);
    // The top-left window covers rows 0 to 2 and columns 0 to 2; the edge cuts the bottom-right one.
    assert_eq!(y, vec![45.0, 39.0, 66.0, 50.0]);
    let d = Depthwise { w: ones, b: vec![0.0], c: 1, ks: 3, stride: 2 };
    let mut yd = vec![0.0; 4];
    d.forward(&x, [4, 4], [2, 2], [0, 0], &mut yd, &pool);
    assert_eq!(yd, y);
}

#[test]
fn pool_and_pad() {
    let x: Vec<f32> = (0..16).map(|v| v as f32).collect();
    let mut y = vec![0.0; 4];
    max_pool(&x, [4, 4], 1, [2, 2], [2, 2], [2, 2], [0, 0], &mut y);
    assert_eq!(y, vec![5.0, 7.0, 13.0, 15.0]);
    // Same padding on 3×3 → 2×2: the last window is cut by the edge.
    let x3: Vec<f32> = (0..9).map(|v| v as f32).collect();
    max_pool(&x3, [3, 3], 1, [2, 2], [2, 2], [2, 2], [0, 0], &mut y);
    assert_eq!(y, vec![4.0, 5.0, 7.0, 8.0]);
    // Channel padding (residual connections widen the channels with zeros), into a buffer that
    // held other values.
    let mut p = vec![9.0; 6];
    pad(&[1.0, 2.0, 3.0, 4.0], &[1, 1, 2, 2], &[[0, 0], [0, 0], [0, 0], [0, 1]], &mut p);
    assert_eq!(p, vec![1.0, 2.0, 0.0, 3.0, 4.0, 0.0]);
    let mut p = vec![9.0; 3];
    pad(&[1.0, 2.0], &[1, 2], &[[0, 0], [1, 0]], &mut p);
    assert_eq!(p, vec![0.0, 1.0, 2.0]);
}

#[test]
fn damaged_files_are_errors_not_panics() {
    assert!(Model::read(b"").is_err());
    assert!(Model::read(&[0xff; 64]).is_err());
    // Every prefix and a few corruptions of a hand-made buffer.
    let mut fb = vec![12, 0, 0, 0, 0, 0, 6, 0, 8, 0, 4, 0, 6, 0, 0, 0, 0, 0, 0, 0];
    for n in 0..fb.len() {
        let _ = Model::read(&fb[..n]);
    }
    for i in 0..fb.len() {
        fb[i] ^= 0x5a;
        let _ = Model::read(&fb);
    }
    // Every prefix of a real (small) model.
    let m = conv_relu();
    for n in 0..m.len() {
        let _ = Model::read(&m[..n]);
    }
}

/// A FlatBuffer value, to build small models in tests.
pub(crate) enum V {
    Int(i32),
    Byte(u8),
    Table(Vec<(usize, V)>),
    Ints(Vec<i32>),
    Bytes(Vec<u8>),
    Tables(Vec<V>),
}

fn put_u32(b: &mut [u8], at: usize, v: usize) {
    b[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
}

/// Append `v` (children after their parent, so every offset points forward); where it starts.
fn put(b: &mut Vec<u8>, v: &V) -> usize {
    match v {
        V::Table(fields) => {
            let slots = fields.iter().map(|f| f.0 + 1).max().unwrap_or(0);
            let vt = b.len();
            let vt_len = 4 + 2 * slots;
            b.resize(vt + vt_len, 0);
            let t = b.len();
            // Every field gets 8 bytes (the reader does not need alignment).
            let t_len = 4 + 8 * fields.len();
            b.resize(t + t_len, 0);
            b[vt..vt + 2].copy_from_slice(&(vt_len as u16).to_le_bytes());
            b[vt + 2..vt + 4].copy_from_slice(&(t_len as u16).to_le_bytes());
            b[t..t + 4].copy_from_slice(&((t - vt) as i32).to_le_bytes());
            for (k, (id, f)) in fields.iter().enumerate() {
                let at = t + 4 + 8 * k;
                b[vt + 4 + 2 * id..vt + 6 + 2 * id].copy_from_slice(&((at - t) as u16).to_le_bytes());
                match f {
                    V::Int(i) => b[at..at + 4].copy_from_slice(&i.to_le_bytes()),
                    V::Byte(x) => b[at] = *x,
                    _ => {
                        let child = put(b, f);
                        put_u32(b, at, child - at);
                    }
                }
            }
            t
        }
        V::Ints(xs) => {
            let p = b.len();
            b.extend((xs.len() as u32).to_le_bytes());
            xs.iter().for_each(|x| b.extend(x.to_le_bytes()));
            p
        }
        V::Bytes(xs) => {
            let p = b.len();
            b.extend((xs.len() as u32).to_le_bytes());
            b.extend(xs);
            p
        }
        V::Tables(ts) => {
            let p = b.len();
            b.extend((ts.len() as u32).to_le_bytes());
            let slots = b.len();
            b.resize(slots + 4 * ts.len(), 0);
            for (i, t) in ts.iter().enumerate() {
                let child = put(b, t);
                put_u32(b, slots + 4 * i, child - (slots + 4 * i));
            }
            p
        }
        V::Int(_) | V::Byte(_) => b.len(),
    }
}

/// A tensor: shape, type and its constant data (`Some(k)`: buffer `k` of the model's).
pub(crate) type T = (Vec<i32>, u8, Option<usize>);
/// An operator: code, inputs, outputs and its options' fields.
pub(crate) type O = (i32, Vec<i32>, Vec<i32>, Vec<(usize, V)>);

/// A `.tflite` file with one graph.
pub(crate) fn model(bufs: Vec<Vec<u8>>, tensors: Vec<T>, ops: Vec<O>, inputs: Vec<i32>, outputs: Vec<i32>) -> Vec<u8> {
    let ts = tensors
        .into_iter()
        .enumerate()
        .map(|(i, (shape, ty, buf))| {
            let mut f = vec![(0, V::Ints(shape)), (1, V::Byte(ty)), (3, V::Bytes(format!("t{i}").into_bytes()))];
            if let Some(k) = buf {
                f.push((2, V::Int(k as i32 + 1)));
            }
            V::Table(f)
        })
        .collect();
    let mut codes = vec![];
    let os = ops
        .into_iter()
        .map(|(code, ins, outs, opts)| {
            let idx = codes.iter().position(|&c| c == code).unwrap_or_else(|| {
                codes.push(code);
                codes.len() - 1
            });
            V::Table(vec![(0, V::Int(idx as i32)), (1, V::Ints(ins)), (2, V::Ints(outs)), (4, V::Table(opts))])
        })
        .collect();
    let g = V::Table(vec![(0, V::Tables(ts)), (1, V::Ints(inputs)), (2, V::Ints(outputs)), (3, V::Tables(os))]);
    let buffers = std::iter::once(V::Table(vec![])).chain(bufs.into_iter().map(|d| V::Table(vec![(0, V::Bytes(d))]))).collect();
    let codes = codes.into_iter().map(|c| V::Table(vec![(3, V::Int(c))])).collect();
    let root = V::Table(vec![(1, V::Tables(codes)), (2, V::Tables(vec![g])), (4, V::Tables(buffers))]);
    let mut b = vec![0u8; 4];
    let r = put(&mut b, &root);
    put_u32(&mut b, 0, r);
    b
}

fn f32s(xs: &[f32]) -> Vec<u8> {
    xs.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Conv2DOptions / Pool2DOptions: padding (0 same, 1 valid) and stride.
fn window_opts(padding: u8, stride: i32) -> Vec<(usize, V)> {
    vec![(0, V::Byte(padding)), (1, V::Int(stride)), (2, V::Int(stride))]
}

fn refused(bytes: &[u8], why: &str) {
    let e = Model::read(bytes).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(e.contains(why), "refused for `{e}`, expected `{why}`");
}

/// A 3×3 stride-2 "same" conv on 4×4, then a RELU: as `asymmetric_conv_padding`.
fn conv_relu() -> Vec<u8> {
    model(
        vec![f32s(&[1.0; 9])],
        vec![
            (vec![1, 4, 4, 1], FLOAT32, None),
            (vec![1, 3, 3, 1], FLOAT32, Some(0)),
            (vec![1, 2, 2, 1], FLOAT32, None),
            (vec![1, 2, 2, 1], FLOAT32, None),
        ],
        vec![(op::CONV_2D, vec![0, 1, -1], vec![2], window_opts(0, 2)), (op::RELU, vec![2], vec![3], vec![])],
        vec![0],
        vec![3],
    )
}

#[test]
fn built_models_load_and_run() {
    let m = Model::read(&conv_relu()).unwrap();
    let c = m.cost().unwrap();
    assert_eq!((c.macs, c.values, c.scratch, c.kernel), (2 * 2 * 9, 8, 2 * 2 * 9, 3));
    // The input's slot is free once the conv has read it, so the RELU writes into it: two slots
    // (16 and 4 floats) and the conv's patch matrix.
    assert_eq!(m.slot_len, vec![16, 4]);
    assert_eq!(c.live, 16 + 4 + 2 * 2 * 9);
    assert_eq!(m.workspace().len(), m.workspace_len());
    let x: Vec<f32> = (0..16).map(|v| v as f32).collect();
    assert_eq!(m.run(&[&x]).unwrap(), vec![vec![45.0, 39.0, 66.0, 50.0]]);
    // The same workspace again, on several threads, with other inputs: the same results as a
    // fresh run.
    let pool = Pool::new(2).unwrap();
    let mut ws = m.workspace();
    for k in 0..3 {
        let x: Vec<f32> = (0..16).map(|v| (v * (k + 1)) as f32 - 20.0).collect();
        m.run_into(&mut ws, &[&x], &pool).unwrap();
        assert_eq!(m.output_values(&ws, 0).unwrap(), m.run(&[&x]).unwrap()[0].as_slice());
    }
}

#[test]
fn slots_are_reused_along_a_chain() {
    // 20 RELUs in a row: each output can take the slot its input's input held.
    let n = 20;
    let m = model(
        vec![],
        (0..=n).map(|_| (vec![1, 64], FLOAT32, None)).collect(),
        (0..n).map(|i| (op::RELU, vec![i], vec![i + 1], vec![])).collect(),
        vec![0],
        vec![n],
    );
    let m = Model::read(&m).unwrap();
    assert_eq!(m.slot_len, vec![64, 64]);
    let x: Vec<f32> = (0..64).map(|v| v as f32 - 32.0).collect();
    let want: Vec<f32> = x.iter().map(|v| v.max(0.0)).collect();
    assert_eq!(m.run(&[&x]).unwrap(), vec![want]);
}

#[test]
fn concatenation_and_add_run_from_their_slots() {
    // y = concat(relu(x), x) + concat(x, x).
    let m = model(
        vec![],
        vec![
            (vec![1, 2], FLOAT32, None),
            (vec![1, 2], FLOAT32, None),
            (vec![1, 4], FLOAT32, None),
            (vec![1, 4], FLOAT32, None),
            (vec![1, 4], FLOAT32, None),
        ],
        vec![
            (op::RELU, vec![0], vec![1], vec![]),
            (op::CONCATENATION, vec![1, 0], vec![2], vec![(0, V::Int(-1))]),
            (op::CONCATENATION, vec![0, 0], vec![3], vec![(0, V::Int(1))]),
            (op::ADD, vec![2, 3], vec![4], vec![]),
        ],
        vec![0],
        vec![4],
    );
    let m = Model::read(&m).unwrap();
    assert_eq!(m.run(&[&[-1.0, 2.0]]).unwrap(), vec![vec![-1.0, 4.0, -2.0, 4.0]]);
}

#[test]
fn graphs_that_read_before_writing_or_write_twice_are_refused() {
    let t = || (vec![1, 2], FLOAT32, None);
    let early =
        model(vec![], vec![t(), t(), t()], vec![(op::RELU, vec![1], vec![2], vec![]), (op::RELU, vec![0], vec![1], vec![])], vec![0], vec![2]);
    refused(&early, "before it is computed");
    let twice = model(vec![], vec![t(), t()], vec![(op::RELU, vec![0], vec![1], vec![]), (op::RELU, vec![0], vec![1], vec![])], vec![0], vec![1]);
    refused(&twice, "already has a value");
    let in_place = model(vec![], vec![t()], vec![(op::RELU, vec![0], vec![0], vec![])], vec![0], vec![0]);
    refused(&in_place, "already has a value");
    let missing = model(vec![], vec![t(), t()], vec![], vec![0], vec![1]);
    refused(&missing, "never computed");
    let resize = model(vec![], vec![t(), (vec![1, 3], FLOAT32, None)], vec![(op::RELU, vec![0], vec![1], vec![])], vec![0], vec![1]);
    refused(&resize, "sizes disagree");
    let broadcast = model(vec![], vec![t(), (vec![1, 1], FLOAT32, None), t()], vec![(op::ADD, vec![0, 1], vec![2], vec![])], vec![0, 1], vec![2]);
    refused(&broadcast, "broadcasting ADD");
}

#[test]
fn a_workspace_of_another_model_or_wrong_inputs_are_errors() {
    let a = Model::read(&conv_relu()).unwrap();
    let b = Model::read(&model(
        vec![],
        vec![(vec![1, 3], FLOAT32, None), (vec![1, 3], FLOAT32, None)],
        vec![(op::RELU, vec![0], vec![1], vec![])],
        vec![0],
        vec![1],
    ))
    .unwrap();
    let mut ws = b.workspace();
    let x = [0.0f32; 16];
    assert!(a.run_into(&mut ws, &[&x], &Pool::single()).is_err());
    let mut ws = a.workspace();
    assert!(a.run_into(&mut ws, &[&x[..15]], &Pool::single()).is_err());
    assert!(a.run_into(&mut ws, &[], &Pool::single()).is_err());
    assert!(a.run_into(&mut ws, &[&x], &Pool::single()).is_ok());
}

/// An 8 KB model whose CONV_2D needs a huge patch matrix is refused at load.
#[test]
fn convolution_scratch_is_bounded() {
    let w = 1 << 25;
    let bomb = model(
        vec![vec![0; 2 * 64 * 64]],
        vec![(vec![1, 1, w, 1], FLOAT32, None), (vec![1, 64, 64, 1], FLOAT16, Some(0)), (vec![1, 1, w, 1], FLOAT32, None)],
        vec![(op::CONV_2D, vec![0, 1, -1], vec![2], window_opts(0, 1))],
        vec![0],
        vec![2],
    );
    refused(&bomb, "kernel is too large");
    // A small kernel over many channels: the patch matrix is `oh·ow·3·3·cin`.
    let (w, cin) = (1 << 20, 256);
    let wide = model(
        vec![vec![0; 2 * 9 * 256]],
        vec![(vec![1, 1, w, cin], FLOAT32, None), (vec![1, 3, 3, cin], FLOAT16, Some(0)), (vec![1, 1, w, 1], FLOAT32, None)],
        vec![(op::CONV_2D, vec![0, 1, -1], vec![2], window_opts(0, 1))],
        vec![0],
        vec![2],
    );
    refused(&wide, "working space is too large");
}

#[test]
fn kernel_sides_are_bounded() {
    let k = MAX_KERNEL as i32 + 1;
    let conv = model(
        vec![vec![0; 2 * (k * k) as usize]],
        vec![(vec![1, 40, 40, 1], FLOAT32, None), (vec![1, k, k, 1], FLOAT16, Some(0)), (vec![1, 40, 40, 1], FLOAT32, None)],
        vec![(op::CONV_2D, vec![0, 1, -1], vec![2], window_opts(0, 1))],
        vec![0],
        vec![2],
    );
    refused(&conv, "kernel is too large");
    let depthwise = model(
        vec![vec![0; 2 * (k * k) as usize]],
        vec![(vec![1, 40, 40, 1], FLOAT32, None), (vec![1, k, k, 1], FLOAT16, Some(0)), (vec![1, 40, 40, 1], FLOAT32, None)],
        vec![(op::DEPTHWISE_CONV_2D, vec![0, 1, -1], vec![2], window_opts(0, 1))],
        vec![0],
        vec![2],
    );
    refused(&depthwise, "kernel is too large");
    // A "same" pool takes any window: a huge one would loop for billions of steps per output.
    let mut opts = window_opts(0, 1);
    opts.extend([(3, V::Int(i32::MAX)), (4, V::Int(i32::MAX))]);
    let pool = model(
        vec![],
        vec![(vec![1, 8, 8, 1], FLOAT32, None), (vec![1, 8, 8, 1], FLOAT32, None)],
        vec![(op::MAX_POOL_2D, vec![0], vec![1], opts)],
        vec![0],
        vec![1],
    );
    refused(&pool, "pooling window is too large");
}

#[test]
fn multiply_adds_are_bounded() {
    // Three 16 → 16 pointwise convolutions over 4096×4096: 3·2³² multiply-adds.
    let s = vec![1, 4096, 4096, 16];
    let t = |buf| (s.clone(), FLOAT32, buf);
    let m = model(
        vec![vec![0; 2 * 16 * 16]],
        vec![t(None), (vec![16, 1, 1, 16], FLOAT16, Some(0)), t(None), t(None), t(None)],
        (0..3).map(|i| (op::CONV_2D, vec![if i == 0 { 0 } else { i + 1 }, 1, -1], vec![i + 2], window_opts(0, 1))).collect(),
        vec![0],
        vec![4],
    );
    refused(&m, "too many multiply-adds");
}

/// 2000 RELUs over 2²⁵ values would load and keep one run busy for minutes.
#[test]
fn elementwise_work_is_bounded() {
    let n = 2000;
    let m = model(
        vec![],
        (0..=n).map(|_| (vec![1, 1 << 25], FLOAT32, None)).collect(),
        (0..n).map(|i| (op::RELU, vec![i], vec![i + 1], vec![])).collect(),
        vec![0],
        vec![n],
    );
    refused(&m, "too much work");
}

#[test]
fn memory_of_a_run_is_bounded() {
    // Five outputs of 2²⁷ values each, all kept, besides the input.
    let m = model(
        vec![],
        (0..6).map(|_| (vec![1, 1 << 27], FLOAT32, None)).collect(),
        (1..6).map(|i| (op::RELU, vec![0], vec![i], vec![])).collect(),
        vec![0],
        (1..6).collect(),
    );
    refused(&m, "too much memory");
}

#[test]
fn shapes_computed_at_run_time_must_match_the_declared_ones() {
    // PAD to 2³⁰ × 2³⁰ while declaring a 1×1 output.
    let pads = vec![0, 1 << 30, 0, 1 << 30].into_iter().flat_map(i32::to_le_bytes).collect();
    let pad = model(
        vec![pads],
        vec![(vec![1, 1], FLOAT32, None), (vec![2, 2], INT32, Some(0)), (vec![1, 1], FLOAT32, None)],
        vec![(op::PAD, vec![0, 1], vec![2], vec![])],
        vec![0],
        vec![2],
    );
    refused(&pad, "padding disagrees");
    // Many large parts concatenated into a small declared output.
    let concat = model(
        vec![],
        vec![(vec![1, 1 << 27], FLOAT32, None), (vec![1, 2], FLOAT32, None)],
        vec![(op::CONCATENATION, vec![0; 64], vec![1], vec![(0, V::Int(1))])],
        vec![0],
        vec![1],
    );
    refused(&concat, "parts disagree");
    // A pool whose output claims more channels than its input has.
    let pool = model(
        vec![],
        vec![(vec![1, 2, 2, 1], FLOAT32, None), (vec![1, 1, 1, 1 << 20], FLOAT32, None)],
        vec![(op::MAX_POOL_2D, vec![0], vec![1], window_opts(1, 2).into_iter().chain([(3, V::Int(2)), (4, V::Int(2))]).collect())],
        vec![0],
        vec![1],
    );
    refused(&pool, "changes the channels");
    // Dimensions may not hide behind a zero.
    let zero = model(vec![], vec![(vec![0, 1 << 30, 1 << 30], FLOAT32, None)], vec![], vec![0], vec![0]);
    refused(&zero, "tensor is too large");
}

#[test]
fn a_folded_dequantize_has_its_declared_size() {
    let half = |n: usize| vec![0u8; 2 * n];
    let fold = |declared: Vec<i32>| {
        model(
            vec![half(4)],
            vec![(vec![1, 4], FLOAT16, Some(0)), (declared, FLOAT32, None)],
            vec![(op::DEQUANTIZE, vec![0], vec![1], vec![])],
            vec![],
            vec![1],
        )
    };
    // A declared output of one value would let the folded four-value constant be copied out per reference.
    refused(&fold(vec![1, 1]), "DEQUANTIZE output size");
    assert!(Model::read(&fold(vec![1, 4])).is_ok());
}

#[test]
fn tensor_metadata_is_bounded_in_all() {
    // A rank of 4 million dimensions of 1 is a valid shape by size, but copied per tensor.
    let m = model(vec![], vec![(vec![1; 1 << 22], FLOAT32, None)], vec![], vec![], vec![]);
    refused(&m, "metadata is too large");
    let ok = model(vec![], (0..70).map(|_| (vec![1, 2, 3], FLOAT32, None)).collect(), vec![], vec![], vec![]);
    assert!(Model::read(&ok).is_ok());
}

#[test]
fn shared_weights_count_once_per_use() {
    // One 128 KB buffer decoded for 2100 tensors would be 2100 copies of 2¹⁶ floats.
    let m = model(vec![vec![0; 2 << 16]], (0..2100).map(|_| (vec![1, 1 << 16], FLOAT16, Some(0))).collect(), vec![], vec![], vec![]);
    refused(&m, "weights are too large");
}

/// A tiny float tensor.
fn tiny() -> T {
    (vec![1, 1], FLOAT32, None)
}

#[test]
fn tensor_counts_are_bounded() {
    let m = model(vec![], (0..=MAX_TENSORS).map(|_| tiny()).collect(), vec![], vec![], vec![]);
    refused(&m, "too many tensors");
}

#[test]
fn operator_counts_are_bounded() {
    // A chain of RELUs over one value each: every step is cheap, but there are too many.
    let n = MAX_OPERATORS as i32 + 1;
    let m = model(vec![], (0..=n).map(|_| tiny()).collect(), (0..n).map(|i| (op::RELU, vec![i], vec![i + 1], vec![])).collect(), vec![0], vec![n]);
    refused(&m, "too many operators");
}

#[test]
fn operator_code_lists_are_bounded() {
    let n = MAX_CODES as i32 + 1;
    let m = model(vec![], vec![tiny(), tiny()], (0..n).map(|c| (1000 + c, vec![0], vec![1], vec![])).collect(), vec![0], vec![1]);
    refused(&m, "too many operator codes");
}

#[test]
fn buffer_counts_are_bounded() {
    let m = model(vec![vec![]; MAX_BUFFERS], vec![tiny()], vec![], vec![0], vec![0]);
    refused(&m, "too many buffers");
}

#[test]
fn graph_inputs_and_outputs_are_bounded() {
    let n = MAX_GRAPH_IO as i32 + 1;
    let ins = model(vec![], (0..n).map(|_| tiny()).collect(), vec![], (0..n).collect(), vec![0]);
    refused(&ins, "too many graph inputs");
    let outs = model(vec![], (0..n).map(|_| tiny()).collect(), vec![], (0..n).collect::<Vec<_>>()[..1].to_vec(), (0..n).collect());
    refused(&outs, "too many graph outputs");
}

#[test]
fn operators_take_their_exact_input_and_output_counts() {
    // Extra inputs would be ignored by the kernel but still planned and kept alive.
    let extra = model(vec![], vec![tiny(), tiny()], vec![(op::RELU, vec![0; 2000], vec![1], vec![])], vec![0], vec![1]);
    refused(&extra, "inputs, not 2000");
    let add = model(vec![], vec![tiny(), tiny()], vec![(op::ADD, vec![0], vec![1], vec![])], vec![0], vec![1]);
    refused(&add, "inputs, not 1");
    let two_out = model(vec![], vec![tiny(), tiny(), tiny()], vec![(op::RELU, vec![0], vec![1, 2], vec![])], vec![0], vec![1]);
    refused(&two_out, "too many outputs of one operator");
    // Zero-size parts pass the shape check in any number.
    let parts = (MAX_CONCAT_INPUTS + 1) as i32;
    let concat = model(
        vec![],
        vec![(vec![1, 0], FLOAT32, None), (vec![1, 0], FLOAT32, None)],
        vec![(op::CONCATENATION, vec![0; parts as usize], vec![1], vec![(0, V::Int(1))])],
        vec![0],
        vec![1],
    );
    refused(&concat, &format!("inputs, not {parts}"));
}

/// A pool over zero channels writes nothing but still walks every window.
#[test]
fn zero_channel_pools_count_their_window_steps() {
    let s = 1 << 14;
    let mut opts = window_opts(0, 1);
    opts.extend([(3, V::Int(32)), (4, V::Int(32))]);
    let m = model(
        vec![],
        vec![(vec![1, s, s, 0], FLOAT32, None), (vec![1, s, s, 0], FLOAT32, None)],
        vec![(op::MAX_POOL_2D, vec![0], vec![1], opts)],
        vec![0],
        vec![1],
    );
    refused(&m, "too many multiply-adds");
}

/// Zero-size parts copy nothing, but the copy loop still runs once per part and outer row.
#[test]
fn zero_size_concatenations_count_their_copies() {
    let rows = 1 << 27;
    let m = model(
        vec![],
        vec![(vec![rows, 0], FLOAT32, None), (vec![rows, 0], FLOAT32, None)],
        vec![(op::CONCATENATION, vec![0; MAX_CONCAT_INPUTS], vec![1], vec![(0, V::Int(1))])],
        vec![0],
        vec![1],
    );
    refused(&m, "too much work");
}

#[test]
fn free_slots_are_reused_or_grown() {
    // Inputs take slots 0 (8) and 1 (2); the RELUs take new slots 2 (8) and 3 (2). Slot 1 is
    // freed after its last read and grown to 10 for the concatenation; slot 2 is then free and
    // fits the next RELU; the last RELU finds only slot 3 free and grows it to 8.
    let m = model(
        vec![],
        vec![
            (vec![1, 8], FLOAT32, None),
            (vec![1, 2], FLOAT32, None),
            (vec![1, 8], FLOAT32, None),
            (vec![1, 2], FLOAT32, None),
            (vec![1, 10], FLOAT32, None),
            (vec![1, 2], FLOAT32, None),
            (vec![1, 8], FLOAT32, None),
        ],
        vec![
            (op::RELU, vec![0], vec![2], vec![]),
            (op::RELU, vec![1], vec![3], vec![]),
            (op::CONCATENATION, vec![2, 3], vec![4], vec![(0, V::Int(1))]),
            (op::RELU, vec![3], vec![5], vec![]),
            (op::RELU, vec![0], vec![6], vec![]),
        ],
        vec![0, 1],
        vec![4, 5, 6],
    );
    let m = Model::read(&m).unwrap();
    assert_eq!(m.slot_len, vec![8, 10, 8, 8]);
    let x: Vec<f32> = (0..8).map(|v| v as f32 - 4.0).collect();
    let out = m.run(&[&x, &[-1.0, 1.0]]).unwrap();
    let relu: Vec<f32> = x.iter().map(|v| v.max(0.0)).collect();
    assert_eq!(out[0], [relu.as_slice(), &[0.0, 1.0]].concat());
    assert_eq!(out[1], vec![0.0, 1.0]);
    assert_eq!(out[2], x.iter().map(|v| v.max(0.0)).collect::<Vec<_>>());
}

/// Two models with the same buffer layout do not share workspaces, and a workspace gives no
/// output until a run on it finishes.
#[test]
fn workspaces_belong_to_one_model() {
    let one = |code| {
        Model::read(&model(
            vec![],
            vec![(vec![1, 3], FLOAT32, None), (vec![1, 3], FLOAT32, None)],
            vec![(code, vec![0], vec![1], vec![])],
            vec![0],
            vec![1],
        ))
        .unwrap()
    };
    let (relu, logistic) = (one(op::RELU), one(op::LOGISTIC));
    assert_eq!(relu.slot_len, logistic.slot_len);
    let pool = Pool::single();
    let mut ws = relu.workspace();
    assert!(relu.output_values(&ws, 0).is_none(), "no run yet");
    relu.run_into(&mut ws, &[&[-1.0, 0.0, 2.0]], &pool).unwrap();
    assert_eq!(relu.output_values(&ws, 0).unwrap(), &[0.0, 0.0, 2.0]);
    assert!(logistic.output_values(&ws, 0).is_none());
    assert!(logistic.run_into(&mut ws, &[&[0.0; 3]], &pool).is_err());
    // A clone is the same model.
    assert_eq!(relu.clone().output_values(&ws, 0).unwrap(), &[0.0, 0.0, 2.0]);
    // A run that fails leaves no output to read.
    assert!(relu.run_into(&mut ws, &[&[0.0; 2]], &pool).is_err());
    assert!(relu.output_values(&ws, 0).is_none());
    assert!(relu.run_into(&mut Workspace::default(), &[&[0.0; 3]], &pool).is_err());
}

/// A folded DEQUANTIZE is checked in graph order: no operator reads its output earlier, and
/// nothing else writes it.
#[test]
fn folded_dequantizes_keep_graph_order() {
    // 1.0 and -2.0 as half floats.
    let half = vec![0x00, 0x3c, 0x00, 0xc0];
    let t = |buf| (vec![1, 2], if buf == Some(0) { FLOAT16 } else { FLOAT32 }, buf);
    let early = model(
        vec![half.clone()],
        vec![t(Some(0)), t(None), t(None)],
        vec![(op::RELU, vec![1], vec![2], vec![]), (op::DEQUANTIZE, vec![0], vec![1], vec![])],
        vec![],
        vec![2],
    );
    refused(&early, "before it is computed");
    let twice = model(
        vec![half.clone()],
        vec![t(Some(0)), t(None)],
        vec![(op::DEQUANTIZE, vec![0], vec![1], vec![]), (op::DEQUANTIZE, vec![0], vec![1], vec![])],
        vec![],
        vec![1],
    );
    refused(&twice, "already has a value");
    let over_step = model(
        vec![half.clone()],
        vec![t(Some(0)), t(None), t(None)],
        vec![(op::RELU, vec![2], vec![1], vec![]), (op::DEQUANTIZE, vec![0], vec![1], vec![])],
        vec![2],
        vec![1],
    );
    refused(&over_step, "already has a value");
    let ok = model(
        vec![half],
        vec![t(Some(0)), t(None), t(None)],
        vec![(op::DEQUANTIZE, vec![0], vec![1], vec![]), (op::RELU, vec![1], vec![2], vec![])],
        vec![],
        vec![2],
    );
    assert_eq!(Model::read(&ok).unwrap().run(&[]).unwrap(), vec![vec![1.0, 0.0]]);
}

/// A second write of a tensor is refused before the operator's input count and shapes are
/// checked, which walk every dimension of the output.
#[test]
fn repeated_writes_are_refused_before_other_checks() {
    let t = |shape: Vec<i32>| (shape, FLOAT32, None);
    let concat = |parts: Vec<i32>| (op::CONCATENATION, parts, vec![1], vec![(0, V::Int(-1))]);
    // The second concatenation's parts do not fit the output either.
    let shapes = model(vec![], vec![t(vec![1, 1]), t(vec![1, 2]), t(vec![1, 3])], vec![concat(vec![0, 0]), concat(vec![2, 2])], vec![0, 2], vec![1]);
    refused(&shapes, "already has a value");
    let count = model(
        vec![],
        vec![t(vec![1, 2]), t(vec![1, 2])],
        vec![(op::RELU, vec![0], vec![1], vec![]), (op::RELU, vec![0; 2000], vec![1], vec![])],
        vec![0],
        vec![1],
    );
    refused(&count, "already has a value");
}

/// Data stored with a graph input is not a constant: the caller's values are used.
#[test]
fn graph_inputs_are_not_folded_or_taken_as_constants() {
    // 1.0 and -2.0 as half floats.
    let half = vec![0x00, 0x3c, 0x00, 0xc0];
    let fold = model(
        vec![half.clone()],
        vec![(vec![1, 2], FLOAT16, Some(0)), (vec![1, 2], FLOAT32, None)],
        vec![(op::DEQUANTIZE, vec![0], vec![1], vec![])],
        vec![0],
        vec![1],
    );
    assert_eq!(Model::read(&fold).unwrap().run(&[&[3.0, 4.0]]).unwrap(), vec![vec![3.0, 4.0]]);
    let alpha = model(
        vec![f32s(&[0.5, 0.5])],
        vec![(vec![1, 2], FLOAT32, None), (vec![2], FLOAT32, Some(0)), (vec![1, 2], FLOAT32, None)],
        vec![(op::PRELU, vec![0, 1], vec![2], vec![])],
        vec![0, 1],
        vec![2],
    );
    refused(&alpha, "is not constant");
}

/// A tensor's metadata is bounded by the lengths its vectors declare, before its dimensions are
/// read.
#[test]
fn tensor_metadata_is_bounded_before_it_is_read() {
    // Four million dynamic dimensions: refused for their number, before any is converted.
    let m = model(vec![], vec![(vec![-1; 1 << 22], FLOAT32, None)], vec![], vec![], vec![]);
    refused(&m, "metadata is too large");
}

/// ADD takes inputs of the output's shape only, not inputs whose sizes happen to match.
#[test]
fn add_inputs_have_the_output_shape() {
    let m = model(
        vec![],
        vec![(vec![2, 1], FLOAT32, None), (vec![1, 2], FLOAT32, None), (vec![2, 1], FLOAT32, None)],
        vec![(op::ADD, vec![0, 1], vec![2], vec![])],
        vec![0, 1],
        vec![2],
    );
    refused(&m, "broadcasting ADD");
}

/// PAD takes exactly two values per input dimension, and a scalar pads to itself.
#[test]
fn pad_values_match_the_input_rank() {
    let pads = |v: Vec<i32>| v.into_iter().flat_map(i32::to_le_bytes).collect();
    let extra = model(
        vec![pads(vec![0, 0, 7])],
        vec![(vec![1], FLOAT32, None), (vec![3], INT32, Some(0)), (vec![1], FLOAT32, None)],
        vec![(op::PAD, vec![0, 1], vec![2], vec![])],
        vec![0],
        vec![2],
    );
    refused(&extra, "padding disagrees");
    let ok = model(
        vec![pads(vec![1, 0])],
        vec![(vec![1], FLOAT32, None), (vec![1, 2], INT32, Some(0)), (vec![2], FLOAT32, None)],
        vec![(op::PAD, vec![0, 1], vec![2], vec![])],
        vec![0],
        vec![2],
    );
    assert_eq!(Model::read(&ok).unwrap().run(&[&[3.0]]).unwrap(), vec![vec![0.0, 3.0]]);
    let mut y = [0.0];
    pad(&[7.0], &[], &[], &mut y);
    assert_eq!(y, [7.0]);
}

/// A pooling stride and padding near `usize::MAX` skip the window cells outside the input.
#[test]
fn huge_pool_strides_skip_cells_outside_the_input() {
    let mut y = [0.0; 2];
    max_pool(&[5.0], [1, 1], 1, [1, 2], [2, 2], [usize::MAX; 2], [0, usize::MAX], &mut y);
    assert_eq!(y, [f32::NEG_INFINITY, 5.0]);
}

#[test]
fn half_floats_decode() {
    assert_eq!(half_to_f32(0x3c00), 1.0);
    assert_eq!(half_to_f32(0xc000), -2.0);
    assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
    assert_eq!(half_to_f32(0x7c00), f32::INFINITY);
    assert!(half_to_f32(0x7e00).is_nan());
    assert_eq!(half_to_f32(0x8000).to_bits(), (-0.0f32).to_bits());
}
