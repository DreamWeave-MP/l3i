//! Contract tests. The chunked kernels are compared with an independent scalar evaluator written
//! here and with the per-element reference interpreter, on both kernel instantiations.
#![allow(clippy::many_single_char_names, clippy::cast_precision_loss, clippy::float_cmp)]
use crate::*;
use std::sync::Arc;

fn domain() -> Arc<Domain> {
    Domain::new(4096, 128)
}
fn bytes_of(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn tensor(values: &[f32], shape: Shape, budget: &Arc<Budget>) -> Tensor {
    Tensor::import(&bytes_of(values), shape, budget).unwrap()
}
fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}
fn read(t: &Tensor) -> Vec<f32> {
    floats(&t.readback().unwrap())
}
/// Bitwise equal, except that any NaN equals any NaN: payload bits are not promised.
fn same(a: f32, b: f32) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}
fn assert_same(actual: &[f32], expected: &[f32], context: &str) {
    assert_eq!(actual.len(), expected.len(), "{context}");
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert!(same(*a, *e), "{context}: element {i}: {a:?} ({:#x}) vs {e:?} ({:#x})", a.to_bits(), e.to_bits());
    }
}
fn isas() -> Vec<Isa> {
    if Isa::detect() == Isa::Baseline { vec![Isa::Baseline] } else { vec![Isa::Baseline, Isa::Avx2] }
}
#[derive(Debug)]
struct Bytes(Vec<u8>);
impl Storage for Bytes {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Deterministic generator; every failure is reproducible from the seed in its message.
struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
    /// Values that exercise every numerical edge the contract names.
    fn value(&mut self) -> f32 {
        match self.below(16) {
            0 => 0.0,
            1 => -0.0,
            2 => f32::NAN,
            3 => f32::INFINITY,
            4 => f32::NEG_INFINITY,
            5 => f32::MIN_POSITIVE / 8.0,
            6 => -f32::MIN_POSITIVE / 3.0,
            7 => f32::MAX,
            8 => 1.000_000_1,
            9 => 0.999_999_9,
            _ => f32::from_bits(u32::try_from(self.next() & 0xffff_ffff).unwrap()),
        }
    }
}

/// An independent model of a random elementwise DAG: evaluates one output element directly.
#[derive(Clone, Debug)]
enum Model {
    Slot(usize),
    Constant(f32),
    Add(usize, usize),
    Mul(usize, usize),
    Relu(usize),
}
fn evaluate(model: &[Model], inputs: &[(Vec<f32>, (usize, usize))], row: usize, col: usize) -> f32 {
    let mut values = Vec::with_capacity(model.len());
    for node in model {
        let value = match *node {
            Model::Slot(s) => {
                let (data, (rows, cols)) = &inputs[s];
                data[(if *rows == 1 { 0 } else { row }) * cols + if *cols == 1 { 0 } else { col }]
            }
            Model::Constant(v) => v,
            Model::Add(a, b) => {
                let x: f32 = values[a];
                x + values[b]
            }
            Model::Mul(a, b) => {
                let x: f32 = values[a];
                x * values[b]
            }
            Model::Relu(a) => {
                let x: f32 = values[a];
                if x <= 0.0 { 0.0 } else { x }
            }
        };
        values.push(value);
    }
    *values.last().unwrap()
}

/// Random graphs over random shapes and storage layouts: chunked kernels, for each instantiation
/// and several chunk sizes, must agree with both the model and the reference interpreter.
#[test]
#[allow(clippy::too_many_lines)]
fn randomized_graphs_views_and_broadcasts_match_both_oracles() {
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let budget = Budget::new(64 * 1024 * 1024);
    let domain = domain();
    for case in 0..400 {
        let large = case % 25 == 0;
        let rows = if large { 1 + random.below(40) } else { random.below(6) };
        let cols = if large { 700 + random.below(1400) } else { random.below(7) };
        // Inputs: full shape, row broadcast, column broadcast or single element; laid out with
        // random padding, a random byte offset and random strides, in storage we own.
        let slot_count = 1 + random.below(4);
        let mut inputs = Vec::new();
        let mut tensors = Vec::new();
        for _ in 0..slot_count {
            let shape_rows = if random.below(3) == 0 { 1 } else { rows };
            let shape_cols = if random.below(3) == 0 { 1 } else { cols };
            let shape = Shape::Matrix(shape_rows, shape_cols);
            // Strides from zero (a repeat) through overlapping to padded, at any byte offset.
            let col_stride = if random.below(2) == 0 { 4 } else { random.below(13) };
            let row_stride = match random.below(4) {
                0 => shape_cols * col_stride,
                1 => shape_cols * col_stride + random.below(9),
                2 => random.below(9),
                _ => 0,
            };
            let offset = random.below(5);
            let size = offset + shape_rows * (row_stride + 4) + shape_cols * (col_stride + 4) + 4;
            let mut bytes = vec![0xAAu8; size];
            for r in 0..shape_rows {
                for c in 0..shape_cols {
                    let at = offset + r * row_stride + c * col_stride;
                    bytes[at..at + 4].copy_from_slice(&random.value().to_le_bytes());
                }
            }
            // Layouts can overlap, so expectations come from the storage's final bytes.
            let mut logical = Vec::new();
            for r in 0..shape_rows {
                for c in 0..shape_cols {
                    let at = offset + r * row_stride + c * col_stride;
                    logical.push(f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
                }
            }
            tensors
                .push(Tensor::from_storage(Arc::new(Bytes(bytes)), shape, offset, [row_stride, col_stride]).unwrap());
            inputs.push((logical, (shape_rows, shape_cols)));
        }
        // A random DAG in topological order, which shares subexpressions by construction.
        let mut model: Vec<Model> = (0..slot_count).map(Model::Slot).collect();
        let mut exprs: Vec<Expr> =
            (0..slot_count).map(|s| Expr::slot(s, tensors[s].shape(), domain.clone()).unwrap()).collect();
        let nodes = 1 + random.below(24);
        for _ in 0..nodes {
            let (a, b) = (random.below(exprs.len()), random.below(exprs.len()));
            let (node, expr) = match random.below(5) {
                0 => {
                    let v = random.value();
                    (Model::Constant(v), Expr::constant(v, domain.clone()).unwrap())
                }
                1 | 2 => (Model::Add(a, b), exprs[a].add(&exprs[b]).unwrap()),
                3 => (Model::Mul(a, b), exprs[a].mul(&exprs[b]).unwrap()),
                _ => (Model::Relu(a), exprs[a].relu().unwrap()),
            };
            model.push(node);
            exprs.push(expr);
        }
        // Tie every slot into the root, so the signature is dense.
        let mut root = exprs.last().unwrap().clone();
        let mut root_id = model.len() - 1;
        for (s, slot) in exprs.iter().enumerate().take(slot_count) {
            root = root.add(slot).unwrap();
            model.push(Model::Add(root_id, s));
            root_id = model.len() - 1;
        }
        // The output covers every broadcast the root reaches: all slots are in it.
        let root_shape = {
            let (mut r, mut c) = (1, 1);
            for (_, (rs, cs)) in &inputs {
                if *rs != 1 {
                    r = *rs;
                }
                if *cs != 1 {
                    c = *cs;
                }
            }
            Shape::Matrix(r, c)
        };
        let program = root.compile().unwrap();
        assert_eq!(program.output_shape(), root_shape, "case {case}");
        let (out_rows, out_cols) = root_shape.dims();
        let mut expected = Vec::with_capacity(out_rows * out_cols);
        for r in 0..out_rows {
            for c in 0..out_cols {
                expected.push(evaluate(&model, &inputs, r, c));
            }
        }
        let views: Vec<View<'_>> = tensors.iter().map(Tensor::borrow).collect();
        let reference = program.run_reference(&views, &budget).unwrap();
        assert_same(&read(&reference), &expected, &format!("case {case}: reference vs model"));
        for isa in isas() {
            for chunk in [1, 7, 256, CHUNK] {
                let mut out = vec![0x55u8; expected.len() * 4];
                program.run_with(&views, &mut out, &budget, isa, chunk).unwrap();
                assert_same(&floats(&out), &expected, &format!("case {case}: {isa:?} chunk {chunk}"));
            }
        }
        let refs: Vec<&Tensor> = tensors.iter().collect();
        assert_same(&read(&program.run(&refs, &budget).unwrap()), &expected, &format!("case {case}: run"));
    }
    assert_eq!(budget.used(), 0);
}

/// An independent triple loop in the contract's order: +0, then multiply-add in increasing k.
fn reference_matmul(a: &[f32], b: &[f32], m: usize, k: usize, n: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; m * n];
    for i in 0..m {
        for j in 0..n {
            let mut sum = 0.0f32;
            for p in 0..k {
                sum += a[i * k + p] * b[p * n + j];
            }
            out[i * n + j] = sum;
        }
    }
    out
}

#[test]
fn randomized_matmul_matches_the_ordered_triple_loop() {
    let mut random = Random(0x1234_5678_9abc_def1);
    let budget = Budget::new(64 * 1024 * 1024);
    let domain = domain();
    for case in 0..120 {
        let wide = case % 12 == 0;
        let (m, k, n) = if wide {
            (1 + random.below(30), 60 + random.below(90), 1 + random.below(70))
        } else {
            (random.below(6), random.below(9), random.below(6))
        };
        let a: Vec<f32> = (0..m * k).map(|_| random.value()).collect();
        let b: Vec<f32> = (0..k * n).map(|_| random.value()).collect();
        let expected = reference_matmul(&a, &b, m, k, n);
        let (ta, tb) = (tensor(&a, Shape::Matrix(m, k), &budget), tensor(&b, Shape::Matrix(k, n), &budget));
        let program = Expr::slot(0, Shape::Matrix(m, k), domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, Shape::Matrix(k, n), domain.clone()).unwrap())
            .unwrap()
            .compile()
            .unwrap();
        let views = [ta.borrow(), tb.borrow()];
        assert_same(&read(&program.run_reference(&views, &budget).unwrap()), &expected, &format!("case {case} ref"));
        for isa in isas() {
            let mut out = vec![0x55u8; m * n * 4];
            program.run_with(&views, &mut out, &budget, isa, CHUNK).unwrap();
            assert_same(&floats(&out), &expected, &format!("case {case} {isa:?}"));
        }
        // A strided left operand packs under budget and gives the same answer.
        if m > 0 && k > 0 {
            let mut padded = Vec::new();
            for row in a.chunks(k) {
                for v in row {
                    padded.extend_from_slice(&v.to_le_bytes());
                    padded.extend_from_slice(&[0xEE; 4]);
                }
            }
            let strided = Tensor::from_storage(Arc::new(Bytes(padded)), Shape::Matrix(m, k), 0, [k * 8, 8]).unwrap();
            assert_same(
                &read(&program.run(&[&strided, &tb], &budget).unwrap()),
                &expected,
                &format!("case {case} strided"),
            );
        }
    }
    assert_eq!(budget.used(), 0);
}

#[test]
fn mixed_stages_release_temporaries_and_share_subexpressions() {
    let budget = Budget::new(1024 * 1024);
    let domain = domain();
    let (m, k) = (3, 4);
    let a: Vec<f32> = (0..m * k).map(|i| i as f32 - 5.0).collect();
    let b: Vec<f32> = (0..k * m).map(|i| 2.0 - i as f32).collect();
    let (ta, tb) = (tensor(&a, Shape::Matrix(m, k), &budget), tensor(&b, Shape::Matrix(k, m), &budget));
    let x = Expr::slot(0, Shape::Matrix(m, k), domain.clone()).unwrap();
    let y = Expr::slot(1, Shape::Matrix(k, m), domain.clone()).unwrap();
    let two = Expr::constant(2.0, domain.clone()).unwrap();
    // relu(x + 2) is shared; an elementwise operand of a matmul materializes once.
    let shared = x.add(&two).unwrap().relu().unwrap();
    let product = shared.matmul(&y).unwrap();
    let root = product.add(&product).unwrap().relu().unwrap();
    let program = root.compile().unwrap();
    assert!(program.dump().contains("matmul"));
    let shifted: Vec<f32> = a
        .iter()
        .map(|v| {
            let w = v + 2.0;
            if w <= 0.0 { 0.0 } else { w }
        })
        .collect();
    let expected: Vec<f32> = reference_matmul(&shifted, &b, m, k, m)
        .iter()
        .map(|v| {
            let w = v + v;
            if w <= 0.0 { 0.0 } else { w }
        })
        .collect();
    let before = budget.used();
    let result = program.run(&[&ta, &tb], &budget).unwrap();
    assert_same(&read(&result), &expected, "mixed");
    // Only the result stays charged: temporaries and scratch released themselves.
    assert_eq!(budget.used() - before, m * m * 4);
    // Peak: inputs, two temporaries, the output and at most scratch for one register.
    assert!(budget.peak() <= before + (m * k + m * m + m * m + m * m) * 4, "peak {}", budget.peak());
}

#[test]
fn one_program_runs_different_inputs_concurrently_and_captures_nothing() {
    let budget = Budget::new(1024 * 1024);
    let domain = Domain::new(8, 1);
    let x = Expr::slot(0, Shape::Matrix(1, 3), domain.clone()).unwrap();
    let y = Expr::constant(2., domain.clone()).unwrap();
    let graph = x.add(&y).unwrap().relu().unwrap();
    let plan = Arc::new(graph.compile().unwrap());
    // The plan budget is one, and compiled programs retain no expression node.
    assert!(graph.compile().is_err());
    drop((graph, x, y));
    assert_eq!(domain.live_nodes(), 0);
    let mut threads = Vec::new();
    for seed in 0..8 {
        let (plan, budget) = (plan.clone(), budget.clone());
        threads.push(std::thread::spawn(move || {
            for _ in 0..100 {
                let t = tensor(&[seed as f32, -3., 1.], Shape::Matrix(1, 3), &budget);
                let owner = Arc::downgrade(&t.owner);
                let result = plan.run(&[&t], &budget).unwrap();
                assert_eq!(read(&result), [seed as f32 + 2., 0., 3.]);
                drop((t, result));
                assert!(owner.upgrade().is_none(), "a program must not retain input storage");
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(budget.used(), 0);
    drop(plan);
    assert_eq!(domain.live_plans(), 0);
}

#[test]
fn input_mismatches_are_rejected_before_any_work() {
    let budget = Budget::new(1024);
    let domain = domain();
    let plan = Expr::slot(0, Shape::Matrix(1, 3), domain.clone())
        .unwrap()
        .add(&Expr::slot(1, Shape::Matrix(1, 3), domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let (good, wrong) = (tensor(&[1., 2., 3.], Shape::Matrix(1, 3), &budget), tensor(&[1.], Shape::Scalar, &budget));
    let live = budget.used();
    assert!(plan.run(&[&good], &budget).is_err());
    assert!(plan.run(&[&good, &wrong], &budget).is_err());
    assert!(plan.run(&[&good, &good, &good], &budget).is_err());
    let mut short = [0u8; 8];
    assert!(plan.run_into(&[good.borrow(), good.borrow()], &mut short, &budget).is_err());
    assert_eq!(budget.used(), live);
    // Aliased inputs are fine: both slots may name one tensor.
    assert_eq!(read(&plan.run(&[&good, &good], &budget).unwrap()), [2., 4., 6.]);
    // Structure errors at compile time: conflicting shapes and sparse ids.
    let (a, b) = (
        Expr::slot(0, Shape::Matrix(1, 3), domain.clone()).unwrap(),
        Expr::slot(0, Shape::Matrix(1, 1), domain.clone()).unwrap(),
    );
    assert!(a.add(&b).unwrap().compile().is_err());
    assert!(Expr::slot(1, Shape::Matrix(1, 1), domain.clone()).unwrap().compile().is_err());
    assert!(Expr::slot(usize::MAX, Shape::Scalar, domain.clone()).is_err());
    // Cross-domain composition is denied.
    let other = Domain::new(8, 1);
    assert!(a.add(&Expr::constant(1., other).unwrap()).is_err());
}

#[test]
fn views_cannot_widen_and_share_their_owner() {
    let budget = Budget::new(1024);
    let t = tensor(&[1., 2., 3., 4., 5., 6.], Shape::Matrix(2, 3), &budget);
    let transposed = t.view(Shape::Matrix(3, 2), 0, [4, 12]).unwrap();
    assert!(t.shares_storage(&transposed));
    assert_eq!(read(&transposed), [1., 4., 2., 5., 3., 6.]);
    let broadcast = t.view(Shape::Matrix(2, 2), 0, [0, 0]).unwrap();
    assert_eq!(read(&broadcast), [1., 1., 1., 1.]);
    assert!(t.view(Shape::Matrix(2, 3), 4, [12, 4]).is_err(), "widens past the owner");
    let narrow = t.view(Shape::Matrix(1, 2), 4, [8, 4]).unwrap();
    assert!(narrow.view(Shape::Matrix(1, 3), 0, [12, 4]).is_err(), "widens past its parent");
    assert!(t.view(Shape::Matrix(usize::MAX, usize::MAX), 0, [usize::MAX, usize::MAX]).is_err());
    assert!(t.view(Shape::Matrix(1, 1), usize::MAX, [0, 0]).is_err());
    assert!(Shape::Matrix(1 << 21, 0).elements().is_err());
    assert!(Tensor::import(&[0; 3], Shape::Matrix(1, 1), &budget).is_err());
    let owner = Arc::downgrade(&t.owner);
    drop(t);
    assert_eq!(read(&transposed), [1., 4., 2., 5., 3., 6.], "views keep the owner alive");
    drop((transposed, broadcast, narrow));
    assert!(owner.upgrade().is_none());
    assert_eq!(budget.used(), 0);
}

#[test]
fn empty_shapes_and_budget_exhaustion_recover() {
    let budget = Budget::new(64);
    let domain = domain();
    // Zero-inner matmul is positive zeros, and the output is still charged.
    let (empty_a, empty_b) = (tensor(&[], Shape::Matrix(2, 0), &budget), tensor(&[], Shape::Matrix(0, 3), &budget));
    let program = Expr::slot(0, Shape::Matrix(2, 0), domain.clone())
        .unwrap()
        .matmul(&Expr::slot(1, Shape::Matrix(0, 3), domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let zeros = program.run(&[&empty_a, &empty_b], &budget).unwrap();
    assert_eq!(read(&zeros).iter().map(|v| v.to_bits()).collect::<Vec<_>>(), [0; 6]);
    drop(zeros);
    // Empty elementwise with a one-to-zero broadcast.
    let e = Expr::slot(0, Shape::Matrix(0, 3), domain.clone())
        .unwrap()
        .add(&Expr::slot(1, Shape::Matrix(1, 3), domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let (zero, one) = (tensor(&[], Shape::Matrix(0, 3), &budget), tensor(&[1., 2., 3.], Shape::Matrix(1, 3), &budget));
    assert_eq!(read(&e.run(&[&zero, &one], &budget).unwrap()), []);
    // Exhaustion fails cleanly, charges nothing further, and recovery succeeds.
    let big = Expr::slot(0, Shape::Matrix(1, 3), domain.clone()).unwrap().compile().unwrap();
    let hog = budget.allocate(52).unwrap();
    let live = budget.used();
    assert!(big.run(&[&one], &budget).is_err());
    assert_eq!(budget.used(), live);
    drop(hog);
    assert_eq!(read(&big.run(&[&one], &budget).unwrap()), [1., 2., 3.]);
    // Excessive work and shape requests never reach allocation.
    let huge = Shape::Matrix(1 << 20, 1);
    assert!(Expr::slot(0, Shape::Matrix(1 << 11, 1 << 11), domain.clone()).is_err());
    let wide = Expr::slot(0, huge, domain.clone()).unwrap();
    let mut chain = wide.clone();
    for _ in 0..70 {
        chain = chain.add(&wide).unwrap();
    }
    assert!(chain.compile().is_err(), "work bound");
    assert!(
        Expr::slot(0, Shape::Matrix(512, 512), domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, Shape::Matrix(512, 512), domain.clone()).unwrap())
            .is_err(),
        "matmul work bound"
    );
}

#[test]
fn node_budgets_and_shared_dags_stay_linear() {
    let domain = Domain::new(2000, 4);
    // A doubling chain has 2^n paths and n nodes: planning must not expand it.
    let x = Expr::slot(0, Shape::Matrix(1, 2), domain.clone()).unwrap();
    let mut value = x.clone();
    for _ in 0..200 {
        value = value.add(&value).unwrap();
    }
    let program = value.compile().unwrap();
    assert_eq!(program.steps().len(), 201);
    let budget = Budget::new(1024);
    let t = tensor(&[1.0, -1.0], Shape::Matrix(1, 2), &budget);
    // 2^200 is finite in f64 but overflows f32: exactly what a scalar evaluation gives.
    assert_eq!(read(&program.run(&[&t], &budget).unwrap()), [f32::INFINITY, f32::NEG_INFINITY]);
    drop(program);
    // Live-node exhaustion, then recovery as nodes drop.
    let tiny = Domain::new(2, 1);
    let a = Expr::slot(0, Shape::Scalar, tiny.clone()).unwrap();
    let b = a.relu().unwrap();
    assert!(a.relu().is_err());
    drop(b);
    assert!(a.relu().is_ok());
    // Depth and node budgets.
    let deep = Domain::new(5000, 4);
    let mut chain = Expr::slot(0, Shape::Scalar, deep.clone()).unwrap();
    let mut failed = false;
    for _ in 0..2000 {
        let Ok(next) = chain.relu() else {
            failed = true;
            break;
        };
        chain = next;
    }
    assert!(failed, "depth budget");
}

#[test]
fn strict_f32_edges_are_exact_on_every_instantiation() {
    let budget = Budget::new(1024 * 1024);
    let domain = domain();
    // A case FMA would change: (1+e)(1-2e) - 1 rounds to zero when separately rounded.
    let (x, y) = (f32::from_bits(0x3f80_0001), f32::from_bits(0x3f7f_fffe));
    let n = 37; // covers vector bodies and scalar tails.
    let (a, b) = (vec![x; n], vec![y; n]);
    let product_minus_one = Expr::slot(0, Shape::Matrix(1, n), domain.clone())
        .unwrap()
        .mul(&Expr::slot(1, Shape::Matrix(1, n), domain.clone()).unwrap())
        .unwrap()
        .add(&Expr::constant(-1.0, domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let (ta, tb) = (tensor(&a, Shape::Matrix(1, n), &budget), tensor(&b, Shape::Matrix(1, n), &budget));
    let separately_rounded = (x * y) + -1.0;
    assert_eq!(separately_rounded.to_bits(), 0, "{separately_rounded}");
    assert_ne!(x.mul_add(y, -1.0).to_bits(), 0, "the fixture must distinguish FMA");
    for isa in isas() {
        let mut out = vec![0x55u8; n * 4];
        product_minus_one.run_with(&[ta.borrow(), tb.borrow()], &mut out, &budget, isa, CHUNK).unwrap();
        assert!(floats(&out).iter().all(|v| v.to_bits() == 0), "{isa:?} contracted");
    }
    // ReLU: signed zeros go to +0, NaN stays NaN, subnormals survive (no flush to zero).
    let specials = [
        0.0,
        -0.0,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MIN_POSITIVE / 4.0,
        -1.0,
        1.0,
        -f32::MIN_POSITIVE / 4.0,
    ];
    let many: Vec<f32> = specials.iter().copied().cycle().take(41).collect();
    let relu = Expr::slot(0, Shape::Matrix(1, 41), domain.clone()).unwrap().relu().unwrap().compile().unwrap();
    let t = tensor(&many, Shape::Matrix(1, 41), &budget);
    let expected: Vec<f32> = many.iter().map(|v| if *v <= 0.0 { 0.0 } else { *v }).collect();
    for isa in isas() {
        let mut out = vec![0x55u8; 41 * 4];
        relu.run_with(&[t.borrow()], &mut out, &budget, isa, CHUNK).unwrap();
        assert_same(&floats(&out), &expected, "relu");
    }
    // Rounding: a subnormal product keeps its value through add and multiply.
    let tiny = f32::MIN_POSITIVE / 4.0;
    let scale = Expr::slot(0, Shape::Scalar, domain.clone())
        .unwrap()
        .mul(&Expr::constant(3.0, domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    assert_eq!(read(&scale.run(&[&tensor(&[tiny], Shape::Scalar, &budget)], &budget).unwrap()), [tiny * 3.0]);
    // 0 * inf inside a matmul stays NaN: no zero-skipping optimization.
    let mm = Expr::slot(0, Shape::Matrix(1, 1), domain.clone())
        .unwrap()
        .matmul(&Expr::slot(1, Shape::Matrix(1, 1), domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    let (zero, inf) =
        (tensor(&[0.0], Shape::Matrix(1, 1), &budget), tensor(&[f32::INFINITY], Shape::Matrix(1, 1), &budget));
    assert!(read(&mm.run(&[&zero, &inf], &budget).unwrap())[0].is_nan());
    // -0 * 1 added to the +0 start is +0.
    let (neg, one) = (tensor(&[-0.0], Shape::Matrix(1, 1), &budget), tensor(&[1.0], Shape::Matrix(1, 1), &budget));
    assert_eq!(read(&mm.run(&[&neg, &one], &budget).unwrap())[0].to_bits(), 0);
    // Constants fold with the same rounding the kernels use.
    let folded = Expr::constant(x, domain.clone())
        .unwrap()
        .mul(&Expr::constant(y, domain.clone()).unwrap())
        .unwrap()
        .add(&Expr::constant(-1.0, domain.clone()).unwrap())
        .unwrap()
        .compile()
        .unwrap();
    assert_eq!(read(&folded.run(&[], &budget).unwrap())[0].to_bits(), 0);
}

#[test]
fn external_owner_is_read_in_place_and_released_with_the_last_view() {
    let budget = Budget::new(0);
    let storage: Arc<dyn Storage> = Arc::new(Bytes(bytes_of(&[1., 2., 3., 4.])));
    let pointer = storage.bytes().as_ptr();
    let t = Tensor::from_storage(storage, Shape::Matrix(2, 2), 0, [8, 4]).unwrap();
    assert_eq!(t.borrow().bytes.as_ptr(), pointer, "no payload copy");
    assert_eq!(budget.used(), 0, "the host's own accounting");
    let weak = Arc::downgrade(&t.owner);
    drop(t);
    assert!(weak.upgrade().is_none());
}
