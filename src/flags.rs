//! Process-global Luau feature-flag policy (`components/luau/runtimeflags.cpp`).
//!
//! Flags are global to the process and several of them change the bytecode the compiler
//! emits, so both VM creation and standalone compilation freeze the same policy before doing
//! anything else. Never Luau's CLI "enable everything": exactly the lists below.

use std::ffi::CString;
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::raw::ffi;

/// l3i's flag policy for the Ast, Bytecode, Compiler, and VM components, which are always
/// linked: every flag here is on for the reason beside it. Runtime flags must be set before
/// `luaL_openlibs`. A flag Luau graduates, its enabled behavior made unconditional, comes off
/// the list: 0.741 graduated `LuauNoDuplicateBinaryPrefix` and `LuauCompileIifeInline`, and
/// 0.743 and 0.744 the compiler's `LuauCompileCleanBlockDeadClose`, `LuauCompileUndoEmitAdjust`,
/// `LuauCompileLoopUnrollZero` and `LuauCompileRecursiveAliases` and the runtime's
/// `LuauTableArrayAdjustCheck`, `LuauSplitTableLookups`, `LuauTableMoveTimeoutFix`,
/// `LuauNewPointerEncode`, `LuauPcallOptimize`, `LuauCallLuauTm`, `LuauBackedgeHeapCheck` and
/// `LuauFastpcallInterrupt`.
pub const LUAU_FLAGS: &[&str] = &[
    // Compiler side
    "LuauCompileNoFoldVectorEqW",
    "LuauCompileConcatTargetTop",
    "LuauIntegerFastcalls",
    "LuauIntegerBufferFastcalls",
    "LuauCompileMoveElision",
    "LuauCompileFastpcall",
    "LuauIntegerType2",
    // Luau 0.742's fixes for two miscompiles. Move elision (above) reused the assignment
    // target's register while a later argument still read it, so `x = id(f(x))` passed `f` to
    // itself. Table constant folding, which no flag gates, kept folding `t.a` after `t` had been
    // an operand whose metamethod could write to it.
    "LuauCompileMoveElisionFix",
    "LuauCompileTableFoldFix",
    // `if local x = f() then ... end` syntax. The C++ binder leaves this off.
    "LuauExperimentalIfLocalSyntax",
    // Runtime side
    "LuauIntegerLibrary",
    "LuauOptimizeStringSplit",
    "LuauLoadRemapOptionalUserdata",
    "LuauTableRobustOom",
    "FixMathNoisePrecision",
    "LuauFastpcall",
    // Luau 0.742: a protected call an error continuation starts inside a resumed coroutine is
    // resumed, where it used to be dropped.
    "LuauPcallErrorContinuationReentry",
    // Luau 0.742: `string.gsub` with a pattern that has no special characters searches for the
    // text directly, 1.9 to 6.9 times faster on literal patterns measured here. A dynamic flag,
    // off upstream by default.
    "LuauOptimizeStringGsub",
    // These two gate APIs the binder exposes (embedder GC integration with userdata marks and
    // weak references; the memory cage). Both are inert until a host installs the corresponding
    // callback.
    "LuauGcTraceUdata",
    "LuauBufferCage",
    // Luau 0.741's fix for GC pacing, which read a heap that shrank between measurements as an
    // enormous allocation rate. A dynamic flag, off upstream by default.
    "LuauGcHeapShrinkFix",
];

/// The rest of the policy, defined in Luau's CodeGen component. CodeGen is compiled only
/// with the `jit` feature; without it these symbols do not exist. The X64/A64 flags are defined
/// in the per-architecture lowering files, which the linker only keeps for the target
/// architecture, so each of those exists at run time on its own architecture only.
/// Luau 0.742 removed `AddReturnExectargetCheck` together with
/// `disableNativeExecutionForFunction`, the one function that needed it; 0.743 and 0.744
/// graduated `LuauCodegenNoLinearFastpcall` and `LuauCodegenLimitVersions`.
pub const LUAU_CODEGEN_FLAGS: &[&str] = &[
    "LuauCodegenPropagateFallbackTags",
    "LuauCodegenX64IntSpillRestore",
    "LuauCodegenA64ForgLoopArray",
    "LuauCodeGenFastpcall",
    "LuauCodegenInteger3",
    "LuauCodegenIntegerCompare",
    "LuauCodegenBufferInteger",
    // Luau 0.742's register allocator fixes: restores a VM exit would clobber are made before
    // the exit's own stores, running out of registers fails the compilation instead of
    // asserting, and a conditional spill no longer marks a lazy restore location materialized.
    // Both are defined in the A64 allocator and read by the X64 one, so they exist on either.
    "LuauCodegenExitSyncUpdate",
    "LuauCodegenScopedSpillKeepLazy",
    // Luau 0.742: a buffer offset multiplied by a constant zero keeps its scale.
    "LuauCodegenNoZeroScale",
    // Constant propagation packs a register's version into twenty bits of an operand and, since
    // 0.744 graduated the first half of the fix, stops propagating along a block chain before a
    // version outgrows them. This flag (0.742) makes stopping sound, applying pending
    // substitutions and discarding the fallback entry tags recorded before the stop.
    "LuauCodegenLimitVersionsExtra",
    // Luau 0.744's fixes. Dead store elimination kept a tag store whose load had been
    // overwritten; `INVOKE_FASTPCALL` can replace the environment, so it now invalidates the
    // safe-environment assumption; a VM exit's register restores are planned per exit (a crash
    // Luau's fuzzer found, superseding the 0.742 fix above); and code with a libunwind unwinder
    // registers each FDE where it used to register the whole frame, which Intel macOS needs.
    "LuauCodegenDseTrackTagInvalidation",
    "LuauCodegenUpdateSafeEnvList",
    "LuauCodegenImprovedExitSync",
    "LuauCodegenIntelMacUnwind",
    // Luau 0.744's A64 fixes: `bit32.lrotate` no longer writes its result over an operand the
    // rotate still reads, and a function with more than 256 constants reloads one past the
    // immediate offset instead of crashing.
    "LuauCodegenA64BitlrotateSelf",
    "LuauCodegenA64ReloadLargeConstOffset",
];

static INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// True for a CodeGen flag that belongs to a lowering backend other than this target's, whose
/// object file (and flag registration) the linker leaves out.
fn is_foreign_architecture_flag(flag: &str) -> bool {
    (flag.contains("A64") && !cfg!(target_arch = "aarch64")) || (flag.contains("X64") && !cfg!(target_arch = "x86_64"))
}

/// Freezes the flag policy. Idempotent and thread-safe; [`crate::Runtime`] creation and
/// [`crate::source::compile`] both call it. An unknown flag name is a logic error rather than a
/// silent no-op, so drift against the linked Luau release is caught immediately.
pub fn initialize() -> Result<()> {
    INITIALIZED
        .get_or_init(|| {
            let codegen: &[&str] = if cfg!(feature = "jit") { LUAU_CODEGEN_FLAGS } else { &[] };
            for flag in LUAU_FLAGS.iter().chain(codegen) {
                let name = CString::new(*flag).expect("flag names are literals");
                // SAFETY: luau_setfflag only walks the static flag list and writes a bool.
                if unsafe { ffi::luau_setfflag(name.as_ptr(), 1) } == 0 && !is_foreign_architecture_flag(flag) {
                    return Err(format!("Luau {} has no fast flag named {flag}", crate::LUAU_VERSION));
                }
            }
            Ok(())
        })
        .clone()
        .map_err(Error::Logic)
}

static COROUTINE_FINALIZERS: OnceLock<std::result::Result<bool, String>> = OnceLock::new();

/// Decides, once per process, whether Luau's coroutine finalizers are on, and returns the
/// decision. `LuauCoroutineFinally` is process-wide, and a VM has to open its coroutine library
/// and collect its threads under the value it goes on to run with, so the flag can only be
/// raised before any VM exists: the first runtime built decides. A later runtime that asks for
/// finalizers in a process that went without them is refused; one that does not ask in a process
/// that has them gets them anyway.
pub(crate) fn decide_coroutine_finalizers(requested: bool) -> Result<bool> {
    let decided = COROUTINE_FINALIZERS
        .get_or_init(|| {
            if requested {
                // SAFETY: luau_setfflag only walks the static flag list and writes a bool. This
                // is the first runtime build of the process, so no VM made by this crate has
                // opened its libraries without the flag.
                unsafe {
                    if ffi::luau_setfflag(c"LuauCoroutineFinally".as_ptr(), 1) == 0 {
                        return Err(format!(
                            "Luau {} has no fast flag named LuauCoroutineFinally",
                            crate::LUAU_VERSION
                        ));
                    }
                    // Types `coroutine.finally`; defined in Analysis, which only the `analysis`
                    // feature links, so its absence is not an error.
                    ffi::luau_setfflag(c"LuauCoroutineFinallyAnalysis".as_ptr(), 1);
                }
            }
            Ok(requested)
        })
        .clone()
        .map_err(Error::Logic)?;
    if requested && !decided {
        return Err(Error::logic(
            "Coroutine finalizers must be chosen by the first runtime of the process: Luau's flag is process-wide and a runtime was already built without them",
        ));
    }
    Ok(decided)
}

/// Whether this build of Luau checks its internal assertions (`LUAU_ASSERT`, `api_check`): on in
/// builds with Rust debug assertions, off otherwise, where a failed check would trap.
pub fn luau_assertions() -> bool {
    // SAFETY: a constant the C++ build fixed; no VM involved.
    unsafe { ffi::l3i_luau_assertions() != 0 }
}

/// Whether this process runs with coroutine finalizers
/// ([`crate::runtime::RuntimeBuilder::coroutine_finalizers`]). False until a runtime is built.
pub fn coroutine_finalizers() -> bool {
    matches!(COROUTINE_FINALIZERS.get(), Some(Ok(true)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_flag_exists_in_the_linked_luau() {
        super::initialize().unwrap();
        super::initialize().unwrap();
    }
}
