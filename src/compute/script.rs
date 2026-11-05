//! The `@dream/compute` extension: typed f32 graphs, reusable programs and immutable tensors from
//! Luau, over a native memory budget and graph limits the host chooses.
//!
//! Every script value carries the budget it charges, so a call reaches its accounting without a
//! lookup and a value can never be charged to another runtime's domain. Constructors are bound per
//! runtime in [`ComputeExtension::install`](crate::extension::Extension::install), which is where a
//! runtime gets its own domain or joins the one the host shares between them.

use std::sync::Arc;

use crate::bind::{Call, VarArgs};
use crate::compute::kernels::Inline;
use crate::compute::{Budget, Domain, Expr, Numeric, Program, Shape, Tensor, View};
use crate::convert::{BufferView, Exact, NewBuffer};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext, TagPolicy};
use crate::userdata::{Owned, Userdata};

const EXTENSION_ID: &str = "dream.compute";
const MODULE: &str = "@dream/compute";

/// What one domain may hold. Native tensors are invisible to Luau's memory limit, so this is what
/// bounds them.
#[derive(Clone, Copy, Debug)]
pub struct ComputeLimits {
    /// Live tensors, prepared weights, temporaries and scratch, in bytes.
    pub memory_bytes: usize,
    /// Live expression nodes.
    pub graph_nodes: usize,
    /// Live compiled programs.
    pub programs: usize,
}

impl Default for ComputeLimits {
    fn default() -> Self {
        Self { memory_bytes: 64 << 20, graph_nodes: 4096, programs: 512 }
    }
}

#[derive(Clone, Debug)]
enum Resources {
    PerRuntime(ComputeLimits),
    Shared(Arc<Budget>, Arc<Domain>),
}

/// The extension. By default every runtime from a plan gets its own [`ComputeLimits::default`].
#[derive(Clone, Debug)]
pub struct ComputeExtension {
    resources: Resources,
}

impl Default for ComputeExtension {
    fn default() -> Self {
        Self::new(ComputeLimits::default())
    }
}

impl ComputeExtension {
    /// Every runtime from the plan gets its own domain with these limits.
    #[must_use]
    pub fn new(limits: ComputeLimits) -> Self {
        Self { resources: Resources::PerRuntime(limits) }
    }

    /// Every runtime from the plan charges this one domain, which the host can also read and share
    /// with its own work.
    #[must_use]
    pub fn shared(budget: Arc<Budget>, domain: Arc<Domain>) -> Self {
        Self { resources: Resources::Shared(budget, domain) }
    }
}

/// An immutable tensor as scripts hold it. Hosts wrap their own, such as one over shared storage.
/// Cloning retains the owner, never the payload.
#[derive(Clone, Debug)]
pub struct ScriptTensor {
    tensor: Tensor,
    budget: Arc<Budget>,
}
/// An expression as scripts hold it.
#[derive(Debug)]
pub struct ScriptExpression {
    expression: Expr,
    budget: Arc<Budget>,
    domain: Arc<Domain>,
}
/// A compiled program as scripts hold it.
#[derive(Debug)]
pub struct ScriptProgram {
    program: Program,
    budget: Arc<Budget>,
}

// SAFETY: these own native payloads with no Lua references, and their destructors only release
// memory and adjust atomic counters, so a GC finalizer running them never calls the Lua API.
unsafe impl Userdata for ScriptTensor {
    const NAME: &'static str = "dream.compute.Tensor";
}
// SAFETY: as for `ScriptTensor`.
unsafe impl Userdata for ScriptExpression {
    const NAME: &'static str = "dream.compute.Expression";
}
// SAFETY: as for `ScriptTensor`.
unsafe impl Userdata for ScriptProgram {
    const NAME: &'static str = "dream.compute.Program";
}

impl ScriptTensor {
    /// Wraps `tensor` for a script. Further results of operating on it charge `budget`.
    #[must_use]
    pub fn new(tensor: Tensor, budget: Arc<Budget>) -> Self {
        Self { tensor, budget }
    }
    #[must_use]
    pub fn tensor(&self) -> &Tensor {
        &self.tensor
    }
}
impl ScriptExpression {
    fn new(
        call: &Call<'_>,
        budget: &Arc<Budget>,
        domain: &Arc<Domain>,
        build: impl FnMut() -> Result<Expr>,
    ) -> Result<Owned<Self>> {
        let expression = retrying(call, || domain.denials(), build)?;
        Ok(Owned(Self { expression, budget: budget.clone(), domain: domain.clone() }))
    }
    fn derive(&self, call: &Call<'_>, build: impl FnMut() -> Result<Expr>) -> Result<Owned<Self>> {
        Self::new(call, &self.budget, &self.domain, build)
    }
    fn compile(&self, call: &Call<'_>, numeric: Numeric) -> Result<Owned<ScriptProgram>> {
        let program = retrying(call, || self.domain.denials(), || self.expression.compile_with(numeric))?;
        Ok(Owned(ScriptProgram { program, budget: self.budget.clone() }))
    }
}
impl ScriptProgram {
    #[must_use]
    pub fn program(&self) -> &Program {
        &self.program
    }
    #[must_use]
    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }
}

/// Native values look small to Luau's collector, so garbage can hold a limit without ever
/// triggering a cycle. When a limit refuses `body`, collect once and retry.
fn retrying<R>(call: &Call<'_>, denials: impl Fn() -> usize, mut body: impl FnMut() -> Result<R>) -> Result<R> {
    let before = denials();
    let first = body();
    if first.is_err() && denials() != before {
        call.collect_garbage();
        return body();
    }
    first
}

fn collecting<R>(call: &Call<'_>, budget: &Budget, body: impl FnMut() -> Result<R>) -> Result<R> {
    retrying(call, || budget.denials(), body)
}

/// Whether two byte ranges share a byte. An empty range shares none. A range's end never overflows,
/// because a slice cannot extend past the end of the address space.
fn overlaps(a: &[u8], b: &[u8]) -> bool {
    let (a, b) = (a.as_ptr_range(), b.as_ptr_range());
    a.start != a.end && b.start != b.end && a.start < b.end && b.start < a.end
}

/// `program:runInto(out, a, b, ...)`: Luau buffers, borrowed for this call and never retained. The
/// first few arrive as separate parameters so that the usual call collects nothing.
fn run_buffers_into(
    program: &Program,
    out: &mut BufferView<'_>,
    first: [Option<&BufferView<'_>>; 3],
    rest: &[BufferView<'_>],
    budget: &Arc<Budget>,
) -> Result<()> {
    let given = first.iter().take_while(|input| input.is_some()).count();
    let contiguous = first[given..].iter().all(Option::is_none) && (given == first.len() || rest.is_empty());
    if !contiguous || given + rest.len() != program.signature().len() {
        return Err(Error::runtime("input slot set mismatch"));
    }
    let inputs = first.iter().flatten().copied().chain(rest);
    let mut views: Inline<View<'_>, 8> = Inline::new();
    // SAFETY: no Lua runs and no other view writes between here and the end of `run_into` below.
    // The exclusive slice is only made after every input's byte range is shown disjoint from it,
    // so this holds for any `BufferView`, including sub-ranges of one buffer. Inputs only read, so
    // they may overlap each other.
    let out_bytes = unsafe {
        let target = out.bytes_unchecked();
        for (input, shape) in inputs.zip(program.signature()) {
            let bytes = input.bytes_unchecked();
            if overlaps(bytes, target) {
                return Err(Error::runtime("output buffer aliases an input"));
            }
            views.push(View::contiguous(bytes, *shape)?);
        }
        out.bytes_mut_unchecked()
    };
    program.run_into(&views, out_bytes, budget)
}

fn describe_tensor(d: &mut ExtensionDescriptor) {
    let mut tensor = d.userdata::<ScriptTensor>(ScriptTensor::NAME);
    tensor
        .tag(TagPolicy::Preferred)
        .doc("Immutable f32 elements with a shape. Views share them; nothing can write them.");
    tensor
        .method("read", |t: &ScriptTensor| t.tensor.readback().map(NewBuffer))
        .signature("(self): buffer")
        .doc("A new buffer of the elements in row-major order, little-endian.");
    tensor
        .method(
            "view",
            |t: &ScriptTensor,
             rows: Exact<usize>,
             cols: Exact<usize>,
             offset: Exact<usize>,
             row_stride: Exact<usize>,
             col_stride: Exact<usize>| {
                let view = t.tensor.view(Shape::Matrix(rows.0, cols.0), offset.0, [row_stride.0, col_stride.0])?;
                Ok::<_, Error>(Owned(ScriptTensor { tensor: view, budget: t.budget.clone() }))
            },
        )
        .signature("(self, rows: number, cols: number, offset: number, rowStride: number, colStride: number): dream_compute_Tensor")
        .doc("A view of elements this tensor already holds, addressed in bytes from its first element, without copying. Addressing anything it does not hold raises.");
    tensor
        .method("prepareRight", |t: &ScriptTensor, call: &Call<'_>| {
            let prepared = collecting(call, &t.budget, || t.tensor.prepare_right(&t.budget))?;
            Ok::<_, Error>(Owned(ScriptTensor { tensor: prepared, budget: t.budget.clone() }))
        })
        .signature("(self): dream_compute_Tensor")
        .doc("The same elements, also packed ahead of time as the right operand of matrix products, charged until the last tensor sharing the packing is collected.");
    tensor
        .method("prepareLeft", |t: &ScriptTensor, call: &Call<'_>| {
            let prepared = collecting(call, &t.budget, || t.tensor.prepare_left(&t.budget))?;
            Ok::<_, Error>(Owned(ScriptTensor { tensor: prepared, budget: t.budget.clone() }))
        })
        .signature("(self): dream_compute_Tensor")
        .doc("The same elements, also packed ahead of time as the left operand of a matrix times a vector, charged the same way.");
}

fn describe_expression(d: &mut ExtensionDescriptor) {
    let mut expression = d.userdata::<ScriptExpression>(ScriptExpression::NAME);
    expression
        .tag(TagPolicy::Never)
        .doc("A typed computation over input slots. Holds structure and shapes, never data.");
    macro_rules! binary {
        ($name:literal, $method:ident, $doc:literal) => {
            expression
                .method($name, |a: &ScriptExpression, call: &Call<'_>, b: &ScriptExpression| {
                    a.derive(call, || a.expression.$method(&b.expression))
                })
                .signature("(self, other: dream_compute_Expression): dream_compute_Expression")
                .doc($doc);
        };
    }
    macro_rules! unary {
        ($name:literal, $method:ident, $doc:literal) => {
            expression
                .method($name, |a: &ScriptExpression, call: &Call<'_>| a.derive(call, || a.expression.$method()))
                .signature("(self): dream_compute_Expression")
                .doc($doc);
        };
    }
    binary!("add", add, "Elementwise sum, with dimension-one broadcasting.");
    binary!("mul", mul, "Elementwise product, with dimension-one broadcasting.");
    binary!("matmul", matmul, "The matrix product.");
    unary!("relu", relu, "Each element clamped below at +0.");
    unary!("softmax", softmax, "Each row's accurate softmax.");
    unary!("sumRows", sum_rows, "Each row's sum, added in column order from +0, as a one-column matrix.");
    unary!("transpose", transpose, "Rows and columns exchanged.");
    expression
        .method("rmsNorm", |a: &ScriptExpression, call: &Call<'_>, epsilon: f64| {
            a.derive(call, || a.expression.normalize(epsilon, false))
        })
        .signature("(self, epsilon: number): dream_compute_Expression")
        .doc("Each row divided by its root mean square, with epsilon added under the root.");
    expression
        .method("layerNorm", |a: &ScriptExpression, call: &Call<'_>, epsilon: f64| {
            a.derive(call, || a.expression.normalize(epsilon, true))
        })
        .signature("(self, epsilon: number): dream_compute_Expression")
        .doc("Each row centered, then divided by its standard deviation, with epsilon added under the root.");
    expression
        .method("compile", |a: &ScriptExpression, call: &Call<'_>| a.compile(call, Numeric::Strict))
        .signature("(self): dream_compute_Program")
        .doc(
            "A reusable program with separately rounded f32 arithmetic in a defined order: the same bits on every CPU.",
        );
    expression
        .method("compileFast", |a: &ScriptExpression, call: &Call<'_>| a.compile(call, Numeric::Fast))
        .signature("(self): dream_compute_Program")
        .doc("A reusable program that may fuse multiply-add where the CPU has it and reorder the additions of long dot products. Results can differ from compile's and between CPUs with and without FMA, by what any summation order can differ by.");
}

fn describe_program(d: &mut ExtensionDescriptor) {
    let mut program = d.userdata::<ScriptProgram>(ScriptProgram::NAME);
    program
        .tag(TagPolicy::Preferred)
        .doc("A validated computation, compiled once and run on any inputs of its slots' shapes.");
    program
        .method("run", |p: &ScriptProgram, call: &Call<'_>, tensors: VarArgs<&ScriptTensor>| {
            let tensors: Inline<&Tensor, 8> = tensors.values.iter().map(|t| &t.tensor).collect();
            let result = collecting(call, &p.budget, || p.program.run(&tensors, &p.budget))?;
            Ok::<_, Error>(Owned(ScriptTensor { tensor: result, budget: p.budget.clone() }))
        })
        .signature("(self, ...: dream_compute_Tensor): dream_compute_Tensor")
        .doc("Runs on tensors, one per slot in order, giving a new tensor charged until collected.");
    program
        .method(
            "runInto",
            |p: &ScriptProgram,
             call: &Call<'_>,
             mut out: BufferView<'_>,
             first: Option<BufferView<'_>>,
             second: Option<BufferView<'_>>,
             third: Option<BufferView<'_>>,
             rest: VarArgs<BufferView<'_>>| {
                let given = [first.as_ref(), second.as_ref(), third.as_ref()];
                collecting(call, &p.budget, || run_buffers_into(&p.program, &mut out, given, &rest.values, &p.budget))
            },
        )
        .signature("(self, out: buffer, ...: buffer)")
        .doc("Runs on buffers of little-endian f32, one per slot in order, writing to out. Nothing is retained. Raises if out overlaps an input.");
    program
        .method(
            "runTensorsInto",
            |p: &ScriptProgram, call: &Call<'_>, mut out: BufferView<'_>, tensors: VarArgs<&ScriptTensor>| {
                let views: Inline<View<'_>, 8> = tensors.values.iter().map(|t| t.tensor.borrow()).collect();
                collecting(call, &p.budget, || {
                    // SAFETY: a tensor's bytes are native, never a Luau buffer, so this exclusive
                    // slice overlaps no input, and no Lua runs while it is alive.
                    let target = unsafe { out.bytes_mut_unchecked() };
                    p.program.run_into(&views, target, &p.budget)
                })
            },
        )
        .signature("(self, out: buffer, ...: dream_compute_Tensor)")
        .doc("Runs on tensors, writing to out.");
}

impl Extension for ComputeExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        describe_tensor(d);
        describe_expression(d);
        describe_program(d);
        let module = d.module(MODULE);
        module.doc("Typed f32 computations compiled once and run on native kernels, with no runtime code generation.");
        module
            .installed("slot")
            .signature("(slot: number, rows: number, cols: number) -> dream_compute_Expression")
            .doc("Input number slot, counted from 0, with this shape.")
            .installed("constant")
            .signature("(value: number) -> dream_compute_Expression")
            .doc("A constant that broadcasts to any shape.")
            .installed("f32")
            .signature("(bytes: buffer, rows: number, cols: number) -> dream_compute_Tensor")
            .doc("A tensor holding a copy of a buffer's little-endian f32 elements in row-major order, charged until collected.")
            .installed("used")
            .signature("() -> number")
            .doc("Native bytes live in this domain right now.")
            .installed("limit")
            .signature("() -> number")
            .doc("The native bytes this domain may hold.");
        Ok(())
    }

    fn install(&self, context: &mut InstallContext<'_>) -> Result<()> {
        let (budget, domain) = match &self.resources {
            Resources::PerRuntime(limits) => {
                (Budget::new(limits.memory_bytes), Domain::new(limits.graph_nodes, limits.programs))
            }
            Resources::Shared(budget, domain) => (budget.clone(), domain.clone()),
        };
        let module = context.module(MODULE)?;
        let (slot_budget, slot_domain) = (budget.clone(), domain.clone());
        module.function(
            "slot",
            move |call: &Call<'_>, slot: Exact<usize>, rows: Exact<usize>, cols: Exact<usize>| {
                ScriptExpression::new(call, &slot_budget, &slot_domain, || {
                    Expr::slot(slot.0, Shape::Matrix(rows.0, cols.0), slot_domain.clone())
                })
            },
        )?;
        let constant_budget = budget.clone();
        module.function("constant", move |call: &Call<'_>, value: f32| {
            ScriptExpression::new(call, &constant_budget, &domain, || Expr::constant(value, domain.clone()))
        })?;
        let import_budget = budget.clone();
        module.function(
            "f32",
            move |call: &Call<'_>, bytes: BufferView<'_>, rows: Exact<usize>, cols: Exact<usize>| {
                let shape = Shape::Matrix(rows.0, cols.0);
                let tensor = collecting(call, &import_budget, || Tensor::import_view(&bytes, shape, &import_budget))?;
                Ok::<_, Error>(Owned(ScriptTensor { tensor, budget: import_budget.clone() }))
            },
        )?;
        let used_budget = budget.clone();
        #[allow(clippy::cast_precision_loss)] // A byte count far below 2^53.
        module.function("used", move || used_budget.used() as f64)?;
        #[allow(clippy::cast_precision_loss)] // As above.
        module.function("limit", move || budget.limit() as f64)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::overlaps;

    /// Sub-ranges with different starts still count: start-address equality would miss these.
    #[test]
    fn overlap_is_a_range_check() {
        let bytes = [0u8; 16];
        assert!(overlaps(&bytes[0..12], &bytes[4..8]), "contained, different start");
        assert!(overlaps(&bytes[0..8], &bytes[4..12]), "partial");
        assert!(overlaps(&bytes[4..12], &bytes[0..8]), "partial, other order");
        assert!(overlaps(&bytes, &bytes), "identical");
        assert!(!overlaps(&bytes[0..8], &bytes[8..16]), "adjacent");
        assert!(!overlaps(&bytes[8..16], &bytes[0..8]), "adjacent, other order");
        assert!(!overlaps(&bytes[4..4], &bytes), "empty");
        assert!(!overlaps(&bytes, &bytes[4..4]), "empty, other side");
    }
}
