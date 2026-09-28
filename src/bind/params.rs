//! Parameter tuples: compile-time descriptors and ordered materialisation
//! (`bindfunction.hpp`: `MakeParameterDescriptors`, `SignatureBase`, `invokeAndPush`).

use std::ffi::c_int;

use super::Call;
use super::diagnostics;
use super::param::{Param, ParamItem, ParamKind};
use crate::error::{Error, Result};

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
    /// The first parameter's expected type, i.e. a method's receiver type.
    const RECEIVER_NAME: Option<&'static str>;
    /// Method-mode descriptors: the same counts over the parameters after the receiver.
    const METHOD_REQUIRED: usize = if Self::KINDS.is_empty() { 0 } else { required_slots(rest_of(Self::KINDS)) };
    const METHOD_MAX: usize = if Self::KINDS.is_empty() { 0 } else { max_slots(rest_of(Self::KINDS)) };
    const METHOD_TERMINATOR: bool = if Self::KINDS.is_empty() { false } else { has_terminator(rest_of(Self::KINDS)) };

    /// `probeForOverload`: argument count fits, and every parameter probes in order.
    fn probe_for_overload(call: &Call<'_>) -> bool;

    /// `invokeAndPush`'s argument half: count checks, ordered conversion, unused check.
    fn materialize<'c>(call: &'c Call<'c>, debug_name: &str) -> Result<Self::Items<'c>>;

    /// `MethodSignatureBase::invoke`'s argument half: the receiver at slot 1 is converted with
    /// its own type error and left out of the numbering, the rest start at slot 2.
    fn materialize_method<'c>(call: &'c Call<'c>, debug_name: &str) -> Result<Self::Items<'c>>;
}

/// Argument-count checks against descriptors computed at compile time: two comparisons.
#[inline(always)]
fn count_checks(required: usize, max: usize, terminator: bool, top: c_int, debug_name: &str) -> Result<()> {
    if top < required as c_int {
        return Err(diagnostics::too_few_arguments(debug_name, required, top));
    }
    if !terminator && top > max as c_int {
        return Err(diagnostics::too_many_arguments(debug_name, max, top));
    }
    Ok(())
}

/// `kinds[1..]`, usable in `const` context.
const fn rest_of(kinds: &[ParamKind]) -> &[ParamKind] {
    match kinds.split_first() {
        Some((_, rest)) => rest,
        None => &[],
    }
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
            const RECEIVER_NAME: Option<&'static str> = {
                let names: &[&'static str] = &[$(<<$p as Param>::Item<'static> as ParamItem<'static>>::EXPECTED,)*];
                if names.is_empty() { None } else { Some(names[0]) }
            };

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
                    if !<<$p as Param>::Item<'_> as ParamItem<'_>>::probe(call, &mut cursor, top, const { allow_mismatch(Self::KINDS, $i) }) {
                        return false;
                    }
                )*
                true
            }

            #[allow(unused_variables, unused_mut, clippy::let_unit_value)]
            #[inline(always)]
            fn materialize<'c>(call: &'c Call<'c>, debug_name: &str) -> Result<Self::Items<'c>> {
                const { assert!(terminators_are_final(Self::KINDS), "VarArgs/ArgView must be the final parameter") };
                let top = call.argument_count();
                count_checks(Self::REQUIRED, Self::MAX, Self::TERMINATOR, top, debug_name)?;
                let mut cursor: c_int = 1;
                let mut position: c_int = 0;
                // Tuple fields evaluate left to right, preserving cursor advancement and
                // first-failing-argument diagnostics.
                let items = ($(
                    <<$p as Param>::Item<'c> as ParamItem<'c>>::materialize(
                        call, &mut cursor, top, &mut position, debug_name, const { allow_mismatch(Self::KINDS, $i) },
                    )?,
                )*);
                if !Self::TERMINATOR && cursor <= top {
                    return Err(diagnostics::unused_arguments(debug_name));
                }
                Ok(items)
            }

            #[allow(unused_variables, unused_mut, clippy::let_unit_value, unreachable_code)]
            #[inline(always)]
            fn materialize_method<'c>(call: &'c Call<'c>, debug_name: &str) -> Result<Self::Items<'c>> {
                const { assert!(terminators_are_final(Self::KINDS), "VarArgs/ArgView must be the final parameter") };
                if Self::KINDS.is_empty() {
                    return Err(Error::logic(format!("{debug_name}: a method needs a receiver parameter")));
                }
                let top = call.argument_count();
                let mut cursor: c_int = 2;
                let mut position: c_int = 0;
                let items = ($(
                    if $i == 0 {
                        // The receiver comes first, with its own type error (including "missing
                        // argument #1" when the call has no receiver at all) and no argument
                        // numbering; only then are the remaining arguments counted.
                        let receiver = <<$p as Param>::Item<'c> as ParamItem<'c>>::read_slot(call.arg(1))?;
                        count_checks(Self::METHOD_REQUIRED, Self::METHOD_MAX, Self::METHOD_TERMINATOR, top - 1, debug_name)?;
                        receiver
                    } else {
                        <<$p as Param>::Item<'c> as ParamItem<'c>>::materialize(
                            call, &mut cursor, top, &mut position, debug_name, const { allow_mismatch(Self::KINDS, $i) },
                        )?
                    },
                )*);
                if !Self::METHOD_TERMINATOR && cursor <= top {
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
