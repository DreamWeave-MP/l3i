//! Native code generation (`components/lua/nativecodegen.*`), available with the `jit` feature.
//!
//! One [`NativeCodeGen`] per runtime owns a shared Luau code generation context with a hard
//! size budget, immutable compilation options (mode, counters, userdata type names, the host's
//! lowering [`hooks`]), and a registry of compiled modules keyed by a 128-bit hash of their
//! bytecode so identical scripts share native code and hash collisions are rejected rather than
//! aliased. Compilation reports Luau's status and stats; with counters on, per-module execution
//! statistics are collected from Luau's block counters.

pub(crate) mod ffi;
pub mod hooks;
pub mod ir;
pub mod vector_buffer;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, c_int, c_uint, c_void};

use crate::error::{Error, Result};
use crate::raw::ffi as lua;
use crate::stack::Scope;
use hooks::HookChain;
pub use hooks::NativeCodeHooks;

/// Which functions are compiled natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCodeMode {
    /// Never compile; `compile` reports [`NativeCodeStatus::Skipped`].
    Off,
    /// Only modules marked `--!native`.
    Annotated,
    /// Every module.
    Eager,
}

/// 4 MiB code blocks, as OpenMW.
pub const BLOCK_SIZE: usize = 4 * 1024 * 1024;
/// 32 MiB of native code per runtime by default, as OpenMW.
pub const DEFAULT_MAX_TOTAL_SIZE: usize = 32 * 1024 * 1024;

/// Host choices for native code generation.
pub struct NativeCodeOptions {
    pub mode: NativeCodeMode,
    /// Hard ceiling for native code in this runtime; clamped to at least one block.
    pub max_total_size: usize,
    /// Record Luau's block execution counters (a regression probe, not a game setting).
    pub record_counters: bool,
    /// Insert random NOP sleds between blocks.
    pub nop_padding: bool,
    /// Userdata type names in type index order: the same list handed to the bytecode
    /// compiler's `userdata_types`, so annotated parameters reach the `userdata_*` hooks as
    /// `bytecode_type::TAGGED_USERDATA_BASE + index`.
    pub userdata_types: Vec<String>,
    /// Lowering hooks, asked in order. Defaults to [`vector_buffer::VectorBufferWriter`].
    pub hooks: Vec<Box<dyn NativeCodeHooks>>,
}

impl std::fmt::Debug for NativeCodeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCodeOptions")
            .field("mode", &self.mode)
            .field("max_total_size", &self.max_total_size)
            .field("record_counters", &self.record_counters)
            .field("nop_padding", &self.nop_padding)
            .field("userdata_types", &self.userdata_types)
            .field("hooks", &self.hooks.len())
            .finish()
    }
}

impl Default for NativeCodeOptions {
    fn default() -> Self {
        NativeCodeOptions {
            mode: NativeCodeMode::Annotated,
            max_total_size: DEFAULT_MAX_TOTAL_SIZE,
            record_counters: false,
            nop_padding: false,
            userdata_types: Vec::new(),
            hooks: vec![Box::new(vector_buffer::VectorBufferWriter)],
        }
    }
}

impl NativeCodeOptions {
    pub fn mode(mut self, mode: NativeCodeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Adds a hook set after the defaults.
    pub fn hooks(mut self, hooks: impl NativeCodeHooks) -> Self {
        self.hooks.push(Box::new(hooks));
        self
    }

    pub fn userdata_types(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.userdata_types = names.into_iter().map(Into::into).collect();
        self
    }
}

/// Outcome of one compilation (`NativeCodeGenStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCodeStatus {
    /// Mode is `Off`.
    Skipped,
    /// The platform has no code generator, or it failed to initialise.
    Unavailable,
    /// Another module with different bytecode already has this id.
    IdentityCollision,
    /// This module ran out of code space before; not retried.
    AllocationRetrySkipped,
    Success,
    NothingToCompile,
    NotNativeModule,
    CodeGenNotInitialized,
    CodeGenOverflowInstructionLimit,
    CodeGenOverflowBlockLimit,
    CodeGenOverflowBlockInstructionLimit,
    CodeGenAssemblerFinalizationFailure,
    CodeGenLoweringFailure,
    AllocationFailed,
    UnknownFailure,
}

impl NativeCodeStatus {
    fn from_luau(result: c_int) -> NativeCodeStatus {
        // `Luau::CodeGen::CodeGenCompilationResult` enumerator order.
        match result {
            0 => NativeCodeStatus::Success,
            1 => NativeCodeStatus::NothingToCompile,
            2 => NativeCodeStatus::NotNativeModule,
            3 => NativeCodeStatus::CodeGenNotInitialized,
            4 => NativeCodeStatus::CodeGenOverflowInstructionLimit,
            5 => NativeCodeStatus::CodeGenOverflowBlockLimit,
            6 => NativeCodeStatus::CodeGenOverflowBlockInstructionLimit,
            7 => NativeCodeStatus::CodeGenAssemblerFinalizationFailure,
            8 => NativeCodeStatus::CodeGenLoweringFailure,
            9 => NativeCodeStatus::AllocationFailed,
            _ => NativeCodeStatus::UnknownFailure,
        }
    }
}

/// A module identity: MurmurHash3 x64 128 of the bytecode with OpenMW's seed.
pub type ModuleId = [u8; 16];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeCodeStats {
    pub native_code_size_bytes: usize,
    pub functions_compiled: u32,
    pub functions_bound: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeCodeResult {
    pub status: NativeCodeStatus,
    pub stats: NativeCodeStats,
    pub module_id: Option<ModuleId>,
}

/// Which machine the assembly dump targets (`Luau::CodeGen::AssemblyOptions::Target`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AssemblyTarget {
    #[default]
    Host,
    A64,
    A64NoFeatures,
    X64Windows,
    X64SystemV,
}

/// What [`NativeCodeGen::assembly`] prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssemblyOptions {
    pub target: AssemblyTarget,
    pub include_assembly: bool,
    pub include_ir: bool,
    pub include_outlined_code: bool,
    pub include_ir_types: bool,
    pub include_reg_spills: bool,
}

impl Default for AssemblyOptions {
    fn default() -> Self {
        AssemblyOptions {
            target: AssemblyTarget::Host,
            include_assembly: true,
            include_ir: false,
            include_outlined_code: false,
            include_ir_types: false,
            include_reg_spills: false,
        }
    }
}

/// One natively compiled function, as reported to the perf log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfEntry {
    pub address: usize,
    pub size: u32,
    pub symbol: String,
}

type PerfLog = Box<dyn Fn(PerfEntry) + Send + 'static>;
static PERF_LOG: std::sync::Mutex<Option<PerfLog>> = std::sync::Mutex::new(None);

unsafe extern "C" fn perf_log_callback(_: *mut c_void, address: usize, size: c_uint, symbol: *const std::ffi::c_char) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    let symbol = if symbol.is_null() {
        String::new()
    } else {
        // SAFETY: Luau passes a NUL-terminated symbol.
        unsafe { std::ffi::CStr::from_ptr(symbol) }.to_string_lossy().into_owned()
    };
    if let Ok(log) = PERF_LOG.lock()
        && let Some(log) = log.as_ref()
    {
        log(PerfEntry { address, size, symbol });
    }
}

/// Installs the process-wide perf log (`Luau::CodeGen::setPerfLog`): every function compiled
/// natively afterwards, in any runtime, is reported with its code address and size, for
/// profiler symbolisation (perf maps, ETW, ...).
pub fn set_perf_log(log: impl Fn(PerfEntry) + Send + 'static) {
    if let Ok(mut slot) = PERF_LOG.lock() {
        *slot = Some(Box::new(log));
    }
    // SAFETY: a process-wide callback pointer write.
    unsafe { ffi::db_codegen_set_perf_log(std::ptr::null_mut(), Some(perf_log_callback)) };
}

/// Removes the perf log.
pub fn clear_perf_log() {
    unsafe { ffi::db_codegen_set_perf_log(std::ptr::null_mut(), None) };
    if let Ok(mut slot) = PERF_LOG.lock() {
        slot.take();
    }
}

unsafe extern "C" fn append_text(ctx: *mut c_void, data: *const std::ffi::c_char, length: usize) {
    // SAFETY: `ctx` is the String below; Luau gives `length` bytes at `data`.
    unsafe {
        (*ctx.cast::<String>())
            .push_str(&String::from_utf8_lossy(std::slice::from_raw_parts(data.cast::<u8>(), length)));
    }
}

/// Block counters summed over every module compiled with counters on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutionStats {
    pub regular_blocks_executed: u64,
    pub vm_exits_taken: u64,
}

/// The runtime's native code generator.
pub struct NativeCodeGen {
    mode: NativeCodeMode,
    record_counters: bool,
    nop_padding: bool,
    available: bool,
    context: *mut c_void,
    /// Boxed so the remapper callback's context pointer stays valid when the generator moves
    /// into its `Runtime`.
    #[allow(clippy::box_collection)]
    userdata_types: Box<Vec<CString>>,
    userdata_type_pointers: Vec<*const std::ffi::c_char>,
    hooks: Box<HookChain>,
    modules: RefCell<HashMap<ModuleId, Vec<u8>>>,
    allocation_failures: RefCell<HashSet<ModuleId>>,
}

static COUNTER_CLOSURES_KEY: u8 = 0;

impl NativeCodeGen {
    /// Creates the generator and initialises native execution on `state`, which must be a
    /// fresh main thread (before any function that should run natively is loaded).
    pub(crate) unsafe fn create(state: *mut lua::lua_State, options: NativeCodeOptions) -> Result<NativeCodeGen> {
        let mut names = Vec::with_capacity(options.userdata_types.len());
        for name in &options.userdata_types {
            names.push(
                CString::new(name.as_str())
                    .map_err(|_| Error::logic(format!("Native code userdata type name {name:?} contains NUL")))?,
            );
        }
        let userdata_types: Box<Vec<CString>> = Box::new(names);
        let mut userdata_type_pointers: Vec<*const std::ffi::c_char> =
            userdata_types.iter().map(|name| name.as_ptr()).collect();
        userdata_type_pointers.push(std::ptr::null());
        let mut generator = NativeCodeGen {
            mode: options.mode,
            record_counters: options.record_counters,
            nop_padding: options.nop_padding,
            available: false,
            context: std::ptr::null_mut(),
            userdata_types,
            userdata_type_pointers,
            hooks: Box::new(HookChain { hooks: options.hooks }),
            modules: RefCell::new(HashMap::new()),
            allocation_failures: RefCell::new(HashSet::new()),
        };
        // SAFETY: fresh state; the shared context outlives the VM (dropped after lua_close).
        unsafe {
            if options.mode != NativeCodeMode::Off && ffi::db_codegen_supported() != 0 {
                let max_total = options.max_total_size.max(BLOCK_SIZE);
                generator.context = ffi::db_codegen_create_shared_context(BLOCK_SIZE, max_total);
                if !generator.context.is_null() {
                    ffi::db_codegen_create(state, generator.context);
                    ffi::db_codegen_set_userdata_remapper(
                        state,
                        (&*generator.userdata_types as *const Vec<CString>).cast_mut().cast(),
                        Some(hooks::remap_userdata_type),
                    );
                    generator.available = ffi::db_codegen_is_native_execution_enabled(state) != 0;
                }
            }
            if generator.record_counters {
                lua::lua_pushlightuserdata(state, (&COUNTER_CLOSURES_KEY as *const u8).cast_mut().cast());
                lua::lua_newtable(state);
                lua::lua_rawset(state, lua::LUA_REGISTRYINDEX);
            }
        }
        Ok(generator)
    }

    /// The generator of the runtime whose VM `scope` runs on, when that runtime was built with
    /// one: what [`crate::Runtime::native_code`] returns, reachable from a bound function's
    /// `Call` or any other scope, so a `require` written in Rust can compile the modules it
    /// loads. `None` for a runtime without native code.
    pub fn for_scope<'s>(scope: &'s impl Scope) -> Option<&'s NativeCodeGen> {
        // SAFETY: the scope's thread is live for `'s`, and the runtime owning it (whose shared
        // block holds the generator) outlives every scope on it.
        unsafe { crate::runtime::shared_for(scope.state()) }.and_then(crate::runtime::shared::Shared::native_code)
    }

    /// Whether native execution is live on this runtime.
    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn mode(&self) -> NativeCodeMode {
        self.mode
    }

    /// Turns native execution on or off for the whole VM (`setNativeExecutionEnabled`).
    pub fn set_native_execution_enabled(&self, scope: &impl Scope, enabled: bool) {
        // SAFETY: live thread; a flag write on the global state.
        unsafe { ffi::db_codegen_set_native_execution_enabled(scope.state(), c_int::from(enabled)) }
    }

    /// Whether native execution is currently on for the VM.
    pub fn is_native_execution_enabled(&self, scope: &impl Scope) -> bool {
        unsafe { ffi::db_codegen_is_native_execution_enabled(scope.state()) != 0 }
    }

    /// Disables native execution for the Lua function running at call-stack `level` (0 is the
    /// innermost), for example from a bound function that detected a problem.
    pub fn disable_native_execution_for_function(&self, scope: &impl Scope, level: c_int) {
        unsafe { ffi::db_codegen_disable_native_execution_for_function(scope.state(), level) }
    }

    /// Compiles the Lua closure at `index` of `scope` (and every nested function) natively.
    /// `bytecode` is the closure's bytecode, which identifies the module.
    pub fn compile(&self, scope: &impl Scope, index: c_int, bytecode: &[u8]) -> Result<NativeCodeResult> {
        let mut result = NativeCodeResult {
            status: NativeCodeStatus::Unavailable,
            stats: NativeCodeStats::default(),
            module_id: None,
        };
        if self.mode == NativeCodeMode::Off {
            result.status = NativeCodeStatus::Skipped;
            return Ok(result);
        }
        if !self.available {
            return Ok(result);
        }
        let module_id = module_id(bytecode);
        result.module_id = Some(module_id);
        {
            let mut modules = self.modules.borrow_mut();
            match modules.get(&module_id) {
                Some(existing) if existing.as_slice() != bytecode => {
                    result.status = NativeCodeStatus::IdentityCollision;
                    return Ok(result);
                }
                Some(_) => {}
                None => {
                    modules.insert(module_id, bytecode.to_vec());
                }
            }
        }
        if self.allocation_failures.borrow().contains(&module_id) {
            result.status = NativeCodeStatus::AllocationRetrySkipped;
            return Ok(result);
        }
        let state = scope.state();
        let index = if index < 0 && index > lua::LUA_REGISTRYINDEX {
            // SAFETY: relative indexes resolve against the live top.
            unsafe { lua::lua_gettop(state) + index + 1 }
        } else {
            index
        };
        // SAFETY: the slot exists on the scope's thread; lua_isfunction/iscfunction only read.
        unsafe {
            if !lua::lua_isfunction(state, index) || lua::lua_iscfunction(state, index) != 0 {
                return Err(Error::logic("Native code generation needs a Lua closure"));
            }
        }
        // SAFETY: the scope's thread is live; its runtime's shared block outlives the compilation.
        let Some(shared) = (unsafe { crate::runtime::shared_for(state) }) else {
            return Err(Error::logic("Native code generation needs a runtime-owned VM"));
        };
        let compile_context = hooks::CompileContext { chain: &self.hooks, shared };
        let table = HookChain::table(&compile_context);
        let options = ffi::db_compilation_options {
            flags: c_uint::from(self.mode == NativeCodeMode::Annotated),
            record_counters: self.record_counters,
            nop_padding: self.nop_padding,
            userdata_types: self.userdata_type_pointers.as_ptr(),
            hooks: &table,
        };
        let mut stats = ffi::db_compilation_stats::default();
        // SAFETY: state live, closure at `index`, options and hook chain outlive the call.
        let status = unsafe { ffi::db_codegen_compile(state, index, module_id.as_ptr(), &options, &mut stats) };
        result.status = NativeCodeStatus::from_luau(status);
        result.stats = NativeCodeStats {
            native_code_size_bytes: stats.native_code_size_bytes,
            functions_compiled: stats.functions_compiled,
            functions_bound: stats.functions_bound,
        };
        if result.status == NativeCodeStatus::AllocationFailed {
            self.allocation_failures.borrow_mut().insert(module_id);
        }
        if self.record_counters && result.status == NativeCodeStatus::Success && stats.functions_bound != 0 {
            // SAFETY: balanced pushes; the closure is retained so its counters stay readable.
            unsafe { retain_counter_closure(state, index, &module_id) };
        }
        Ok(result)
    }

    /// The assembly and/or IR Luau generates for the Lua closure at `index` of `scope` (and its
    /// nested functions), as text, with this generator's hooks and userdata types in effect.
    /// Does not install the code. Empty output means nothing could be lowered.
    pub fn assembly(&self, scope: &impl Scope, index: c_int, options: AssemblyOptions) -> Result<String> {
        let state = scope.state();
        // SAFETY: the scope's thread is live; its runtime's shared block outlives the call.
        let Some(shared) = (unsafe { crate::runtime::shared_for(state) }) else {
            return Err(Error::logic("Assembly dumps need a runtime-owned VM"));
        };
        let compile_context = hooks::CompileContext { chain: &self.hooks, shared };
        let table = HookChain::table(&compile_context);
        let compilation = ffi::db_compilation_options {
            flags: 0,
            record_counters: false,
            nop_padding: self.nop_padding,
            userdata_types: self.userdata_type_pointers.as_ptr(),
            hooks: &table,
        };
        let raw = ffi::db_assembly_options {
            target: match options.target {
                AssemblyTarget::Host => 0,
                AssemblyTarget::A64 => 1,
                AssemblyTarget::A64NoFeatures => 2,
                AssemblyTarget::X64Windows => 3,
                AssemblyTarget::X64SystemV => 4,
            },
            include_assembly: options.include_assembly,
            include_ir: options.include_ir,
            include_outlined_code: options.include_outlined_code,
            include_ir_types: options.include_ir_types,
            include_reg_spills: options.include_reg_spills,
            compilation: &compilation,
        };
        let mut text = String::new();
        // SAFETY: the closure index is the caller's; options and the hook table outlive the call.
        let status =
            unsafe { ffi::db_codegen_get_assembly(state, index, &raw, append_text, (&mut text as *mut String).cast()) };
        if status == 2 {
            return Err(Error::runtime("Luau failed to generate assembly for this function"));
        }
        Ok(text)
    }

    /// Sums Luau's block counters over every retained module (counters must be on).
    pub fn execution_stats(&self, scope: &impl Scope) -> ExecutionStats {
        let mut stats = ExecutionStats::default();
        if !self.record_counters {
            return stats;
        }
        let state = scope.state();
        // SAFETY: balanced traversal of the registry table; lua_getcounters only reads.
        unsafe {
            let top = lua::lua_gettop(state);
            lua::lua_pushlightuserdata(state, (&COUNTER_CLOSURES_KEY as *const u8).cast_mut().cast());
            lua::lua_rawget(state, lua::LUA_REGISTRYINDEX);
            if lua::lua_istable(state, -1) {
                lua::lua_pushnil(state);
                while lua::lua_next(state, -2) != 0 {
                    if lua::lua_isfunction(state, -1) && lua::lua_iscfunction(state, -1) == 0 {
                        lua::lua_getcounters(
                            state,
                            -1,
                            (&mut stats as *mut ExecutionStats).cast(),
                            visit_counter_function,
                            accumulate_counter,
                        );
                    }
                    lua::lua_pop(state, 1);
                }
            }
            lua::lua_settop(state, top);
        }
        stats
    }
}

impl Drop for NativeCodeGen {
    fn drop(&mut self) {
        // SAFETY: Runtime drops this after lua_close, as Luau requires for shared contexts.
        unsafe { ffi::db_codegen_destroy_shared_context(self.context) };
    }
}

unsafe extern "C" fn visit_counter_function(_: *mut c_void, _: *const std::ffi::c_char, _: c_int) {}

unsafe extern "C" fn accumulate_counter(context: *mut c_void, kind: c_int, _: c_int, hits: u64) {
    // SAFETY: `context` is the ExecutionStats being filled.
    let stats = unsafe { &mut *context.cast::<ExecutionStats>() };
    match kind {
        1 => stats.regular_blocks_executed += hits,
        3 => stats.vm_exits_taken += hits,
        _ => {}
    }
}

unsafe fn retain_counter_closure(state: *mut lua::lua_State, index: c_int, module_id: &ModuleId) {
    unsafe {
        lua::lua_pushlightuserdata(state, (&COUNTER_CLOSURES_KEY as *const u8).cast_mut().cast());
        lua::lua_rawget(state, lua::LUA_REGISTRYINDEX);
        if !lua::lua_istable(state, -1) {
            lua::lua_pop(state, 1);
            return;
        }
        lua::lua_pushlstring(state, module_id.as_ptr().cast(), module_id.len());
        lua::lua_rawget(state, -2);
        let retained = !lua::lua_isnil(state, -1);
        lua::lua_pop(state, 1);
        if !retained {
            lua::lua_pushlstring(state, module_id.as_ptr().cast(), module_id.len());
            lua::lua_pushvalue(state, index);
            lua::lua_rawset(state, -3);
        }
        lua::lua_pop(state, 1);
    }
}

/// MurmurHash3 x64 128 of `bytecode`, seeded like OpenMW (`0x4f4d574c` in both states).
pub fn module_id(bytecode: &[u8]) -> ModuleId {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    let seed = 0x4f4d_574c_u64;
    let (mut h1, mut h2) = (seed, seed);
    let blocks = bytecode.len() / 16;
    for block in 0..blocks {
        let at = block * 16;
        let mut k1 = u64::from_le_bytes(bytecode[at..at + 8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(bytecode[at + 8..at + 16].try_into().unwrap());
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27).wrapping_add(h2).wrapping_mul(5).wrapping_add(0x52dc_e729);
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31).wrapping_add(h1).wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }
    let tail = &bytecode[blocks * 16..];
    let (mut k1, mut k2) = (0u64, 0u64);
    for (i, byte) in tail.iter().enumerate() {
        if i < 8 {
            k1 ^= u64::from(*byte) << (8 * i);
        } else {
            k2 ^= u64::from(*byte) << (8 * (i - 8));
        }
    }
    if tail.len() > 8 {
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
    }
    let length = bytecode.len() as u64;
    h1 ^= length;
    h2 ^= length;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&h1.to_le_bytes());
    id[8..].copy_from_slice(&h2.to_le_bytes());
    id
}

fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}
