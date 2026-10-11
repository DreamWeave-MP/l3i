//! Process-global Luau feature-flag policy (`components/luau/runtimeflags.cpp`).
//!
//! Flags are global to the process and several of them change the bytecode the compiler
//! emits, so both VM creation and standalone compilation freeze the same policy before doing
//! anything else. Never Luau's CLI "enable everything": exactly the lists below.

use std::ffi::CString;
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::raw::ffi;

/// Luau feature flags OpenMW enables that live in the Ast, Bytecode, Compiler, and VM
/// components, which are always linked. Runtime flags must be set before `luaL_openlibs`.
/// Luau 0.741 graduated two of OpenMW's flags, `LuauNoDuplicateBinaryPrefix` and
/// `LuauCompileIifeInline`: their enabled behavior is now unconditional, so they are gone.
pub const LUAU_FLAGS: &[&str] = &[
    // Compiler side
    "LuauCompileCleanBlockDeadClose",
    "LuauCompileUndoEmitAdjust",
    "LuauCompileLoopUnrollZero",
    "LuauCompileNoFoldVectorEqW",
    "LuauCompileRecursiveAliases",
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
    // DreamWeave addition: `if local x = f() then ... end` syntax. OpenMW leaves this off.
    "LuauExperimentalIfLocalSyntax",
    // Runtime side
    "LuauIntegerLibrary",
    "LuauTableArrayAdjustCheck",
    "LuauSplitTableLookups",
    "LuauOptimizeStringSplit",
    "LuauLoadRemapOptionalUserdata",
    "LuauTableMoveTimeoutFix",
    "LuauTableRobustOom",
    "FixMathNoisePrecision",
    "LuauNewPointerEncode",
    "LuauPcallOptimize",
    "LuauCallLuauTm",
    "LuauBackedgeHeapCheck",
    "LuauFastpcall",
    "LuauFastpcallInterrupt",
    // Luau 0.742: a protected call an error continuation starts inside a resumed coroutine is
    // resumed, where it used to be dropped.
    "LuauPcallErrorContinuationReentry",
    // Not in OpenMW's policy: these two gate APIs the binder exposes (embedder GC integration
    // with userdata marks and weak references; the buffer cage). Both are inert until a host
    // installs the corresponding callback.
    "LuauGcTraceUdata",
    "LuauBufferCage",
    // Not in OpenMW's policy either: Luau 0.741's fix for GC pacing, which read a heap that shrank
    // between measurements as an enormous allocation rate. A dynamic flag, off upstream by default.
    "LuauGcHeapShrinkFix",
];

/// The remaining OpenMW flags, defined in Luau's CodeGen component. CodeGen is compiled only
/// with the `jit` feature; without it these symbols do not exist. The X64/A64 flags are defined
/// in the per-architecture lowering files, which the linker only keeps for the target
/// architecture, so each of those exists at run time on its own architecture only.
/// Luau 0.742 removed `AddReturnExectargetCheck` together with
/// `disableNativeExecutionForFunction`, the one function that needed it.
pub const LUAU_CODEGEN_FLAGS: &[&str] = &[
    "LuauCodegenPropagateFallbackTags",
    "LuauCodegenX64IntSpillRestore",
    "LuauCodegenA64ForgLoopArray",
    "LuauCodeGenFastpcall",
    "LuauCodegenNoLinearFastpcall",
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
    // Constant propagation packs a register's version into twenty bits of an operand. The first
    // flag (0.741) stops propagating along a block chain before a version outgrows them; the
    // second (0.742) makes stopping sound, applying pending substitutions and discarding the
    // fallback entry tags recorded before the stop. Neither is correct without the other.
    "LuauCodegenLimitVersions",
    "LuauCodegenLimitVersionsExtra",
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

#[cfg(test)]
mod tests {
    #[test]
    fn every_flag_exists_in_the_linked_luau() {
        super::initialize().unwrap();
        super::initialize().unwrap();
    }
}
