//! Parameter tuples: compile-time descriptors and ordered materialisation
//! (`bindfunction.hpp`: `MakeParameterDescriptors`, `SignatureBase`, `invokeAndPush`).

use std::ffi::c_int;

use super::diagnostics;
use super::param::{Param, ParamItem, ParamKind};
use super::Call;
use crate::error::Result;

/// A tuple of [`Param`] markers.
pub trait Params {
    type Items<'c>;

    /// Parameter kinds in order; the descriptors below derive from it.
    const KINDS: &'static [ParamKind];
    /// Arguments a caller must supply: one per regular parameter.
    const REQUIRED: usize = required_slots(Self::KINDS);
    /// Upper arity for fixed signatures: every non-injected parameter takes at most one.
    const MAX: usize = max_slots(Self::KINDS);
    /// True once a `VarArgs`/`ArgView` tail absorbs the remaining arguments.
    const TERMINATOR: bool = has_terminator(Self::KINDS);

    /// `probeForOverload`: argument count fits, and every parameter probes in order.
    fn probe_for_overload(call: &Call<'_>) -> bool;

    /// `invokeAndPush`'s argument half: count checks, ordered conversion, unused check.
    fn materialize<'c>(call: &'c Call<'c>, debug_name: &str) -> Result<Self::Items<'c>>;
}

const fn required_slots(kinds: &[ParamKind]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < kinds.len() {
        if matches!(kinds[i], ParamKind::Regular) {
            count += 1;
        }
        i += 1;
    }
    count
}

const fn max_slots(kinds: &[ParamKind]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < kinds.len() {
        if !matches!(kinds[i], ParamKind::Injected) {
            count += 1;
        }
        i += 1;
    }
    count
}

const fn has_terminator(kinds: &[ParamKind]) -> bool {
    let mut i = 0;
    while i < kinds.len() {
        if matches!(kinds[i], ParamKind::VarArgs | ParamKind::ArgView) {
            return true;
        }
        i += 1;
    }
    false
}

/// A terminator must be final and unique.
const fn terminators_are_final(kinds: &[ParamKind]) -> bool {
    let mut i = 0;
    while i < kinds.len() {
        if matches!(kinds[i], ParamKind::VarArgs | ParamKind::ArgView) && i + 1 != kinds.len() {
            return false;
        }
        i += 1;
    }
    true
}

/// `kAllowOptionalMismatch`: an optional may be skipped when a required parameter follows.
const fn allow_mismatch(kinds: &[ParamKind], index: usize) -> bool {
    if !matches!(kinds[index], ParamKind::Optional) {
        return false;
    }
    let mut j = index + 1;
    while j < kinds.len() {
        if matches!(kinds[j], ParamKind::Regular) {
            return true;
        }
        j += 1;
    }
    false
}

macro_rules! params_impls {
    ($(($($p:ident $i:tt),*) ;)*) => {$(
        impl<$($p: Param,)*> Params for ($($p,)*) {
            type Items<'c> = ($(<$p as Param>::Item<'c>,)*);

            const KINDS: &'static [ParamKind] = &[$(<<$p as Param>::Item<'static> as ParamItem<'static>>::KIND,)*];

            #[allow(unused_variables, unused_mut, clippy::let_unit_value)]
            fn probe_for_overload(call: &Call<'_>) -> bool {
                let top = call.argument_count();
                if top < Self::REQUIRED as c_int {
                    return false;
                }
                if !Self::TERMINATOR && top > Self::MAX as c_int {
                    return false;
                }
                let mut cursor: c_int = 1;
                $(
                    if !<<$p as Param>::Item<'_> as ParamItem<'_>>::probe(call, &mut cursor, top, allow_mismatch(Self::KINDS, $i)) {
                        return false;
                    }
                )*
                true
            }

            #[allow(unused_variables, unused_mut, clippy::let_unit_value)]
            fn materialize<'c>(call: &'c Call<'c>, debug_name: &str) -> Result<Self::Items<'c>> {
                const { assert!(terminators_are_final(Self::KINDS), "VarArgs/ArgView must be the final parameter") };
                let top = call.argument_count();
                if top < Self::REQUIRED as c_int {
                    return Err(diagnostics::too_few_arguments(debug_name, Self::REQUIRED, top));
                }
                if !Self::TERMINATOR && top > Self::MAX as c_int {
                    return Err(diagnostics::too_many_arguments(debug_name, Self::MAX, top));
                }
                let mut cursor: c_int = 1;
                let mut position: c_int = 0;
                // Tuple fields evaluate left to right, preserving cursor advancement and
                // first-failing-argument diagnostics.
                let items = ($(
                    <<$p as Param>::Item<'c> as ParamItem<'c>>::materialize(
                        call, &mut cursor, top, &mut position, debug_name, allow_mismatch(Self::KINDS, $i),
                    )?,
                )*);
                if !Self::TERMINATOR && cursor <= top {
                    return Err(diagnostics::unused_arguments(debug_name));
                }
                Ok(items)
            }
        }
    )*};
}

params_impls! {
    ();
    (A 0);
    (A 0, B 1);
    (A 0, B 1, C 2);
    (A 0, B 1, C 2, D 3);
    (A 0, B 1, C 2, D 3, E 4);
    (A 0, B 1, C 2, D 3, E 4, F 5);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
}
