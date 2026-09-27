//! Process-global and per-VM runtime policy shared by every DreamWeave host.
//!
//! The host owns a [`Runtime`], which owns the `lua_State`. The binder makes the same
//! intentional choices OpenMW's `Lua::State` makes, in the same order, so bytecode, integer
//! semantics, and performance characteristics match.

use std::ffi::{CStr, CString};
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::source::{CompileOptions, compile};
use crate::stack::Stack;

/// Luau feature flags OpenMW enables (`components/luau/runtimeflags.cpp`) that live in the
/// Ast, Bytecode, Compiler, and VM components, which are always linked.
///
/// Compiler flags shape the bytecode; runtime flags must be set before `luaL_openlibs`, so
/// before the first VM is created. Every name is verified against the linked Luau release;
/// an unknown flag is a logic error rather than a silent no-op.
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

static FLAGS_INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Freezes the process-global Luau flag policy. Idempotent; [`Runtime::new`] calls it.
///
/// Never adopts Luau's CLI "enable everything" policy: exactly [`LUAU_FLAGS`] (plus
/// [`LUAU_CODEGEN_FLAGS`] with `jit`) are turned on.
pub fn initialize_luau_flags() -> Result<()> {
    FLAGS_INITIALIZED
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

/// Owns one Luau VM. Dropping it closes the VM, so every owned reference into it must be gone
/// first; borrowed views cannot outlive it by construction.
pub struct Runtime {
    state: *mut ffi::lua_State,
}

impl Runtime {
    /// Creates a VM with OpenMW's flag policy applied and the standard libraries opened.
    pub fn new() -> Result<Runtime> {
        initialize_luau_flags()?;
        // SAFETY: luaL_newstate uses the default allocator; a null result is out of memory.
        let state = unsafe { ffi::luaL_newstate() };
        if state.is_null() {
            return Err(Error::runtime("Unable to allocate a Luau state"));
        }
        let runtime = Runtime { state };
        // SAFETY: fresh live state; openlibs raises only on out of memory.
        unsafe { ffi::luaL_openlibs(state) };
        Ok(runtime)
    }

    /// The main thread of this VM.
    #[allow(dead_code)] // the tagged userdata slice registers through it
    pub(crate) fn state(&self) -> *mut ffi::lua_State {
        self.state
    }

    /// A borrowed view of the main thread's stack, valid while the runtime is.
    pub fn stack(&self) -> Stack<'_> {
        // SAFETY: the state lives as long as `&self`.
        unsafe { Stack::from_raw(self.state) }
    }

    /// Compiles `source` and leaves the resulting chunk function on the stack.
    pub fn load(&self, chunk_name: &str, source: &str, options: &CompileOptions) -> Result<()> {
        let bytecode = compile(source, options)?;
        let name = CString::new(chunk_name).map_err(|_| Error::logic("Chunk name cannot contain NUL"))?;
        // SAFETY: live state; the bytecode slice and name outlive the call. luau_load
        // reports failure by status and leaves the message on the stack.
        let status = unsafe { ffi::luau_load(self.state, name.as_ptr(), bytecode.as_ptr().cast(), bytecode.len(), 0) };
        if status != ffi::LUA_OK {
            return Err(self.pop_error());
        }
        Ok(())
    }

    /// Compiles and runs `source` on the main thread, discarding results.
    pub fn exec(&self, source: &str) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|_| {
            self.load("=exec", source, &CompileOptions::default())?;
            // SAFETY: the chunk function is on top; pcall never unwinds into this frame.
            let status = unsafe { ffi::lua_pcall(self.state, 0, 0, 0) };
            if status != ffi::LUA_OK {
                return Err(self.pop_error());
            }
            Ok(())
        })
    }

    /// Converts the error object on top of the stack to an `Error::Runtime`, popping it.
    pub(crate) fn pop_error(&self) -> Error {
        // SAFETY: the caller guarantees an error object is on top.
        let message = unsafe {
            let mut length = 0usize;
            let text = ffi::lua_tolstring(self.state, -1, &mut length);
            let message = if text.is_null() {
                CStr::from_ptr(ffi::luaL_typename(self.state, -1)).to_string_lossy().into_owned()
            } else {
                String::from_utf8_lossy(std::slice::from_raw_parts(text.cast::<u8>(), length)).into_owned()
            };
            ffi::lua_pop(self.state, 1);
            message
        };
        Error::Runtime(message)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: we own the state and nothing borrowed from it can outlive `self`.
        unsafe { ffi::lua_close(self.state) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_openmw_flag_exists_in_the_linked_luau() {
        initialize_luau_flags().unwrap();
        initialize_luau_flags().unwrap();
    }

    #[test]
    fn integer_library_is_live_after_flags() {
        let runtime = Runtime::new().unwrap();
        // LuauIntegerType2 parses `42i`; LuauIntegerLibrary provides the integer library.
        runtime.exec("assert(typeof(42i) == 'integer', typeof(42i))").unwrap();
    }

    #[test]
    fn if_local_expressions_parse() {
        let runtime = Runtime::new().unwrap();
        runtime
            .exec("local t = {x = 3} local r = if local v = t.x then v * 2 else 0 assert(r == 6, r)")
            .unwrap();
    }

    #[test]
    fn fastpcall_bytecode_loads_and_runs() {
        let runtime = Runtime::new().unwrap();
        runtime
            .exec("local ok, err = pcall(function() error('x') end) assert(not ok and err:find('x'))")
            .unwrap();
    }

    #[test]
    fn errors_report_the_lua_message() {
        let runtime = Runtime::new().unwrap();
        assert_eq!(runtime.exec("error('boom', 0)").unwrap_err(), Error::runtime("boom"));
        assert!(runtime.exec("local = 1").unwrap_err().to_string().contains("Expected identifier"));
        assert_eq!(runtime.stack().top(), 0);
    }
}
