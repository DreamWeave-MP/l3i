//! Compile-time direct dispatch registry (`components/lua/directregistry.hpp`).
//!
//! An atom catalogue plus slot descriptors, folded into a dense `(tag, kind, atom) -> slot`
//! table at compile time. Slot values are contiguous and fixed by catalogue order, never by
//! registration order, so they can be treated as protocol identifiers. Luau's per-instruction
//! cache is a `u16` shared between all userdata types and access kinds, so a cached slot is
//! trusted only after [`Registry::cached_slot_matches`] confirms it names this exact
//! `(tag, atom, kind)`.

use super::{ACCESS_KIND_COUNT, AccessKind, Atom};
use crate::TAG_LIMIT;
use crate::userdata::RuntimeTag;

/// Slot 0 is `Unknown`, the value Luau starts every instruction cache with.
pub const UNKNOWN_SLOT: u16 = 0;

/// One dispatch entry: `(tag, atom, kind)` resolves to `slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub slot: u16,
    pub tag: RuntimeTag,
    pub atom: Atom,
    pub kind: AccessKind,
}

/// `ATOMS` catalogued names starting at `first_atom`, `SLOTS` descriptors starting at
/// `first_slot`, and the dense lookup table.
#[derive(Debug)]
pub struct Registry<const ATOMS: usize, const SLOTS: usize> {
    atoms: [(&'static str, Atom); ATOMS],
    descriptors: [Descriptor; SLOTS],
    first_atom: Atom,
    first_slot: u16,
    slots: [[[u16; ATOMS]; ACCESS_KIND_COUNT]; TAG_LIMIT as usize],
}

impl<const ATOMS: usize, const SLOTS: usize> Registry<ATOMS, SLOTS> {
    /// Builds and validates the registry at compile time. Atoms must be contiguous from
    /// `first_atom` in catalogue order with unique non-empty names; slots must be contiguous
    /// from `first_slot` (above 0) in descriptor order, each naming a catalogued atom and a
    /// usable tag, and no `(tag, atom, kind)` may repeat.
    pub const fn new(atoms: [(&'static str, Atom); ATOMS], descriptors: [Descriptor; SLOTS]) -> Registry<ATOMS, SLOTS> {
        let first_atom = if ATOMS == 0 { 0 } else { atoms[0].1 };
        let first_slot = if SLOTS == 0 { 1 } else { descriptors[0].slot };
        assert!(first_slot > UNKNOWN_SLOT, "the first slot must be above Unknown (0)");
        let mut i = 0;
        while i < ATOMS {
            assert!(!atoms[i].0.is_empty(), "atom names cannot be empty");
            assert!(atoms[i].1 == first_atom + i as Atom, "atoms must be contiguous in catalogue order");
            let mut j = 0;
            while j < i {
                assert!(!super::const_str_eq(atoms[j].0, atoms[i].0), "atom names must be unique");
                j += 1;
            }
            i += 1;
        }
        let mut slots = [[[UNKNOWN_SLOT; ATOMS]; ACCESS_KIND_COUNT]; TAG_LIMIT as usize];
        let mut i = 0;
        while i < SLOTS {
            let d = descriptors[i];
            assert!(d.slot == first_slot + i as u16, "slots must be contiguous in descriptor order");
            assert!(d.tag != 0 && d.tag < TAG_LIMIT, "descriptor tag is outside the usable range");
            let atom_index = d.atom - first_atom;
            assert!(atom_index >= 0 && (atom_index as usize) < ATOMS, "descriptor names an uncatalogued atom");
            let mut j = 0;
            while j < i {
                let other = descriptors[j];
                assert!(
                    !(other.tag == d.tag && other.atom == d.atom && other.kind as u8 == d.kind as u8),
                    "duplicate (tag, atom, kind) descriptor"
                );
                j += 1;
            }
            slots[d.tag as usize][d.kind as usize][atom_index as usize] = d.slot;
            i += 1;
        }
        Registry { atoms, descriptors, first_atom, first_slot, slots }
    }

    pub const fn atoms(&self) -> &[(&'static str, Atom); ATOMS] {
        &self.atoms
    }

    pub const fn descriptors(&self) -> &[Descriptor; SLOTS] {
        &self.descriptors
    }

    const fn contains(&self, atom: Atom) -> bool {
        atom >= self.first_atom && ((atom - self.first_atom) as usize) < ATOMS
    }

    pub const fn atom_of(&self, name: &str) -> Option<Atom> {
        let mut i = 0;
        while i < ATOMS {
            if super::const_str_eq(self.atoms[i].0, name) {
                return Some(self.atoms[i].1);
            }
            i += 1;
        }
        None
    }

    pub const fn name_of(&self, atom: Atom) -> &'static str {
        if self.contains(atom) { self.atoms[(atom - self.first_atom) as usize].0 } else { "unknown" }
    }

    /// The slot for `(tag, atom, kind)`, or [`UNKNOWN_SLOT`]. Bounds checked.
    pub const fn resolve_slot(&self, tag: i32, atom: Atom, kind: AccessKind) -> u16 {
        if tag < 0 || tag >= TAG_LIMIT as i32 || !self.contains(atom) {
            return UNKNOWN_SLOT;
        }
        self.slots[tag as usize][kind as usize][(atom - self.first_atom) as usize]
    }

    /// True when `cached` is one of this registry's slots and names exactly `(tag, atom, kind)`.
    pub const fn cached_slot_matches(&self, cached: u16, tag: i32, atom: Atom, kind: AccessKind) -> bool {
        if cached < self.first_slot || (cached - self.first_slot) as usize >= SLOTS {
            return false;
        }
        let d = self.descriptors[(cached - self.first_slot) as usize];
        d.tag as i32 == tag && d.atom == atom && d.kind as u8 == kind as u8
    }

    /// Resolves through Luau's per-instruction cache: a cache hit that validates is returned,
    /// otherwise the slot is resolved and written back (or 0 on a miss).
    pub fn resolve_cached_slot(&self, cached: &mut u16, tag: i32, atom: Atom, kind: AccessKind) -> u16 {
        if self.cached_slot_matches(*cached, tag, atom, kind) {
            return *cached;
        }
        let resolved = self.resolve_slot(tag, atom, kind);
        *cached = resolved;
        resolved
    }

    /// True when exactly the atoms in `first..=last` have a slot of `kind` on `tag`; for code
    /// that dispatches on an atom range and must fail to compile when a row moves.
    pub const fn atom_range_matches_slots(&self, tag: RuntimeTag, kind: AccessKind, first: Atom, last: Atom) -> bool {
        let mut i = 0;
        while i < ATOMS {
            let atom = self.atoms[i].1;
            let in_range = atom >= first && atom <= last;
            let has_slot = self.resolve_slot(tag as i32, atom, kind) != UNKNOWN_SLOT;
            if in_range != has_slot {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The catalogue as `AtomCatalogue` entries.
    pub const fn catalogue_entries(&self) -> &[(&'static str, Atom); ATOMS] {
        &self.atoms
    }
}
