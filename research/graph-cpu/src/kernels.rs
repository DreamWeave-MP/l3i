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
//! a program compiled with [`crate::Numeric::Fast`] uses `mul_add`, and only where asked.
// `inline(always)` is the mechanism: it is how a body gets compiled under each instantiation's
// target features. The matmul names (a, b, m, k, n, p) are the mathematics'.
#![allow(clippy::inline_always, clippy::many_single_char_names, clippy::items_after_statements)]
use crate::program::{Dst, Elementwise, Op, Operand, TILE_STEPS, Tile, TileStep};
use crate::{Shape, View};

/// Elements per dispatch. `research/results` holds the sweep behind this value.
pub const CHUNK: usize = 1024;
/// Inner-dimension rows per matmul pass, ascending so each element's addition order is intact.
const K_BLOCK: usize = 128;

/// Which instantiation of the kernels executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    Baseline,
    Avx2,
    /// AVX2 and FMA. Executes a `Numeric::Fast` program's fused tiles; strict programs run as AVX2.
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
#[cfg(target_arch = "x86_64")]
step!(MaSS, true, true, false, false, |acc, xs, ys, vx, vy, i| acc.mul_add(at(xs, i), at(ys, i)));
#[cfg(target_arch = "x86_64")]
step!(MaSV, true, false, false, true, |acc, xs, ys, vx, vy, i| acc.mul_add(at(xs, i), vy));
#[cfg(target_arch = "x86_64")]
step!(MaVS, false, true, true, false, |acc, xs, ys, vx, vy, i| acc.mul_add(vx, at(ys, i)));
#[cfg(target_arch = "x86_64")]
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
    #[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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
        #[cfg(target_arch = "x86_64")]
        y: is_slice(step.y),
    };
    let bound_tiles: Vec<(KernelFn, Dst, Binding<'_>)> = tiles
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
        for (kernel, dst, binding) in &bound_tiles {
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
pub(crate) fn with_scratch<R>(bytes: usize, body: impl FnOnce(&mut [u8]) -> R) -> l3i::Result<R> {
    let mut scratch = Vec::new();
    scratch.try_reserve_exact(bytes).map_err(|_| crate::error("scratch allocation failed"))?;
    scratch.resize(bytes, 0);
    Ok(body(&mut scratch))
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
    assert!(isa.available(), "{isa:?} is not available on this host");
    let r = stage.registers;
    match (isa, stage.fast.is_empty()) {
        #[cfg(target_arch = "x86_64")]
        (Isa::Fma, false) => elementwise_loop(&stage.fast, r, sources, scratch, out, chunk, select_fast::<x86::Fma>),
        #[cfg(target_arch = "x86_64")]
        (Isa::Avx2 | Isa::Fma, _) => {
            elementwise_loop(&stage.tiles, r, sources, scratch, out, chunk, select_strict::<x86::Avx2>);
        }
        _ => elementwise_loop(&stage.tiles, r, sources, scratch, out, chunk, select_strict::<Base>),
    }
}

/// Output rows and columns one matmul microkernel holds in registers.
const MR: usize = 6;
const NR: usize = 16;

/// Accumulates one `ROWS x NR` output block over ascending rows of `b`, with the block held in SIMD
/// registers throughout. `a` is packed, one `[f32; ROWS]` per row of `b`. Every element still adds its
/// products in increasing `k`, starting from what the previous block left, so strict results are
/// bit-identical to the triple loop. With `FUSED`, each product and add round once instead, which
/// only `Numeric::Fast` asks for.
#[inline(always)]
fn micro<const FUSED: bool, const ROWS: usize>(
    a: &[[f32; ROWS]],
    b: &[[Element; NR]],
    n: usize,
    j: usize,
    out: &mut [Element],
    i: usize,
) {
    let mut block = [[0.0f32; NR]; ROWS];
    for (r, row) in block.iter_mut().enumerate() {
        let source: &[Element; NR] = out[(i + r) * n + j..][..NR].try_into().expect("in bounds");
        for (value, element) in row.iter_mut().zip(source) {
            *value = read(*element);
        }
    }
    for (scales, b_row) in a.iter().zip(b) {
        let factor: [f32; NR] = std::array::from_fn(|c| read(b_row[c]));
        for (row, scale) in block.iter_mut().zip(scales) {
            for (value, f) in row.iter_mut().zip(&factor) {
                *value = if FUSED { scale.mul_add(*f, *value) } else { *value + *scale * *f };
            }
        }
    }
    for (r, row) in block.iter().enumerate() {
        let target: &mut [Element; NR] = (&mut out[(i + r) * n + j..][..NR]).try_into().expect("in bounds");
        for (element, value) in target.iter_mut().zip(row) {
            *element = value.to_le_bytes();
        }
    }
}

#[inline(always)]
fn accumulate1<const FUSED: bool>(out: &mut [Element], a: f32, b: &[Element]) {
    for (slot, b) in out.iter_mut().zip(b) {
        let value = read(*slot);
        *slot = (if FUSED { a.mul_add(read(*b), value) } else { value + a * read(*b) }).to_le_bytes();
    }
}

/// Bytes of scratch `matmul` needs: one `K_BLOCK` of `b`, packed by column block. Nothing when the
/// call does no packing, which includes every product with no elements or no inner dimension.
pub(crate) fn matmul_scratch(m: usize, k: usize, n: usize) -> usize {
    if m <= 1 { 0 } else { k.min(K_BLOCK) * (n / NR) * NR * 4 }
}

/// The row-major `a @ b` kernel. `k` advances in ascending blocks, so each output element sees its
/// products in increasing `k` whichever order its neighbours are computed in. Each block of `b` is
/// packed once, so column blocks read contiguous memory however wide `b`'s rows are.
#[inline(always)]
fn matmul_body<const FUSED: bool>(
    a: &[Element],
    b: &[Element],
    k: usize,
    n: usize,
    out: &mut [Element],
    scratch: &mut [Element],
) {
    let m = out.len() / n;
    // A single row has no reuse of packed weights. Stream each row of b directly,
    // retaining ascending k and the same separately rounded strict operations.
    if m == 1 {
        for (scale, row) in a.iter().zip(b.chunks_exact(n)) {
            accumulate1::<FUSED>(out, read(*scale), row);
        }
        return;
    }
    if n == 1 {
        const ROWS: usize = 4;
        let whole = m / ROWS * ROWS;
        for i in (0..whole).step_by(ROWS) {
            let mut sums = [0.0f32; ROWS];
            for p in 0..k {
                let weight = read(b[p]);
                for (r, sum) in sums.iter_mut().enumerate() {
                    let value = read(a[(i + r) * k + p]);
                    *sum = if FUSED { value.mul_add(weight, *sum) } else { *sum + value * weight };
                }
            }
            for (r, sum) in sums.iter().enumerate() {
                out[i + r] = sum.to_le_bytes();
            }
        }
        for i in whole..m {
            let mut sum = 0.0f32;
            for p in 0..k {
                let value = read(a[i * k + p]);
                let weight = read(b[p]);
                sum = if FUSED { value.mul_add(weight, sum) } else { sum + value * weight };
            }
            out[i] = sum.to_le_bytes();
        }
        return;
    }
    let (whole_rows, whole_columns) = (m / MR * MR, n / NR * NR);
    let column_blocks = n / NR;
    let (pack, _) = scratch.as_chunks_mut::<NR>();
    let mut packed = [[0.0f32; MR]; K_BLOCK];
    for start in (0..k).step_by(K_BLOCK) {
        let end = (start + K_BLOCK).min(k);
        let rows = end - start;
        if column_blocks > 0 {
            for (block, column) in pack[..column_blocks * rows].chunks_exact_mut(rows).enumerate() {
                for (p, target) in column.iter_mut().enumerate() {
                    target.copy_from_slice(&b[(start + p) * n + block * NR..][..NR]);
                }
            }
            for i in (0..whole_rows).step_by(MR) {
                // Packed once per row group, then read by every column block of that group.
                for (p, scales) in (start..end).zip(&mut packed) {
                    for (r, scale) in scales.iter_mut().enumerate() {
                        *scale = read(a[(i + r) * k + p]);
                    }
                }
                for (block, column) in pack[..column_blocks * rows].chunks_exact(rows).enumerate() {
                    micro::<FUSED, MR>(&packed[..rows], column, n, block * NR, out, i);
                }
            }
            // Leftover rows keep the same packed columns, one row in registers at a time.
            let mut single = [[0.0f32; 1]; K_BLOCK];
            for i in whole_rows..m {
                for (p, scale) in (start..end).zip(&mut single) {
                    scale[0] = read(a[i * k + p]);
                }
                for (block, column) in pack[..column_blocks * rows].chunks_exact(rows).enumerate() {
                    micro::<FUSED, 1>(&single[..rows], column, n, block * NR, out, i);
                }
            }
        }
        if whole_columns < n {
            // Keep each tail output in a scalar register across k, rather than dispatching
            // a tiny slice loop and reloading/storing the output for every product.
            for i in 0..m {
                for j in whole_columns..n {
                    let mut value = read(out[i * n + j]);
                    for p in start..end {
                        let left = read(a[i * k + p]);
                        let right = read(b[p * n + j]);
                        value = if FUSED { left.mul_add(right, value) } else { value + left * right };
                    }
                    out[i * n + j] = value.to_le_bytes();
                }
            }
        }
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn matmul_avx2(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<false>(a, b, k, n, out, scratch);
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn matmul_fma(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<true>(a, b, k, n, out, scratch);
}
#[inline(never)]
fn matmul_baseline(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [Element], scratch: &mut [Element]) {
    matmul_body::<false>(a, b, k, n, out, scratch);
}

/// `a @ b` into row-major `out`. Operands are contiguous elements (`packed` by the caller when
/// their views are not); `+0` starts every element, and no zero is skipped: `0 * inf` is NaN.
/// `Isa::Fma` fuses, so `execute` only passes it for a `Numeric::Fast` program.
pub(crate) fn matmul(a: &[Element], b: &[Element], k: usize, n: usize, out: &mut [u8], scratch: &mut [u8], isa: Isa) {
    out.fill(0);
    if out.is_empty() || k == 0 {
        return;
    }
    let (out, _) = out.as_chunks_mut::<4>();
    let (scratch, _) = scratch.as_chunks_mut::<4>();
    assert!(isa.available(), "{isa:?} is not available on this host");
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was just asserted.
        Isa::Avx2 => unsafe { matmul_avx2(a, b, k, n, out, scratch) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: availability was just asserted.
        Isa::Fma => unsafe { matmul_fma(a, b, k, n, out, scratch) },
        _ => matmul_baseline(a, b, k, n, out, scratch),
    }
}
