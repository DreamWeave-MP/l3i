//! Immutable tensor storage, checked views and the host-owned payload budget.
use crate::Result;
use crate::compute::{ADDRESSABLE_ELEMENTS, Limits, error};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Host-provided immutable storage. The owner must retain its bytes for its entire lifetime.
/// Jess can adapt its `SharedBuffer` here without copying, moving its allocation, or exposing writes.
pub trait Storage: Send + Sync + std::fmt::Debug {
    fn bytes(&self) -> &[u8];
}

/// Live native payload charged to one host domain. Views and programs never recharge.
#[derive(Debug)]
pub struct Budget {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
    /// Elements one tensor made under this budget may have, which memory alone can't bound for a
    /// view repeating a few elements.
    elements: usize,
    allocations: AtomicUsize,
    denials: AtomicUsize,
}
impl Budget {
    /// `limit` bytes, and [`Limits::default`]'s tensor size.
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Self::with_elements(limit, Limits::default().elements)
    }
    /// `limit` bytes, and tensors of up to `elements` elements.
    #[must_use]
    pub fn with_elements(limit: usize, elements: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            denials: AtomicUsize::new(0),
            limit,
            elements,
        })
    }
    /// Refuses a tensor shape over this budget's element limit. Not a denial: no collection helps.
    pub(crate) fn admit(&self, shape: Shape) -> Result<()> {
        shape.within(self.elements).map(drop)
    }
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn allocations(&self) -> usize {
        self.allocations.load(Ordering::Relaxed)
    }
    /// How many requests the limit has refused. A host whose payload can be held only by
    /// garbage compares this around a failed call, collects, and retries.
    #[must_use]
    pub fn denials(&self) -> usize {
        self.denials.load(Ordering::Relaxed)
    }
    fn reserve(&self, len: usize) -> Result<()> {
        let before = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| used.checked_add(len).filter(|n| *n <= self.limit))
            .map_err(|_| {
                self.denials.fetch_add(1, Ordering::Relaxed);
                error("live storage budget exceeded")
            })?;
        self.peak.fetch_max(before + len, Ordering::Relaxed);
        Ok(())
    }
    /// Charges `len` bytes that live elsewhere (scratch) until the guard drops.
    pub(crate) fn charge(&self, len: usize) -> Result<Charge<'_>> {
        self.reserve(len)?;
        Ok(Charge(self, len))
    }
    pub(crate) fn allocate(self: &Arc<Self>, len: usize) -> Result<Allocation> {
        self.reserve(len)?;
        let mut allocation = Allocation { bytes: Vec::new(), charge: len, budget: self.clone() };
        if len > 0 {
            if !crate::compute::may_allocate(len) {
                return Err(error("allocation failed"));
            }
            allocation.bytes.try_reserve_exact(len).map_err(|_| error("allocation failed"))?;
            allocation.bytes.resize(len, 0);
            self.allocations.fetch_add(1, Ordering::Relaxed);
        }
        Ok(allocation)
    }
}

pub(crate) struct Charge<'a>(&'a Budget, usize);
impl Drop for Charge<'_> {
    fn drop(&mut self) {
        self.0.used.fetch_sub(self.1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct Allocation {
    pub(crate) bytes: Vec<u8>,
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
    /// How many elements this shape has, if that is a number this crate can represent.
    pub fn elements(self) -> Result<usize> {
        self.within(ADDRESSABLE_ELEMENTS)
    }
    /// The element count, when it and each dimension are at most `limit`. Each dimension is bounded
    /// even when another is zero.
    pub(crate) fn within(self, limit: usize) -> Result<usize> {
        let limit = limit.min(ADDRESSABLE_ELEMENTS);
        let (rows, cols) = self.dims();
        if rows > limit || cols > limit {
            return Err(error("dimension budget exceeded"));
        }
        rows.checked_mul(cols).filter(|n| *n <= limit).ok_or_else(|| error("element budget exceeded"))
    }
    #[must_use]
    pub fn dims(self) -> (usize, usize) {
        match self {
            Self::Scalar => (1, 1),
            Self::Matrix(r, c) => (r, c),
        }
    }
    pub(crate) fn broadcast(self, other: Self) -> Result<Self> {
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

/// The last byte a view can touch, with every step checked.
fn view_end(shape: Shape, offset: usize, strides: [usize; 2]) -> Result<usize> {
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
    end.ok_or_else(|| error("view outside storage"))
}

/// A borrowed, checked f32 view: the one thing executors read. A view of a [`Tensor`] borrows
/// its owner; a view of a Luau buffer lasts one native call and is never retained.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) shape: Shape,
    pub(crate) offset: usize,
    pub(crate) strides: [usize; 2],
    pub(crate) prepared: Option<&'a Prepared>,
}
impl<'a> View<'a> {
    /// A row-major contiguous view of exactly `bytes`.
    pub fn contiguous(bytes: &'a [u8], shape: Shape) -> Result<Self> {
        if bytes.len() != shape.elements()? * 4 {
            return Err(error("input byte size mismatch"));
        }
        Ok(Self { bytes, shape, offset: 0, strides: [shape.dims().1 * 4, 4], prepared: None })
    }
    #[must_use]
    pub fn shape(&self) -> Shape {
        self.shape
    }
    /// One element, by the checked per-element path the reference executor uses.
    #[inline]
    #[must_use]
    pub fn at(&self, row: usize, col: usize) -> f32 {
        let (rows, cols) = self.shape.dims();
        let index = self.offset
            + if rows == 1 { 0 } else { row * self.strides[0] }
            + if cols == 1 { 0 } else { col * self.strides[1] };
        f32::from_le_bytes(self.bytes[index..index + 4].try_into().expect("validated view"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreparedSide {
    Left,
    Right,
}
/// A tensor's elements packed ahead of time, under the budget that was asked for them.
#[derive(Debug)]
pub struct Prepared {
    pack: Allocation,
    /// Where the elements start: a cache line, so that no vector load of them straddles two.
    start: usize,
    side: PreparedSide,
}
impl Prepared {
    pub(crate) fn elements(&self, side: PreparedSide) -> Option<&[[u8; 4]]> {
        (self.side == side).then(|| self.pack.bytes[self.start..].as_chunks::<4>().0)
    }
}

/// A view's element addresses in a form that decides membership at once: two steps where the
/// smaller's whole run fits inside one larger step, so every address has one decomposition. Each
/// pair is a count and a step in bytes.
struct Lattice {
    origin: usize,
    outer: (usize, usize),
    inner: (usize, usize),
}
impl Lattice {
    /// `None` when a view's steps can reach one address in two ways, or its elements partly overlap:
    /// then no single decomposition exists, and this declines to search for one.
    fn of(view: &Tensor) -> Result<Option<Self>> {
        let (rows, cols) = view.shape.dims();
        if view.shape.elements()? == 0 {
            return Ok(Some(Self { origin: view.offset, outer: (0, 1), inner: (1, 1) }));
        }
        // A repeated dimension adds no addresses.
        let row = if rows == 1 || view.strides[0] == 0 { (1, 1) } else { (rows, view.strides[0]) };
        let col = if cols == 1 || view.strides[1] == 0 { (1, 1) } else { (cols, view.strides[1]) };
        let (outer, inner) = if row.1 >= col.1 { (row, col) } else { (col, row) };
        let partial = (inner.0 > 1 && inner.1 < 4) || (outer.0 > 1 && outer.1 < 4);
        if partial || (inner.0 > 1 && (inner.0 - 1) * inner.1 + 4 > outer.1) {
            return Ok(None);
        }
        Ok(Some(Self { origin: view.offset, outer, inner }))
    }
    /// Whether the elements are one gapless run from the origin.
    fn contiguous(&self) -> bool {
        if self.inner.0 == 1 {
            self.outer.0 == 1 || self.outer.1 == 4
        } else {
            self.inner.1 == 4 && self.outer.1 == self.inner.0 * 4
        }
    }
    fn contains(&self, address: usize) -> bool {
        let Some(distance) = address.checked_sub(self.origin) else {
            return false;
        };
        let (outer, inner) = (distance / self.outer.1, distance % self.outer.1);
        outer < self.outer.0
            && if self.inner.0 == 1 {
                inner == 0
            } else {
                inner.is_multiple_of(self.inner.1) && inner / self.inner.1 < self.inner.0
            }
    }
}

/// An owner, a checked byte interval and a shape. Cloning retains the owner, never the payload.
#[derive(Clone, Debug)]
pub struct Tensor {
    pub(crate) owner: Arc<dyn Storage>,
    shape: Shape,
    offset: usize,
    strides: [usize; 2],
    end: usize,
    prepared: Option<Arc<Prepared>>,
}
impl Tensor {
    /// Trusted host entry: retain an existing immutable owner, without a payload copy.
    /// External allocation accounting belongs to its host, not this executor's output budget.
    pub fn from_storage(owner: Arc<dyn Storage>, shape: Shape, offset: usize, strides: [usize; 2]) -> Result<Self> {
        let end = view_end(shape, offset, strides)?;
        if end > owner.bytes().len() {
            return Err(error("view outside storage"));
        }
        Ok(Self { owner, shape, offset, strides, end, prepared: None })
    }
    pub fn import(bytes: &[u8], shape: Shape, budget: &Arc<Budget>) -> Result<Self> {
        if bytes.len() != shape.within(budget.elements)? * 4 {
            return Err(error("input byte size mismatch"));
        }
        let mut allocation = budget.allocate(bytes.len())?;
        allocation.bytes.copy_from_slice(bytes);
        Self::contiguous(allocation, shape)
    }
    pub fn import_view(bytes: &crate::convert::BufferView<'_>, shape: Shape, budget: &Arc<Budget>) -> Result<Self> {
        if bytes.len() != shape.within(budget.elements)? * 4 {
            return Err(error("input byte size mismatch"));
        }
        let mut allocation = budget.allocate(bytes.len())?;
        bytes.read(0, &mut allocation.bytes)?;
        Self::contiguous(allocation, shape)
    }
    pub(crate) fn contiguous(allocation: Allocation, shape: Shape) -> Result<Self> {
        Self::from_storage(Arc::new(allocation), shape, 0, [shape.dims().1 * 4, 4])
    }
    /// Derives a view of the same owner, with `offset` in bytes from this view's first element.
    ///
    /// A view is an element-level capability: every element of the result must be an element of
    /// this view, at the same byte address. Gaps a strided parent steps over stay unreachable, and so
    /// does any reinterpretation of bytes across its elements. Repeating a parent's elements is
    /// allowed, since it reveals nothing new. A parent whose own strides make different positions
    /// overlap can only re-derive itself. The check costs one comparison when the parent is
    /// contiguous and the result's strides are whole elements, and otherwise one per derived element.
    /// A derived view carries no [`Prepared`] weights: they describe the parent's elements.
    pub fn view(&self, shape: Shape, offset: usize, strides: [usize; 2]) -> Result<Self> {
        let offset = self.offset.checked_add(offset).ok_or_else(|| error("offset overflow"))?;
        let view = Self::from_storage(self.owner.clone(), shape, offset, strides)?;
        if view.end > self.end {
            return Err(error("view widens parent elements"));
        }
        if !self.covers(&view)? {
            return Err(error("view reaches outside parent elements"));
        }
        Ok(view)
    }
    /// Whether every element of `child` sits at the byte address of one of this view's elements.
    fn covers(&self, child: &Self) -> Result<bool> {
        let (rows, cols) = child.shape.dims();
        if child.shape.elements()? == 0 {
            return Ok(true);
        }
        if self.shape == child.shape && self.offset == child.offset && self.strides == child.strides {
            return Ok(true);
        }
        let Some(lattice) = Lattice::of(self)? else {
            return Err(error("view of a parent whose elements overlap"));
        };
        // A contiguous parent is one interval, already checked against `end`.
        let step = |count: usize, stride: usize| count == 1 || stride.is_multiple_of(4);
        if lattice.contiguous()
            && (child.offset - lattice.origin).is_multiple_of(4)
            && step(rows, child.strides[0])
            && step(cols, child.strides[1])
        {
            return Ok(true);
        }
        for row in 0..rows {
            for col in 0..cols {
                if !lattice.contains(child.offset + row * child.strides[0] + col * child.strides[1]) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
    /// A tensor over the same elements that also holds them packed for use as the right operand of
    /// a matmul with more than one output row and column, so repeated products skip packing and its
    /// scratch. The packing is charged to `budget` until the last tensor sharing it drops. The owner
    /// is immutable, so a packing can never go stale. Views derived from the result don't inherit it.
    pub fn prepare_right(&self, budget: &Arc<Budget>) -> Result<Self> {
        self.prepare(budget, PreparedSide::Right)
    }
    /// A tensor over the same elements that also holds them in column panels for use as the left
    /// operand of a matmul with one output column, a weight matrix times a vector. Packing rows
    /// sideways is what lets strict additions use every SIMD lane, so unlike [`Self::prepare_right`]
    /// this changes the kernel rather than only saving a copy. Charged and shared the same way.
    pub fn prepare_left(&self, budget: &Arc<Budget>) -> Result<Self> {
        self.prepare(budget, PreparedSide::Left)
    }
    fn prepare(&self, budget: &Arc<Budget>, side: PreparedSide) -> Result<Self> {
        budget.admit(self.shape)?;
        let (rows, cols) = self.shape.dims();
        let elements = match side {
            PreparedSide::Right => crate::compute::kernels::prepared_right_elements(rows, cols),
            PreparedSide::Left => crate::compute::kernels::prepared_left_elements(rows, cols),
        };
        let padding = if elements == 0 { 0 } else { crate::compute::kernels::CACHE_LINE };
        let mut pack = budget.allocate(elements * 4 + padding)?;
        let start = pack.bytes.as_ptr().align_offset(crate::compute::kernels::CACHE_LINE).min(pack.bytes.len());
        let source = crate::compute::kernels::Source::bind(self.borrow(), self.shape);
        let walked;
        let contiguous = if let crate::compute::kernels::Source::Direct(contiguous) = source {
            contiguous
        } else {
            walked = {
                let mut copy = budget.allocate(self.shape.elements()? * 4)?;
                source.materialize(&mut copy.bytes);
                copy
            };
            walked.bytes.as_chunks::<4>().0
        };
        let target = pack.bytes[start..].as_chunks_mut::<4>().0;
        match side {
            PreparedSide::Right => crate::compute::kernels::prepare_right(contiguous, rows, cols, target),
            PreparedSide::Left => crate::compute::kernels::prepare_left(contiguous, rows, cols, target),
        }
        Ok(Self { prepared: Some(Arc::new(Prepared { pack, start, side })), ..self.clone() })
    }
    /// Budgeted bytes this tensor's [`Prepared`] weights hold, shared with whatever else holds them.
    #[must_use]
    pub fn prepared_bytes(&self) -> usize {
        self.prepared.as_ref().map_or(0, |prepared| prepared.pack.bytes.len())
    }
    #[must_use]
    pub fn borrow(&self) -> View<'_> {
        View {
            bytes: self.owner.bytes(),
            shape: self.shape,
            offset: self.offset,
            strides: self.strides,
            prepared: self.prepared.as_deref(),
        }
    }
    #[must_use]
    pub fn shape(&self) -> Shape {
        self.shape
    }
    #[must_use]
    pub fn shares_storage(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
    }
    pub fn readback(&self) -> Result<Vec<u8>> {
        let count = self.shape.elements()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(count * 4).map_err(|_| error("readback allocation failed"))?;
        bytes.resize(count * 4, 0);
        crate::compute::kernels::Source::bind(self.borrow(), self.shape).materialize(&mut bytes);
        Ok(bytes)
    }
}
