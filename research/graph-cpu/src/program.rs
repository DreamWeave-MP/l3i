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
/// A fused elementwise computation of one materialized value.
#[derive(Debug)]
pub(crate) struct Elementwise {
    pub(crate) shape: Shape,
    pub(crate) sources: Vec<Value>,
    pub(crate) code: Vec<Kernel>,
    pub(crate) registers: usize,
}
/// A materialized value: a caller's input, or an earlier stage's result.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Value {
    Slot(u16),
    Temp(u16),
}
#[derive(Debug)]
enum Work {
    Elementwise(Elementwise),
    MatMul { a: Value, b: Value, shape: Shape, inner: usize },
}
#[derive(Debug)]
struct Stage {
    work: Work,
    /// Temporaries nothing later reads, released after this stage to bound peak memory.
    release: Vec<u16>,
}

/// The validated, reusable computation.
#[derive(Debug)]
pub struct Program {
    _lease: Lease,
    pub(crate) steps: Vec<Step>,
    pub(crate) slots: Vec<Shape>,
    stages: Vec<Stage>,
}

/// One node as an elementwise stage sees it.
#[derive(Clone, Copy)]
enum Planned {
    Constant(f32),
    Register(u16),
    Source(Value),
}

/// Plans one materialized elementwise root over the steps it needs.
#[allow(clippy::too_many_lines)] // One linear pass over the nodes; splitting it hides the invariants.
fn plan_elementwise(steps: &[Step], root: usize, boundary: &[Option<Value>]) -> Elementwise {
    // Which nodes this stage computes, and each one's last reader, in topological order.
    let mut needed = vec![false; root + 1];
    needed[root] = true;
    let mut last_use = vec![0usize; root + 1];
    for id in (0..=root).rev() {
        if !needed[id] || (id != root && boundary[id].is_some()) {
            continue;
        }
        let mut read = |operand: usize| {
            needed[operand] = true;
            last_use[operand] = last_use[operand].max(id);
        };
        match steps[id].op {
            Instruction::Add(a, b) | Instruction::Mul(a, b) => {
                read(a);
                read(b);
            }
            Instruction::Relu(a) => read(a),
            Instruction::Slot(_) | Instruction::Constant(_) | Instruction::MatMul(..) => (),
        }
    }
    let mut planned: Vec<Option<Planned>> = vec![None; root + 1];
    let mut sources: Vec<Value> = Vec::new();
    let mut code = Vec::new();
    let mut free: Vec<u16> = Vec::new();
    let mut registers = 0usize;
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
            Instruction::Slot(_) | Instruction::MatMul(..) => {
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
        // The destination is chosen before operands retire, so it never overlaps them.
        let dst = if id == root {
            Dst::Output
        } else {
            Dst::Register(free.pop().unwrap_or_else(|| {
                registers += 1;
                u16::try_from(registers - 1).expect("bounded by MAX_NODES")
            }))
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
        let (first, second) = (operands.0, operands.1);
        for operand in [Some(first), second].into_iter().flatten() {
            if last_use[operand] == id
                && let Some(Planned::Register(r)) = planned[operand]
                && !free.contains(&r)
            {
                free.push(r);
            }
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
    Elementwise { shape: steps[root].shape, sources, code, registers }
}

impl Program {
    pub(crate) fn plan(steps: Vec<Step>, lease: Lease) -> Result<Self> {
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
        // Materialization boundaries: the root, matmuls and their elementwise operands.
        let mut materialized = vec![false; steps.len()];
        materialized[root] = true;
        for step in &steps {
            if let Instruction::MatMul(a, b) = step.op {
                materialized[a] = true;
                materialized[b] = true;
            }
        }
        for (id, step) in steps.iter().enumerate() {
            if matches!(step.op, Instruction::MatMul(..)) {
                materialized[id] = true;
            }
        }
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
            let work = if let Instruction::MatMul(a, b) = steps[id].op {
                Work::MatMul {
                    a: boundary[a].expect("operand materialized first"),
                    b: boundary[b].expect("operand materialized first"),
                    shape: steps[id].shape,
                    inner: steps[a].shape.dims().1,
                }
            } else {
                Work::Elementwise(plan_elementwise(&steps, id, &boundary))
            };
            boundary[id] = Some(Value::Temp(temps));
            temps += 1;
            stages.push(Stage { work, release: Vec::new() });
        }
        // Bound the work one invocation may perform, from what the stages actually execute.
        let mut total = 0usize;
        for stage in &stages {
            let (shape, factor) = match &stage.work {
                Work::Elementwise(e) => (e.shape, e.code.len()),
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
            let mut read = |value: Value| {
                if let Value::Temp(t) = value {
                    last_reader[t as usize] = Some(index);
                }
            };
            match &stage.work {
                Work::Elementwise(e) => e.sources.iter().copied().for_each(&mut read),
                Work::MatMul { a, b, .. } => {
                    read(*a);
                    read(*b);
                }
            }
        }
        for (temp, reader) in last_reader.into_iter().enumerate() {
            if let Some(reader) = reader {
                stages[reader].release.push(u16::try_from(temp).expect("bounded by MAX_NODES"));
            }
        }
        Ok(Self { _lease: lease, steps, slots, stages })
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
                    for kernel in &e.code {
                        writeln!(text, "  {:?} {:?} <- {:?}, {:?}", kernel.kind, kernel.dst, kernel.a, kernel.b)
                            .unwrap();
                    }
                }
                Work::MatMul { a, b, shape, .. } => {
                    writeln!(text, "stage {index}: matmul {shape:?} {a:?} {b:?}").unwrap();
                }
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
        self.execute(&views, &mut output.bytes, budget, Isa::detect(), CHUNK)?;
        Tensor::contiguous(output, shape)
    }

    /// Runs against borrowed views into caller storage of exactly the output's size. The
    /// caller guarantees `out` overlaps no input, which `&mut` already proves in safe code.
    pub fn run_into(&self, inputs: &[View<'_>], out: &mut [u8], budget: &Arc<Budget>) -> Result<()> {
        self.check(inputs)?;
        if out.len() != self.output_shape().elements()? * 4 {
            return Err(error("output byte size mismatch"));
        }
        self.execute(inputs, out, budget, Isa::detect(), CHUNK)
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
        // Only a multi-stage program has temporaries; a single stage performs no heap work here.
        let last = self.stages.len() - 1;
        let mut temps: Vec<Option<Tensor>> = Vec::new();
        temps.resize_with(last, || None);
        for (index, stage) in self.stages.iter().enumerate() {
            // Only the last stage writes to `out`; earlier results are budgeted temporaries.
            let mut allocation = None;
            let shape = match &stage.work {
                Work::Elementwise(e) => e.shape,
                Work::MatMul { shape, .. } => *shape,
            };
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
                Work::Elementwise(e) => {
                    let sources = kernels::bind_sources(e.sources.iter().map(|v| view_of(*v)), e.shape);
                    let needed = kernels::scratch_elements(e, &sources, target.len() / 4, chunk) * 4;
                    let _charge = budget.charge(needed)?;
                    kernels::with_scratch(needed, |scratch| {
                        kernels::elementwise(e, &sources, scratch, target, isa, chunk);
                    })?;
                }
                Work::MatMul { a, b, .. } => {
                    let (a, b) = (view_of(*a), view_of(*b));
                    let (a_elements, b_elements) = (contiguous(a, budget)?, contiguous(b, budget)?);
                    let (k, n) = (a.shape.dims().1, b.shape.dims().1);
                    kernels::matmul(a_elements.elements(), b_elements.elements(), k, n, target, isa);
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
