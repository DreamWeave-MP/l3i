//! Per-VM runtime ownership and the host-configurable creation sequence.
//!
//! The host owns a [`Runtime`], which owns the `lua_State`. Creation follows OpenMW's
//! `Lua::State` order (`components/lua/luastate.cpp`): flag policy, `luaL_newstate`, pointer
//! encoding seed, the per-VM shared state behind `lua_Callbacks.userdata`, the interrupt and
//! allocation callbacks when a limit or the profiler asks for them, then the standard
//! libraries. Every step is a builder choice so nothing is entrenched by the constructor.

mod call_scope;
pub mod profiler;
pub(crate) mod shared;

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CString, c_int};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::rc::Rc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::flags;
use crate::raw::ffi;
use crate::source::{CompileOptions, compile_raw};
use crate::stack::{Frame, Stack, ValueView, same_vm};

pub use call_scope::{CallContext, CallKind, CallScope};
use shared::Shared;

/// The shared block of the runtime owning `state`, for as long as that runtime lives.
///
/// # Safety
/// `state` is a live thread; the returned borrow must not outlive the runtime.
pub(crate) unsafe fn shared_for<'a>(state: *mut ffi::lua_State) -> Option<&'a Shared> {
    // SAFETY: forwarded contract.
    unsafe { shared::shared_of(state).as_ref() }
}

/// The VM lifetime token of the runtime owning `state`, or a dead token for a foreign VM.
///
/// # Safety
/// `state` is a live thread.
pub(crate) unsafe fn vm_lifetime(state: *mut ffi::lua_State) -> std::rc::Weak<()> {
    // SAFETY: forwarded contract.
    let shared = unsafe { shared::shared_of(state) };
    if shared.is_null() { std::rc::Weak::new() } else { unsafe { (*shared).lifetime() } }
}
pub use shared::{CallStats, Limits, MemoryCategory, SampledLocation, Samples};

/// Host choices made before the VM exists.
pub struct RuntimeBuilder {
    debug_roots: Vec<Box<str>>,
    pointer_encoding: bool,
    standard_libraries: bool,
    limits: Limits,
    profiler: bool,
    initialization_category: MemoryCategory,
    #[cfg(feature = "jit")]
    native_code: Option<crate::native_code::NativeCodeOptions>,
    atom_catalogue: Option<crate::direct::AtomCatalogue>,
    buffer_cage: Option<Box<dyn crate::memory::BufferCage>>,
}

impl RuntimeBuilder {
    /// Routes every `buffer` allocation through `cage` (`lua_setbuffercage`), installed right
    /// after the state is created so no buffer exists outside it.
    pub fn buffer_cage(mut self, cage: impl crate::memory::BufferCage) -> Self {
        self.buffer_cage = Some(Box::new(cage));
        self
    }

    /// The atom catalogue for this VM, installed before the standard libraries open (OpenMW's
    /// order). Each runtime may carry its own.
    pub fn atom_catalogue(mut self, catalogue: crate::direct::AtomCatalogue) -> Self {
        self.atom_catalogue = Some(catalogue);
        self
    }

    /// Root vocabulary for debug names (`openmw`, `string`, `vector` in OpenMW). Every native
    /// function and userdata type registered into the VM must be named under one of these.
    pub fn debug_roots<R: AsRef<str>>(mut self, roots: &[R]) -> Self {
        self.debug_roots = roots.iter().map(|root| Box::from(root.as_ref())).collect();
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

    /// Wall-clock budget per outermost script call, enforced at Luau safepoints. Zero disables.
    pub fn execution_time_limit(mut self, limit: Duration) -> Self {
        self.limits.execution_time = limit;
        self
    }

    /// Heap ceiling in bytes across every memory category, matching OpenMW's watchdog: it is
    /// **polled every 64th safepoint**, not enforced by the allocator, so a script can overshoot
    /// it by whatever it allocates between two polls before the error fires. It bounds runaway
    /// scripts; it is not a hard quota against a hostile one (that would need allocator-level
    /// policy). Zero disables.
    pub fn memory_limit(mut self, bytes: usize) -> Self {
        self.limits.memory_bytes = bytes;
        self
    }

    /// Time script calls, switch memory categories per call, and record allocation activity.
    pub fn profiler(mut self, enabled: bool) -> Self {
        self.profiler = enabled;
        self
    }

    /// The memory category charged while sandboxes and templates are set up. Default 0.
    pub fn initialization_category(mut self, category: MemoryCategory) -> Self {
        self.initialization_category = category;
        self
    }

    /// Enables Luau native code generation with `options` (`jit` feature). Templates loaded
    /// through a sandbox are compiled according to the mode; `Runtime::native_code` exposes the
    /// generator for host-driven compilation. Off by default.
    #[cfg(feature = "jit")]
    pub fn native_code(mut self, options: crate::native_code::NativeCodeOptions) -> Self {
        self.native_code = Some(options);
        self
    }

    pub fn build(self) -> Result<Runtime> {
        flags::initialize()?;
        // SAFETY: luaL_newstate uses the default allocator; a null result is out of memory.
        let state = unsafe { ffi::luaL_newstate() };
        if state.is_null() {
            return Err(Error::runtime("Unable to allocate a Luau state"));
        }
        // Creation order (luastate.cpp): pointer-encoding key first, before any table or library
        // exists; then the buffer cage, the per-VM state and callbacks, native code generation,
        // the atom catalogue, and last the standard libraries.
        if self.pointer_encoding {
            let key = PointerEncodingKey::random();
            // SAFETY: the state is fresh; this must precede any table or library creation.
            unsafe { ffi::lua_setpointerencodekey(state, key.0[0], key.0[1], key.0[2], key.0[3]) };
        }
        let buffer_cage = self.buffer_cage.map(Box::new);
        if let Some(cage) = &buffer_cage {
            // SAFETY: fresh state, before any buffer; the outer Box keeps the cage's address
            // stable for the VM's life (it is dropped after lua_close).
            unsafe {
                ffi::lua_setbuffercage(
                    state,
                    crate::memory::cage_callback,
                    (&**cage as *const Box<dyn crate::memory::BufferCage>).cast_mut().cast(),
                );
            }
        }
        let shared = Box::new(Shared::new(self.limits, self.profiler));
        // SAFETY: fresh state; the main thread's record lives until `Drop` detaches it, just
        // before lua_close (a closed state must not be touched), and the `userthread` callback
        // gives every other thread its own. The callback block belongs to this VM; `shared` is
        // never moved out of its Box.
        unsafe {
            shared::attach_thread_record(state);
            let callbacks = ffi::lua_callbacks(state);
            (*callbacks).userthread = Some(crate::debug::on_user_thread);
            (*callbacks).userdata = (&*shared as *const Shared).cast_mut().cast();
            if shared.profiler_enabled() {
                (*callbacks).onallocate = Some(shared::on_allocate);
            }
        }
        // From here on the Runtime owns the state: every later step that can fail returns
        // through `?` and `Runtime::drop` closes the VM and frees the records.
        #[allow(unused_mut)]
        let mut runtime = Runtime {
            state,
            debug_roots: self.debug_roots,
            shared,
            initialization_category: self.initialization_category,
            #[cfg(feature = "jit")]
            native_code: None,
            buffer_cage,
            plan: RefCell::new(None),
            states: RefCell::new(HashMap::new()),
            compile_options: RefCell::new(CompileOptions::default()),
            module_members: RefCell::new(None),
        };
        // SAFETY: the state has its pointer key and callbacks; native execution must be set up
        // before any function is loaded, which nothing below this point does before it.
        #[cfg(feature = "jit")]
        if let Some(options) = self.native_code {
            runtime.native_code = Some(unsafe { crate::native_code::NativeCodeGen::create(state, options) }?);
        }
        runtime.update_interrupt_hook();
        if let Some(catalogue) = self.atom_catalogue {
            crate::direct::install_atom_callback(&runtime, catalogue)?;
        }
        if self.standard_libraries {
            runtime.open_standard_libraries();
        }
        // The binder reads argument slots through a mirror of Luau's value layout; prove the
        // mirror against this build's API before any binding trusts it.
        // SAFETY: a fresh main thread with a free stack.
        unsafe { crate::convert::raw::self_test(state)? };
        for kind in crate::packed::builtin_kinds() {
            runtime.shared().register_packed_kind(kind)?;
        }
        Ok(runtime)
    }
}

/// `(owner, type)` of a runtime-owned state value; `None` is the host's namespace.
type StateKey = (Option<&'static str>, TypeId);

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
    debug_roots: Vec<Box<str>>,
    /// Per-VM state reachable from callbacks through `lua_Callbacks.userdata`.
    shared: Box<Shared>,
    initialization_category: MemoryCategory,
    /// Dropped after `lua_close` (the `Drop` body closes the VM first), as Luau requires for
    /// shared code contexts.
    #[cfg(feature = "jit")]
    native_code: Option<crate::native_code::NativeCodeGen>,
    /// Boxed twice so the pointer Luau holds stays valid while the runtime moves; kept only to
    /// outlive `lua_close`.
    #[allow(dead_code, clippy::box_collection)]
    buffer_cage: Option<Box<Box<dyn crate::memory::BufferCage>>>,
    /// The plan this runtime was made from, for introspection; `None` for a builder-made VM.
    plan: RefCell<Option<Rc<crate::extension::RuntimePlan>>>,
    /// Extension-owned state by type, dropped before `lua_close`.
    /// Runtime-owned state by `(owner, type)`: `None` is the host's namespace, an extension id
    /// its own, so two extensions storing the same Rust type never overwrite each other.
    states: RefCell<HashMap<StateKey, Rc<dyn Any>>>,
    /// The compiler options `exec` and `load_function` use; a plan sets the known libraries.
    compile_options: RefCell<CompileOptions>,
    /// Module members installed by a plan, for type definitions.
    module_members: RefCell<Option<Rc<crate::extension::install_detail::ModuleMembers>>>,
}

/// The context id call scopes use for sandbox and template setup.
pub const INITIALIZATION_CONTEXT: u64 = u64::MAX;

impl Runtime {
    /// Starts configuring a VM. Defaults: debug root `dreamweave`, pointer encoding seeded,
    /// standard libraries opened, no limits, profiler off.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder {
            debug_roots: vec![Box::from("dreamweave")],
            pointer_encoding: true,
            standard_libraries: true,
            limits: Limits::default(),
            profiler: false,
            initialization_category: MemoryCategory(0),
            #[cfg(feature = "jit")]
            native_code: None,
            atom_catalogue: None,
            buffer_cage: None,
        }
    }

    /// The call context for initialization work: [`INITIALIZATION_CONTEXT`] in the configured
    /// initialization category.
    pub fn initialization_context(&self) -> CallContext {
        CallContext { id: INITIALIZATION_CONTEXT, category: self.initialization_category }
    }

    /// The native code generator, when the runtime was built with one (`jit` feature).
    #[cfg(feature = "jit")]
    pub fn native_code(&self) -> Option<&crate::native_code::NativeCodeGen> {
        self.native_code.as_ref()
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Installs or removes the interrupt hook according to the limits and sampler state
    /// (`State::updateInterruptHook`).
    pub(crate) fn update_interrupt_hook(&self) {
        let needed = self.shared.needs_interrupt();
        // SAFETY: interrupt is documented safe to set at any time.
        unsafe {
            (*ffi::lua_callbacks(self.state)).interrupt = if needed { Some(shared::interrupt) } else { None };
        }
    }

    /// The configured limits.
    pub fn limits(&self) -> Limits {
        self.shared.limits()
    }

    /// Changes the limits; takes effect for the next call scope.
    pub fn set_limits(&self, limits: Limits) {
        self.shared.set_limits(limits);
        self.update_interrupt_hook();
    }

    /// Bytes currently allocated in one memory category.
    pub fn total_bytes_in(&self, category: MemoryCategory) -> usize {
        unsafe { ffi::lua_totalbytes(self.state, c_int::from(category.0)) }
    }

    /// The active memory category of the main thread. Allocations are attributed to it.
    pub fn set_memory_category(&self, category: MemoryCategory) {
        unsafe { ffi::lua_setmemcat(self.state, c_int::from(category.0)) }
    }

    /// Accumulated call statistics (profiler only).
    pub fn call_stats(&self) -> CallStats {
        self.shared.stats()
    }

    /// Selects one call context to sample at safepoints (every 32nd, while it is the innermost
    /// active call), or stops sampling. Changing the selection clears the samples. Costs nothing
    /// while nothing is sampled.
    pub fn set_sampled_context(&self, context: Option<u64>) {
        self.shared.set_sampled_context(context);
        self.update_interrupt_hook();
    }

    pub fn sampled_context(&self) -> Option<u64> {
        self.shared.sampled_context()
    }

    /// A copy of the samples taken for the sampled context so far.
    pub fn samples(&self) -> Samples {
        self.shared.samples()
    }

    /// The host's frame counter for statistics; [`profiler::FrameStats::catch_up`] folds
    /// per-frame accumulators against it.
    pub fn stats_frame(&self) -> u64 {
        self.shared.stats_frame().get()
    }

    pub fn advance_stats_frame(&self) {
        let frame = self.shared.stats_frame();
        frame.set(frame.get() + 1);
    }

    /// Runs one incremental GC step of `steps` kilobytes (0 = one basic step); true when a
    /// cycle finished.
    pub fn gc_step(&self, steps: i32) -> bool {
        // SAFETY: live state; an incremental step is always permitted from the host.
        unsafe { ffi::lua_gc(self.state, ffi::LUA_GCSTEP, steps) == 1 }
    }

    /// [`Runtime::gc_step`] with its wall time, for GC accounting (`LuaManager::gcStep`).
    pub fn gc_step_timed(&self, steps: i32) -> (bool, Duration) {
        let start = std::time::Instant::now();
        let finished = self.gc_step(steps);
        (finished, start.elapsed())
    }

    /// `source:line` of the innermost Lua code running on the main thread: the script line
    /// that called the running native function. Empty when no Lua code is running.
    pub fn caller_location(&self) -> String {
        caller_location(&self.stack())
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
    pub fn debug_roots(&self) -> &[Box<str>] {
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

    /// The main thread's root stack at host level, valid while the runtime is. Only one root
    /// stack may be alive at a time; helpers that take a `&Runtime` acquire it themselves, so
    /// call them between root stacks, not while holding one. Inside a bound function use the
    /// [`crate::bind::Call`] scope instead: the host's root is suspended there, and a second
    /// root on the same frame is refused.
    ///
    /// # Panics
    /// If a root stack from this runtime is alive and not suspended inside a Lua call.
    pub fn stack(&self) -> Stack<'_> {
        // SAFETY: the state lives as long as `&self` and belongs to this runtime.
        unsafe { Stack::lease_root(self.state) }
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
        let bytecode = compile_raw(source, options)?;
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

    /// [`Runtime::load`] with the chunk's environment set to `env` (the `env` argument of
    /// `luau_load`), so its globals resolve through that table instead of the real globals.
    pub fn load_with_env<'f>(
        &self,
        frame: &'f Frame<'_>,
        chunk_name: &str,
        source: &str,
        options: &CompileOptions,
        env: &crate::value::Table,
    ) -> Result<ValueView<'f>> {
        if !same_vm(frame.state(), self.state) {
            return Err(Error::logic("Frame belongs to a different Lua VM"));
        }
        let bytecode = compile_raw(source, options)?;
        let name = CString::new(chunk_name).map_err(|_| Error::logic("Chunk name cannot contain NUL"))?;
        let env_view = env.push_to(frame)?;
        // SAFETY: the environment table is on the frame; luau_load reads it by index and the
        // frame owns the loaded chunk (or the error message).
        let status = unsafe {
            ffi::luau_load(frame.state(), name.as_ptr(), bytecode.as_ptr().cast(), bytecode.len(), env_view.index())
        };
        if status != ffi::LUA_OK {
            return Err(unsafe { crate::raw::protect::pop_error(frame.state(), status) });
        }
        Ok(frame.top_value())
    }

    /// Creates a runtime from a finalised extension plan: the VM, every extension installed in
    /// dependency order, direct dispatch, modules, and compiler metadata, published together.
    pub fn from_plan(plan: &Rc<crate::extension::RuntimePlan>) -> Result<Runtime> {
        crate::extension::install_detail::instantiate(plan)
    }

    /// The plan this runtime was created from, if any.
    pub fn plan(&self) -> Option<Rc<crate::extension::RuntimePlan>> {
        self.plan.borrow().clone()
    }

    pub(crate) fn set_plan(&self, plan: Rc<crate::extension::RuntimePlan>) {
        *self.plan.borrow_mut() = Some(plan);
    }

    /// Registers `T` as the owner of its packed kind on this VM, so `Packed<T>` may cross the
    /// boundary here. l3i's own kinds are registered at creation; a plan registers the kinds
    /// its extensions declare. Registering the same type twice is a no-op; another type on the
    /// same kind number is a logic error.
    pub fn register_packed<T: crate::packed::PackedScalar>(&self) -> Result<()> {
        self.refuse_if_planned("register a packed kind")?;
        self.shared().register_packed_kind(crate::packed::PackedKind::of::<T>())
    }

    /// A runtime made from a plan has one immutable shape: its packed kinds, compiler
    /// metadata, tags, and slots were resolved together and stay that way.
    fn refuse_if_planned(&self, action: &str) -> Result<()> {
        if self.plan.borrow().is_some() {
            return Err(Error::logic(format!(
                "cannot {action} on a runtime made from a plan; declare it in the plan (the runtime's shape is immutable)"
            )));
        }
        Ok(())
    }

    /// Stores host-owned runtime state by type (one value per type in the host's namespace),
    /// dropped before the VM closes. Extensions store theirs through `InstallContext`, in
    /// their own namespace.
    pub fn insert_state<S: 'static>(&self, state: S) -> Rc<S> {
        self.insert_state_for(None, state)
    }

    /// Host-owned runtime state of type `S`, if stored.
    pub fn host_state<S: 'static>(&self) -> Option<Rc<S>> {
        self.state_for::<S>(None)
    }

    /// The state of type `S` that extension `owner` stored, if any.
    pub fn state_of<S: 'static>(&self, owner: &'static str) -> Option<Rc<S>> {
        self.state_for::<S>(Some(owner))
    }

    pub(crate) fn insert_state_for<S: 'static>(&self, owner: Option<&'static str>, state: S) -> Rc<S> {
        let shared = Rc::new(state);
        self.states.borrow_mut().insert((owner, TypeId::of::<S>()), Rc::clone(&shared) as Rc<dyn Any>);
        shared
    }

    pub(crate) fn state_for<S: 'static>(&self, owner: Option<&'static str>) -> Option<Rc<S>> {
        self.states.borrow().get(&(owner, TypeId::of::<S>())).and_then(crate::extension::install_detail::downcast::<S>)
    }

    /// The compiler options `exec` and `load_function` use (a plan fills in the known
    /// libraries and userdata types).
    pub fn compile_options(&self) -> CompileOptions {
        self.compile_options.borrow().clone()
    }

    /// Replaces the compiler options of a manually assembled runtime; a runtime made from a
    /// plan derives them from the plan and refuses.
    pub fn set_compile_options(&self, options: CompileOptions) -> Result<()> {
        self.refuse_if_planned("replace the compiler options")?;
        self.install_compile_options(options);
        Ok(())
    }

    /// The planner's own setter: the options derived from the plan.
    pub(crate) fn install_compile_options(&self, options: CompileOptions) {
        *self.compile_options.borrow_mut() = options;
    }

    pub(crate) fn set_module_members(&self, members: Rc<crate::extension::install_detail::ModuleMembers>) {
        *self.module_members.borrow_mut() = Some(members);
    }

    /// Luau type definitions for this runtime's plan, including installed module members.
    pub fn type_definitions(&self) -> Option<String> {
        let plan = self.plan()?;
        let members = self.module_members.borrow();
        Some(crate::extension::install_detail::render_definitions(&plan, members.as_deref()))
    }

    /// `luaL_sandbox`: globals and the standard library tables become read-only and the
    /// globals table a safe environment. Call after every module is installed.
    pub fn sandbox_globals(&self) {
        // SAFETY: live main thread; luaL_sandbox raises only on out of memory.
        unsafe { ffi::luaL_sandbox(self.state) };
    }

    /// Compiles and runs `source`, which must return one function, and pins that function.
    /// The usual way to get a Lua closure into Rust hands for tests and host setup.
    pub fn load_function(&self, source: &str) -> Result<crate::value::Function> {
        let options = self.compile_options();
        let stack = self.stack();
        stack.with_frame(|frame| {
            let chunk = self.load(frame, "=load_function", source, &options)?;
            chunk.as_function()?.invoke::<crate::value::Function, ()>(frame, ())
        })
    }

    /// Compiles and runs `source` on the main thread, discarding results.
    pub fn exec(&self, source: &str) -> Result<()> {
        let options = self.compile_options();
        let stack = self.stack();
        stack.with_frame(|frame| {
            self.load(frame, "=exec", source, &options)?;
            // SAFETY: the chunk function is on top; pcall contains any raise.
            let _lua_call = unsafe { shared::LuaCall::enter(self.state) };
            let status = unsafe { ffi::lua_pcall(self.state, 0, 0, 0) };
            if status != ffi::LUA_OK {
                return Err(unsafe { crate::raw::protect::pop_error(self.state, status) });
            }
            Ok(())
        })
    }
}

/// `source:line` of the innermost Lua code running on `scope`'s thread; from inside a bound
/// function, the script line that called it. Empty when no Lua code is running there.
pub fn caller_location(scope: &impl crate::stack::Scope) -> String {
    shared::caller_location(scope.state())
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Extension state may hold pinned values; it goes first, while the VM is live, then the
        // plan (extensions hold no Lua values).
        self.states.borrow_mut().clear();
        self.module_members.borrow_mut().take();
        self.plan.borrow_mut().take();
        // Values pinned on this VM check the lifetime token before touching it; ending it first
        // turns every surviving `Value` into an inert invalid one.
        self.shared.end_lifetime();
        // SAFETY: we own the state and nothing borrowed from it can outlive `self`. The main
        // thread's record is freed while the state is still open (touching a closed state is
        // not allowed); coroutine records go through `userthread` as lua_close frees their
        // threads, which needs `shared` alive, and struct fields drop after this body.
        unsafe {
            (*ffi::lua_callbacks(self.state)).interrupt = None;
            (*ffi::lua_callbacks(self.state)).onallocate = None;
            shared::detach_thread_record(self.state);
            ffi::lua_close(self.state);
        }
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
        runtime.exec("local t = {x = 3} local r = if local v = t.x then v * 2 else 0 assert(r == 6, r)").unwrap();
    }

    #[test]
    fn fastpcall_bytecode_loads_and_runs() {
        let runtime = Runtime::new().unwrap();
        runtime.exec("local ok, err = pcall(function() error('x') end) assert(not ok and err:find('x'))").unwrap();
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
        runtime.exec("local k = {} local t = {[k] = 1} for i = 1, 100 do t[{}] = i end assert(t[k] == 1)").unwrap();
    }
}
