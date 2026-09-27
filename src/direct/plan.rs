//! A direct dispatch plan resolved against one runtime.
//!
//! [`registry::Registry`](super::registry::Registry) is the compile-time table for hosts whose
//! tags and atoms are fixed constants. A [`DirectPlan`] is the same `(tag, kind, atom) -> slot`
//! table built at run time from what this VM actually assigned: a type's tag comes from the
//! runtime's tag plan, a member's atom from the VM's catalogue, and the slot ids are the host's
//! protocol identifiers. Handlers written against a plan keep working when the same type is tag
//! 8 in one VM and tag 17 in another. The cache-hit path compares the cached descriptor's
//! `TypeId`, so it never needs a tag lookup.

use std::any::TypeId;
use std::rc::Rc;

use super::registry::UNKNOWN_SLOT;
use super::{ACCESS_KIND_COUNT, AccessKind, Atom};
use crate::TAG_LIMIT;
use crate::error::{Error, Result};
use crate::runtime::Runtime;
use crate::stack::Scope;
use crate::userdata::{RuntimeTag, Userdata, tagged};

/// One resolved dispatch entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    pub slot: u16,
    pub tag: RuntimeTag,
    pub atom: Atom,
    pub kind: AccessKind,
    pub type_id: TypeId,
    pub type_name: &'static str,
    pub member: String,
}

/// Slots and the catalogued atom span a plan accepts. Planners allocate both densely; a plan
/// is a dense table, so a two-member plan with atoms 1 and 30000 would cost megabytes.
pub const MAX_SLOT: u16 = 4096;
pub const MAX_ATOM_SPAN: usize = 4096;

/// The dense table; see the module docs.
#[derive(Debug)]
pub struct DirectPlan {
    entries: Vec<PlanEntry>,
    /// Entry index per slot id, for the O(1) cache-hit check.
    by_slot: Vec<Option<u32>>,
    first_atom: Atom,
    atom_count: usize,
    /// `slots[tag * ACCESS_KIND_COUNT * atom_count + kind * atom_count + (atom - first_atom)]`.
    slots: Vec<u16>,
}

impl DirectPlan {
    fn index(&self, tag: i32, kind: AccessKind, atom: Atom) -> Option<usize> {
        if tag < 0 || tag >= i32::from(TAG_LIMIT) || atom < self.first_atom {
            return None;
        }
        let offset = (atom - self.first_atom) as usize;
        if offset >= self.atom_count {
            return None;
        }
        Some(tag as usize * ACCESS_KIND_COUNT * self.atom_count + kind as usize * self.atom_count + offset)
    }

    pub fn entries(&self) -> &[PlanEntry] {
        &self.entries
    }

    /// The slot for `(tag, atom, kind)`, or [`UNKNOWN_SLOT`].
    pub fn resolve_slot(&self, tag: i32, atom: Atom, kind: AccessKind) -> u16 {
        self.index(tag, kind, atom).map_or(UNKNOWN_SLOT, |index| self.slots[index])
    }

    #[inline]
    fn entry(&self, slot: u16) -> Option<&PlanEntry> {
        self.by_slot.get(usize::from(slot)).copied().flatten().map(|index| &self.entries[index as usize])
    }

    /// True when `cached` names exactly `(T, atom, kind)` in this plan: the cache-hit test, with
    /// no tag lookup.
    pub fn cached_slot_matches<T: Userdata>(&self, cached: u16, atom: Atom, kind: AccessKind) -> bool {
        self.entry(cached)
            .is_some_and(|entry| entry.type_id == TypeId::of::<T>() && entry.atom == atom && entry.kind == kind)
    }

    /// Resolves through Luau's per-instruction cache for a handler of `T`: a validated hit is
    /// returned at once; a miss resolves `T`'s tag in `scope`'s VM and writes the slot back.
    pub fn resolve_cached_slot<T: Userdata>(
        &self,
        scope: &impl Scope,
        cached: &mut u16,
        atom: Atom,
        kind: AccessKind,
    ) -> u16 {
        if self.cached_slot_matches::<T>(*cached, atom, kind) {
            return *cached;
        }
        let resolved = match tagged::tag_of::<T>(scope) {
            Some(tag) => self.resolve_slot(i32::from(tag), atom, kind),
            None => UNKNOWN_SLOT,
        };
        *cached = resolved;
        resolved
    }
}

/// Builds a [`DirectPlan`] for one runtime.
pub struct DirectPlanBuilder<'r> {
    runtime: &'r Runtime,
    entries: Vec<PlanEntry>,
}

impl<'r> DirectPlanBuilder<'r> {
    pub fn new(runtime: &'r Runtime) -> Self {
        DirectPlanBuilder { runtime, entries: Vec::new() }
    }

    /// Maps `T.member` accessed as `kind` to `slot` (a host protocol id above 0, unique in the
    /// plan). `T` must be registered tagged in this runtime and `member` catalogued as an atom.
    pub fn slot<T: Userdata>(mut self, kind: AccessKind, member: &str, slot: u16) -> Result<Self> {
        if slot == UNKNOWN_SLOT {
            return Err(Error::logic("Direct plan slots must be above 0 (Luau's cache starts at 0)"));
        }
        if slot > MAX_SLOT {
            return Err(Error::logic(format!("Direct plan slot {slot} exceeds {MAX_SLOT}; allocate slots densely")));
        }
        let Some(tag) = tagged::tag_of::<T>(&self.runtime.stack()) else {
            return Err(Error::logic(format!(
                "'{}' is not tagged in this runtime; direct dispatch needs a tag",
                T::NAME
            )));
        };
        let Some(atom) = self.runtime.atom_of(member) else {
            return Err(Error::logic(format!("'{member}' is not in this runtime's atom catalogue")));
        };
        if self.entries.iter().any(|entry| entry.slot == slot) {
            return Err(Error::logic(format!("Direct plan slot {slot} is used twice")));
        }
        if self.entries.iter().any(|entry| entry.tag == tag && entry.atom == atom && entry.kind == kind) {
            return Err(Error::logic(format!("Direct plan already maps '{}'.{member} for this access kind", T::NAME)));
        }
        self.entries.push(PlanEntry {
            slot,
            tag,
            atom,
            kind,
            type_id: TypeId::of::<T>(),
            type_name: T::NAME,
            member: member.to_owned(),
        });
        Ok(self)
    }

    /// Builds the dense table and installs the plan as the runtime's (replacing any earlier one).
    pub fn finish(self) -> Result<Rc<DirectPlan>> {
        let first_atom = self.entries.iter().map(|entry| entry.atom).min().unwrap_or(0);
        let last_atom = self.entries.iter().map(|entry| entry.atom).max().unwrap_or(-1);
        let atom_count = if last_atom < first_atom { 0 } else { (last_atom - first_atom) as usize + 1 };
        if atom_count > MAX_ATOM_SPAN {
            return Err(Error::logic(format!(
                "Direct plan atoms span {atom_count} ids (from {first_atom} to {last_atom}); catalogue them densely (at most {MAX_ATOM_SPAN})"
            )));
        }
        let mut slots = vec![UNKNOWN_SLOT; usize::from(TAG_LIMIT) * ACCESS_KIND_COUNT * atom_count];
        let max_slot = self.entries.iter().map(|entry| entry.slot).max().unwrap_or(0);
        let mut by_slot = vec![None; usize::from(max_slot) + 1];
        for (index, entry) in self.entries.iter().enumerate() {
            by_slot[usize::from(entry.slot)] = Some(index as u32);
        }
        let mut plan = DirectPlan { entries: self.entries, by_slot, first_atom, atom_count, slots: Vec::new() };
        for entry in &plan.entries {
            let index = plan.index(i32::from(entry.tag), entry.kind, entry.atom).expect("entry atoms lie in range");
            slots[index] = entry.slot;
        }
        plan.slots = slots;
        let plan = Rc::new(plan);
        *self.runtime.shared().direct_plan().borrow_mut() = Some(Rc::clone(&plan));
        Ok(plan)
    }
}

impl std::fmt::Debug for DirectPlanBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectPlanBuilder").field("entries", &self.entries).finish()
    }
}

/// The plan installed on `scope`'s VM, if any. Cheap: one shared-block read and an `Rc` clone.
pub fn plan(scope: &impl Scope) -> Option<Rc<DirectPlan>> {
    // SAFETY: a scope proves its thread is live.
    unsafe { crate::runtime::shared_for(scope.state()) }.and_then(|shared| shared.direct_plan().borrow().clone())
}

pub(crate) type DirectPlanSlot = std::cell::RefCell<Option<Rc<DirectPlan>>>;
