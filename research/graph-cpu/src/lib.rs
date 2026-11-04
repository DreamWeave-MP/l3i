//! Isolated typed graph and synchronous CPU executor. No production extension.
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
pub mod lowering;
use l3i::{Error, Result};
use std::fmt::Write;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
const MAX_NODES: usize = 64;
static LIVE_NODES: AtomicUsize = AtomicUsize::new(0);
static LIVE_PLANS: AtomicUsize = AtomicUsize::new(0);
/// Host-owned accounting domain; process limits remain an additional safeguard.
#[derive(Debug)]
pub struct Domain {
    nodes: AtomicUsize,
    plans: AtomicUsize,
    node_limit: usize,
    plan_limit: usize,
}
impl Domain {
    #[must_use]
    pub fn new(node_limit: usize, plan_limit: usize) -> Arc<Self> {
        Arc::new(Self { nodes: AtomicUsize::new(0), plans: AtomicUsize::new(0), node_limit, plan_limit })
    }
}
fn default_domain() -> Arc<Domain> {
    static DOMAIN: std::sync::OnceLock<Arc<Domain>> = std::sync::OnceLock::new();
    DOMAIN.get_or_init(|| Domain::new(4096, 128)).clone()
}
#[derive(Debug)]
struct Lease {
    global: &'static AtomicUsize,
    domain: Arc<Domain>,
    plan: bool,
}
impl Lease {
    fn acquire(domain: Arc<Domain>, plan: bool) -> Result<Self> {
        let (global, limit, local, local_limit) = if plan {
            (&LIVE_PLANS, 128, &domain.plans, domain.plan_limit)
        } else {
            (&LIVE_NODES, 4096, &domain.nodes, domain.node_limit)
        };
        global
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < limit).then_some(n + 1))
            .map_err(|_| error("process graph safeguard exhausted"))?;
        if local.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < local_limit).then_some(n + 1)).is_err() {
            global.fetch_sub(1, Ordering::Relaxed);
            return Err(error("domain graph budget exhausted"));
        }
        Ok(Self { global, domain, plan })
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.global.fetch_sub(1, Ordering::Relaxed);
        let local = if self.plan { &self.domain.plans } else { &self.domain.nodes };
        local.fetch_sub(1, Ordering::Relaxed);
    }
}

const MAX_ELEMENTS: usize = 1_048_576;
const MAX_WORK: usize = 67_108_864;
fn error(message: &str) -> Error {
    Error::runtime(message)
}

/// Host-provided immutable storage. The owner must retain its bytes for its entire lifetime.
/// Jess can adapt its `SharedBuffer` here without copying, moving its allocation, or exposing writes.
pub trait Storage: Send + Sync + std::fmt::Debug {
    fn bytes(&self) -> &[u8];
}

#[derive(Debug)]
pub struct Budget {
    used: AtomicUsize,
    limit: usize,
    allocations: AtomicUsize,
}
impl Budget {
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { used: AtomicUsize::new(0), allocations: AtomicUsize::new(0), limit })
    }
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn allocations(&self) -> usize {
        self.allocations.load(Ordering::Relaxed)
    }
    fn allocate(self: &Arc<Self>, len: usize) -> Result<Allocation> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| used.checked_add(len).filter(|n| *n <= self.limit))
            .map_err(|_| error("live storage budget exceeded"))?;
        let mut allocation = Allocation { bytes: Vec::new(), charge: len, budget: self.clone() };
        allocation.bytes.try_reserve_exact(len).map_err(|_| error("allocation failed"))?;
        allocation.bytes.resize(len, 0);
        self.allocations.fetch_add(1, Ordering::Relaxed);
        Ok(allocation)
    }
}
#[derive(Debug)]
struct Allocation {
    bytes: Vec<u8>,
    charge: usize,
    budget: Arc<Budget>,
}
impl Drop for Allocation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.charge, Ordering::Relaxed);
    }
}
impl Storage for Allocation {
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Minimal concrete f32 type: scalar or two-dimensional tensor. No implicit dtype conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Scalar,
    Matrix(usize, usize),
}
impl Shape {
    pub fn elements(self) -> Result<usize> {
        let (rows, cols) = self.dims();
        // Bound each dimension even when another dimension is zero.
        if rows > MAX_ELEMENTS || cols > MAX_ELEMENTS {
            return Err(error("dimension budget exceeded"));
        }
        rows.checked_mul(cols).filter(|n| *n <= MAX_ELEMENTS).ok_or_else(|| error("element budget exceeded"))
    }
    #[must_use]
    pub fn dims(self) -> (usize, usize) {
        match self {
            Self::Scalar => (1, 1),
            Self::Matrix(r, c) => (r, c),
        }
    }
    fn broadcast(self, other: Self) -> Result<Self> {
        if self == Self::Scalar {
            return Ok(other);
        }
        if other == Self::Scalar {
            return Ok(self);
        }
        let dimension = |a, b| {
            if a == b || b == 1 {
                Ok(a)
            } else if a == 1 {
                Ok(b)
            } else {
                Err(error("incompatible broadcast"))
            }
        };
        let (ar, ac) = self.dims();
        let (br, bc) = other.dims();
        let result = Self::Matrix(dimension(ar, br)?, dimension(ac, bc)?);
        result.elements()?;
        Ok(result)
    }
}

#[derive(Clone, Debug)]
pub struct Tensor {
    owner: Arc<dyn Storage>,
    shape: Shape,
    offset: usize,
    strides: [usize; 2],
    end: usize,
}
impl Tensor {
    /// Trusted host entry: retain an existing immutable owner, without a payload copy.
    /// External allocation accounting belongs to its host, not this executor's output budget.
    pub fn from_storage(owner: Arc<dyn Storage>, shape: Shape, offset: usize, strides: [usize; 2]) -> Result<Self> {
        let count = shape.elements()?;
        let (rows, cols) = shape.dims();
        let end = if count == 0 {
            Some(offset)
        } else {
            (rows - 1)
                .checked_mul(strides[0])
                .and_then(|r| (cols - 1).checked_mul(strides[1]).and_then(|c| r.checked_add(c)))
                .and_then(|n| n.checked_add(offset))
                .and_then(|n| n.checked_add(4))
        };
        if end.is_none_or(|end| end > owner.bytes().len()) {
            return Err(error("view outside storage"));
        }
        Ok(Self { owner, shape, offset, strides, end: end.expect("validated end") })
    }
    pub fn import(bytes: &[u8], shape: Shape, budget: &Arc<Budget>) -> Result<Self> {
        if bytes.len() != shape.elements()? * 4 {
            return Err(error("input byte size mismatch"));
        }
        let mut allocation = budget.allocate(bytes.len())?;
        allocation.bytes.copy_from_slice(bytes);
        Self::contiguous(allocation, shape)
    }
    pub fn import_view(bytes: &l3i::convert::BufferView<'_>, shape: Shape, budget: &Arc<Budget>) -> Result<Self> {
        if bytes.len() != shape.elements()? * 4 {
            return Err(error("input byte size mismatch"));
        }
        let mut allocation = budget.allocate(bytes.len())?;
        bytes.read(0, &mut allocation.bytes)?;
        Self::contiguous(allocation, shape)
    }
    fn contiguous(allocation: Allocation, shape: Shape) -> Result<Self> {
        Self::from_storage(Arc::new(allocation), shape, 0, [shape.dims().1 * 4, 4])
    }
    pub fn view(&self, shape: Shape, offset: usize, strides: [usize; 2]) -> Result<Self> {
        // A derived view cannot widen its parent backing byte interval.
        let offset = self.offset.checked_add(offset).ok_or_else(|| error("offset overflow"))?;
        let view = Self::from_storage(self.owner.clone(), shape, offset, strides)?;
        if view.end > self.end {
            return Err(error("view widens parent byte interval"));
        }
        Ok(view)
    }
    #[must_use]
    pub fn shares_storage(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
    }
    fn at(&self, row: usize, col: usize) -> f32 {
        let (rows, cols) = self.shape.dims();
        let index = self.offset
            + if rows == 1 { 0 } else { row * self.strides[0] }
            + if cols == 1 { 0 } else { col * self.strides[1] };
        f32::from_le_bytes(self.owner.bytes()[index..index + 4].try_into().expect("validated view"))
    }
    pub fn readback(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(self.shape.elements()? * 4).map_err(|_| error("readback allocation failed"))?;
        let (rows, cols) = self.shape.dims();
        for row in 0..rows {
            for col in 0..cols {
                bytes.extend_from_slice(&self.at(row, col).to_le_bytes());
            }
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug)]
enum Op {
    Slot(usize),
    Input(Tensor),
    Constant(f32),
    Add(Expr, Expr),
    Mul(Expr, Expr),
    Relu(Expr),
    MatMul(Expr, Expr),
}
#[derive(Debug)]
struct Node {
    lease: Lease,
    shape: Shape,
    op: Op,
    depth: usize,
}
#[derive(Clone, Debug)]
pub struct Expr(Arc<Node>);
impl Expr {
    fn new_in(shape: Shape, op: Op, depth: usize, domain: Arc<Domain>) -> Result<Self> {
        shape.elements()?;
        if depth > MAX_NODES {
            return Err(error("graph depth budget exceeded"));
        }
        Ok(Self(Arc::new(Node { shape, op, depth, lease: Lease::acquire(domain, false)? })))
    }
    pub fn slot(id: usize, shape: Shape) -> Result<Self> {
        Self::slot_in(id, shape, default_domain())
    }
    pub fn slot_in(id: usize, shape: Shape, domain: Arc<Domain>) -> Result<Self> {
        if id >= MAX_NODES {
            return Err(error("slot id budget exceeded"));
        }
        Self::new_in(shape, Op::Slot(id), 1, domain)
    }
    pub fn input(tensor: Tensor) -> Result<Self> {
        Self::input_in(tensor, default_domain())
    }
    pub fn input_in(tensor: Tensor, domain: Arc<Domain>) -> Result<Self> {
        Self::new_in(tensor.shape, Op::Input(tensor), 1, domain)
    }
    pub fn constant(value: f32) -> Result<Self> {
        Self::constant_in(value, default_domain())
    }
    pub fn constant_in(value: f32, domain: Arc<Domain>) -> Result<Self> {
        Self::new_in(Shape::Scalar, Op::Constant(value), 1, domain)
    }
    fn same_domain(&self, rhs: &Self) -> Result<()> {
        if Arc::ptr_eq(&self.0.lease.domain, &rhs.0.lease.domain) {
            Ok(())
        } else {
            Err(error("cross-domain graph composition denied"))
        }
    }
    pub fn add(&self, rhs: &Self) -> Result<Self> {
        self.same_domain(rhs)?;
        Self::new_in(
            self.0.shape.broadcast(rhs.0.shape)?,
            Op::Add(self.clone(), rhs.clone()),
            1 + self.0.depth.max(rhs.0.depth),
            self.0.lease.domain.clone(),
        )
    }
    pub fn mul(&self, rhs: &Self) -> Result<Self> {
        self.same_domain(rhs)?;
        Self::new_in(
            self.0.shape.broadcast(rhs.0.shape)?,
            Op::Mul(self.clone(), rhs.clone()),
            1 + self.0.depth.max(rhs.0.depth),
            self.0.lease.domain.clone(),
        )
    }
    pub fn relu(&self) -> Result<Self> {
        Self::new_in(self.0.shape, Op::Relu(self.clone()), 1 + self.0.depth, self.0.lease.domain.clone())
    }
    pub fn matmul(&self, rhs: &Self) -> Result<Self> {
        self.same_domain(rhs)?;
        let (Shape::Matrix(m, k), Shape::Matrix(k2, n)) = (self.0.shape, rhs.0.shape) else {
            return Err(error("matmul requires rank two"));
        };
        if k != k2 {
            return Err(error("matmul inner dimensions differ"));
        }
        let shape = Shape::Matrix(m, n);
        shape
            .elements()?
            .checked_mul(k)
            .filter(|n| *n <= MAX_WORK)
            .ok_or_else(|| error("matmul work budget exceeded"))?;
        Self::new_in(
            shape,
            Op::MatMul(self.clone(), rhs.clone()),
            1 + self.0.depth.max(rhs.0.depth),
            self.0.lease.domain.clone(),
        )
    }
    pub fn compile(&self) -> Result<Executable> {
        fn visit(expr: &Expr, nodes: &mut Vec<Expr>) -> Result<usize> {
            if let Some(id) = nodes.iter().position(|node| Arc::ptr_eq(&node.0, &expr.0)) {
                return Ok(id);
            }
            match &expr.0.op {
                Op::Add(a, b) | Op::Mul(a, b) | Op::MatMul(a, b) => {
                    visit(a, nodes)?;
                    visit(b, nodes)?;
                }
                Op::Relu(a) => {
                    visit(a, nodes)?;
                }
                _ => (),
            }
            if nodes.len() >= MAX_NODES {
                return Err(error("graph node budget exceeded"));
            }
            nodes.push(expr.clone());
            Ok(nodes.len() - 1)
        }
        let mut nodes = Vec::new();
        visit(self, &mut nodes)?;
        let id = |expr: &Expr| nodes.iter().position(|node| Arc::ptr_eq(&node.0, &expr.0)).expect("topological node");
        let steps: Vec<_> = nodes
            .iter()
            .map(|node| Step {
                shape: node.0.shape,
                op: match &node.0.op {
                    Op::Slot(id) => Instruction::Slot(*id),
                    Op::Input(t) => Instruction::Input(t.clone()),
                    Op::Constant(v) => Instruction::Constant(*v),
                    Op::Add(a, b) => Instruction::Add(id(a), id(b)),
                    Op::Mul(a, b) => Instruction::Mul(id(a), id(b)),
                    Op::Relu(a) => Instruction::Relu(id(a)),
                    Op::MatMul(a, b) => Instruction::MatMul(id(a), id(b)),
                },
            })
            .collect();
        let mut slots = std::collections::BTreeMap::new();
        for step in &steps {
            if let Instruction::Slot(id) = step.op
                && slots.insert(id, step.shape).is_some_and(|shape| shape != step.shape)
            {
                return Err(error("conflicting slot types"));
            }
        }
        let fused = !steps.iter().any(|step| matches!(step.op, Instruction::MatMul(..)));
        let mut work = 0usize;
        for step in &steps {
            let count = if fused { self.0.shape.elements()? } else { step.shape.elements()? };
            let factor = if let Instruction::MatMul(a, _) = step.op { steps[a].shape.dims().1 } else { 1 };
            work = count
                .checked_mul(factor)
                .and_then(|n| work.checked_add(n))
                .filter(|n| *n <= MAX_WORK)
                .ok_or_else(|| error("graph work budget exceeded"))?;
        }
        Ok(Executable { steps, slots, fused, _lease: Lease::acquire(self.0.lease.domain.clone(), true)? })
    }
}
#[derive(Debug)]
/// Validated f32 operations, independent of a kernel language or execution backend.
pub enum Instruction {
    Slot(usize),
    Input(Tensor),
    Constant(f32),
    Add(usize, usize),
    Mul(usize, usize),
    Relu(usize),
    MatMul(usize, usize),
}
#[derive(Debug)]
/// One typed value in the validated topological plan.
pub struct Step {
    pub shape: Shape,
    pub op: Instruction,
}
#[derive(Debug)]
pub struct Executable {
    _lease: Lease,
    steps: Vec<Step>,
    slots: std::collections::BTreeMap<usize, Shape>,
    fused: bool,
}
/// Execution-owned bindings. Cloning retains immutable owners, never payload copies.
#[derive(Clone, Debug, Default)]
pub struct Bindings(std::collections::BTreeMap<usize, Tensor>);
impl Bindings {
    pub fn set(&mut self, slot: usize, tensor: Tensor) -> Result<()> {
        if slot >= MAX_NODES {
            return Err(error("slot id budget exceeded"));
        }
        self.0.insert(slot, tensor);
        Ok(())
    }
}
fn relu(value: f32) -> f32 {
    if value <= 0.0 { 0.0 } else { value }
}
impl Executable {
    pub fn validate_bindings(&self, bindings: &Bindings) -> Result<()> {
        if bindings.0.len() != self.slots.len() {
            return Err(error("input slot set mismatch"));
        }
        for (slot, shape) in &self.slots {
            if bindings.0.get(slot).is_none_or(|tensor| tensor.shape != *shape) {
                return Err(error("input slot type/shape mismatch"));
            }
        }
        Ok(())
    }
    /// Read-only adapter boundary. Plans can only be created by validation.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
    #[must_use]
    pub fn dump(&self) -> String {
        let mut text = format!("f32 strict; synchronous; fused={}\n", self.fused);
        for (id, step) in self.steps.iter().enumerate() {
            let op = match &step.op {
                Instruction::Input(_) => "Input(owner retained)".to_owned(),
                other => format!("{other:?}"),
            };
            writeln!(text, "%{id}: {:?} = {op}", step.shape).expect("string writing");
        }
        text
    }
    pub fn run(&self, budget: &Arc<Budget>) -> Result<Tensor> {
        self.run_bound(&Bindings::default(), budget)
    }
    pub fn run_bound(&self, bindings: &Bindings, budget: &Arc<Budget>) -> Result<Tensor> {
        self.validate_bindings(bindings)?;

        let shape = self.steps.last().expect("nonempty compiled graph").shape;
        if self.fused {
            let mut output = budget.allocate(shape.elements()? * 4)?;
            let mut registers = [0.0f32; MAX_NODES];
            let (rows, cols) = shape.dims();
            for row in 0..rows {
                for col in 0..cols {
                    for (i, step) in self.steps.iter().enumerate() {
                        registers[i] = match &step.op {
                            Instruction::Slot(id) => bindings.0[id].at(row, col),
                            Instruction::Input(t) => t.at(row, col),
                            Instruction::Constant(v) => *v,
                            Instruction::Add(a, b) => registers[*a] + registers[*b],
                            Instruction::Mul(a, b) => registers[*a] * registers[*b],
                            Instruction::Relu(a) => relu(registers[*a]),
                            Instruction::MatMul(..) => unreachable!("fused graph"),
                        };
                    }
                    let offset = (row * cols + col) * 4;
                    output.bytes[offset..offset + 4].copy_from_slice(&registers[self.steps.len() - 1].to_le_bytes());
                }
            }
            return Tensor::contiguous(output, shape);
        }
        // Reference partition path: matrices are native bulk loops; materialized intermediates
        // are charged to the same host budget and released on failure or return.
        let mut values: Vec<Tensor> = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            if let Instruction::Slot(id) = step.op {
                values.push(bindings.0[&id].clone());
                continue;
            }
            if let Instruction::Input(t) = &step.op {
                values.push(t.clone());
                continue;
            }
            let mut output = budget.allocate(step.shape.elements()? * 4)?;
            let (rows, cols) = step.shape.dims();
            for row in 0..rows {
                for col in 0..cols {
                    let value = match step.op {
                        Instruction::Constant(v) => v,
                        Instruction::Add(a, b) => values[a].at(row, col) + values[b].at(row, col),
                        Instruction::Mul(a, b) => values[a].at(row, col) * values[b].at(row, col),
                        Instruction::Relu(a) => relu(values[a].at(row, col)),
                        Instruction::MatMul(a, b) => {
                            let mut sum = 0.0f32;
                            for k in 0..values[a].shape.dims().1 {
                                let product = values[a].at(row, k) * values[b].at(k, col);
                                sum += product;
                            }
                            sum
                        }
                        Instruction::Slot(_) | Instruction::Input(_) => unreachable!("input already retained"),
                    };
                    let offset = (row * cols + col) * 4;
                    output.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
            values.push(Tensor::contiguous(output, step.shape)?);
        }
        Ok(values.pop().expect("nonempty compiled graph"))
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::cast_precision_loss, clippy::many_single_char_names)]
mod tests {
    use super::*;
    static TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn tensor(v: &[f32], shape: Shape, budget: &Arc<Budget>) -> Tensor {
        let bytes: Vec<_> = v.iter().flat_map(|v| v.to_le_bytes()).collect();
        Tensor::import(&bytes, shape, budget).unwrap()
    }
    fn floats(t: &Tensor) -> Vec<f32> {
        t.readback().unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
    }
    #[test]
    #[allow(clippy::items_after_statements)] // Local test storage adapter.
    fn aggregate_limits_and_external_owner() {
        let _guard = TEST_GATE.lock().unwrap();
        let nodes: Vec<_> = (0..4096).map(|_| Expr::constant(1.).unwrap()).collect();
        assert!(Expr::constant(2.).is_err());
        let plans: Vec<_> = (0..128).map(|_| nodes[0].compile().unwrap()).collect();
        assert!(nodes[0].compile().is_err());
        drop(plans);
        assert!(nodes[0].compile().is_ok());
        drop(nodes);
        assert_eq!(LIVE_NODES.load(Ordering::Relaxed), 0);
        #[derive(Debug)]
        struct External(Arc<[u8]>);
        impl Storage for External {
            fn bytes(&self) -> &[u8] {
                &self.0
            }
        }
        let bytes: Arc<[u8]> = Arc::from(3.0f32.to_le_bytes());
        let weak = Arc::downgrade(&bytes);
        let address = bytes.as_ptr();
        let owner: Arc<dyn Storage> = Arc::new(External(bytes));
        let t = Tensor::from_storage(owner.clone(), Shape::Scalar, 0, [0, 0]).unwrap();
        assert_eq!(t.owner.bytes().as_ptr(), address); // host allocation imported without copying
        drop(owner);
        assert_eq!(floats(&t), [3.]);
        drop(t);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn ownership_aliases_threads_and_budget_release() {
        let _guard = TEST_GATE.lock().unwrap();
        let budget = Budget::new(24);
        let a = tensor(&[1., 2., 3., 4., 5., 6.], Shape::Matrix(2, 3), &budget);
        let weak = Arc::downgrade(&a.owner);
        let view = a.view(Shape::Matrix(3, 2), 0, [4, 12]).unwrap();
        assert!(a.shares_storage(&view));
        assert_eq!(budget.used(), 24);
        let narrow = a.view(Shape::Matrix(1, 1), 4, [0, 0]).unwrap();
        assert!(narrow.view(Shape::Matrix(1, 2), 0, [0, 4]).is_err());
        assert!(a.view(Shape::Matrix(2, 3), usize::MAX, [12, 4]).is_err());
        assert!(a.view(Shape::Matrix(2, 3), 0, [usize::MAX, 4]).is_err());
        let executable =
            Expr::input(view.clone()).unwrap().add(&Expr::constant(2.).unwrap()).unwrap().compile().unwrap();
        assert!(executable.run(&budget).is_err()); // live inputs already occupy the entire budget
        assert_eq!(budget.used(), 24);
        drop(executable);
        drop(a);
        drop(narrow);
        assert!(weak.upgrade().is_some());
        let thread = std::thread::spawn(move || {
            assert_eq!(floats(&view), [1., 4., 2., 5., 3., 6.]);
        });
        thread.join().unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(budget.used(), 0);
        let a = tensor(&[1.], Shape::Scalar, &budget);
        assert_eq!(floats(&a), [1.]);
    }
    #[test]
    fn shapes_empty_zero_inner_and_allocation_boundaries() {
        let _guard = TEST_GATE.lock().unwrap();
        let budget = Budget::new(4 * MAX_ELEMENTS);
        assert!(Shape::Matrix(usize::MAX, 0).elements().is_err());
        assert!(Shape::Matrix(MAX_ELEMENTS, MAX_ELEMENTS).elements().is_err());
        assert_eq!(Shape::Matrix(0, 3).broadcast(Shape::Matrix(1, 3)).unwrap(), Shape::Matrix(0, 3));
        assert!(Shape::Matrix(2, 3).broadcast(Shape::Matrix(2, 2)).is_err());
        let a = Expr::input(tensor(&[], Shape::Matrix(MAX_ELEMENTS, 0), &budget)).unwrap();
        let b = Expr::input(tensor(&[], Shape::Matrix(0, 1), &budget)).unwrap();
        let executable = a.matmul(&b).unwrap().compile().unwrap();
        let out = executable.run(&budget).unwrap();
        assert_eq!(budget.used(), 4 * MAX_ELEMENTS);
        assert!(floats(&out).iter().all(|v| v.to_bits() == 0));
        assert!(executable.run(&budget).is_err());
        drop(out);
        assert_eq!(budget.used(), 0);
        assert!(a.matmul(&Expr::input(tensor(&[], Shape::Matrix(0, 2), &budget)).unwrap()).is_err());
        let empty = Expr::input(tensor(&[], Shape::Matrix(0, 3), &budget))
            .unwrap()
            .add(&Expr::constant(2.).unwrap())
            .unwrap()
            .compile()
            .unwrap()
            .run(&budget)
            .unwrap();
        assert_eq!(floats(&empty), [] as [f32; 0]);
    }
    #[test]
    fn matrix_reference_fusion_and_special_values() {
        let _guard = TEST_GATE.lock().unwrap();
        let budget = Budget::new(4096);
        let a = Expr::input(tensor(&[1., 2., 3., 4., 5., 6.], Shape::Matrix(2, 3), &budget)).unwrap();
        let b = Expr::input(tensor(&[7., 8., 9., 10., 11., 12.], Shape::Matrix(3, 2), &budget)).unwrap();
        let graph = a.matmul(&b).unwrap().add(&Expr::constant(2.).unwrap()).unwrap();
        assert_eq!(floats(&graph.compile().unwrap().run(&budget).unwrap()), [60., 66., 141., 156.]);
        let rows = Expr::input(tensor(&[1., 2.], Shape::Matrix(2, 1), &budget)).unwrap();
        let cols = Expr::input(tensor(&[10., 20., 30.], Shape::Matrix(1, 3), &budget)).unwrap();
        assert_eq!(
            floats(&rows.add(&cols).unwrap().compile().unwrap().run(&budget).unwrap()),
            [11., 21., 31., 12., 22., 32.]
        );
        let scalar = tensor(&[7.], Shape::Scalar, &budget);
        let broadcast = scalar.view(Shape::Matrix(2, 3), 0, [0, 0]).unwrap();
        assert_eq!(floats(&broadcast), [7.; 6]);
        let values = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0., 0., f32::from_bits(1)];
        let a = Expr::input(tensor(&values, Shape::Matrix(1, 6), &budget)).unwrap();
        let result = floats(&a.relu().unwrap().compile().unwrap().run(&budget).unwrap());
        assert!(result[0].is_nan());
        assert_eq!(result[1], f32::INFINITY);
        assert!(result[2..5].iter().all(|v| v.to_bits() == 0));
        assert_eq!(result[5].to_bits(), 1);
        // This pair distinguishes rounded multiplication then addition from FMA.
        let x = Expr::constant(f32::from_bits(0x3f80_0001)).unwrap();
        let y = Expr::constant(f32::from_bits(0x3f7f_fffe)).unwrap();
        let z = x.mul(&y).unwrap().add(&Expr::constant(-1.).unwrap()).unwrap();
        assert_eq!(floats(&z.compile().unwrap().run(&budget).unwrap())[0].to_bits(), 0);
    }
    #[test]
    fn bounded_compilation_and_failure_cleanup() {
        let _guard = TEST_GATE.lock().unwrap();
        let budget = Budget::new(20);
        let a = Expr::input(tensor(&[2.], Shape::Matrix(1, 1), &budget)).unwrap();
        let matrix = a.matmul(&a).unwrap().add(&Expr::constant(1.).unwrap()).unwrap();
        let plan = matrix.compile().unwrap();
        // Input (4), matrix (4), constant (4), output (4): just enough at 20.
        let out = plan.run(&budget).unwrap();
        assert_eq!(floats(&out), [5.]);
        let retained = tensor(&[0., 0.], Shape::Matrix(1, 2), &budget);
        assert!(plan.run(&budget).is_err()); // partial intermediate allocations roll back
        assert_eq!(budget.used(), 16);
        drop(retained);
        drop(out);
        assert_eq!(floats(&plan.run(&budget).unwrap()), [5.]);
        let mut graph = Expr::constant(1.).unwrap();
        for _ in 1..MAX_NODES {
            graph = graph.relu().unwrap();
        }
        assert!(graph.relu().is_err());
        assert!(graph.compile().is_ok());
        // Many unique nodes at shallow depth fail the node cap, without exponential DAG expansion.
        let mut level: Vec<_> = (0..64).map(|i| Expr::constant(i as f32).unwrap()).collect();
        while level.len() > 1 {
            level = level.as_chunks::<2>().0.iter().map(|p| p[0].add(&p[1]).unwrap()).collect();
        }
        assert!(level[0].compile().is_err());
        assert!(Expr::constant(1.).unwrap().compile().is_ok());
    }
    #[test]
    fn rebindable_program_domains_and_concurrent_owners() {
        let _guard = TEST_GATE.lock().unwrap();
        let domain = Domain::new(8, 1);
        let other = Domain::new(8, 1);
        let x = Expr::slot_in(0, Shape::Matrix(1, 3), domain.clone()).unwrap();
        let y = Expr::constant_in(2., domain.clone()).unwrap();
        assert!(x.add(&Expr::constant_in(1., other.clone()).unwrap()).is_err());
        let graph = x.add(&y).unwrap().relu().unwrap();
        let plan = Arc::new(graph.compile().unwrap());
        assert!(graph.compile().is_err());
        assert!(Expr::constant_in(1., other).unwrap().compile().is_ok());
        drop(graph);
        drop(x);
        drop(y);
        let budget = Budget::new(1024);
        let mut threads = Vec::new();
        for seed in 0..8 {
            let plan = plan.clone();
            let budget = budget.clone();
            threads.push(std::thread::spawn(move || {
                let t = tensor(&[seed as f32, -3., 1.], Shape::Matrix(1, 3), &budget);
                let weak = Arc::downgrade(&t.owner);
                let mut bound = Bindings::default();
                bound.set(0, t).unwrap();
                let result = plan.run_bound(&bound, &budget).unwrap();
                assert_eq!(floats(&result), [seed as f32 + 2., 0., 3.]);
                drop(bound);
                assert!(weak.upgrade().is_none()); // plan never captured this input
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(budget.used(), 0);
        assert!(plan.run_bound(&Bindings::default(), &budget).is_err());
        let mut bad = Bindings::default();
        bad.set(0, tensor(&[1.], Shape::Scalar, &budget)).unwrap();
        assert!(plan.run_bound(&bad, &budget).is_err());
        let a = Expr::slot(0, Shape::Matrix(1, 3)).unwrap();
        let b = Expr::slot(0, Shape::Matrix(1, 1)).unwrap();
        assert!(a.add(&b).unwrap().compile().is_err());
        drop(plan);
        assert!(Expr::constant_in(0., domain).unwrap().compile().is_ok());
    }
    #[test]
    #[allow(clippy::items_after_statements)]
    fn randomized_strides_broadcast_and_empty_dimensions() {
        let _guard = TEST_GATE.lock().unwrap();
        let budget = Budget::new(65536);
        let mut state = 17u32;
        let mut random = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        for _ in 0..256 {
            let rows = (random() % 7) as usize;
            let cols = (random() % 7) as usize;
            let shape = Shape::Matrix(rows, cols);
            let stride = (random() % 5) as usize * 4;
            let offset = (random() % 4) as usize; // deliberately unaligned
            let row_stride = cols * stride + 8;
            let size = offset + rows * row_stride + cols * stride + 4;
            let mut bytes = vec![0u8; size];
            // Values are a function of byte position, so alias/zero-stride expectations are
            // independently evaluated from the actual final storage, never from Tensor::at.
            for r in 0..rows {
                for c in 0..cols {
                    let at = offset + r * row_stride + c * stride;
                    bytes[at..at + 4].copy_from_slice(&((at % 29) as f32 - 14.).to_le_bytes());
                }
            }
            let base =
                Tensor::import(&vec![0u8; size.next_multiple_of(4)], Shape::Matrix(1, size.div_ceil(4)), &budget)
                    .unwrap();
            // Use a bounded host owner holding precisely the delegated byte interval.
            #[derive(Debug)]
            struct Bytes(Vec<u8>);
            impl Storage for Bytes {
                fn bytes(&self) -> &[u8] {
                    &self.0
                }
            }
            let t = Tensor::from_storage(Arc::new(Bytes(bytes.clone())), shape, offset, [row_stride, stride]).unwrap();
            let column = tensor(&vec![2.; rows], Shape::Matrix(rows, 1), &budget);
            let plan = Expr::slot(0, shape)
                .unwrap()
                .add(&Expr::slot(1, Shape::Matrix(rows, 1)).unwrap())
                .unwrap()
                .compile()
                .unwrap();
            let mut bound = Bindings::default();
            bound.set(0, t).unwrap();
            bound.set(1, column).unwrap();
            let actual = floats(&plan.run_bound(&bound, &budget).unwrap());
            let mut expected = Vec::new();
            for r in 0..rows {
                for c in 0..cols {
                    let at = offset + r * row_stride + c * stride;
                    expected.push(f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) + 2.);
                }
            }
            assert_eq!(actual, expected);
            assert!(base.view(Shape::Matrix(1, 2), size.next_multiple_of(4), [0, 4]).is_err());
        }
        assert_eq!(budget.used(), 0);
    }
}
