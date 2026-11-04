//! Typed expression nodes: structure and shapes only. Nodes never hold input data.
use crate::program::{Numeric, Program};
use crate::{Shape, error};
use l3i::Result;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Largest compiled graph, and the longest path through it.
pub(crate) const MAX_NODES: usize = 1024;
/// Scalar operations one invocation may perform.
pub(crate) const MAX_WORK: usize = 67_108_864;

/// Host-owned accounting domain. Every limit here belongs to its caller; nothing is process-wide.
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
    #[must_use]
    pub fn live_plans(&self) -> usize {
        self.plans.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn live_nodes(&self) -> usize {
        self.nodes.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct Lease {
    domain: Arc<Domain>,
    plan: bool,
}
impl Lease {
    pub(crate) fn acquire(domain: Arc<Domain>, plan: bool) -> Result<Self> {
        let (counter, limit) =
            if plan { (&domain.plans, domain.plan_limit) } else { (&domain.nodes, domain.node_limit) };
        counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < limit).then_some(n + 1))
            .map_err(|_| error("domain graph budget exhausted"))?;
        Ok(Self { domain, plan })
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let counter = if self.plan { &self.domain.plans } else { &self.domain.nodes };
        counter.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
enum Op {
    Slot(usize),
    Constant(f32),
    Add(Expr, Expr),
    Mul(Expr, Expr),
    Relu(Expr),
    SumRows(Expr),
    Softmax(Expr),
    MatMul(Expr, Expr),
}
#[derive(Debug)]
struct Node {
    lease: Lease,
    shape: Shape,
    op: Op,
    depth: usize,
}
/// One immutable typed value in a computation. Cloning shares the node.
#[derive(Clone, Debug)]
pub struct Expr(Arc<Node>);
impl Expr {
    fn new(shape: Shape, op: Op, depth: usize, domain: Arc<Domain>) -> Result<Self> {
        shape.elements()?;
        if depth > MAX_NODES {
            return Err(error("graph depth budget exceeded"));
        }
        Ok(Self(Arc::new(Node { shape, op, depth, lease: Lease::acquire(domain, false)? })))
    }
    /// Input position `id` of the compiled program. Ids must be dense when compiled.
    pub fn slot(id: usize, shape: Shape, domain: Arc<Domain>) -> Result<Self> {
        if id >= MAX_NODES {
            return Err(error("slot id budget exceeded"));
        }
        Self::new(shape, Op::Slot(id), 1, domain)
    }
    pub fn constant(value: f32, domain: Arc<Domain>) -> Result<Self> {
        Self::new(Shape::Scalar, Op::Constant(value), 1, domain)
    }
    fn same_domain(&self, rhs: &Self) -> Result<()> {
        if Arc::ptr_eq(&self.0.lease.domain, &rhs.0.lease.domain) {
            Ok(())
        } else {
            Err(error("cross-domain graph composition denied"))
        }
    }
    fn binary(&self, rhs: &Self, shape: Shape, op: Op) -> Result<Self> {
        self.same_domain(rhs)?;
        Self::new(shape, op, 1 + self.0.depth.max(rhs.0.depth), self.0.lease.domain.clone())
    }
    pub fn add(&self, rhs: &Self) -> Result<Self> {
        self.binary(rhs, self.0.shape.broadcast(rhs.0.shape)?, Op::Add(self.clone(), rhs.clone()))
    }
    pub fn mul(&self, rhs: &Self) -> Result<Self> {
        self.binary(rhs, self.0.shape.broadcast(rhs.0.shape)?, Op::Mul(self.clone(), rhs.clone()))
    }
    pub fn relu(&self) -> Result<Self> {
        Self::new(self.0.shape, Op::Relu(self.clone()), 1 + self.0.depth, self.0.lease.domain.clone())
    }
    /// Ordered f32 sum across columns, retaining one column per row.
    /// Accurate softmax independently across each row; see [`crate::reduction::softmax`].
    pub fn softmax(&self) -> Result<Self> {
        Self::new(self.0.shape, Op::Softmax(self.clone()), 1 + self.0.depth, self.0.lease.domain.clone())
    }
    pub fn sum_rows(&self) -> Result<Self> {
        let (rows, _) = self.0.shape.dims();
        Self::new(Shape::Matrix(rows, 1), Op::SumRows(self.clone()), 1 + self.0.depth, self.0.lease.domain.clone())
    }
    pub fn matmul(&self, rhs: &Self) -> Result<Self> {
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
        self.binary(rhs, shape, Op::MatMul(self.clone(), rhs.clone()))
    }
    /// Validates the DAG into a [`Program`]. The result retains no node and no input data.
    pub fn compile(&self) -> Result<Program> {
        self.compile_with(Numeric::Strict)
    }
    /// [`Self::compile`] under an explicit numerical mode.
    pub fn compile_with(&self, numeric: Numeric) -> Result<Program> {
        type Ids = HashMap<*const Node, usize>;
        fn visit(expr: &Expr, ids: &mut Ids, steps: &mut Vec<Step>) -> Result<usize> {
            let key = Arc::as_ptr(&expr.0);
            if let Some(id) = ids.get(&key) {
                return Ok(*id);
            }
            let op = match &expr.0.op {
                Op::Slot(id) => Instruction::Slot(*id),
                Op::Constant(v) => Instruction::Constant(*v),
                Op::Add(a, b) => Instruction::Add(visit(a, ids, steps)?, visit(b, ids, steps)?),
                Op::Mul(a, b) => Instruction::Mul(visit(a, ids, steps)?, visit(b, ids, steps)?),
                Op::MatMul(a, b) => Instruction::MatMul(visit(a, ids, steps)?, visit(b, ids, steps)?),
                Op::Softmax(a) => Instruction::Softmax(visit(a, ids, steps)?),
                Op::SumRows(a) => Instruction::SumRows(visit(a, ids, steps)?),
                Op::Relu(a) => Instruction::Relu(visit(a, ids, steps)?),
            };
            if steps.len() >= MAX_NODES {
                return Err(error("graph node budget exceeded"));
            }
            steps.push(Step { shape: expr.0.shape, op });
            ids.insert(key, steps.len() - 1);
            Ok(steps.len() - 1)
        }
        let mut steps = Vec::new();
        visit(self, &mut Ids::new(), &mut steps)?;
        Program::plan(steps, Lease::acquire(self.0.lease.domain.clone(), true)?, numeric)
    }
}

/// Validated f32 operations, independent of any execution backend.
#[derive(Clone, Copy, Debug)]
pub enum Instruction {
    Slot(usize),
    Constant(f32),
    Add(usize, usize),
    Mul(usize, usize),
    Relu(usize),
    SumRows(usize),
    Softmax(usize),
    MatMul(usize, usize),
}
/// One typed value in the validated topological plan.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    pub shape: Shape,
    pub op: Instruction,
}
