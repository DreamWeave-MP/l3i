//! Immutable tensor storage, checked views and the host-owned payload budget.
use crate::{MAX_ELEMENTS, error};
use l3i::Result;
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
    allocations: AtomicUsize,
    denials: AtomicUsize,
}
impl Budget {
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            denials: AtomicUsize::new(0),
            limit,
        })
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
}
impl<'a> View<'a> {
    /// A row-major contiguous view of exactly `bytes`.
    pub fn contiguous(bytes: &'a [u8], shape: Shape) -> Result<Self> {
        if bytes.len() != shape.elements()? * 4 {
            return Err(error("input byte size mismatch"));
        }
        Ok(Self { bytes, shape, offset: 0, strides: [shape.dims().1 * 4, 4] })
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

/// An owner, a checked byte interval and a shape. Cloning retains the owner, never the payload.
#[derive(Clone, Debug)]
pub struct Tensor {
    pub(crate) owner: Arc<dyn Storage>,
    shape: Shape,
    offset: usize,
    strides: [usize; 2],
    end: usize,
}
impl Tensor {
    /// Trusted host entry: retain an existing immutable owner, without a payload copy.
    /// External allocation accounting belongs to its host, not this executor's output budget.
    pub fn from_storage(owner: Arc<dyn Storage>, shape: Shape, offset: usize, strides: [usize; 2]) -> Result<Self> {
        let end = view_end(shape, offset, strides)?;
        if end > owner.bytes().len() {
            return Err(error("view outside storage"));
        }
        Ok(Self { owner, shape, offset, strides, end })
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
    pub(crate) fn contiguous(allocation: Allocation, shape: Shape) -> Result<Self> {
        Self::from_storage(Arc::new(allocation), shape, 0, [shape.dims().1 * 4, 4])
    }
    /// Derive a view of the same owner; it cannot widen the parent's backing byte interval.
    pub fn view(&self, shape: Shape, offset: usize, strides: [usize; 2]) -> Result<Self> {
        let offset = self.offset.checked_add(offset).ok_or_else(|| error("offset overflow"))?;
        let view = Self::from_storage(self.owner.clone(), shape, offset, strides)?;
        if view.end > self.end {
            return Err(error("view widens parent byte interval"));
        }
        Ok(view)
    }
    #[must_use]
    pub fn borrow(&self) -> View<'_> {
        View { bytes: self.owner.bytes(), shape: self.shape, offset: self.offset, strides: self.strides }
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
        crate::kernels::Source::bind(self.borrow(), self.shape).materialize(&mut bytes);
        Ok(bytes)
    }
}
