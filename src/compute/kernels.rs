//! The execution kernels: fixed loops compiled ahead of time for every short sequence of steps,
//! once for the baseline target and once for each SIMD instantiation. Nothing here is generated
//! at run time, and nothing dispatches per element: dynamic dispatch happens once per tile per
//! [`CHUNK`]. A tile holds its running value in a SIMD register across its steps, so a chain costs
//! one store per tile rather than one per operation.
//!
//! Operands are `[u8; 4]` little-endian elements, so input and output may sit at any byte
//! offset, the same safe code serves registers, Luau buffers and host storage, and LLVM still
//! vectorizes through the byte conversions (`vmovups`, `vaddps`). There is no `unsafe` beyond
//! calling a SIMD instantiation after detecting it.
//!
//! Floating point: strict tiles use only plain Rust `+`, `*` and `<=`. Rust grants LLVM no
//! contraction or reassociation, so each add and multiply rounds to f32 separately on every
//! instantiation, and matmul keeps its additions in increasing `k` for every output element. Only
//! a program compiled with [`crate::compute::Numeric::Fast`] uses `mul_add`, and only where asked.
// `inline(always)` is the mechanism: it is how a body gets compiled under each instantiation's
// target features. The matmul names (a, b, m, k, n, p) are the mathematics'.
#![allow(clippy::inline_always, clippy::many_single_char_names, clippy::items_after_statements)]
use crate::compute::program::{Dst, Elementwise, Op, Operand, TILE_STEPS, Tile, TileStep};
use crate::compute::{Shape, View};

/// Elements per dispatch. `benches/compute-results` holds the sweep behind this value.
pub const CHUNK: usize = 1024;
/// Inner-dimension rows per matmul pass, ascending so each element's addition order is intact.
const K_BLOCK: usize = 128;

/// Which instantiation of the kernels executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    Baseline,
    Avx2,
    /// Hardware fused multiply-add: AVX2 and FMA on `x86_64`, the baseline itself on `aarch64`. Executes
    /// a `Numeric::Fast` program's fused tiles; strict programs run as the best strict instantiation.
    Fma,
}
impl Isa {
    /// The best instantiation a strict program can use here.
    #[must_use]
    pub fn detect() -> Self {
        if Self::Avx2.available() { Self::Avx2 } else { Self::Baseline }
    }
    /// The best instantiation a `Numeric::Fast` program can use here.
    #[must_use]
    pub fn detect_fast() -> Self {
        if Self::Fma.available() { Self::Fma } else { Self::detect() }
    }
    /// Whether this host can execute this instantiation.
    #[must_use]
    pub fn available(self) -> bool {
        match self {
            Self::Baseline => true,
            #[cfg(target_arch = "x86_64")]
            Self::Avx2 => std::is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "x86_64")]
            Self::Fma => std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma"),
            #[cfg(target_arch = "aarch64")]
            Self::Fma => true,
            #[cfg(not(target_arch = "x86_64"))]
            _ => false,
        }
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
/// A view read one row at a time, in its own shape. A row whose columns are adjacent in memory is
/// borrowed in place, even when the rows themselves are spread apart.
#[derive(Clone, Copy)]
pub(crate) struct Rows<'a> {
    walk: Walk<'a>,
    adjacent: bool,
}
impl<'a> Rows<'a> {
    pub(crate) fn new(view: View<'a>) -> Self {
        let (rows, cols) = view.shape.dims();
        let row_stride = if rows == 1 { 0 } else { view.strides[0] };
        let col_stride = if cols == 1 { 4 } else { view.strides[1] };
        let walk = Walk { bytes: view.bytes, offset: view.offset, row_stride, col_stride, cols, register: 0 };
        Self { walk, adjacent: col_stride == 4 }
    }
    /// Elements of scratch [`Self::row`] needs for each row.
    pub(crate) fn copied_elements(&self) -> usize {
        if self.adjacent { 0 } else { self.walk.cols }
    }
    /// One row: in place when its columns are adjacent, otherwise copied to `scratch`.
    #[inline(always)]
    pub(crate) fn row<'s>(&self, row: usize, scratch: &'s mut [Element]) -> &'s [Element]
    where
        'a: 's,
    {
        let Walk { bytes, offset, row_stride, cols, .. } = self.walk;
        if self.adjacent {
            let at = offset + row * row_stride;
            bytes[at..at + cols * 4].as_chunks::<4>().0
        } else {
            self.walk.fill(row * cols, scratch);
            scratch
        }
    }
    /// All rows' elements and the distance between rows in elements, when rows are adjacent runs a
    /// whole number of elements apart.
    pub(crate) fn evenly_spaced(&self) -> Option<(&'a [Element], usize)> {
        let Walk { bytes, offset, row_stride, .. } = self.walk;
        (self.adjacent && row_stride.is_multiple_of(4)).then(|| (bytes[offset..].as_chunks::<4>().0, row_stride / 4))
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

/// An operand as bound for a run: which forms it can take never changes between chunks.
#[derive(Clone, Copy)]
enum Bound<'a> {
    Direct(&'a [Element]),
    Register(usize),
    Walked(usize),
    Value(f32),
}
impl Bound<'_> {
    #[inline(always)]
    fn value(self) -> f32 {
        if let Self::Value(value) = self { value } else { 0.0 }
    }
}
/// One tile's operands for a run.
#[derive(Clone, Copy)]
struct Binding<'a> {
    start: Bound<'a>,
    x: [Bound<'a>; TILE_STEPS],
    y: [Bound<'a>; TILE_STEPS],
}
/// Where this chunk's operands live. Registers are split around the destination, which is never
/// an operand of its own tile (the planner guarantees it), so both borrows are safe.
struct Frame<'a> {
    boundary: usize,
    before: &'a [Element],
    after: &'a [Element],
    walked: &'a [Element],
    width: usize,
    start: usize,
    len: usize,
}
impl<'a> Frame<'a> {
    #[inline(always)]
    fn slice(&self, bound: Bound<'a>) -> &'a [Element] {
        match bound {
            Bound::Direct(elements) => &elements[self.start..self.start + self.len],
            Bound::Register(r) => {
                if r < self.boundary {
                    &self.before[r * self.width..][..self.len]
                } else {
                    &self.after[(r - self.boundary - 1) * self.width..][..self.len]
                }
            }
            Bound::Walked(r) => &self.walked[r * self.width..][..self.len],
            Bound::Value(_) => &[],
        }
    }
}

/// One element of a slice `fused` has cut to its output's length. A checked index here makes LLVM
/// keep a scalar fallback that runs the last vector's worth of every chunk one element at a time.
#[inline(always)]
fn at(slice: &[Element], i: usize) -> f32 {
    debug_assert!(i < slice.len());
    // SAFETY: `fused` passes every step slices of exactly `out.len()` and `i < out.len()`.
    read(unsafe { *slice.get_unchecked(i) })
}

/// One operation of a fused loop, applied to the running value. Zero-sized: every combination of
/// these is a separately compiled and separately vectorized loop.
trait Step {
    /// Which operands this step reads as slices, and which as constants.
    const SX: bool;
    const SY: bool;
    const VX: bool;
    const VY: bool;
    fn apply(acc: f32, x: &[Element], y: &[Element], vx: f32, vy: f32, i: usize) -> f32;
}
macro_rules! step {
    ($name:ident, $sx:literal, $sy:literal, $vx:literal, $vy:literal, |$acc:ident, $xs:ident, $ys:ident, $cx:ident, $cy:ident, $i:ident| $body:expr) => {
        struct $name;
        impl Step for $name {
            const SX: bool = $sx;
            const SY: bool = $sy;
            const VX: bool = $vx;
            const VY: bool = $vy;
            #[inline(always)]
            #[allow(unused_variables)]
            fn apply($acc: f32, $xs: &[Element], $ys: &[Element], $cx: f32, $cy: f32, $i: usize) -> f32 {
                $body
            }
        }
    };
}
step!(Nop, false, false, false, false, |acc, xs, ys, vx, vy, i| acc);
step!(AddS, true, false, false, false, |acc, xs, ys, vx, vy, i| acc + at(xs, i));
step!(AddV, false, false, true, false, |acc, xs, ys, vx, vy, i| acc + vx);
step!(MulS, true, false, false, false, |acc, xs, ys, vx, vy, i| acc * at(xs, i));
step!(MulV, false, false, true, false, |acc, xs, ys, vx, vy, i| acc * vx);
step!(Relu, false, false, false, false, |acc, xs, ys, vx, vy, i| relu(acc));
// `Numeric::Fast` only: a fused multiply-add, under an instantiation that has the instruction.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
step!(MaSS, true, true, false, false, |acc, xs, ys, vx, vy, i| acc.mul_add(at(xs, i), at(ys, i)));
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
step!(MaSV, true, false, false, true, |acc, xs, ys, vx, vy, i| acc.mul_add(at(xs, i), vy));
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
step!(MaVS, false, true, true, false, |acc, xs, ys, vx, vy, i| acc.mul_add(vx, at(ys, i)));
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
step!(MaVV, false, false, true, true, |acc, xs, ys, vx, vy, i| acc.mul_add(vx, vy));

/// How a fused loop obtains its first value.
trait Start {
    const SLICE: bool;
    fn get(slice: &[Element], value: f32, i: usize) -> f32;
}
struct FromSlice;
impl Start for FromSlice {
    const SLICE: bool = true;
    #[inline(always)]
    fn get(slice: &[Element], _: f32, i: usize) -> f32 {
        at(slice, i)
    }
}
struct FromValue;
impl Start for FromValue {
    const SLICE: bool = false;
    #[inline(always)]
    fn get(_: &[Element], value: f32, _: usize) -> f32 {
        value
    }
}

/// One fused loop, resolving only the operands its steps read. Every slice is exactly `out.len()`
/// long, which is what lets `at` skip its check and LLVM vectorize the whole body with no scalar
/// tail beyond the true remainder.
#[inline(always)]
fn fused<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]) {
    let n = out.len();
    let s = cut(frame, n, S::SLICE, bind.start);
    let (ax, ay) = (cut(frame, n, A::SX, bind.x[0]), cut(frame, n, A::SY, bind.y[0]));
    let (bx, by) = (cut(frame, n, B::SX, bind.x[1]), cut(frame, n, B::SY, bind.y[1]));
    let (cx, cy) = (cut(frame, n, C::SX, bind.x[2]), cut(frame, n, C::SY, bind.y[2]));
    let constant = |wanted: bool, bound: Bound<'_>| if wanted { bound.value() } else { 0.0 };
    let start_value = constant(!S::SLICE, bind.start);
    let (avx, avy) = (constant(A::VX, bind.x[0]), constant(A::VY, bind.y[0]));
    let (bvx, bvy) = (constant(B::VX, bind.x[1]), constant(B::VY, bind.y[1]));
    let (cvx, cvy) = (constant(C::VX, bind.x[2]), constant(C::VY, bind.y[2]));
    for (i, slot) in out.iter_mut().enumerate() {
        let mut v = S::get(s, start_value, i);
        v = A::apply(v, ax, ay, avx, avy, i);
        v = B::apply(v, bx, by, bvx, bvy, i);
        v = C::apply(v, cx, cy, cvx, cvy, i);
        *slot = v.to_le_bytes();
    }
}
/// A bound operand's slice when a step reads it, asserted to be the output's length so `at`'s
/// contract holds.
#[inline(always)]
fn cut<'c>(frame: &Frame<'c>, n: usize, wanted: bool, bound: Bound<'c>) -> &'c [Element] {
    if !wanted {
        return &[];
    }
    let slice = frame.slice(bound);
    assert_eq!(slice.len(), n);
    slice
}

/// An instantiation of every fused loop under one set of target features.
trait Arch {
    fn run<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]);
}
struct Base;
impl Arch for Base {
    #[inline(never)]
    fn run<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]) {
        fused::<S, A, B, C>(frame, bind, out);
    }
}
#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{Arch, Binding, Element, Frame, Start, Step, fused};
    #[target_feature(enable = "avx2")]
    fn run_avx2<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]) {
        fused::<S, A, B, C>(frame, bind, out);
    }
    #[target_feature(enable = "avx2,fma")]
    fn run_fma<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]) {
        fused::<S, A, B, C>(frame, bind, out);
    }
    pub(super) struct Avx2;
    impl Arch for Avx2 {
        #[inline]
        fn run<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]) {
            // SAFETY: `Avx2` is only selected by `elementwise` after `Isa::available` confirms it.
            unsafe { run_avx2::<S, A, B, C>(frame, bind, out) }
        }
    }
    pub(super) struct Fma;
    impl Arch for Fma {
        #[inline]
        fn run<'c, S: Start, A: Step, B: Step, C: Step>(frame: &Frame<'c>, bind: &Binding<'c>, out: &mut [Element]) {
            // SAFETY: as for `Avx2`.
            unsafe { run_fma::<S, A, B, C>(frame, bind, out) }
        }
    }
}

/// The compiled loop for one tile, chosen once per run.
type KernelFn = for<'c> fn(&Frame<'c>, &Binding<'c>, &mut [Element]);

/// What loop a step needs, once a run's bindings say which operands are slices.
#[derive(Clone, Copy)]
struct Form {
    op: Op,
    x: bool,
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    y: bool,
}

/// Evaluates `$body` with `$T` naming the step type for `$form`.
macro_rules! with_step {
    ($form:expr, $T:ident => $body:expr, plain) => {
        match ($form.op, $form.x) {
            (Op::None, _) => {
                type $T = Nop;
                $body
            }
            (Op::Add, true) => {
                type $T = AddS;
                $body
            }
            (Op::Add, false) => {
                type $T = AddV;
                $body
            }
            (Op::Mul, true) => {
                type $T = MulS;
                $body
            }
            (Op::Mul, false) => {
                type $T = MulV;
                $body
            }
            (Op::Relu, _) => {
                type $T = Relu;
                $body
            }
            (Op::MulAdd, _) => unreachable!("strict tiles have no fused multiply-add"),
        }
    };
    ($form:expr, $T:ident => $body:expr, fused) => {
        match ($form.op, $form.x, $form.y) {
            (Op::None, _, _) => {
                type $T = Nop;
                $body
            }
            (Op::Add, true, _) => {
                type $T = AddS;
                $body
            }
            (Op::Add, false, _) => {
                type $T = AddV;
                $body
            }
            (Op::Mul, true, _) => {
                type $T = MulS;
                $body
            }
            (Op::Mul, false, _) => {
                type $T = MulV;
                $body
            }
            (Op::Relu, _, _) => {
                type $T = Relu;
                $body
            }
            (Op::MulAdd, true, true) => {
                type $T = MaSS;
                $body
            }
            (Op::MulAdd, true, false) => {
                type $T = MaSV;
                $body
            }
            (Op::MulAdd, false, true) => {
                type $T = MaVS;
                $body
            }
            (Op::MulAdd, false, false) => {
                type $T = MaVV;
                $body
            }
        }
    };
}

/// Chooses the compiled loop for a strict tile: up to three steps.
fn select_strict<R: Arch>(start: bool, f: [Form; TILE_STEPS]) -> KernelFn {
    fn third<R: Arch, S: Start, A: Step, B: Step>(f: [Form; TILE_STEPS]) -> KernelFn {
        with_step!(f[2], C => R::run::<S, A, B, C>, plain)
    }
    fn second<R: Arch, S: Start, A: Step>(f: [Form; TILE_STEPS]) -> KernelFn {
        if f[1].op == Op::None {
            return R::run::<S, A, Nop, Nop>;
        }
        with_step!(f[1], B => third::<R, S, A, B>(f), plain)
    }
    fn first<R: Arch, S: Start>(f: [Form; TILE_STEPS]) -> KernelFn {
        if f[0].op == Op::None {
            return R::run::<S, Nop, Nop, Nop>;
        }
        with_step!(f[0], A => second::<R, S, A>(f), plain)
    }
    if start { first::<R, FromSlice>(f) } else { first::<R, FromValue>(f) }
}

/// Chooses the compiled loop for a `Numeric::Fast` tile: up to two steps, multiply-adds counting once.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn select_fast<R: Arch>(start: bool, f: [Form; TILE_STEPS]) -> KernelFn {
    fn second<R: Arch, S: Start, A: Step>(f: [Form; TILE_STEPS]) -> KernelFn {
        with_step!(f[1], B => R::run::<S, A, B, Nop>, fused)
    }
    fn first<R: Arch, S: Start>(f: [Form; TILE_STEPS]) -> KernelFn {
        if f[0].op == Op::None {
            return R::run::<S, Nop, Nop, Nop>;
        }
        with_step!(f[0], A => second::<R, S, A>(f), fused)
    }
    if start { first::<R, FromSlice>(f) } else { first::<R, FromValue>(f) }
}

/// The chunk loop, shared by every instantiation: one indirect call per tile per chunk. Loops and
/// operands are bound once, here, because a run's bindings cannot change between chunks.
fn elementwise_loop(
    tiles: &[Tile],
    registers_used: usize,
    sources: &[Source<'_>],
    scratch: &mut [Element],
    out: &mut [Element],
    chunk: usize,
    select: impl Fn(bool, [Form; TILE_STEPS]) -> KernelFn,
) {
    let bound = |operand: Operand| match operand {
        Operand::Constant(value) => Bound::Value(value),
        Operand::Register(r) => Bound::Register(r as usize),
        Operand::Source(s) => match &sources[s as usize] {
            Source::Direct(elements) => Bound::Direct(elements),
            Source::Splat(value) => Bound::Value(*value),
            Source::Walk(walk) => Bound::Walked(walk.register),
        },
    };
    let is_slice = |operand: Operand| !matches!(bound(operand), Bound::Value(_));
    let form = |step: TileStep| Form {
        op: step.op,
        x: is_slice(step.x),
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        y: is_slice(step.y),
    };
    let bound_tiles: Inline<(KernelFn, Dst, Binding<'_>), 8> = tiles
        .iter()
        .map(|tile| {
            let kernel = select(is_slice(tile.start), [form(tile.steps[0]), form(tile.steps[1]), form(tile.steps[2])]);
            let binding = Binding {
                start: bound(tile.start),
                x: [bound(tile.steps[0].x), bound(tile.steps[1].x), bound(tile.steps[2].x)],
                y: [bound(tile.steps[0].y), bound(tile.steps[1].y), bound(tile.steps[2].y)],
            };
            (kernel, tile.dst, binding)
        })
        .collect();
    let any_walk = sources.iter().any(|source| matches!(source, Source::Walk(_)));
    let width = out.len().min(chunk);
    let (registers, walked) = scratch.split_at_mut(registers_used * width);
    let mut start = 0;
    for out_chunk in out.chunks_mut(chunk) {
        let len = out_chunk.len();
        if any_walk {
            for source in sources {
                if let Source::Walk(walk) = source {
                    walk.fill(start, &mut walked[walk.register * width..][..len]);
                }
            }
        }
        for (kernel, dst, binding) in bound_tiles.iter() {
            match *dst {
                Dst::Output => {
                    let frame =
                        Frame { boundary: registers_used, before: registers, after: &[], walked, width, start, len };
                    kernel(&frame, binding, out_chunk);
                }
                Dst::Register(r) => {
                    let r = r as usize;
                    let (before, rest) = registers.split_at_mut(r * width);
                    let (register, after) = rest.split_at_mut(width);
                    let frame = Frame { boundary: r, before, after, walked, width, start, len };
                    kernel(&frame, binding, &mut register[..len]);
                }
            }
        }
        start += len;
    }
}

/// Scratch lives only for the charged call. No thread-local allocation survives its budget.
/// The planner writes every register before reading it.
pub(crate) fn with_scratch<R>(bytes: usize, body: impl FnOnce(&mut [u8]) -> R) -> crate::Result<R> {
    if bytes > 0 && !crate::compute::may_allocate(bytes) {
        return Err(crate::compute::error("scratch allocation failed"));
    }
    let mut scratch = crate::compute::storage::Aligned::zeroed(bytes)
        .ok_or_else(|| crate::compute::error("scratch allocation failed"))?;
    Ok(body(&mut scratch))
}
/// `len` copies of `value`, reporting allocation failure. Touches the heap only when `len` is not 0.
pub(crate) fn filled_scratch<T: Clone>(len: usize, value: T) -> crate::Result<Vec<T>> {
    let mut scratch = Vec::new();
    if len > 0 && !crate::compute::may_allocate(len * size_of::<T>()) {
        return Err(crate::compute::error("scratch allocation failed"));
    }
    scratch.try_reserve_exact(len).map_err(|_| crate::compute::error("scratch allocation failed"))?;
    scratch.resize(len, value);
    Ok(scratch)
}

/// Scratch elements one elementwise stage needs for `count` output elements.
pub(crate) fn scratch_elements(stage: &Elementwise, sources: &[Source<'_>], count: usize, chunk: usize) -> usize {
    let walked = sources.iter().filter(|s| matches!(s, Source::Walk(_))).count();
    (stage.registers + walked) * count.min(chunk)
}

/// A short list held inline, so the common small plan binds without touching the heap, spilling for
/// the rare long one.
pub(crate) struct Inline<T: Copy, const N: usize> {
    items: [std::mem::MaybeUninit<T>; N],
    len: usize,
    spilled: Vec<T>,
}
impl<T: Copy, const N: usize> Inline<T, N> {
    pub(crate) fn new() -> Self {
        Self { items: [std::mem::MaybeUninit::uninit(); N], len: 0, spilled: Vec::new() }
    }
    #[inline]
    pub(crate) fn push(&mut self, item: T) {
        if self.spilled.is_empty() && self.len < N {
            self.items[self.len].write(item);
            self.len += 1;
        } else {
            if self.spilled.is_empty() {
                let held: &[T] = self;
                self.spilled = held.to_vec();
            }
            self.spilled.push(item);
        }
    }
}
impl<T: Copy, const N: usize> FromIterator<T> for Inline<T, N> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut list = Self::new();
        for item in items {
            list.push(item);
        }
        list
    }
}
impl<T: Copy, const N: usize> std::ops::Deref for Inline<T, N> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        if self.spilled.is_empty() {
            // SAFETY: `push` initialized the first `len` items, and `len <= N`.
            unsafe { std::slice::from_raw_parts(self.items.as_ptr().cast::<T>(), self.len) }
        } else {
            &self.spilled
        }
    }
}

/// Resolves every view against `shape` and numbers the walked ones.
pub(crate) fn bind_sources<'a>(views: impl Iterator<Item = View<'a>>, shape: Shape) -> Inline<Source<'a>, 8> {
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
    assert!(isa.available(), "{isa:?} is not available on this host");
    let r = stage.registers;
    match (isa, stage.fast.is_empty()) {
        #[cfg(target_arch = "x86_64")]
        (Isa::Fma, false) => elementwise_loop(&stage.fast, r, sources, scratch, out, chunk, select_fast::<x86::Fma>),
        // AArch64's baseline already has the instruction `mul_add` compiles to.
        #[cfg(target_arch = "aarch64")]
        (Isa::Fma, false) => elementwise_loop(&stage.fast, r, sources, scratch, out, chunk, select_fast::<Base>),
        #[cfg(target_arch = "x86_64")]
        (Isa::Avx2 | Isa::Fma, _) => {
            elementwise_loop(&stage.tiles, r, sources, scratch, out, chunk, select_strict::<x86::Avx2>);
        }
        _ => elementwise_loop(&stage.tiles, r, sources, scratch, out, chunk, select_strict::<Base>),
    }
}

/// Output rows and columns one matmul microkernel holds in registers, and the narrower column
/// block short remainders use. Both fit in sixteen 256-bit registers and in thirty-two NEON ones.
const MR: usize = 6;
const NR: usize = 16;
const NR_NARROW: usize = 8;
/// Columns per packed pass, so one pass of `b` (`K_BLOCK * N_BLOCK * 4` bytes) fits half of the
/// smallest second-level cache this targets. `k` still ascends for every element, inside each pass.
const N_BLOCK: usize = 256;

/// The width a remainder of `columns < NR` packs to: its lanes past `columns` hold zeros and their
/// results are discarded, so a remainder costs one fixed kernel and never a scalar loop.
const fn remainder_width(columns: usize) -> usize {
    if columns == 0 {
        0
    } else if columns <= NR_NARROW {
        NR_NARROW
    } else {
        NR
    }
}

/// Where one kernel's output block sits: `rows` rows of `cols <= WIDTH` elements each.
#[derive(Clone, Copy)]
struct Block {
    base: usize,
    stride: usize,
    cols: usize,
}

/// Accumulates one `ROWS x WIDTH` output block over ascending rows of `b`, with the block held in
/// SIMD registers throughout. `scales` holds `ROWS` runs of `a`, read in place, one element for each
/// of the `rows` rows of `b`; broadcasting straight from `a` costs what a packed copy would, without
/// the copy. Every
/// element still adds its products in increasing `k`, starting from what the previous block left,
/// so strict results are bit-identical to the triple loop. With `FUSED`, each product and add round
/// once instead, which only `Numeric::Fast` asks for.
///
/// Only fixed-size arrays touch the block between its load and its store, which keeps most of it in
/// registers: LLVM cannot split an array that a variable index reaches. The largest block still
/// leaves a row on the stack, so [`fused_6x16`] takes that size on the path that has to be fastest.
#[inline(always)]
fn at_unchecked(run: &[Element], p: usize) -> f32 {
    debug_assert!(p < run.len());
    // SAFETY: `micro` asserts each run has `rows` elements, and `p` counts up from 0 below `rows`.
    read(unsafe { *run.get_unchecked(p) })
}

#[inline(always)]
fn micro<const FUSED: bool, const ROWS: usize, const WIDTH: usize>(
    scales: [&[Element]; ROWS],
    rows: usize,
    factors: impl Iterator<Item = [Element; WIDTH]>,
    out: &mut [Element],
    at: Block,
) {
    for run in scales {
        assert_eq!(run.len(), rows);
    }
    let mut block = [[0.0f32; WIDTH]; ROWS];
    for (r, row) in block.iter_mut().enumerate() {
        let mut source = [[0u8; 4]; WIDTH];
        source[..at.cols].copy_from_slice(&out[at.base + r * at.stride..][..at.cols]);
        *row = source.map(read);
    }
    for (p, factor) in (0..rows).zip(factors) {
        let factor: [f32; WIDTH] = factor.map(read);
        for (row, run) in block.iter_mut().zip(&scales) {
            // A checked index here would keep a compare and a branch for each row of each product.
            let scale = at_unchecked(run, p);
            for (value, f) in row.iter_mut().zip(&factor) {
                *value = if FUSED { scale.mul_add(*f, *value) } else { *value + scale * *f };
            }
        }
    }
    for (r, row) in block.iter().enumerate() {
        let target = row.map(f32::to_le_bytes);
        out[at.base + r * at.stride..][..at.cols].copy_from_slice(&target[..at.cols]);
    }
}

/// The full-size kernels, their twelve accumulators named. LLVM kept one row of [`micro`]'s block on
/// the stack, reading it back through failed store-forwards on every call.
#[cfg(target_arch = "x86_64")]
macro_rules! kernel_6x16 {
    ($name:ident, $features:literal, $step:path) => {
        // Never inlined, so that LLVM can't see how the six runs relate and rebuild their addresses
        // from one register each iteration.
        #[inline(never)]
        #[target_feature(enable = $features)]
        #[allow(clippy::cast_ptr_alignment)] // Every access below tolerates any alignment.
        fn $name(scales: [&[Element]; 6], factors: Rows16<'_>, out: &mut [Element], at: Block) {
            use std::arch::x86_64::{_mm256_loadu_ps, _mm256_set1_ps, _mm256_storeu_ps};
            let rows = scales[0].len();
            for run in scales {
                assert_eq!(run.len(), rows);
            }
            assert!(rows == 0 || (rows - 1) * factors.step + 16 <= factors.elements.len());
            assert!(at.cols == 16 && at.base + 5 * at.stride + 16 <= out.len());
            let base = out[at.base..].as_mut_ptr().cast::<f32>();
            let [s0, s1, s2, s3, s4, s5] = scales.map(|run| run.as_ptr().cast::<f32>());
            // SAFETY: the assertions put six rows of 16 elements, `stride` apart, inside `out`, `rows`
            // rows of 16 factors, `step` apart, inside `elements`, and a scale in each run for every
            // one of them. Every access tolerates any alignment, and x86 is little-endian, so these
            // are the f32s `read` would give. `f` only leaves `elements` after the last of them.
            unsafe {
                let row = |r: usize| base.add(r * at.stride);
                let (mut a0, mut b0) = (_mm256_loadu_ps(row(0)), _mm256_loadu_ps(row(0).add(8)));
                let (mut a1, mut b1) = (_mm256_loadu_ps(row(1)), _mm256_loadu_ps(row(1).add(8)));
                let (mut a2, mut b2) = (_mm256_loadu_ps(row(2)), _mm256_loadu_ps(row(2).add(8)));
                let (mut a3, mut b3) = (_mm256_loadu_ps(row(3)), _mm256_loadu_ps(row(3).add(8)));
                let (mut a4, mut b4) = (_mm256_loadu_ps(row(4)), _mm256_loadu_ps(row(4).add(8)));
                let (mut a5, mut b5) = (_mm256_loadu_ps(row(5)), _mm256_loadu_ps(row(5).add(8)));
                let mut f = factors.elements.as_ptr().cast::<f32>();
                for p in 0..rows {
                    let (lo, hi) = (_mm256_loadu_ps(f), _mm256_loadu_ps(f.add(8)));
                    let x = _mm256_set1_ps(s0.add(p).read_unaligned());
                    a0 = $step(x, lo, a0);
                    b0 = $step(x, hi, b0);
                    let x = _mm256_set1_ps(s1.add(p).read_unaligned());
                    a1 = $step(x, lo, a1);
                    b1 = $step(x, hi, b1);
                    let x = _mm256_set1_ps(s2.add(p).read_unaligned());
                    a2 = $step(x, lo, a2);
                    b2 = $step(x, hi, b2);
                    let x = _mm256_set1_ps(s3.add(p).read_unaligned());
                    a3 = $step(x, lo, a3);
                    b3 = $step(x, hi, b3);
                    let x = _mm256_set1_ps(s4.add(p).read_unaligned());
                    a4 = $step(x, lo, a4);
                    b4 = $step(x, hi, b4);
                    let x = _mm256_set1_ps(s5.add(p).read_unaligned());
                    a5 = $step(x, lo, a5);
                    b5 = $step(x, hi, b5);
                    f = f.wrapping_add(factors.step);
                }
                _mm256_storeu_ps(row(0), a0);
                _mm256_storeu_ps(row(0).add(8), b0);
                _mm256_storeu_ps(row(1), a1);
                _mm256_storeu_ps(row(1).add(8), b1);
                _mm256_storeu_ps(row(2), a2);
                _mm256_storeu_ps(row(2).add(8), b2);
                _mm256_storeu_ps(row(3), a3);
                _mm256_storeu_ps(row(3).add(8), b3);
                _mm256_storeu_ps(row(4), a4);
                _mm256_storeu_ps(row(4).add(8), b4);
                _mm256_storeu_ps(row(5), a5);
                _mm256_storeu_ps(row(5).add(8), b5);
            }
        }
    };
}
/// Rows of 16 factors, `step` elements apart: adjacent when packed, a column block of `b` when not.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
struct Rows16<'a> {
    elements: &'a [Element],
    step: usize,
}
/// One rounding per product and sum.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2,fma")]
fn fused_step(
    scale: std::arch::x86_64::__m256,
    factor: std::arch::x86_64::__m256,
    sum: std::arch::x86_64::__m256,
) -> std::arch::x86_64::__m256 {
    std::arch::x86_64::_mm256_fmadd_ps(scale, factor, sum)
}
/// A rounding for the product and another for the sum, with no contraction: intrinsics are exact.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
fn strict_step(
    scale: std::arch::x86_64::__m256,
    factor: std::arch::x86_64::__m256,
    sum: std::arch::x86_64::__m256,
) -> std::arch::x86_64::__m256 {
    use std::arch::x86_64::{_mm256_add_ps, _mm256_mul_ps};
    _mm256_add_ps(sum, _mm256_mul_ps(scale, factor))
}
#[cfg(target_arch = "x86_64")]
kernel_6x16!(fused_6x16, "avx2,fma", fused_step);
#[cfg(target_arch = "x86_64")]
kernel_6x16!(strict_6x16, "avx2", strict_step);

/// One pass's view of `b`: `WIDTH` elements per row of `b`, from column `column` of the pass, padded
/// with zeros past `cols`.
trait Factors: Copy {
    fn column<const WIDTH: usize>(self, column: usize, cols: usize) -> impl Iterator<Item = [Element; WIDTH]>;
    /// The rows of the 16 columns from column `column`, as they lie in memory.
    #[cfg(target_arch = "x86_64")]
    fn rows16(&self, column: usize) -> Rows16<'_>;
}
/// Rows of `b` read in place: no copy, for the products too short to repay one.
#[derive(Clone, Copy)]
struct InPlace<'a> {
    b: &'a [Element],
    n: usize,
    first_column: usize,
    start: usize,
    end: usize,
}
impl Factors for InPlace<'_> {
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn rows16(&self, column: usize) -> Rows16<'_> {
        Rows16 { elements: &self.b[self.start * self.n + self.first_column + column..], step: self.n }
    }
    #[inline(always)]
    fn column<const WIDTH: usize>(self, column: usize, cols: usize) -> impl Iterator<Item = [Element; WIDTH]> {
        (self.start..self.end).map(move |p| {
            let mut row = [[0u8; 4]; WIDTH];
            row[..cols].copy_from_slice(&self.b[p * self.n + self.first_column + column..][..cols]);
            row
        })
    }
}
/// One pass of `b`, packed by column block: each block's rows are adjacent, at `rows` elements per
/// column of the pass.
#[derive(Clone, Copy)]
struct Packed<'a> {
    pack: &'a [Element],
    rows: usize,
}
impl Factors for Packed<'_> {
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn rows16(&self, column: usize) -> Rows16<'_> {
        Rows16 { elements: &self.pack[column * self.rows..][..self.rows * 16], step: 16 }
    }
    #[inline(always)]
    fn column<const WIDTH: usize>(self, column: usize, _cols: usize) -> impl Iterator<Item = [Element; WIDTH]> {
        self.pack[column * self.rows..][..self.rows * WIDTH].as_chunks::<WIDTH>().0.iter().copied()
    }
}
/// Packs rows `start..end` of columns `first..first + cols` of `b` for [`Packed`].
fn pack_pass(b: &[Element], n: usize, first: usize, cols: usize, start: usize, end: usize, pack: &mut [Element]) {
    let rows = end - start;
    let whole = cols / NR * NR;
    let tail = remainder_width(cols - whole);
    let (blocks, rest) = pack[..rows * (whole + tail)].split_at_mut(rows * whole);
    for (block, target) in blocks.chunks_exact_mut(rows * NR).enumerate() {
        for (p, row) in (start..end).zip(target.as_chunks_mut::<NR>().0) {
            row.copy_from_slice(&b[p * n + first + block * NR..][..NR]);
        }
    }
    if tail != 0 {
        for (p, row) in (start..end).zip(rest.chunks_exact_mut(tail)) {
            // The lanes past `cols` are computed and discarded, so whatever they hold can't matter.
            row[..cols - whole].copy_from_slice(&b[p * n + first + whole..][..cols - whole]);
        }
    }
}

/// What every kernel of one pass over `b` shares.
#[derive(Clone, Copy)]
struct Pass<'a> {
    a: &'a [Element],
    k: usize,
    start: usize,
    rows: usize,
    first: usize,
    n: usize,
    /// Whether this instantiation may call the explicit x86 kernels.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    vectors: bool,
}
/// One `ROWS x WIDTH` block of the output.
#[inline(always)]
fn group<const FUSED: bool, const ROWS: usize, const WIDTH: usize>(
    pass: Pass<'_>,
    factors: impl Factors,
    out: &mut [Element],
    row: usize,
    (column, cols): (usize, usize),
) {
    let Pass { a, k, start, rows, first, n, .. } = pass;
    let scales: [&[Element]; ROWS] = std::array::from_fn(|r| &a[(row + r) * k + start..][..rows]);
    let at = Block { base: row * n + first + column, stride: n, cols };
    #[cfg(target_arch = "x86_64")]
    if pass.vectors && ROWS == 6 && WIDTH == 16 && cols == 16 {
        let six: [&[Element]; 6] = scales[..].try_into().expect("six rows");
        let factors = factors.rows16(column);
        // SAFETY: `vectors` is only set by `matmul_avx2` and `matmul_fma`, which `matmul` reaches only
        // after `Isa::available` confirms their features, and only `matmul_fma` sets `FUSED`.
        unsafe {
            if FUSED {
                fused_6x16(six, factors, out, at);
            } else {
                strict_6x16(six, factors, out, at);
            }
        }
        return;
    }
    micro::<FUSED, ROWS, WIDTH>(scales, rows, factors.column::<WIDTH>(column, cols), out, at);
}
/// One row group, across every column block of the pass. This order keeps the group's runs of `a`
/// and its stretch of `out` in the first-level cache, while `b`'s packed blocks stream from the
/// second. The other order, one block of `b` across all rows, measured 15% slower at 256x256x256:
/// `a` and `out` then take the misses, and `out`'s are scattered.
#[inline(always)]
fn row_block<const FUSED: bool, const ROWS: usize>(
    pass: Pass<'_>,
    factors: impl Factors,
    out: &mut [Element],
    row: usize,
    cols: usize,
) {
    let whole = cols / NR * NR;
    for column in (0..whole).step_by(NR) {
        group::<FUSED, ROWS, NR>(pass, factors, out, row, (column, NR));
    }
    let tail = cols - whole;
    match remainder_width(tail) {
        0 => {}
        NR_NARROW => group::<FUSED, ROWS, NR_NARROW>(pass, factors, out, row, (whole, tail)),
        _ => group::<FUSED, ROWS, NR>(pass, factors, out, row, (whole, tail)),
    }
}
/// Every row group of one pass over `b`, `cols` columns wide.
#[inline(always)]
fn whole_pass<const FUSED: bool>(pass: Pass<'_>, factors: impl Factors, out: &mut [Element], m: usize, cols: usize) {
    let whole = m / MR * MR;
    for row in (0..whole).step_by(MR) {
        row_block::<FUSED, MR>(pass, factors, out, row, cols);
    }
    match m - whole {
        0 => {}
        1 => row_block::<FUSED, 1>(pass, factors, out, whole, cols),
        2 => row_block::<FUSED, 2>(pass, factors, out, whole, cols),
        3 => row_block::<FUSED, 3>(pass, factors, out, whole, cols),
        4 => row_block::<FUSED, 4>(pass, factors, out, whole, cols),
        _ => row_block::<FUSED, 5>(pass, factors, out, whole, cols),
    }
}

/// One output row, streaming `b` by rows. Four rows at a time hold each output in a register across
/// four products, instead of loading and storing it for each one.
#[inline(always)]
fn stream_row<const FUSED: bool>(a: &[Element], b: &[Element], n: usize, out: &mut [Element]) {
    let term = |sum: f32, scale: f32, factor: Element| {
        if FUSED { scale.mul_add(read(factor), sum) } else { sum + scale * read(factor) }
    };
    const STREAM_ROWS: usize = 4;
    const STREAM_WIDTH: usize = 8;
    let (scales, rest) = a.as_chunks::<STREAM_ROWS>();
    let mut rows = b.chunks_exact(n);
    for scale in scales {
        let s = scale.map(read);
        let lanes: [&[Element]; STREAM_ROWS] = std::array::from_fn(|_| rows.next().expect("k rows of n columns"));
        let (blocks, tail) = out.as_chunks_mut::<STREAM_WIDTH>();
        let [r0, r1, r2, r3] = lanes.map(|lane| lane.as_chunks::<STREAM_WIDTH>());
        for ((((block, x0), x1), x2), x3) in blocks.iter_mut().zip(r0.0).zip(r1.0).zip(r2.0).zip(r3.0) {
            let (mut sums, x0, x1, x2, x3) = (block.map(read), *x0, *x1, *x2, *x3);
            for l in 0..STREAM_WIDTH {
                sums[l] = term(term(term(term(sums[l], s[0], x0[l]), s[1], x1[l]), s[2], x2[l]), s[3], x3[l]);
            }
            *block = sums.map(f32::to_le_bytes);
        }
        for ((((slot, x0), x1), x2), x3) in tail.iter_mut().zip(r0.1).zip(r1.1).zip(r2.1).zip(r3.1) {
            *slot = term(term(term(term(read(*slot), s[0], *x0), s[1], *x1), s[2], *x2), s[3], *x3).to_le_bytes();
        }
    }
    for (scale, row) in rest.iter().zip(rows) {
        let scale = read(*scale);
        for (slot, factor) in out.iter_mut().zip(row) {
            *slot = term(read(*slot), scale, *factor).to_le_bytes();
        }
    }
}

/// Elements in the lanes of [`relaxed_dot`], and the shortest row it pays for: the ordered kernel
/// overlaps four rows in the pipeline, which beats combining lanes for every row until then.
const RELAXED_LANES: usize = 32;
const RELAXED_FROM: usize = 64;

/// A dot product under `Numeric::Fast`, accumulated in fixed lanes and combined in a fixed order.
/// The lane count is not the vector width, so this gives the same bits on every fusing CPU. Rounding
/// differs from an ordered sum's but stays within its bound, `k * u * sum(|a[i] * b[i]|)` to first
/// order; cancellation can make that large relative to the result, as in any floating-point sum.
#[inline(always)]
fn relaxed_dot(a: &[Element], b: &[Element]) -> f32 {
    let (left, left_tail) = a.as_chunks::<RELAXED_LANES>();
    let (right, right_tail) = b.as_chunks::<RELAXED_LANES>();
    let mut lanes = [[0.0f32; 8]; 4];
    for (x, y) in left.iter().zip(right) {
        for (r, lane) in lanes.iter_mut().enumerate() {
            for (l, sum) in lane.iter_mut().enumerate() {
                *sum = read(x[r * 8 + l]).mul_add(read(y[r * 8 + l]), *sum);
            }
        }
    }
    let [l0, l1, l2, l3] = lanes;
    let s: [f32; 8] = std::array::from_fn(|l| (l0[l] + l2[l]) + (l1[l] + l3[l]));
    let total = ((s[0] + s[4]) + (s[2] + s[6])) + ((s[1] + s[5]) + (s[3] + s[7]));
    left_tail.iter().zip(right_tail).fold(total, |sum, (x, y)| read(*x).mul_add(read(*y), sum))
}

/// One output column: independent ordered sums, four at once, overlapping in the pipeline.
#[inline(always)]
fn column_rows<const FUSED: bool>(a: &[Element], b: &[Element], k: usize, out: &mut [Element]) {
    if FUSED && k >= RELAXED_FROM {
        for (slot, row) in out.iter_mut().zip(a.chunks_exact(k)) {
            *slot = relaxed_dot(row, b).to_le_bytes();
        }
        return;
    }
    const ROWS: usize = 4;
    let term = |sum: f32, left: Element, right: f32| {
        if FUSED { read(left).mul_add(right, sum) } else { sum + read(left) * right }
    };
    let (groups, rest) = out.as_chunks_mut::<ROWS>();
    let mut rows = a.chunks_exact(k);
    for group in groups {
        let mut next = || &rows.next().expect("m rows of k columns")[..b.len()];
        let lanes = [next(), next(), next(), next()];
        let mut sums = [0.0f32; ROWS];
        for (p, weight) in b.iter().enumerate() {
            let weight = read(*weight);
            for (sum, lane) in sums.iter_mut().zip(&lanes) {
                *sum = term(*sum, lane[p], weight);
            }
        }
        *group = sums.map(f32::to_le_bytes);
    }
    for (slot, row) in rest.iter_mut().zip(rows) {
        let sum = row.iter().zip(b).fold(0.0f32, |sum, (left, right)| term(sum, *left, read(*right)));
        *slot = sum.to_le_bytes();
    }
}

/// One packing pass per block of rows of `b` and per block of its columns, in the order `matmul_body`
/// reads them.
fn passes(k: usize, n: usize) -> impl Iterator<Item = (usize, usize, usize, usize)> {
    (0..n).step_by(N_BLOCK).flat_map(move |first| {
        let cols = (n - first).min(N_BLOCK);
        (0..k).step_by(K_BLOCK).map(move |start| (start, (start + K_BLOCK).min(k), first, cols))
    })
}
fn pass_elements(start: usize, end: usize, cols: usize) -> usize {
    (end - start) * (cols / NR * NR + remainder_width(cols % NR))
}
/// Elements [`prepare_right`] writes for a `k x n` operand.
pub(crate) fn prepared_right_elements(k: usize, n: usize) -> usize {
    if k == 0 {
        return 0;
    }
    passes(k, n).map(|(start, end, _, cols)| pass_elements(start, end, cols)).sum()
}
/// Packs all of `b`, every pass adjacent, in the order `matmul_body` reads them.
pub(crate) fn prepare_right(b: &[Element], k: usize, n: usize, mut pack: &mut [Element]) {
    if k == 0 {
        return;
    }
    for (start, end, first, cols) in passes(k, n) {
        let (pass, rest) = pack.split_at_mut(pass_elements(start, end, cols));
        pack_pass(b, n, first, cols, start, end, pass);
        pack = rest;
    }
}
/// Rows of `a` per column panel, one lane each.
const PANEL: usize = NR;
/// Elements [`prepare_left`] writes for an `m x k` operand.
pub(crate) const fn prepared_left_elements(m: usize, k: usize) -> usize {
    (m / PANEL * PANEL + remainder_width(m % PANEL)) * k
}
/// Packs `a` as column panels: for each panel of rows and each column of `a`, one lane per row.
pub(crate) fn prepare_left(a: &[Element], m: usize, k: usize, panels: &mut [Element]) {
    if k == 0 {
        return;
    }
    let whole = m / PANEL * PANEL;
    let mut rest = panels;
    let mut row = 0;
    while row < m {
        let lanes = if row < whole { PANEL } else { remainder_width(m - whole) };
        let (panel, after) = rest.split_at_mut(lanes * k);
        for (p, column) in panel.chunks_exact_mut(lanes).enumerate() {
            for (lane, slot) in column.iter_mut().enumerate() {
                *slot = if row + lane < m { a[(row + lane) * k + p] } else { [0; 4] };
            }
        }
        rest = after;
        row += lanes;
    }
}
/// A weight matrix in column panels times one column: every strict addition of every row happens in
/// increasing `k`, across SIMD lanes rather than one register per row.
#[inline(always)]
fn panels_times_column<const FUSED: bool>(panels: &[Element], b: &[Element], k: usize, out: &mut [Element]) {
    let m = out.len();
    let whole = m / PANEL * PANEL;
    let tail = m - whole;
    for start in (0..k).step_by(K_BLOCK) {
        let end = (start + K_BLOCK).min(k);
        let rows = end - start;
        let scales = [&b[start..end]];
        for row in (0..whole).step_by(PANEL) {
            let columns = &panels[row * k + start * PANEL..][..rows * PANEL];
            let factors = columns.as_chunks::<PANEL>().0.iter().copied();
            micro::<FUSED, 1, PANEL>(scales, rows, factors, out, Block { base: row, stride: 0, cols: PANEL });
        }
        let last = &panels[whole * k..];
        let at = Block { base: whole, stride: 0, cols: tail };
        match remainder_width(tail) {
            0 => {}
            NR_NARROW => {
                let columns = &last[start * NR_NARROW..][..rows * NR_NARROW];
                micro::<FUSED, 1, NR_NARROW>(scales, rows, columns.as_chunks::<NR_NARROW>().0.iter().copied(), out, at);
            }
            _ => {
                let columns = &last[start * NR..][..rows * NR];
                micro::<FUSED, 1, NR>(scales, rows, columns.as_chunks::<NR>().0.iter().copied(), out, at);
            }
        }
    }
}

/// Bytes of scratch `matmul` needs: one pass of `b`, packed by column block. Nothing when the call
/// does no packing, which includes every product with no elements or no inner dimension and every
/// product with a single row group to read the packing.
pub(crate) fn matmul_scratch(m: usize, k: usize, n: usize) -> usize {
    if m <= MR || n <= 1 {
        return 0;
    }
    let widest = if n >= N_BLOCK { N_BLOCK } else { n / NR * NR + remainder_width(n % NR) };
    k.min(K_BLOCK) * widest * 4
}

/// Most scalar products a plain loop beats the block machinery at, whose setup and partial blocks
/// cost more than this many multiply-adds.
const TINY_PRODUCT: usize = 512;
/// A small product by the textbook loop, each element adding its products in increasing `k`.
#[inline(always)]
fn tiny_product<const FUSED: bool>(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element]) {
    for (row, sums) in a.chunks_exact(k).zip(out.chunks_exact_mut(n)) {
        for (scale, factors) in row.iter().zip(b.chunks_exact(n)) {
            let scale = read(*scale);
            for (sum, factor) in sums.iter_mut().zip(factors) {
                let value = read(*sum);
                let factor = read(*factor);
                *sum = (if FUSED { scale.mul_add(factor, value) } else { value + scale * factor }).to_le_bytes();
            }
        }
    }
}

/// The row-major `a @ b` kernel. Each pass of `b` covers ascending `k`, so each output element sees
/// its products in increasing `k` whichever order its neighbours are computed in.
#[inline(always)]
fn matmul_body<const FUSED: bool>(
    operands: &Operands<'_>,
    out: &mut [Element],
    scratch: &mut [Element],
    vectors: bool,
) {
    let &Operands { a, b, k, n, left, right } = operands;
    let m = out.len() / n;
    if m * k * n <= TINY_PRODUCT && k < RELAXED_FROM && left.is_none() && right.is_none() {
        return tiny_product::<FUSED>(a, b, k, n, out);
    }
    if n == 1 {
        if let Some(panels) = left {
            return panels_times_column::<FUSED>(panels, b, k, out);
        }
        return column_rows::<FUSED>(a, b, k, out);
    }
    if m == 1 {
        return stream_row::<FUSED>(a, b, n, out);
    }
    let mut prepared = right;
    for (start, end, first, cols) in passes(k, n) {
        let pass = Pass { a, k, start, rows: end - start, first, n, vectors };
        // A single row group would read a packing once, which a copy cannot repay.
        if m <= MR && prepared.is_none() {
            let factors = InPlace { b, n, first_column: first, start, end };
            whole_pass::<FUSED>(pass, factors, out, m, cols);
            continue;
        }
        let pack: &[Element] = if let Some(weights) = prepared {
            let (this, rest) = weights.split_at(pass_elements(start, end, cols));
            prepared = Some(rest);
            this
        } else {
            pack_pass(b, n, first, cols, start, end, scratch);
            scratch
        };
        whole_pass::<FUSED>(pass, Packed { pack, rows: end - start }, out, m, cols);
    }
}
/// A matmul's inputs: contiguous operands, unless weights packed ahead of time stand in for one.
pub(crate) struct Operands<'a> {
    pub(crate) a: &'a [Element],
    pub(crate) b: &'a [Element],
    pub(crate) k: usize,
    pub(crate) n: usize,
    pub(crate) left: Option<&'a [Element]>,
    pub(crate) right: Option<&'a [Element]>,
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn matmul_avx2(operands: &Operands<'_>, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<false>(operands, out, scratch, true);
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn matmul_fma(operands: &Operands<'_>, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<true>(operands, out, scratch, true);
}
/// `aarch64` always has the fused instruction, so its fused instantiation needs no target feature.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn matmul_fma(operands: &Operands<'_>, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<true>(operands, out, scratch, false);
}
#[inline(never)]
fn matmul_baseline(operands: &Operands<'_>, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<false>(operands, out, scratch, false);
}

/// `a @ b` into row-major `out`, which holds at least one element and for which `k` is not zero:
/// [`crate::compute::Program`] answers those products without calling this. Operands are contiguous elements
/// (`packed` by the caller when their views are not); `+0` starts every element, and no zero is
/// skipped: `0 * inf` is NaN. `Isa::Fma` fuses, so `execute` only passes it for a `Numeric::Fast` program.
pub(crate) fn matmul(operands: &Operands<'_>, out: &mut [u8], scratch: &mut [u8], isa: Isa) {
    out.fill(0);
    let (out, _) = out.as_chunks_mut::<4>();
    let (scratch, _) = scratch.as_chunks_mut::<4>();
    assert!(isa.available(), "{isa:?} is not available on this host");
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was just asserted.
        Isa::Avx2 => unsafe { matmul_avx2(operands, out, scratch) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was just asserted.
        Isa::Fma => unsafe { matmul_fma(operands, out, scratch) },
        #[cfg(target_arch = "aarch64")]
        Isa::Fma => matmul_fma(operands, out, scratch),
        _ => matmul_baseline(operands, out, scratch),
    }
}
