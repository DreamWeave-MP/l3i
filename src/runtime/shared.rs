//! Per-VM state reachable from Luau callbacks (`Lua::State` in luastate.cpp), and the
//! callbacks themselves: the interrupt watchdog and allocation activity.

use std::any::TypeId;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, c_int, c_void};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::TAG_LIMIT;
use crate::direct::AtomCatalogue;
use crate::raw::ffi;
use crate::userdata::RuntimeTag;

/// A Luau memory category (0..256). Ids and their meanings are host data; OpenMW uses 0 for
/// shared, 1 global, 2 menu, 3 player, and so on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MemoryCategory(pub u8);

/// Watchdog limits. Zero disables a limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock budget per outermost script call.
    pub execution_time: Duration,
    /// Heap ceiling in bytes across all categories, polled at safepoints (see
    /// `RuntimeBuilder::memory_limit` for what that does and does not guarantee).
    pub memory_bytes: usize,
}

/// One active call on the scope stack.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveCall {
    pub context: u64,
    pub category: MemoryCategory,
    pub nested_ms: f64,
    pub allocated_bytes: u64,
}

/// A watchdog deadline: when the outermost call started and when it must end.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    pub start: Instant,
    pub end: Instant,
}

/// Accumulated profiler statistics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallStats {
    /// Total measured self time of script calls, in milliseconds.
    pub total_script_ms: f64,
    /// Self time per host context id.
    pub time_by_context_ms: HashMap<u64, f64>,
    /// Allocation activity (bytes grown) per host context id.
    pub allocation_by_context: HashMap<u64, u64>,
    /// Timed calls, and the overhead samples taken every 64th call.
    pub timed_calls: u64,
    pub overhead_samples: u64,
    pub overhead_total_ms: f64,
}

pub(crate) const SAFEPOINTS_PER_WATCHDOG_POLL: u32 = 64;
pub(crate) const SAFEPOINTS_PER_SAMPLE: u32 = 32;
const TIMED_CALLS_PER_OVERHEAD_SAMPLE: u64 = 64;
/// How many Lua frames one sample walks.
const SAMPLE_MAX_DEPTH: c_int = 32;

/// One sampled location: where the sampler found Lua code running, and how often.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SampledLocation {
    /// The function's name, empty when it has none.
    pub function: String,
    pub samples: u64,
}

/// Where a sampled context spends its time: innermost Lua frames by `source:line`, and every
/// Lua function on the sampled stacks by `source:linedefined` (counted once per sample however
/// deep it recurses).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Samples {
    pub lines: BTreeMap<String, SampledLocation>,
    pub functions: BTreeMap<String, SampledLocation>,
    /// Samples that found at least one Lua frame.
    pub count: u64,
}

pub(crate) struct Shared {
    limits: Cell<Limits>,
    profiler: bool,
    active_calls: RefCell<Vec<ActiveCall>>,
    deadline: Cell<Option<Deadline>>,
    safepoints_until_poll: Cell<u32>,
    stats: RefCell<CallStats>,
    sampled_context: Cell<Option<u64>>,
    safepoints_until_sample: Cell<u32>,
    samples: RefCell<Samples>,
    stats_frame: Cell<u64>,
    /// The VM lifetime token: `Value`s hold a `Weak` to it and go inert when it ends.
    lifetime: RefCell<Option<Rc<()>>>,
    /// This VM's atom catalogue, read by `useratom`.
    atoms: RefCell<Option<Rc<AtomCatalogue>>>,
    /// This VM's tag plan: which Rust type each Luau tag carries, assigned by the host at
    /// registration.
    tags: TagPlan,
    /// Host hooks for the debugger and lifecycle callback slots.
    hooks: crate::debug::HookSlot,
    /// The embedder half of cross-heap GC marking.
    embedder_gc: crate::memory::EmbedderGcSlot,
    /// The runtime-resolved direct dispatch plan.
    direct_plan: crate::direct::plan::DirectPlanSlot,
    /// The host's require navigator.
    require_navigator: crate::require::NavigatorSlot,
}

/// The host's tag assignments for one VM. `by_tag` answers the hot-path question ("is the
/// userdata at this slot a `T`?") with an array read and a `TypeId` compare; `by_type` answers
/// the registration and push question ("which tag does `T` have here?").
pub(crate) struct TagPlan {
    by_tag: Vec<Cell<Option<TypeId>>>,
    by_type: RefCell<HashMap<TypeId, RuntimeTag>>,
}

impl TagPlan {
    fn new() -> TagPlan {
        TagPlan { by_tag: (0..TAG_LIMIT).map(|_| Cell::new(None)).collect(), by_type: RefCell::new(HashMap::new()) }
    }
}

impl Shared {
    pub(crate) fn new(limits: Limits, profiler: bool) -> Shared {
        Shared {
            limits: Cell::new(limits),
            profiler,
            active_calls: RefCell::new(Vec::new()),
            deadline: Cell::new(None),
            safepoints_until_poll: Cell::new(0),
            stats: RefCell::new(CallStats::default()),
            sampled_context: Cell::new(None),
            safepoints_until_sample: Cell::new(0),
            samples: RefCell::new(Samples::default()),
            stats_frame: Cell::new(0),
            lifetime: RefCell::new(Some(Rc::new(()))),
            atoms: RefCell::new(None),
            tags: TagPlan::new(),
            hooks: crate::debug::HookSlot::new(),
            embedder_gc: RefCell::new(None),
            direct_plan: RefCell::new(None),
            require_navigator: RefCell::new(None),
        }
    }

    pub(crate) fn require_navigator(&self) -> &crate::require::NavigatorSlot {
        &self.require_navigator
    }

    pub(crate) fn direct_plan(&self) -> &crate::direct::plan::DirectPlanSlot {
        &self.direct_plan
    }

    pub(crate) fn embedder_gc(&self) -> &crate::memory::EmbedderGcSlot {
        &self.embedder_gc
    }

    pub(crate) fn hooks(&self) -> &crate::debug::HookSlot {
        &self.hooks
    }

    /// The tag this VM assigned to the Rust type `id`, if any.
    pub(crate) fn tag_of_type(&self, id: TypeId) -> Option<RuntimeTag> {
        self.tags.by_type.borrow().get(&id).copied()
    }

    /// The Rust type this VM assigned to `tag`, if any. Out-of-range tags have none.
    #[inline]
    pub(crate) fn type_of_tag(&self, tag: c_int) -> Option<TypeId> {
        usize::try_from(tag).ok().and_then(|tag| self.tags.by_tag.get(tag)).and_then(Cell::get)
    }

    /// Records `tag -> id`; the caller has checked the range and both directions for conflicts.
    pub(crate) fn assign_tag(&self, id: TypeId, tag: RuntimeTag) {
        self.tags.by_tag[usize::from(tag)].set(Some(id));
        self.tags.by_type.borrow_mut().insert(id, tag);
    }

    /// The installed catalogue; `None` while a (re)installation is in progress, so `useratom`
    /// never panics on a busy cell.
    pub(crate) fn atom_catalogue(&self) -> Option<Rc<AtomCatalogue>> {
        self.atoms.try_borrow().ok().and_then(|atoms| atoms.clone())
    }

    pub(crate) fn set_atom_catalogue(&self, catalogue: Rc<AtomCatalogue>) {
        *self.atoms.borrow_mut() = Some(catalogue);
    }

    pub(crate) fn clear_atom_catalogue(&self) {
        self.atoms.borrow_mut().take();
    }

    /// A weak handle that stays alive exactly as long as the VM does.
    pub(crate) fn lifetime(&self) -> Weak<()> {
        self.lifetime.borrow().as_ref().map_or_else(Weak::new, Rc::downgrade)
    }

    /// Ends the VM lifetime; every `Weak` handed out is dead from here on.
    pub(crate) fn end_lifetime(&self) {
        self.lifetime.borrow_mut().take();
    }

    pub(crate) fn sampled_context(&self) -> Option<u64> {
        self.sampled_context.get()
    }

    /// Selects the context to sample, clearing earlier samples (`setSampledScript`).
    pub(crate) fn set_sampled_context(&self, context: Option<u64>) {
        if context == self.sampled_context.get() {
            return;
        }
        self.sampled_context.set(context);
        *self.samples.borrow_mut() = Samples::default();
        self.safepoints_until_sample.set(0);
    }

    pub(crate) fn samples(&self) -> Samples {
        self.samples.borrow().clone()
    }

    pub(crate) fn stats_frame(&self) -> &Cell<u64> {
        &self.stats_frame
    }

    pub(crate) fn profiler_enabled(&self) -> bool {
        self.profiler
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits.get()
    }

    pub(crate) fn set_limits(&self, limits: Limits) {
        self.limits.set(limits);
    }

    pub(crate) fn needs_interrupt(&self) -> bool {
        let limits = self.limits.get();
        !limits.execution_time.is_zero() || limits.memory_bytes != 0 || self.sampled_context.get().is_some()
    }

    pub(crate) fn stats(&self) -> CallStats {
        self.stats.borrow().clone()
    }

    pub(crate) fn active_calls(&self) -> &RefCell<Vec<ActiveCall>> {
        &self.active_calls
    }

    pub(crate) fn deadline(&self) -> &Cell<Option<Deadline>> {
        &self.deadline
    }

    pub(crate) fn reset_watchdog_poll(&self) {
        self.safepoints_until_poll.set(0);
    }

    /// Records a measured call; returns true when this call should sample its own overhead.
    pub(crate) fn add_call_time(&self, context: u64, self_ms: f64) -> bool {
        let mut stats = self.stats.borrow_mut();
        stats.total_script_ms += self_ms;
        *stats.time_by_context_ms.entry(context).or_insert(0.0) += self_ms;
        stats.timed_calls += 1;
        stats.timed_calls.is_multiple_of(TIMED_CALLS_PER_OVERHEAD_SAMPLE)
    }

    pub(crate) fn add_overhead_sample(&self, elapsed: Duration) {
        let mut stats = self.stats.borrow_mut();
        stats.overhead_samples += 1;
        stats.overhead_total_ms += elapsed.as_secs_f64() * 1000.0;
    }

    pub(crate) fn add_allocation_activity(&self, context: u64, bytes: u64) {
        *self.stats.borrow_mut().allocation_by_context.entry(context).or_insert(0) += bytes;
    }
}

/// Per-Lua-thread bookkeeping, stored in the thread's `lua_setthreaddata` slot (the host's own
/// thread data lives inside it, see [`crate::thread::Thread::set_data`]). Created by the
/// runtime's `userthread` callback for every thread of the VM and for the main thread at build.
pub(crate) struct ThreadRecord {
    /// Live `Stack` handles on this thread (root and native-call).
    alive_stacks: Cell<u32>,
    /// Host-to-Lua transitions in progress on this thread (`LuaCall` guards).
    lua_calls: Cell<u32>,
    /// The host's thread data.
    pub(crate) host_data: Cell<*mut c_void>,
}

impl ThreadRecord {
    fn new() -> ThreadRecord {
        ThreadRecord { alive_stacks: Cell::new(0), lua_calls: Cell::new(0), host_data: Cell::new(std::ptr::null_mut()) }
    }

    /// Registers a live `Stack` on this thread. A root stack is admitted only when every stack
    /// alive on the thread is suspended inside a Lua call (one call per stack): then no frame of
    /// theirs can be touched until the call returns, so a fresh root cannot alias them.
    /// Native-call stacks arise only from Luau calling into Rust, which implies the same.
    pub(crate) fn register_stack(&self, root: bool) {
        if root {
            assert!(
                self.alive_stacks.get() == self.lua_calls.get(),
                "a root stack for this Lua thread is already alive; open frames from it instead"
            );
        }
        self.alive_stacks.set(self.alive_stacks.get() + 1);
    }

    pub(crate) fn unregister_stack(&self) {
        self.alive_stacks.set(self.alive_stacks.get() - 1);
    }
}

/// The record of `state`'s thread, or `None` for a thread no `Runtime` manages.
///
/// # Safety
/// `state` is a live thread.
pub(crate) unsafe fn thread_record<'a>(state: *mut ffi::lua_State) -> Option<&'a ThreadRecord> {
    // SAFETY: the slot holds a record installed by `attach_thread_record` for the thread's life.
    unsafe { ffi::lua_getthreaddata(state).cast_const().cast::<ThreadRecord>().as_ref() }
}

/// Gives `state` a fresh record. Called from the `userthread` callback and for the main thread.
///
/// # Safety
/// `state` is a live thread with no record yet.
pub(crate) unsafe fn attach_thread_record(state: *mut ffi::lua_State) {
    let record = Box::into_raw(Box::new(ThreadRecord::new()));
    unsafe { ffi::lua_setthreaddata(state, record.cast()) };
}

/// Frees `state`'s record, if any. Called when Luau destroys the thread and for the main thread
/// after `lua_close`.
///
/// # Safety
/// `state` is the thread being destroyed; nothing uses its record afterwards.
pub(crate) unsafe fn detach_thread_record(state: *mut ffi::lua_State) {
    unsafe {
        let record = ffi::lua_getthreaddata(state).cast::<ThreadRecord>();
        if !record.is_null() {
            ffi::lua_setthreaddata(state, std::ptr::null_mut());
            drop(Box::from_raw(record));
        }
    }
}

/// Marks one host-to-Lua transition (`lua_call`/`lua_pcall`/`lua_resume`) on `state`'s thread
/// for as long as the guard lives. A no-op on threads no `Runtime` manages.
pub(crate) struct LuaCall(*const ThreadRecord);

impl LuaCall {
    /// # Safety
    /// `state` is a live thread.
    pub(crate) unsafe fn enter(state: *mut ffi::lua_State) -> LuaCall {
        // SAFETY: forwarded contract.
        let record = unsafe { thread_record(state) };
        if let Some(record) = record {
            record.lua_calls.set(record.lua_calls.get() + 1);
        }
        LuaCall(record.map_or(std::ptr::null(), |record| record as *const ThreadRecord))
    }
}

impl Drop for LuaCall {
    fn drop(&mut self) {
        // SAFETY: null or a record that outlives the guarded call on its own thread.
        if let Some(record) = unsafe { self.0.as_ref() } {
            record.lua_calls.set(record.lua_calls.get() - 1);
        }
    }
}

/// The `Shared` block of the VM that owns `state`, or null when the VM was not created by a
/// [`crate::Runtime`].
///
/// # Safety
/// `state` is a live thread.
pub(crate) unsafe fn shared_of(state: *mut ffi::lua_State) -> *const Shared {
    unsafe { (*ffi::lua_callbacks(state)).userdata.cast_const().cast::<Shared>() }
}

/// The interrupt hook (`State::interruptHook`): runs at VM safepoints (`gc == -1`) and during
/// GC (any other value, ignored so collection never charges a script's budget). Every 64th
/// safepoint it polls the deadline and the memory limit and raises when either is exceeded.
pub(crate) unsafe extern "C-unwind" fn interrupt(state: *mut ffi::lua_State, gc: c_int) {
    if gc != -1 {
        return;
    }
    // SAFETY: userdata is the Shared block installed at build time and outlives the VM.
    let shared = unsafe { shared_of(state) };
    if shared.is_null() {
        return;
    }
    let shared = unsafe { &*shared };
    let innermost = shared.active_calls.borrow().last().map(|call| call.context);
    let Some(innermost) = innermost else { return };

    if let Some(sampled) = shared.sampled_context.get() {
        let remaining = shared.safepoints_until_sample.get();
        if remaining != 0 {
            shared.safepoints_until_sample.set(remaining - 1);
        } else {
            shared.safepoints_until_sample.set(SAFEPOINTS_PER_SAMPLE - 1);
            if innermost == sampled {
                // SAFETY: the interrupt runs at a safepoint of `state`; lua_getinfo only reads.
                unsafe { sample(state, &mut shared.samples.borrow_mut()) };
            }
        }
    }

    let remaining = shared.safepoints_until_poll.get();
    if remaining != 0 {
        shared.safepoints_until_poll.set(remaining - 1);
        return;
    }
    shared.safepoints_until_poll.set(SAFEPOINTS_PER_WATCHDOG_POLL - 1);

    if let Some(deadline) = shared.deadline.get() {
        let now = Instant::now();
        if now >= deadline.end {
            let elapsed = now.duration_since(deadline.start).as_millis();
            let message = format!("Lua execution time limit exceeded after {elapsed} ms");
            // SAFETY: raising from the interrupt is how Luau's watchdog contract works; no
            // Rust value that must drop is live here once `message` is pushed.
            unsafe {
                ffi::lua_pushlstring(state, message.as_ptr().cast(), message.len());
                drop(message);
                ffi::lua_error(state);
            }
        }
    }
    let memory_limit = shared.limits.get().memory_bytes;
    // SAFETY: lua_totalbytes(-1) is a plain read.
    if memory_limit != 0 && unsafe { ffi::lua_totalbytes(state, -1) } > memory_limit {
        unsafe {
            ffi::lua_pushlstring(state, c"Lua memory limit exceeded".as_ptr(), 25);
            ffi::lua_error(state);
        }
    }
}

/// Allocation activity (`State::onAllocate`): growth is charged to the innermost active call.
/// Luau does not report every fixed GC allocation here, so this is activity, not ownership.
pub(crate) unsafe extern "C" fn on_allocate(
    state: *mut ffi::lua_State,
    _block: *mut c_void,
    old_size: usize,
    new_size: usize,
    _category: u8,
    _tt: c_int,
    _tag: c_int,
) {
    if new_size <= old_size {
        return;
    }
    let shared = unsafe { shared_of(state) };
    if shared.is_null() {
        return;
    }
    let shared = unsafe { &*shared };
    if let Ok(mut calls) = shared.active_calls.try_borrow_mut()
        && let Some(innermost) = calls.last_mut()
    {
        innermost.allocated_bytes += (new_size - old_size) as u64;
    }
}

/// One Lua frame's `sln` debug info, or `None` for native frames.
struct LuaFrameInfo {
    source: String,
    name: String,
    current_line: c_int,
    line_defined: c_int,
}

/// Reads level `level` of `state`'s call stack; `Some(None)` is a native frame, `None` is the
/// end of the stack.
///
/// # Safety
/// `state` is live; called from the host or from a safepoint of `state`.
unsafe fn lua_frame_info(state: *mut ffi::lua_State, level: c_int, what: &CStr) -> Option<Option<LuaFrameInfo>> {
    let mut ar = std::mem::MaybeUninit::<ffi::lua_Debug>::zeroed();
    // SAFETY: lua_getinfo fills the requested fields; pointers reference ar.ssbuf or interned
    // strings that outlive the read.
    unsafe {
        if ffi::lua_getinfo(state, level, what.as_ptr(), ar.as_mut_ptr()) == 0 {
            return None;
        }
        let ar = ar.assume_init_ref();
        if ar.what.is_null() || CStr::from_ptr(ar.what).to_bytes() != b"Lua" {
            return Some(None);
        }
        let source = if ar.source.is_null() { ar.short_src } else { ar.source };
        let mut source = CStr::from_ptr(source).to_string_lossy().into_owned();
        if source.starts_with('@') || source.starts_with('=') {
            source.remove(0);
        }
        let name =
            if ar.name.is_null() { String::new() } else { CStr::from_ptr(ar.name).to_string_lossy().into_owned() };
        Some(Some(LuaFrameInfo { source, name, current_line: ar.currentline, line_defined: ar.linedefined }))
    }
}

/// Takes one sample of `state` (`State::sample`): the innermost Lua frame counts for its line,
/// every Lua function on the stack counts once for its definition.
///
/// # Safety
/// `state` is live and at a safepoint.
unsafe fn sample(state: *mut ffi::lua_State, samples: &mut Samples) {
    let mut innermost = true;
    let mut counted: Vec<String> = Vec::new();
    for level in 0..SAMPLE_MAX_DEPTH {
        let Some(frame) = (unsafe { lua_frame_info(state, level, c"sln") }) else { break };
        let Some(frame) = frame else { continue };
        if innermost {
            let line = samples.lines.entry(format!("{}:{}", frame.source, frame.current_line)).or_default();
            if line.function.is_empty() {
                line.function.clone_from(&frame.name);
            }
            line.samples += 1;
            innermost = false;
        }
        let key = format!("{}:{}", frame.source, frame.line_defined);
        if counted.contains(&key) {
            continue;
        }
        let function = samples.functions.entry(key.clone()).or_default();
        if function.function.is_empty() {
            function.function = frame.name;
        }
        function.samples += 1;
        counted.push(key);
    }
    if !innermost {
        samples.count += 1;
    }
}

/// `source:line` of the innermost Lua frame on `state`, or empty.
pub(crate) fn caller_location(state: *mut ffi::lua_State) -> String {
    let mut level = 0;
    // SAFETY: the host or a bound function calls this on a live thread.
    while let Some(frame) = unsafe { lua_frame_info(state, level, c"sl") } {
        if let Some(frame) = frame {
            return format!("{}:{}", frame.source, frame.current_line);
        }
        level += 1;
    }
    String::new()
}
