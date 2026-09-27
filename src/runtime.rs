//! Per-VM runtime ownership and the host-configurable creation sequence.
//!
//! The host owns a [`Runtime`], which owns the `lua_State`. Creation follows OpenMW's
//! `Lua::State` order (`components/lua/luastate.cpp`): flag policy, `luaL_newstate`, pointer
//! encoding seed, then (later slices) callbacks and the atom catalogue, then the standard
//! libraries. Every step is a builder choice so nothing is entrenched by the constructor.

use std::ffi::CString;
use std::hash::{BuildHasher, Hasher, RandomState};

use crate::error::{Error, Result};
use crate::flags;
use crate::raw::ffi;
use crate::source::{CompileOptions, compile};
use crate::stack::{Frame, Stack, ValueView, same_vm};

/// Host choices made before the VM exists.
#[derive(Clone, Debug)]
pub struct RuntimeBuilder {
    debug_roots: Vec<&'static str>,
    pointer_encoding: bool,
    standard_libraries: bool,
}

impl RuntimeBuilder {
    /// Root vocabulary for debug names (`openmw`, `string`, `vector` in OpenMW). Every native
    /// function and userdata type registered into the VM must be named under one of these.
    pub fn debug_roots(mut self, roots: &[&'static str]) -> Self {
        self.debug_roots = roots.to_vec();
        self
    }

    /// Whether to seed Luau's pointer-encoding key from OS entropy right after the state is
    /// created (`components/luau/pointerencoding.cpp`). Default on; off leaves Luau's identity
    /// mapping, which is only appropriate for deterministic test fixtures.
    pub fn pointer_encoding(mut self, enabled: bool) -> Self {
        self.pointer_encoding = enabled;
        self
    }

    /// Whether `build` opens Luau's standard libraries. Default on. Hosts that need to install
    /// callbacks first turn it off and call [`Runtime::open_standard_libraries`] themselves.
    pub fn standard_libraries(mut self, enabled: bool) -> Self {
        self.standard_libraries = enabled;
        self
    }

    pub fn build(self) -> Result<Runtime> {
        flags::initialize()?;
        // SAFETY: luaL_newstate uses the default allocator; a null result is out of memory.
        let state = unsafe { ffi::luaL_newstate() };
        if state.is_null() {
            return Err(Error::runtime("Unable to allocate a Luau state"));
        }
        let runtime = Runtime { state, debug_roots: self.debug_roots };
        if self.pointer_encoding {
            let key = PointerEncodingKey::random();
            // SAFETY: the state is fresh; this must precede any table or library creation.
            unsafe { ffi::lua_setpointerencodekey(state, key.0[0], key.0[1], key.0[2], key.0[3]) };
        }
        if self.standard_libraries {
            runtime.open_standard_libraries();
        }
        Ok(runtime)
    }
}

/// Four 64-bit words for `lua_setpointerencodekey`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerEncodingKey(pub [u64; 4]);

impl PointerEncodingKey {
    /// `lua_setpointerencodekey` clears `a`'s low bit and sets `b`'s low bit; these normalised
    /// values reproduce Luau's default identity map, which a random key must never be.
    pub fn is_identity(&self) -> bool {
        let [a, b, c, d] = self.0;
        (a & !1) == 0 && (b | 1) == 1 && c == 0 && d == 0
    }

    /// A fresh key from OS entropy, redrawn while it would be the identity map.
    ///
    /// `RandomState` is seeded per thread from the operating system; hashing distinct counters
    /// through it yields independent 64-bit words without adding a dependency.
    pub fn random() -> Self {
        let entropy = RandomState::new();
        let mut counter: u64 = 0;
        loop {
            let mut words = [0u64; 4];
            for word in &mut words {
                let mut hasher = entropy.build_hasher();
                hasher.write_u64(counter);
                counter += 1;
                *word = hasher.finish();
            }
            let key = PointerEncodingKey(words);
            if !key.is_identity() {
                return key;
            }
        }
    }
}

/// Owns one Luau VM. Dropping it closes the VM, so every owned reference into it must be gone
/// first; borrowed views cannot outlive it by construction.
pub struct Runtime {
    state: *mut ffi::lua_State,
    debug_roots: Vec<&'static str>,
}

impl Runtime {
    /// Starts configuring a VM. Defaults: debug root `dreamweave`, pointer encoding seeded,
    /// standard libraries opened.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder { debug_roots: vec!["dreamweave"], pointer_encoding: true, standard_libraries: true }
    }

    /// A VM with the default configuration.
    pub fn new() -> Result<Runtime> {
        Runtime::builder().build()
    }

    /// The main thread of this VM.
    #[cfg(test)]
    pub(crate) fn state(&self) -> *mut ffi::lua_State {
        self.state
    }

    /// The configured debug-name roots.
    pub fn debug_roots(&self) -> &[&'static str] {
        &self.debug_roots
    }

    /// Opens every Luau standard library (`luaL_openlibs`). Idempotent in effect; call once.
    pub fn open_standard_libraries(&self) {
        // SAFETY: live state; openlibs raises only on out of memory.
        unsafe { ffi::luaL_openlibs(self.state) };
    }

    /// Runs a full collection cycle. Rust destructors of unreachable userdata run inside.
    pub fn collect_garbage(&self) {
        // SAFETY: live state; a full collect is always permitted from the host.
        unsafe { ffi::lua_gc(self.state, ffi::LUA_GCCOLLECT, 0) };
    }

    /// Bytes currently allocated by the VM across all memory categories.
    pub fn total_bytes(&self) -> usize {
        unsafe { ffi::lua_totalbytes(self.state, -1) }
    }

    /// The main thread's stack at host level, valid while the runtime is.
    pub fn stack(&self) -> Stack<'_> {
        // SAFETY: the state lives as long as `&self`; host code is the only user of the main
        // thread while no Lua call is running.
        unsafe { Stack::from_raw(self.state, true) }
    }

    /// Compiles `source` and pushes the resulting chunk function onto `frame`, which may be on
    /// any thread of this VM.
    pub fn load<'f>(
        &self,
        frame: &'f Frame<'_>,
        chunk_name: &str,
        source: &str,
        options: &CompileOptions,
    ) -> Result<ValueView<'f>> {
        if !same_vm(frame.state(), self.state) {
            return Err(Error::logic("Frame belongs to a different Lua VM"));
        }
        let bytecode = compile(source, options)?;
        let name = CString::new(chunk_name).map_err(|_| Error::logic("Chunk name cannot contain NUL"))?;
        // SAFETY: the frame's thread is live and in this VM; the bytecode slice and name
        // outlive the call. luau_load reports failure by status and leaves the message on top.
        let status =
            unsafe { ffi::luau_load(frame.state(), name.as_ptr(), bytecode.as_ptr().cast(), bytecode.len(), 0) };
        if status != ffi::LUA_OK {
            // SAFETY: the message is on the frame's thread.
            return Err(unsafe { crate::raw::protect::pop_error(frame.state(), status) });
        }
        Ok(frame.top_value())
    }

    /// Compiles and runs `source`, which must return one function, and pins that function.
    /// The usual way to get a Lua closure into Rust hands for tests and host setup.
    pub fn load_function(&self, source: &str) -> Result<crate::value::Function> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            let chunk = self.load(frame, "=load_function", source, &CompileOptions::default())?;
            chunk.as_function()?.invoke::<crate::value::Function, ()>(frame, ())
        })
    }

    /// Compiles and runs `source` on the main thread, discarding results.
    pub fn exec(&self, source: &str) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            self.load(frame, "=exec", source, &CompileOptions::default())?;
            // SAFETY: the chunk function is on top; pcall contains any raise.
            let status = unsafe { ffi::lua_pcall(self.state, 0, 0, 0) };
            if status != ffi::LUA_OK {
                return Err(unsafe { crate::raw::protect::pop_error(self.state, status) });
            }
            Ok(())
        })
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

    #[test]
    fn standard_libraries_can_be_deferred() {
        let runtime = Runtime::builder().standard_libraries(false).build().unwrap();
        runtime.exec("return 1").unwrap();
        assert!(runtime.exec("assert(true)").unwrap_err().to_string().contains("attempt to call a nil value"));
        runtime.open_standard_libraries();
        runtime.exec("assert(true)").unwrap();
    }

    #[test]
    fn pointer_encoding_keys_are_random_and_never_identity() {
        assert!(PointerEncodingKey([0, 1, 0, 0]).is_identity());
        assert!(PointerEncodingKey([1, 0, 0, 0]).is_identity());
        assert!(!PointerEncodingKey([2, 0, 0, 0]).is_identity());
        let first = PointerEncodingKey::random();
        let second = PointerEncodingKey::random();
        assert!(!first.is_identity());
        assert_ne!(first, second);
        // Table keying by pointer still works with a non-identity key.
        let runtime = Runtime::new().unwrap();
        runtime
            .exec("local k = {} local t = {[k] = 1} for i = 1, 100 do t[{}] = i end assert(t[k] == 1)")
            .unwrap();
    }
}
