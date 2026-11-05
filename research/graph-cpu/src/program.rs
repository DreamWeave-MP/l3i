//! A compiled computation: typed structure and a stage plan, with no input data and no codegen.
//!
//! Planning splits the validated DAG at matrix multiplications. Each run of elementwise nodes
//! becomes one [`Elementwise`] stage, a short list of [`Kernel`]s over a small register file;
//! constants fold with the same f32 operations the kernels perform. A `Program` is immutable,
//! `Send + Sync`, and can run concurrently against any compatible inputs.
use crate::graph::{Instruction, Lease, MAX_WORK, Step};
use crate::kernels::{self, CHUNK, Element, Isa, Source};
use crate::storage::Allocation;
use crate::{Budget, Shape, Tensor, View, error};
use l3i::Result;
use std::fmt::Write;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Add,
    Mul,
    Relu,
    Copy,
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Operand {
    Constant(f32),
    Register(u16),
    Source(u16),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dst {
    Register(u16),
    Output,
}
/// One bulk operation: `dst = kind(a, b)` over a chunk. `dst` is never `a` or `b`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Kernel {
    pub(crate) kind: Kind,
    pub(crate) dst: Dst,
    pub(crate) a: Operand,
    pub(crate) b: Operand,
}
/// Steps one compiled loop can hold.
pub(crate) const TILE_STEPS: usize = 3;
/// A `Numeric::Fast` tile's limit. Fused loops are compiled for two steps, a multiply-add counting
/// as one, which keeps the number of compiled combinations in proportion.
const FAST_TILE_STEPS: usize = 2;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    None,
    Add,
    Mul,
    Relu,
    /// `value * x + y`, fused. Only in `Numeric::Fast` tiles.
    MulAdd,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct TileStep {
    pub(crate) op: Op,
    pub(crate) x: Operand,
    pub(crate) y: Operand,
}
const NO_STEP: TileStep = TileStep { op: Op::None, x: Operand::Constant(0.0), y: Operand::Constant(0.0) };
/// One compiled loop: `dst = start`, then each step applied to the running value, which stays in
/// a SIMD register. `dst` is never one of the tile's own operands.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tile {
    pub(crate) dst: Dst,
    pub(crate) start: Operand,
    pub(crate) steps: [TileStep; TILE_STEPS],
}
/// A fused elementwise computation of one materialized value.
#[derive(Debug)]
pub(crate) struct Elementwise {
    pub(crate) shape: Shape,
    pub(crate) sources: Vec<Value>,
    pub(crate) tiles: Vec<Tile>,
    /// Empty unless the program is `Numeric::Fast`.
    pub(crate) fast: Vec<Tile>,
    pub(crate) registers: usize,
    /// Operations before tiling: what the work bound charges per element.
    pub(crate) operations: usize,
}
/// A materialized value: a caller's input, or an earlier stage's result.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Value {
    Slot(u16),
    Temp(u16),
}
#[derive(Debug)]
enum RowMap {
    Softmax,
    Normalize { epsilon: f64, center: bool },
}
#[derive(Debug)]
enum Work {
    Elementwise(Elementwise),
    SumRows { source: Value, shape: Shape, columns: usize },
    RowMap { source: Value, shape: Shape, operation: RowMap },
    Transpose { source: Value, shape: Shape },
    MatMul { a: Value, b: Value, shape: Shape, inner: usize },
}
impl Work {
    fn shape(&self) -> Shape {
        match self {
            Self::Elementwise(e) => e.shape,
            Self::RowMap { shape, .. }
            | Self::SumRows { shape, .. }
            | Self::Transpose { shape, .. }
            | Self::MatMul { shape, .. } => *shape,
        }
    }
    /// Whether this stage's result depends on any element of its inputs. An empty result depends
    /// on none, however large an input is, and neither does a nonempty product with no inner
    /// dimension: it is all positive zeros.
    fn reads_inputs(&self) -> bool {
        let (rows, cols) = self.shape().dims();
        rows != 0 && cols != 0 && !matches!(self, Self::MatMul { inner: 0, .. })
    }
    fn sources(&self, mut read: impl FnMut(Value)) {
        match self {
            Self::Elementwise(e) => e.sources.iter().copied().for_each(read),
            Self::RowMap { source, .. } | Self::SumRows { source, .. } | Self::Transpose { source, .. } => {
                read(*source);
            }
            Self::MatMul { a, b, .. } => {
                read(*a);
                read(*b);
            }
        }
    }
}
#[derive(Debug)]
struct Stage {
    work: Work,
    /// Temporaries nothing later reads, released after this stage to bound peak memory.
    release: Vec<u16>,
    /// False when nothing that reaches the result reads this stage's value: running it could only
    /// spend budget. Its temporary stays unset, and whatever would read it does not.
    live: bool,
}

/// The validated, reusable computation.
#[derive(Debug)]
pub struct Program {
    _lease: Lease,
    pub(crate) steps: Vec<Step>,
    pub(crate) slots: Vec<Shape>,
    stages: Vec<Stage>,
    numeric: Numeric,
}

/// What a program promises numerically. Chosen at compile time, never implied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Numeric {
    /// Every add and multiply rounds to f32 separately: the same bits on every instantiation.
    #[default]
    Strict,
    /// `a * b + c` may round once, where the host has FMA. More accurate per step and faster, but
    /// results then depend on the host and on how the plan was tiled.
    Fast,
}

/// One node as an elementwise stage sees it.
#[derive(Clone, Copy)]
enum Planned {
    Constant(f32),
    Register(u16),
    Source(Value),
}

/// Whether `operand` reads register `r`.
fn reads(operand: Operand, r: u16) -> bool {
    matches!(operand, Operand::Register(other) if other == r)
}

/// Merges single-operation kernels into tiles wherever one kernel's only reader is the next one.
/// Add and multiply commute, except for NaN payload bits, which are not promised.
fn tile(code: &[Kernel], numeric: Numeric) -> Vec<Tile> {
    let limit = if numeric == Numeric::Fast { FAST_TILE_STEPS } else { TILE_STEPS };
    let binary = |kernel: &Kernel| matches!(kernel.kind, Kind::Add | Kind::Mul);
    let op = |kernel: &Kernel| match kernel.kind {
        Kind::Add => Op::Add,
        Kind::Mul => Op::Mul,
        Kind::Relu => Op::Relu,
        Kind::Copy => Op::None,
    };
    // Each value is one virtual register here, so its reader count decides whether it can merge.
    let mut uses = vec![0usize; code.len()];
    for kernel in code {
        for operand in [Some(kernel.a), binary(kernel).then_some(kernel.b)].into_iter().flatten() {
            if let Operand::Register(r) = operand {
                uses[r as usize] += 1;
            }
        }
    }
    let mut tiles = Vec::with_capacity(code.len());
    let mut index = 0;
    while index < code.len() {
        let first = code[index];
        let mut dst = first.dst;
        let mut steps: Vec<TileStep> = Vec::with_capacity(TILE_STEPS);
        if first.kind != Kind::Copy {
            steps.push(TileStep {
                op: op(&first),
                x: if binary(&first) { first.b } else { Operand::Constant(0.0) },
                y: Operand::Constant(0.0),
            });
        }
        while let (Dst::Register(r), Some(next)) = (dst, code.get(index + 1)) {
            if next.kind == Kind::Copy {
                break;
            }
            // The next kernel must read the running value exactly once, and nothing after it may
            // read that value before the register is written again.
            let other = match (reads(next.a, r), binary(next) && reads(next.b, r)) {
                (true, false) => next.b,
                (false, true) => next.a,
                _ => break,
            };
            if uses[r as usize] != 1 {
                break;
            }
            // A multiply followed by its add fuses, only when asked for.
            let fuses = numeric == Numeric::Fast
                && next.kind == Kind::Add
                && steps.last().is_some_and(|step| step.op == Op::Mul);
            if !fuses && steps.len() == limit {
                break;
            }
            if fuses {
                let last = steps.last_mut().expect("checked above");
                *last = TileStep { op: Op::MulAdd, x: last.x, y: other };
            } else {
                steps.push(TileStep {
                    op: op(next),
                    x: if binary(next) { other } else { Operand::Constant(0.0) },
                    y: Operand::Constant(0.0),
                });
            }
            dst = next.dst;
            index += 1;
        }
        let mut array = [NO_STEP; TILE_STEPS];
        array[..steps.len()].copy_from_slice(&steps);
        tiles.push(Tile { dst, start: first.a, steps: array });
        index += 1;
    }
    tiles
}

/// Assigns physical registers to tiled virtual ones, reusing each after its last reader. A
/// destination is chosen before its operands retire, so it is never one of them.
fn allocate(tiles: &mut [Tile]) -> usize {
    let mut last_read = std::collections::HashMap::new();
    for (index, tile) in tiles.iter().enumerate() {
        for operand in operands(tile) {
            if let Operand::Register(r) = operand {
                last_read.insert(r, index);
            }
        }
    }
    let mut physical = std::collections::HashMap::new();
    let mut free: Vec<u16> = Vec::new();
    let mut count = 0usize;
    let rename = |operand: &mut Operand, physical: &std::collections::HashMap<u16, u16>| {
        if let Operand::Register(r) = operand {
            *r = physical[r];
        }
    };
    for (index, tile) in tiles.iter_mut().enumerate() {
        let retiring: Vec<u16> =
            operands(tile).filter_map(|o| if let Operand::Register(r) = o { Some(r) } else { None }).collect();
        rename(&mut tile.start, &physical);
        for step in &mut tile.steps {
            rename(&mut step.x, &physical);
            rename(&mut step.y, &physical);
        }
        if let Dst::Register(virtual_register) = tile.dst {
            let chosen = free.pop().unwrap_or_else(|| {
                count += 1;
                u16::try_from(count - 1).expect("bounded by MAX_NODES")
            });
            physical.insert(virtual_register, chosen);
            tile.dst = Dst::Register(chosen);
        }
        for r in retiring {
            if last_read[&r] == index && !free.contains(&physical[&r]) {
                free.push(physical[&r]);
            }
        }
    }
    count
}
fn operands(tile: &Tile) -> impl Iterator<Item = Operand> + '_ {
    std::iter::once(tile.start).chain(tile.steps.iter().flat_map(|step| [step.x, step.y]))
}

/// Plans one materialized elementwise root over the steps it needs.
#[allow(clippy::too_many_lines)] // One linear pass over the nodes; splitting it hides the invariants.
fn plan_elementwise(steps: &[Step], root: usize, boundary: &[Option<Value>], numeric: Numeric) -> Elementwise {
    // Which nodes this stage computes, and each one's last reader, in topological order.
    let mut needed = vec![false; root + 1];
    needed[root] = true;
    for id in (0..=root).rev() {
        if !needed[id] || (id != root && boundary[id].is_some()) {
            continue;
        }
        let mut read = |operand: usize| needed[operand] = true;
        match steps[id].op {
            Instruction::Add(a, b) | Instruction::Mul(a, b) => {
                read(a);
                read(b);
            }
            Instruction::Relu(a) => read(a),
            Instruction::Slot(_)
            | Instruction::Constant(_)
            | Instruction::SumRows(_)
            | Instruction::Softmax(_)
            | Instruction::Transpose(_)
            | Instruction::Normalize(..)
            | Instruction::MatMul(..) => (),
        }
    }
    let mut planned: Vec<Option<Planned>> = vec![None; root + 1];
    let mut sources: Vec<Value> = Vec::new();
    let mut code = Vec::new();
    let mut virtual_registers = 0u16;
    for id in 0..=root {
        if !needed[id] {
            continue;
        }
        let constant = |p: Option<Planned>| match p {
            Some(Planned::Constant(v)) => Some(v),
            _ => None,
        };
        let operands = match steps[id].op {
            Instruction::Add(a, b) | Instruction::Mul(a, b) => (a, Some(b)),
            Instruction::Relu(a) => (a, None),
            Instruction::Constant(v) => {
                planned[id] = Some(Planned::Constant(v));
                continue;
            }
            Instruction::Slot(_)
            | Instruction::SumRows(_)
            | Instruction::Softmax(_)
            | Instruction::Transpose(_)
            | Instruction::Normalize(..)
            | Instruction::MatMul(..) => {
                let value = boundary[id].expect("slots and matmuls are materialized");
                planned[id] = Some(Planned::Source(value));
                continue;
            }
        };
        if id != root
            && let Some(value) = boundary[id]
        {
            planned[id] = Some(Planned::Source(value));
            continue;
        }
        let a = planned[operands.0].expect("topological");
        let b = operands.1.map(|b| planned[b].expect("topological"));
        // Constant folding performs the very f32 operation the kernel would.
        let folded = match (steps[id].op, constant(Some(a)), b.and_then(|b| constant(Some(b)))) {
            (Instruction::Add(..), Some(x), Some(y)) => Some(x + y),
            (Instruction::Mul(..), Some(x), Some(y)) => Some(x * y),
            (Instruction::Relu(_), Some(x), _) => Some(if x <= 0.0 { 0.0 } else { x }),
            _ => None,
        };
        if let Some(value) = folded {
            planned[id] = Some(Planned::Constant(value));
            continue;
        }
        let mut operand = |p: Planned| match p {
            Planned::Constant(v) => Operand::Constant(v),
            Planned::Register(r) => Operand::Register(r),
            Planned::Source(value) => {
                let index = sources.iter().position(|s| match (*s, value) {
                    (Value::Slot(x), Value::Slot(y)) | (Value::Temp(x), Value::Temp(y)) => x == y,
                    _ => false,
                });
                Operand::Source(
                    u16::try_from(index.unwrap_or_else(|| {
                        sources.push(value);
                        sources.len() - 1
                    }))
                    .expect("bounded by MAX_NODES"),
                )
            }
        };
        let a_operand = operand(a);
        let b_operand = b.map_or(Operand::Constant(0.0), &mut operand);
        let dst = if id == root {
            Dst::Output
        } else {
            virtual_registers += 1;
            Dst::Register(virtual_registers - 1)
        };
        let kind = match steps[id].op {
            Instruction::Add(..) => Kind::Add,
            Instruction::Mul(..) => Kind::Mul,
            _ => Kind::Relu,
        };
        code.push(Kernel { kind, dst, a: a_operand, b: b_operand });
        if let Dst::Register(r) = dst {
            planned[id] = Some(Planned::Register(r));
        }
    }
    // A root that is itself a constant, a folded expression or an input still fills the output.
    if code.last().is_none_or(|kernel| kernel.dst != Dst::Output) {
        let a = match planned[root].expect("root is planned") {
            Planned::Constant(v) => Operand::Constant(v),
            Planned::Source(value) => {
                sources.push(value);
                Operand::Source(u16::try_from(sources.len() - 1).expect("bounded by MAX_NODES"))
            }
            Planned::Register(r) => Operand::Register(r),
        };
        code.push(Kernel { kind: Kind::Copy, dst: Dst::Output, a, b: Operand::Constant(0.0) });
    }
    let mut tiles = tile(&code, Numeric::Strict);
    let mut registers = allocate(&mut tiles);
    let mut fast = if numeric == Numeric::Fast { tile(&code, Numeric::Fast) } else { Vec::new() };
    registers = registers.max(allocate(&mut fast));
    Elementwise { shape: steps[root].shape, sources, tiles, fast, registers, operations: code.len() }
}

impl Program {
    #[allow(clippy::too_many_lines)] // Linear passes over one plan; each depends on the one before.
    pub(crate) fn plan(steps: Vec<Step>, lease: Lease, numeric: Numeric) -> Result<Self> {
        let root = steps.len() - 1;
        // The signature: dense slot ids, one shape each.
        let mut slots: Vec<Option<Shape>> = Vec::new();
        for step in &steps {
            if let Instruction::Slot(id) = step.op {
                if slots.len() <= id {
                    slots.resize(id + 1, None);
                }
                if slots[id].is_some_and(|shape| shape != step.shape) {
                    return Err(error("conflicting slot types"));
                }
                slots[id] = Some(step.shape);
            }
        }
        let slots: Vec<Shape> =
            slots.into_iter().collect::<Option<_>>().ok_or_else(|| error("slot ids must be dense"))?;
        let materialized = materialization_boundaries(&steps);
        let mut boundary: Vec<Option<Value>> = vec![None; steps.len()];
        let mut stages = Vec::new();
        let mut temps = 0u16;
        for id in 0..steps.len() {
            if let Instruction::Slot(slot) = steps[id].op {
                boundary[id] = Some(Value::Slot(u16::try_from(slot).expect("bounded by MAX_NODES")));
                if id != root {
                    continue;
                }
            } else if !materialized[id] {
                continue;
            }
            let work = if let Instruction::Normalize(a, epsilon, center) = steps[id].op {
                Work::RowMap {
                    source: boundary[a].expect("operand materialized"),
                    shape: steps[id].shape,
                    operation: RowMap::Normalize { epsilon, center },
                }
            } else if let Instruction::Softmax(a) = steps[id].op {
                Work::RowMap {
                    source: boundary[a].expect("operand materialized"),
                    shape: steps[id].shape,
                    operation: RowMap::Softmax,
                }
            } else if let Instruction::Transpose(a) = steps[id].op {
                Work::Transpose { source: boundary[a].expect("operand materialized"), shape: steps[id].shape }
            } else if let Instruction::SumRows(a) = steps[id].op {
                Work::SumRows {
                    source: boundary[a].expect("operand materialized"),
                    shape: steps[id].shape,
                    columns: steps[a].shape.dims().1,
                }
            } else if let Instruction::MatMul(a, b) = steps[id].op {
                Work::MatMul {
                    a: boundary[a].expect("operand materialized first"),
                    b: boundary[b].expect("operand materialized first"),
                    shape: steps[id].shape,
                    inner: steps[a].shape.dims().1,
                }
            } else {
                Work::Elementwise(plan_elementwise(&steps, id, &boundary, numeric))
            };
            boundary[id] = Some(Value::Temp(temps));
            temps += 1;
            stages.push(Stage { work, release: Vec::new(), live: false });
        }
        // A temporary's number is its stage's index. Stages only read earlier ones, so one backward
        // pass finds everything the result depends on. Input validation is independent of this:
        // `check` holds every slot to its shape, read or not.
        let last = stages.len() - 1;
        stages[last].live = true;
        for index in (0..=last).rev() {
            if stages[index].live && stages[index].work.reads_inputs() {
                let mut read = Vec::new();
                stages[index].work.sources(|value| {
                    if let Value::Temp(t) = value {
                        read.push(t as usize);
                    }
                });
                for t in read {
                    stages[t].live = true;
                }
            }
        }
        // Bound the work one invocation may perform, from what the stages actually execute.
        let mut total = 0usize;
        for stage in stages.iter().filter(|stage| stage.live) {
            let (shape, factor) = match &stage.work {
                Work::Elementwise(e) => (e.shape, e.operations),
                Work::RowMap { shape, .. } => (*shape, 6),
                Work::Transpose { shape, .. } => (*shape, 1),
                Work::SumRows { shape, columns, .. } => (*shape, *columns),
                Work::MatMul { shape, inner, .. } => (*shape, *inner),
            };
            total = shape
                .elements()?
                .checked_mul(factor.max(1))
                .and_then(|n| total.checked_add(n))
                .filter(|n| *n <= MAX_WORK)
                .ok_or_else(|| error("graph work budget exceeded"))?;
        }
        // Release each temporary after its last reader (the last stage's result is returned).
        let mut last_reader = vec![None; temps as usize];
        for (index, stage) in stages.iter().enumerate() {
            if stage.live && stage.work.reads_inputs() {
                stage.work.sources(|value| {
                    if let Value::Temp(t) = value {
                        last_reader[t as usize] = Some(index);
                    }
                });
            }
        }
        for (temp, reader) in last_reader.into_iter().enumerate() {
            if let Some(reader) = reader {
                stages[reader].release.push(u16::try_from(temp).expect("bounded by MAX_NODES"));
            }
        }
        Ok(Self { _lease: lease, steps, slots, stages, numeric })
    }

    /// The instantiation this program executes with on this host.
    fn isa(&self) -> Isa {
        match self.numeric {
            Numeric::Strict => Isa::detect(),
            Numeric::Fast => Isa::detect_fast(),
        }
    }
    /// Read-only adapter boundary. Plans can only be created by validation.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
    /// The shapes `run` takes, by position.
    #[must_use]
    pub fn signature(&self) -> &[Shape] {
        &self.slots
    }
    #[must_use]
    pub fn output_shape(&self) -> Shape {
        self.steps.last().expect("nonempty compiled graph").shape
    }
    #[must_use]
    pub fn dump(&self) -> String {
        let mut text = String::new();
        for (index, stage) in self.stages.iter().enumerate() {
            match &stage.work {
                Work::Elementwise(e) => {
                    writeln!(text, "stage {index}: elementwise {:?} registers={}", e.shape, e.registers).unwrap();
                    for (name, tiles) in [("strict", &e.tiles), ("fast", &e.fast)] {
                        for tile in tiles {
                            write!(text, "  {name} {:?} <- {:?}", tile.dst, tile.start).unwrap();
                            for step in tile.steps.iter().take_while(|step| step.op != Op::None) {
                                write!(text, ", {:?} {:?} {:?}", step.op, step.x, step.y).unwrap();
                            }
                            writeln!(text).unwrap();
                        }
                    }
                }
                Work::RowMap { source, shape, operation } => {
                    writeln!(text, "stage {index}: row {operation:?} {shape:?} {source:?}").unwrap();
                }
                Work::SumRows { source, shape, .. } => {
                    writeln!(text, "stage {index}: sum_rows {shape:?} {source:?}").unwrap();
                }
                Work::Transpose { source, shape } => {
                    writeln!(text, "stage {index}: transpose {shape:?} {source:?}").unwrap();
                }
                Work::MatMul { a, b, shape, .. } => {
                    writeln!(text, "stage {index}: matmul {shape:?} {a:?} {b:?}").unwrap();
                }
            }
            if !stage.live {
                writeln!(text, "  (not executed: nothing reaching the result reads it)").unwrap();
            }
        }
        text
    }

    pub(crate) fn check(&self, inputs: &[View<'_>]) -> Result<()> {
        if inputs.len() != self.slots.len() {
            return Err(error("input slot set mismatch"));
        }
        if inputs.iter().zip(&self.slots).any(|(view, shape)| view.shape != *shape) {
            return Err(error("input slot type/shape mismatch"));
        }
        Ok(())
    }

    /// Runs against `inputs` by position, into a fresh immutable result charged to `budget`.
    pub fn run(&self, inputs: &[&Tensor], budget: &Arc<Budget>) -> Result<Tensor> {
        let views: Vec<View<'_>> = inputs.iter().map(|t| t.borrow()).collect();
        self.check(&views)?;
        let shape = self.output_shape();
        let mut output = budget.allocate(shape.elements()? * 4)?;
        self.execute(&views, &mut output.bytes, budget, self.isa(), CHUNK)?;
        Tensor::contiguous(output, shape)
    }

    /// Runs against borrowed views into caller storage of exactly the output's size. The
    /// caller guarantees `out` overlaps no input, which `&mut` already proves in safe code.
    pub fn run_into(&self, inputs: &[View<'_>], out: &mut [u8], budget: &Arc<Budget>) -> Result<()> {
        self.check(inputs)?;
        if out.len() != self.output_shape().elements()? * 4 {
            return Err(error("output byte size mismatch"));
        }
        self.execute(inputs, out, budget, self.isa(), CHUNK)
    }

    /// `run_into` with the kernel instantiation and chunk chosen: tests and sweeps only.
    pub fn run_with(
        &self,
        inputs: &[View<'_>],
        out: &mut [u8],
        budget: &Arc<Budget>,
        isa: Isa,
        chunk: usize,
    ) -> Result<()> {
        self.check(inputs)?;
        if out.len() != self.output_shape().elements()? * 4 {
            return Err(error("output byte size mismatch"));
        }
        assert!(chunk > 0);
        self.execute(inputs, out, budget, isa, chunk)
    }

    fn execute(&self, inputs: &[View<'_>], out: &mut [u8], budget: &Arc<Budget>, isa: Isa, chunk: usize) -> Result<()> {
        // A strict program never fuses, whatever it is asked to run with.
        let isa = if isa == Isa::Fma && self.numeric == Numeric::Strict { Isa::Avx2 } else { isa };
        // Only a multi-stage program has temporaries; a single stage performs no heap work here.
        let last = self.stages.len() - 1;
        let mut temps: Vec<Option<Tensor>> = Vec::new();
        temps.resize_with(last, || None);
        for (index, stage) in self.stages.iter().enumerate() {
            if !stage.live {
                continue;
            }
            // Only the last stage writes to `out`; earlier results are budgeted temporaries.
            let mut allocation = None;
            let shape = stage.work.shape();
            if index != last {
                allocation = Some(budget.allocate(shape.elements()? * 4)?);
            }
            let view_of = |value: Value| match value {
                Value::Slot(s) => inputs[s as usize],
                Value::Temp(t) => temps[t as usize].as_ref().expect("computed before use").borrow(),
            };
            let target: &mut [u8] = match &mut allocation {
                Some(allocation) => &mut allocation.bytes,
                None => &mut *out,
            };
            match &stage.work {
                // Binds no view, charges no scratch, packs nothing: its operands may never have run.
                work if !work.reads_inputs() => target.fill(0),
                Work::Elementwise(e) => {
                    let sources = kernels::bind_sources(e.sources.iter().map(|v| view_of(*v)), e.shape);
                    let needed = kernels::scratch_elements(e, &sources, target.len() / 4, chunk) * 4;
                    let _charge = budget.charge(needed)?;
                    kernels::with_scratch(needed, |scratch| {
                        kernels::elementwise(e, &sources, scratch, target, isa, chunk);
                    })?;
                }
                Work::RowMap { source, shape, operation } => {
                    let input = view_of(*source);
                    let (rows, cols) = shape.dims();
                    let scratch_columns = if rows == 0 { 0 } else { cols };
                    let _charge = budget.charge(scratch_columns * 8)?;
                    let mut values = Vec::new();
                    let mut output = Vec::new();
                    values
                        .try_reserve_exact(scratch_columns)
                        .map_err(|_| error("softmax scratch allocation failed"))?;
                    output
                        .try_reserve_exact(scratch_columns)
                        .map_err(|_| error("softmax scratch allocation failed"))?;
                    values.resize(scratch_columns, 0.0f32);
                    output.resize(scratch_columns, 0.0f32);
                    for row in 0..rows {
                        for (col, value) in values.iter_mut().enumerate() {
                            *value = input.at(row, col);
                        }
                        match operation {
                            RowMap::Softmax => crate::reduction::softmax(&values, &mut output),
                            RowMap::Normalize { epsilon, center } => {
                                crate::reduction::normalize(&values, &mut output, *epsilon, *center)?;
                            }
                        }
                        for (col, value) in output.iter().enumerate() {
                            target[(row * cols + col) * 4..][..4].copy_from_slice(&value.to_le_bytes());
                        }
                    }
                }
                Work::Transpose { source, shape } => {
                    let input = view_of(*source);
                    let swapped = View { shape: *shape, strides: [input.strides[1], input.strides[0]], ..input };
                    Source::bind(swapped, *shape).materialize(target);
                }
                Work::SumRows { source, shape, columns } => {
                    let input = view_of(*source);
                    for row in 0..shape.dims().0 {
                        let mut sum = 0.0f32;
                        for col in 0..*columns {
                            sum += input.at(row, col);
                        }
                        target[row * 4..][..4].copy_from_slice(&sum.to_le_bytes());
                    }
                }
                Work::MatMul { a, b, .. } => {
                    let (a, b) = (view_of(*a), view_of(*b));
                    let (a_elements, b_elements) = (contiguous(a, budget)?, contiguous(b, budget)?);
                    let (k, n) = (a.shape.dims().1, b.shape.dims().1);
                    let needed = kernels::matmul_scratch(target.len() / 4 / n, k, n);
                    let _charge = budget.charge(needed)?;
                    kernels::with_scratch(needed, |scratch| {
                        kernels::matmul(a_elements.elements(), b_elements.elements(), k, n, target, scratch, isa);
                    })?;
                }
            }
            if let Some(allocation) = allocation {
                temps[index] = Some(Tensor::contiguous(allocation, shape)?);
            }
            for temp in &stage.release {
                temps[*temp as usize] = None;
            }
        }
        Ok(())
    }
}

/// Contiguous elements of a view: borrowed when they already are, packed under budget if not.
enum Packed<'a> {
    Borrowed(&'a [Element]),
    Owned(Allocation),
}
impl Packed<'_> {
    fn elements(&self) -> &[Element] {
        match self {
            Self::Borrowed(elements) => elements,
            Self::Owned(pack) => pack.bytes.as_chunks::<4>().0,
        }
    }
}
fn contiguous<'a>(view: View<'a>, budget: &Arc<Budget>) -> Result<Packed<'a>> {
    match Source::bind(view, view.shape) {
        Source::Direct(elements) => Ok(Packed::Borrowed(elements)),
        source => {
            let mut pack = budget.allocate(view.shape.elements()? * 4)?;
            source.materialize(&mut pack.bytes);
            Ok(Packed::Owned(pack))
        }
    }
}

fn materialization_boundaries(steps: &[Step]) -> Vec<bool> {
    // Materialization boundaries: the root, matmuls and their elementwise operands.
    let mut materialized = vec![false; steps.len()];
    materialized[steps.len() - 1] = true;
    for step in steps {
        if let Instruction::SumRows(a)
        | Instruction::Softmax(a)
        | Instruction::Normalize(a, ..)
        | Instruction::Transpose(a) = step.op
        {
            materialized[a] = true;
        }
        if let Instruction::MatMul(a, b) = step.op {
            materialized[a] = true;
            materialized[b] = true;
        }
    }
    for (id, step) in steps.iter().enumerate() {
        if matches!(
            step.op,
            Instruction::MatMul(..)
                | Instruction::SumRows(_)
                | Instruction::Softmax(_)
                | Instruction::Transpose(_)
                | Instruction::Normalize(..)
        ) {
            materialized[id] = true;
        }
    }
    materialized
}
