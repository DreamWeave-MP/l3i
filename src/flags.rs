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
pub const LUAU_FLAGS: &[&str] = &[
    // Compiler side
    "LuauCompileCleanBlockDeadClose",
    "LuauCompileUndoEmitAdjust",
    "LuauCompileLoopUnrollZero",
    "LuauCompileNoFoldVectorEqW",
    "LuauCompileRecursiveAliases",
    "LuauNoDuplicateBinaryPrefix",
    "LuauCompileConcatTargetTop",
    "LuauIntegerFastcalls",
    "LuauIntegerBufferFastcalls",
    "LuauCompileIifeInline",
    "LuauCompileMoveElision",
    "LuauCompileFastpcall",
    "LuauIntegerType2",
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
];

/// The remaining OpenMW flags, defined in Luau's CodeGen component. CodeGen is compiled only
/// with the `jit` feature; without it these symbols do not exist.
pub const LUAU_CODEGEN_FLAGS: &[&str] = &[
    "LuauCodegenPropagateFallbackTags",
    "LuauCodegenX64IntSpillRestore",
    "LuauCodegenA64ForgLoopArray",
    "LuauCodeGenFastpcall",
    "LuauCodegenNoLinearFastpcall",
    "LuauCodegenInteger3",
    "LuauCodegenIntegerCompare",
    "LuauCodegenBufferInteger",
    "AddReturnExectargetCheck",
];

static INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();

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
                if unsafe { ffi::luau_setfflag(name.as_ptr(), 1) } == 0 {
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
