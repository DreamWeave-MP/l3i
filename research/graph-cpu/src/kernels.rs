//! The execution kernels: fixed monomorphic loops, one per operation and operand form, built
//! once for the baseline target and once for AVX2. Nothing here is generated at run time, and
//! nothing dispatches per element: dynamic dispatch happens once per operation per [`CHUNK`].
//!
//! Operands are `[u8; 4]` little-endian elements, so input and output may sit at any byte
//! offset, the same safe code serves registers, Luau buffers and host storage, and LLVM still
//! vectorizes through the byte conversions (`vmovups`, `vaddps`). There is no `unsafe` beyond
//! calling the AVX2 instantiation after detecting it.
//!
//! Floating point: only plain Rust `+`, `*` and `<=`. Rust grants LLVM no contraction or
//! reassociation, so each add and multiply rounds to f32 separately on every instantiation, and
//! matmul keeps its additions in increasing `k` for every output element.
// `inline(always)` is the mechanism: it is how a body gets compiled under each instantiation's
// target features. The matmul names (a, b, m, k, n, p) are the mathematics'.
#![allow(clippy::inline_always, clippy::many_single_char_names)]
use crate::program::{Dst, Elementwise, Kernel, Kind, Operand};
use crate::{Shape, View};

/// Elements per dispatch. `research/results` holds the sweep behind this value.
pub const CHUNK: usize = 1024;
/// Inner-dimension rows per matmul pass, ascending so each element's addition order is intact.
const K_BLOCK: usize = 64;

/// Which instantiation of the kernels executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    Baseline,
    Avx2,
}
impl Isa {
    #[must_use]
    pub fn detect() -> Self {
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            return Self::Avx2;
        }
        Self::Baseline
    }
}

pub(crate) type Element = [u8; 4];

#[inline(always)]
fn read(element: Element) -> f32 {
    f32::from_le_bytes(element)
}
#[inline(always)]
fn relu(value: f32) -> f32 {
    if value <= 0.0 { 0.0 } else { value }
}

/// A view resolved against the output's shape, ready to be read chunk by chunk.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Source<'a> {
    /// Output-order elements in memory: read in place, no copy.
    Direct(&'a [Element]),
    /// One value for every output element.
    Splat(f32),
    /// Anything else: walked into a scratch register per chunk.
    Walk(Walk<'a>),
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Walk<'a> {
    bytes: &'a [u8],
    offset: usize,
    row_stride: usize,
    col_stride: usize,
    cols: usize,
    register: usize,
}
impl<'a> Source<'a> {
    /// Applies dimension-one broadcasting by zeroing that dimension's stride. The caller
    /// guarantees `view` broadcasts to `output`.
    pub(crate) fn bind(view: View<'a>, output: Shape) -> Self {
        let (view_rows, view_cols) = view.shape.dims();
        let (rows, cols) = output.dims();
        let row_stride = if view_rows == 1 { 0 } else { view.strides[0] };
        let col_stride = if view_cols == 1 { 0 } else { view.strides[1] };
        let count = rows * cols;
        if count == 0 {
            return Self::Direct(&[]);
        }
        if (rows == 1 || row_stride == 0) && (cols == 1 || col_stride == 0) {
            return Self::Splat(read(view.bytes[view.offset..view.offset + 4].try_into().expect("validated view")));
        }
        if (rows == 1 || row_stride == cols * 4) && (cols == 1 || col_stride == 4) {
            return Self::Direct(view.bytes[view.offset..view.offset + count * 4].as_chunks::<4>().0);
        }
        Self::Walk(Walk { bytes: view.bytes, offset: view.offset, row_stride, col_stride, cols, register: 0 })
    }
    /// Copies every output-order element to `out` (row-major f32, little-endian).
    pub(crate) fn materialize(self, out: &mut [u8]) {
        let (out, _) = out.as_chunks_mut::<4>();
        match self {
            Self::Direct(elements) => out.copy_from_slice(elements),
            Self::Splat(value) => out.fill(value.to_le_bytes()),
            Self::Walk(walk) => walk.fill(0, out),
        }
    }
}
impl Walk<'_> {
    /// Fills `out` with the elements from output position `start`, in row-major order.
    #[inline(always)]
    fn fill(&self, start: usize, out: &mut [Element]) {
        let (mut row, mut col) = (start / self.cols, start % self.cols);
        let mut out = out;
        while !out.is_empty() {
            let take = (self.cols - col).min(out.len());
            let (run, rest) = out.split_at_mut(take);
            out = rest;
            let at = self.offset + row * self.row_stride + col * self.col_stride;
            match self.col_stride {
                0 => run.fill(self.bytes[at..at + 4].try_into().expect("validated view")),
                4 => run.copy_from_slice(self.bytes[at..at + take * 4].as_chunks::<4>().0),
                stride => {
                    let span = &self.bytes[at..at + (take - 1) * stride + 4];
                    // Strides below four overlap, so this indexes rather than splits the span.
                    for (index, slot) in run.iter_mut().enumerate() {
                        *slot = span[index * stride..index * stride + 4].try_into().expect("validated view");
                    }
                }
            }
            row += 1;
            col = 0;
        }
    }
}

/// One resolved operand of one kernel.
#[derive(Clone, Copy)]
enum Arg<'a> {
    Slice(&'a [Element]),
    Value(f32),
}

macro_rules! binary {
    ($name:ident, $op:tt) => {
        #[inline(always)]
        fn $name(a: Arg<'_>, b: Arg<'_>, out: &mut [Element]) {
            match (a, b) {
                (Arg::Slice(a), Arg::Slice(b)) => {
                    for ((slot, a), b) in out.iter_mut().zip(a).zip(b) {
                        *slot = (read(*a) $op read(*b)).to_le_bytes();
                    }
                }
                (Arg::Slice(a), Arg::Value(b)) => {
                    for (slot, a) in out.iter_mut().zip(a) {
                        *slot = (read(*a) $op b).to_le_bytes();
                    }
                }
                (Arg::Value(a), Arg::Slice(b)) => {
                    for (slot, b) in out.iter_mut().zip(b) {
                        *slot = (a $op read(*b)).to_le_bytes();
                    }
                }
                (Arg::Value(a), Arg::Value(b)) => out.fill((a $op b).to_le_bytes()),
            }
        }
    };
}
binary!(add, +);
binary!(mul, *);

#[inline(always)]
fn unary_relu(a: Arg<'_>, out: &mut [Element]) {
    match a {
        Arg::Slice(a) => {
            for (slot, a) in out.iter_mut().zip(a) {
                *slot = relu(read(*a)).to_le_bytes();
            }
        }
        Arg::Value(a) => out.fill(relu(a).to_le_bytes()),
    }
}

#[inline(always)]
fn copy(a: Arg<'_>, out: &mut [Element]) {
    match a {
        Arg::Slice(a) => out.copy_from_slice(a),
        Arg::Value(a) => out.fill(a.to_le_bytes()),
    }
}

/// The body every instantiation shares; `inline(always)` so each kernel is compiled under its
/// instantiation's target features. `chunk` is a parameter only so the sweep can vary it.
#[inline(always)]
fn elementwise_body(
    stage: &Elementwise,
    sources: &[Source<'_>],
    scratch: &mut [Element],
    out: &mut [Element],
    chunk: usize,
) {
    let width = out.len().min(chunk);
    let (registers, walked) = scratch.split_at_mut(stage.registers * width);
    let mut start = 0;
    for out_chunk in out.chunks_mut(chunk) {
        let len = out_chunk.len();
        for source in sources {
            if let Source::Walk(walk) = source {
                walk.fill(start, &mut walked[walk.register * width..][..len]);
            }
        }
        for Kernel { kind, dst, a, b } in &stage.code {
            // The destination register is never an operand of its own kernel (the planner
            // guarantees it), so splitting around it gives both safe borrows.
            let (boundary, before, destination, after): (usize, &[Element], &mut [Element], &[Element]) = match dst {
                Dst::Output => (stage.registers, &*registers, &mut *out_chunk, &[]),
                Dst::Register(r) => {
                    let r = *r as usize;
                    let (before, rest) = registers.split_at_mut(r * width);
                    let (register, after) = rest.split_at_mut(width);
                    (r, &*before, &mut register[..len], &*after)
                }
            };
            let walked = &*walked;
            let resolve = |operand: Operand| -> Arg<'_> {
                match operand {
                    Operand::Constant(value) => Arg::Value(value),
                    Operand::Register(r) => {
                        let r = r as usize;
                        Arg::Slice(if r < boundary {
                            &before[r * width..][..len]
                        } else {
                            &after[(r - boundary - 1) * width..][..len]
                        })
                    }
                    Operand::Source(s) => match &sources[s as usize] {
                        Source::Direct(elements) => Arg::Slice(&elements[start..start + len]),
                        Source::Splat(value) => Arg::Value(*value),
                        Source::Walk(walk) => Arg::Slice(&walked[walk.register * width..][..len]),
                    },
                }
            };
            match kind {
                Kind::Add => add(resolve(*a), resolve(*b), destination),
                Kind::Mul => mul(resolve(*a), resolve(*b), destination),
                Kind::Relu => unary_relu(resolve(*a), destination),
                Kind::Copy => copy(resolve(*a), destination),
            }
        }
        start += len;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn elementwise_avx2(s: &Elementwise, sources: &[Source<'_>], scratch: &mut [Element], out: &mut [Element], c: usize) {
    elementwise_body(s, sources, scratch, out, c);
}
#[inline(never)]
fn elementwise_baseline(
    s: &Elementwise,
    sources: &[Source<'_>],
    scratch: &mut [Element],
    out: &mut [Element],
    c: usize,
) {
    elementwise_body(s, sources, scratch, out, c);
}

/// A thread's retained scratch: the planner writes every register before reading it, so its old
/// contents are never observed. Larger requests are served and freed, never retained.
const SCRATCH_RETAINED: usize = 64 * 1024;
thread_local! {
    static SCRATCH: std::cell::Cell<Vec<u8>> = const { std::cell::Cell::new(Vec::new()) };
}
pub(crate) fn with_scratch<R>(bytes: usize, body: impl FnOnce(&mut [u8]) -> R) -> l3i::Result<R> {
    let mut scratch = SCRATCH.take();
    if scratch.len() < bytes {
        scratch.try_reserve_exact(bytes - scratch.len()).map_err(|_| crate::error("scratch allocation failed"))?;
        scratch.resize(bytes, 0);
    }
    let result = body(&mut scratch[..bytes]);
    if scratch.len() <= SCRATCH_RETAINED {
        SCRATCH.set(scratch);
    }
    Ok(result)
}

/// Scratch elements one elementwise stage needs for `count` output elements.
pub(crate) fn scratch_elements(stage: &Elementwise, sources: &[Source<'_>], count: usize, chunk: usize) -> usize {
    let walked = sources.iter().filter(|s| matches!(s, Source::Walk(_))).count();
    (stage.registers + walked) * count.min(chunk)
}

/// Resolves every view against `shape` and numbers the walked ones.
pub(crate) fn bind_sources<'a>(views: impl Iterator<Item = View<'a>>, shape: Shape) -> Vec<Source<'a>> {
    let mut walked = 0;
    views
        .map(|view| {
            let mut source = Source::bind(view, shape);
            if let Source::Walk(walk) = &mut source {
                walk.register = walked;
                walked += 1;
            }
            source
        })
        .collect()
}

/// Runs one elementwise stage into row-major `out`, with kernels of `isa`.
pub(crate) fn elementwise(
    stage: &Elementwise,
    sources: &[Source<'_>],
    scratch: &mut [u8],
    out: &mut [u8],
    isa: Isa,
    chunk: usize,
) {
    let (out, _) = out.as_chunks_mut::<4>();
    let (scratch, _) = scratch.as_chunks_mut::<4>();
    if out.is_empty() {
        return;
    }
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `Isa::Avx2` comes from `Isa::detect`, or from a caller that detected it.
        Isa::Avx2 => unsafe { elementwise_avx2(stage, sources, scratch, out, chunk) },
        _ => elementwise_baseline(stage, sources, scratch, out, chunk),
    }
}

/// `out[j] += a0*b0[j]; out[j] += a1*b1[j]; ...`: one load and store of `out` per four rows,
/// with each addition still rounded separately and in order.
#[inline(always)]
fn accumulate4(out: &mut [Element], a: [f32; 4], b: [&[Element]; 4]) {
    let n = out.len();
    let (b0, b1, b2, b3) = (&b[0][..n], &b[1][..n], &b[2][..n], &b[3][..n]);
    for j in 0..n {
        let mut value = read(out[j]);
        value += a[0] * read(b0[j]);
        value += a[1] * read(b1[j]);
        value += a[2] * read(b2[j]);
        value += a[3] * read(b3[j]);
        out[j] = value.to_le_bytes();
    }
}
#[inline(always)]
fn accumulate1(out: &mut [Element], a: f32, b: &[Element]) {
    for (slot, b) in out.iter_mut().zip(b) {
        *slot = (read(*slot) + a * read(*b)).to_le_bytes();
    }
}

/// The row-major `a @ b` kernel: `i`-`k`-`j` order, `b` rows visited in ascending blocks.
#[inline(always)]
fn matmul_body(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element]) {
    for block in (0..k).step_by(K_BLOCK) {
        let end = (block + K_BLOCK).min(k);
        for (out_row, a_row) in out.chunks_exact_mut(n).zip(a.chunks_exact(k)) {
            let row_of = |q: usize| &b[q * n..(q + 1) * n];
            let mut p = block;
            while p + 4 <= end {
                accumulate4(
                    out_row,
                    [read(a_row[p]), read(a_row[p + 1]), read(a_row[p + 2]), read(a_row[p + 3])],
                    [row_of(p), row_of(p + 1), row_of(p + 2), row_of(p + 3)],
                );
                p += 4;
            }
            while p < end {
                accumulate1(out_row, read(a_row[p]), row_of(p));
                p += 1;
            }
        }
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn matmul_avx2(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element]) {
    matmul_body(a, b, k, n, out);
}
#[inline(never)]
fn matmul_baseline(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element]) {
    matmul_body(a, b, k, n, out);
}

/// `a @ b` into row-major `out`. Operands are contiguous elements (`packed` by the caller when
/// their views are not); `+0` starts every element, and no zero is skipped: `0 * inf` is NaN.
pub(crate) fn matmul(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [u8], isa: Isa) {
    out.fill(0);
    if out.is_empty() || k == 0 {
        return;
    }
    let (out, _) = out.as_chunks_mut::<4>();
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as in `elementwise`.
        Isa::Avx2 => unsafe { matmul_avx2(a, b, k, n, out) },
        _ => matmul_baseline(a, b, k, n, out),
    }
}
