//! Luau's debug API: activation records, locals, upvalues, arguments, tracebacks, single
//! stepping, breakpoints, coverage, and the VM callbacks a debugger or host needs.
//!
//! Everything here reads or drives the VM from host code or from a bound function. Hooks
//! ([`RuntimeHooks`]) run inside Luau callbacks: they must not panic (panics abort), and the
//! ones Luau documents as non-reentrant (`user_thread`, `user_finalizer`, `on_free`,
//! `pre_resume`/`post_resume`) do no Lua API work at all.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_int, c_void};
use std::mem::MaybeUninit;

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::{Scope, Stack, ValueView};

/// One activation record (`lua_getinfo` with `"slnua"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugInfo {
    /// `"Lua"` or `"C"`.
    pub what: String,
    /// The chunk name as given to `load`, without the leading `=` or `@`.
    pub source: String,
    /// Luau's printable form of the chunk name.
    pub short_source: String,
    /// The function's name where Luau knows it.
    pub name: Option<String>,
    pub line_defined: i32,
    /// The line being executed for a running function, `-1` when unknown.
    pub current_line: i32,
    pub upvalue_count: u8,
    pub parameter_count: u8,
    pub is_vararg: bool,
}

/// Hit counts of one function from `lua_getcoverage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageEntry {
    pub function: Option<String>,
    pub line_defined: i32,
    /// Nesting depth below the queried function.
    pub depth: i32,
    /// Hits per line; `-1` marks lines without executable code.
    pub hits: Vec<i32>,
}

unsafe fn text(pointer: *const std::ffi::c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: Luau's debug strings are NUL-terminated and outlive the read.
    Some(unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned())
}

fn info_from(ar: &ffi::lua_Debug) -> DebugInfo {
    // SAFETY: the fields were filled by lua_getinfo for the requested options.
    unsafe {
        let mut source = text(ar.source).unwrap_or_default();
        if source.starts_with('@') || source.starts_with('=') {
            source.remove(0);
        }
        DebugInfo {
            what: text(ar.what).unwrap_or_default(),
            source,
            short_source: text(ar.short_src).unwrap_or_default(),
            name: text(ar.name).filter(|name| !name.is_empty()),
            line_defined: ar.linedefined,
            current_line: ar.currentline,
            upvalue_count: ar.nupvals,
            parameter_count: ar.nparams,
            is_vararg: ar.isvararg != 0,
        }
    }
}

/// Debug queries over the call stack of a scope's thread.
pub trait DebugScope: Scope + Sized {
    /// The activation record `level` frames up the call stack (0 is the running function), or
    /// `None` past the bottom.
    fn debug_info(&self, level: c_int) -> Option<DebugInfo> {
        let mut ar = MaybeUninit::<ffi::lua_Debug>::zeroed();
        // SAFETY: lua_getinfo with a level never touches the stack.
        unsafe {
            if ffi::lua_getinfo(self.state(), level, c"slnua".as_ptr(), ar.as_mut_ptr()) == 0 {
                return None;
            }
            Some(info_from(ar.assume_init_ref()))
        }
    }

    /// The activation record of the function at stack slot `function`.
    fn function_info(&self, function: ValueView<'_>) -> Result<DebugInfo> {
        if !function.is_function() {
            return Err(Error::logic("Debug info needs a function"));
        }
        let mut ar = MaybeUninit::<ffi::lua_Debug>::zeroed();
        // SAFETY: a negative level is a relative stack index for lua_getinfo; `f` is not
        // requested, so the stack is untouched.
        unsafe {
            let relative = function.index() - ffi::lua_gettop(self.state()) - 1;
            if ffi::lua_getinfo(self.state(), relative, c"slnua".as_ptr(), ar.as_mut_ptr()) == 0 {
                return Err(Error::logic("Debug info is unavailable for this function"));
            }
            Ok(info_from(ar.assume_init_ref()))
        }
    }

    /// Number of Lua and C frames on this thread.
    fn stack_depth(&self) -> c_int {
        unsafe { ffi::lua_stackdepth(self.state()) }
    }

    /// Luau's own multi-line trace of this thread's call stack.
    fn debug_trace(&self) -> String {
        // SAFETY: lua_debugtrace returns a pointer into a per-thread buffer, copied at once.
        unsafe { text(ffi::lua_debugtrace(self.state())).unwrap_or_default() }
    }

    /// `luaL_traceback` for this thread starting at `level`, with an optional leading message.
    fn traceback(&self, message: Option<&str>, level: c_int) -> Result<String> {
        let message =
            message.map(CString::new).transpose().map_err(|_| Error::logic("Traceback message contains NUL"))?;
        self.with_frame(|frame| {
            // SAFETY: luaL_traceback pushes one string; the frame pops it.
            unsafe {
                ffi::luaL_traceback(
                    frame.state(),
                    frame.state(),
                    message.as_ref().map_or(std::ptr::null(), |m| m.as_ptr()),
                    level,
                );
            }
            frame.top_value().read::<String>()
        })
    }

    /// Pushes local `n` (1-based) of the function `level` frames up and returns its name with
    /// the view, or `None` when there is no such local.
    fn local<'s>(&'s self, level: c_int, n: c_int) -> Option<(String, ValueView<'s>)> {
        // SAFETY: lua_getlocal pushes the value when it returns a name.
        let name = unsafe { text(ffi::lua_getlocal(self.state(), level, n)) }?;
        Some((name, self.top_value()))
    }

    /// Pops the top value into local `n` of the function `level` frames up; returns the local's
    /// name, or `None` (value still popped) when there is no such local.
    fn set_local(&self, level: c_int, n: c_int) -> Option<String> {
        unsafe { text(ffi::lua_setlocal(self.state(), level, n)) }
    }

    /// Pushes argument `n` of the function `level` frames up (vararg-aware); `None` when absent.
    fn argument<'s>(&'s self, level: c_int, n: c_int) -> Option<ValueView<'s>> {
        // SAFETY: lua_getargument pushes the argument when it returns nonzero.
        if unsafe { ffi::lua_getargument(self.state(), level, n) } == 0 {
            return None;
        }
        Some(self.top_value())
    }

    /// Pushes upvalue `n` of the function at `function` and returns its name with the view.
    fn upvalue<'s>(&'s self, function: ValueView<'_>, n: c_int) -> Option<(String, ValueView<'s>)> {
        let name = unsafe { text(ffi::lua_getupvalue(self.state(), function.index(), n)) }?;
        Some((name, self.top_value()))
    }

    /// Pops the top value into upvalue `n` of the function at `function`; returns its name.
    fn set_upvalue(&self, function: ValueView<'_>, n: c_int) -> Option<String> {
        unsafe { text(ffi::lua_setupvalue(self.state(), function.index(), n)) }
    }

    /// Enables or disables single stepping on this thread; each instruction then reaches
    /// [`RuntimeHooks::debug_step`].
    fn single_step(&self, enabled: bool) {
        unsafe { ffi::lua_singlestep(self.state(), c_int::from(enabled)) }
    }

    /// Sets or clears a breakpoint on `line` of the Lua function at `function`. Returns the
    /// line the breakpoint landed on (the next line with code), or an error when none exists.
    fn set_breakpoint(&self, function: ValueView<'_>, line: c_int, enabled: bool) -> Result<c_int> {
        if !function.is_function() {
            return Err(Error::logic("Breakpoints need a Lua function"));
        }
        // SAFETY: lua_breakpoint only reads and patches the function's prototype.
        let landed = unsafe { ffi::lua_breakpoint(self.state(), function.index(), line, c_int::from(enabled)) };
        if landed < 0 {
            return Err(Error::logic(format!("No executable line at or after {line}")));
        }
        Ok(landed)
    }

    /// Line hit counts of the Lua function at `function` and its nested functions
    /// (`lua_getcoverage`; the VM must be built with coverage enabled for hits to be counted).
    fn coverage(&self, function: ValueView<'_>) -> Result<Vec<CoverageEntry>> {
        if !function.is_function() {
            return Err(Error::logic("Coverage needs a Lua function"));
        }
        let mut entries: Vec<CoverageEntry> = Vec::new();
        // SAFETY: the callback receives our Vec as context and copies the hit arrays.
        unsafe {
            ffi::lua_getcoverage(
                self.state(),
                function.index(),
                (&mut entries as *mut Vec<CoverageEntry>).cast(),
                collect_coverage,
            );
        }
        Ok(entries)
    }
}

impl<S: Scope> DebugScope for S {}

unsafe extern "C" fn collect_coverage(
    context: *mut c_void,
    function: *const std::ffi::c_char,
    linedefined: c_int,
    depth: c_int,
    hits: *const c_int,
    size: usize,
) {
    // SAFETY: `context` is the Vec passed by `coverage`; Luau gives `size` ints at `hits`.
    unsafe {
        let entries = &mut *context.cast::<Vec<CoverageEntry>>();
        let hits = if hits.is_null() { Vec::new() } else { std::slice::from_raw_parts(hits, size).to_vec() };
        entries.push(CoverageEntry { function: text(function), line_defined: linedefined, depth, hits });
    }
}

/// What a debugger hook wants the VM to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugAction {
    Continue,
    /// Stop the thread with `LUA_BREAK` (`lua_break`); the host resumes it later. Only possible
    /// on a thread the host drives through [`crate::thread::Thread`]; on the main thread Luau
    /// raises "attempt to break across metamethod/C-call boundary".
    Break,
}

/// Host callbacks for the remaining `lua_Callbacks` slots. Every method has a no-op default;
/// [`HookSet`] selects which slots are installed so the VM pays only for the ones in use.
/// Hooks that receive a [`Stack`] run at a point where the Lua API may be used on that thread;
/// the ones that receive raw states may not touch the Lua API (Luau's documented restriction).
pub trait RuntimeHooks: 'static {
    /// An unprotected error was raised (only reachable with `LUA_USE_LONGJMP` builds).
    fn panic(&self, error_code: c_int) {
        let _ = error_code;
    }
    /// A thread was created (`parent` is `Some`) or is being destroyed (`None`). No Lua API.
    fn user_thread(&self, parent: Option<*mut ffi::lua_State>, thread: *mut ffi::lua_State) {
        let _ = (parent, thread);
    }
    /// A finalizer is about to be attached to `coroutine` by the current thread. No Lua API.
    fn user_finalizer(&self, thread: *mut ffi::lua_State, coroutine: *mut ffi::lua_State) {
        let _ = (thread, coroutine);
    }
    /// A `BREAK` instruction (breakpoint) was reached; `info` describes the function. Return
    /// [`DebugAction::Break`] to stop the thread there. Resuming re-executes the instruction, so
    /// the hook is called again for the same breakpoint and must answer `Continue` to step off
    /// it (Luau's own tests break on odd hits and continue on even ones).
    fn debug_break(&self, stack: &Stack<'_>, info: &DebugInfo) -> DebugAction {
        let _ = (stack, info);
        DebugAction::Continue
    }
    /// One instruction executed in single-step mode.
    fn debug_step(&self, stack: &Stack<'_>, info: &DebugInfo) -> DebugAction {
        let _ = (stack, info);
        DebugAction::Continue
    }
    /// This thread's execution was interrupted by a break in `interrupted`, a coroutine it
    /// resumed; resume that thread to continue.
    fn debug_interrupt(&self, stack: &Stack<'_>, info: &DebugInfo, interrupted: *mut ffi::lua_State) -> DebugAction {
        let _ = (stack, info, interrupted);
        DebugAction::Continue
    }
    /// A protected call is about to unwind with an error; the error object is on top.
    fn debug_protected_error(&self, stack: &Stack<'_>) {
        let _ = stack;
    }
    /// `lua_resume` is about to run a coroutine. No Lua API.
    fn pre_resume(&self, thread: *mut ffi::lua_State) {
        let _ = thread;
    }
    /// `lua_resume` returned (yield, return, or error). No Lua API.
    fn post_resume(&self, thread: *mut ffi::lua_State) {
        let _ = thread;
    }
    /// A block is being freed. No Lua API.
    fn on_free(&self, thread: *mut ffi::lua_State, block: *mut c_void) {
        let _ = (thread, block);
    }
}

/// Which [`RuntimeHooks`] slots to install.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookSet {
    pub panic: bool,
    pub user_thread: bool,
    pub user_finalizer: bool,
    pub debug_break: bool,
    pub debug_step: bool,
    pub debug_interrupt: bool,
    pub debug_protected_error: bool,
    pub pre_resume: bool,
    pub post_resume: bool,
    pub on_free: bool,
}

impl HookSet {
    /// The debugger slots: break, step, interrupt, protected error.
    pub const DEBUGGER: HookSet = HookSet {
        debug_break: true,
        debug_step: true,
        debug_interrupt: true,
        debug_protected_error: true,
        ..HookSet::NONE
    };
    pub const NONE: HookSet = HookSet {
        panic: false,
        user_thread: false,
        user_finalizer: false,
        debug_break: false,
        debug_step: false,
        debug_interrupt: false,
        debug_protected_error: false,
        pre_resume: false,
        post_resume: false,
        on_free: false,
    };
    pub const ALL: HookSet = HookSet {
        panic: true,
        user_thread: true,
        user_finalizer: true,
        debug_break: true,
        debug_step: true,
        debug_interrupt: true,
        debug_protected_error: true,
        pre_resume: true,
        post_resume: true,
        on_free: true,
    };
}

/// The installed hooks, kept in the runtime's shared block.
pub(crate) struct HookSlot {
    pub(crate) hooks: RefCell<Option<std::rc::Rc<dyn RuntimeHooks>>>,
}

impl HookSlot {
    pub(crate) fn new() -> HookSlot {
        HookSlot { hooks: RefCell::new(None) }
    }
}

/// Runs `body` with the installed hooks, if any, aborting on panic (we are inside a callback).
unsafe fn with_hooks(state: *mut ffi::lua_State, body: impl FnOnce(&dyn RuntimeHooks)) {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    // SAFETY: the callback's thread is live; Shared outlives every thread of its VM.
    let Some(shared) = (unsafe { crate::runtime::shared_for(state) }) else { return };
    if let Ok(slot) = shared.hooks().hooks.try_borrow()
        && let Some(hooks) = slot.as_ref()
    {
        body(hooks.as_ref());
    }
}

unsafe extern "C-unwind" fn on_panic(_: *mut ffi::lua_State, code: c_int) {
    // No Lua API: the state is mid-unwind.
    let _guard = crate::raw::trampoline::AbortOnPanic;
    HOOKS_FOR_PANIC.with(|slot| {
        if let Some(hooks) = slot.borrow().as_ref() {
            hooks.panic(code);
        }
    });
}

thread_local! {
    /// The panic hook cannot look the VM up (the state is unusable by then), so it is kept here
    /// per thread; one VM per thread is the norm and the last installed wins otherwise.
    static HOOKS_FOR_PANIC: RefCell<Option<std::rc::Rc<dyn RuntimeHooks>>> = const { RefCell::new(None) };
}

unsafe extern "C" fn on_user_thread(parent: *mut ffi::lua_State, thread: *mut ffi::lua_State) {
    // The thread being destroyed may be the one we look Shared up through; use whichever is live.
    let live = if parent.is_null() { thread } else { parent };
    unsafe { with_hooks(live, |hooks| hooks.user_thread((!parent.is_null()).then_some(parent), thread)) }
}

unsafe extern "C" fn on_user_finalizer(thread: *mut ffi::lua_State, coroutine: *mut ffi::lua_State) {
    unsafe { with_hooks(thread, |hooks| hooks.user_finalizer(thread, coroutine)) }
}

unsafe fn debug_event(
    state: *mut ffi::lua_State,
    ar: *mut ffi::lua_Debug,
    event: fn(&dyn RuntimeHooks, &Stack<'_>, &DebugInfo, *mut c_void) -> DebugAction,
) {
    let mut action = DebugAction::Continue;
    unsafe {
        with_hooks(state, |hooks| {
            let mut record = MaybeUninit::<ffi::lua_Debug>::zeroed();
            // Luau's record carries the current line; the rest is read from level 0.
            let mut info = if ffi::lua_getinfo(state, 0, c"slnua".as_ptr(), record.as_mut_ptr()) != 0 {
                info_from(record.assume_init_ref())
            } else {
                DebugInfo {
                    what: String::new(),
                    source: String::new(),
                    short_source: String::new(),
                    name: None,
                    line_defined: -1,
                    current_line: -1,
                    upvalue_count: 0,
                    parameter_count: 0,
                    is_vararg: false,
                }
            };
            if !ar.is_null() {
                info.current_line = (*ar).currentline;
            }
            let userdata = if ar.is_null() { std::ptr::null_mut() } else { (*ar).userdata };
            // SAFETY: the hook runs on the thread Luau passed, as a native callback would.
            let stack = Stack::from_raw(state, false);
            action = event(hooks, &stack, &info, userdata);
        });
        if action == DebugAction::Break {
            ffi::lua_break(state);
        }
    }
}

unsafe extern "C-unwind" fn on_debug_break(state: *mut ffi::lua_State, ar: *mut ffi::lua_Debug) {
    unsafe { debug_event(state, ar, |hooks, stack, info, _| hooks.debug_break(stack, info)) }
}

unsafe extern "C-unwind" fn on_debug_step(state: *mut ffi::lua_State, ar: *mut ffi::lua_Debug) {
    unsafe { debug_event(state, ar, |hooks, stack, info, _| hooks.debug_step(stack, info)) }
}

unsafe extern "C-unwind" fn on_debug_interrupt(state: *mut ffi::lua_State, ar: *mut ffi::lua_Debug) {
    unsafe {
        debug_event(state, ar, |hooks, stack, info, interrupted| hooks.debug_interrupt(stack, info, interrupted.cast()))
    }
}

unsafe extern "C-unwind" fn on_debug_protected_error(state: *mut ffi::lua_State) {
    unsafe {
        with_hooks(state, |hooks| {
            let stack = Stack::from_raw(state, false);
            hooks.debug_protected_error(&stack);
        })
    }
}

unsafe extern "C" fn on_pre_resume(state: *mut ffi::lua_State) {
    unsafe { with_hooks(state, |hooks| hooks.pre_resume(state)) }
}

unsafe extern "C" fn on_post_resume(state: *mut ffi::lua_State) {
    unsafe { with_hooks(state, |hooks| hooks.post_resume(state)) }
}

unsafe extern "C" fn on_free(state: *mut ffi::lua_State, block: *mut c_void) {
    unsafe { with_hooks(state, |hooks| hooks.on_free(state, block)) }
}

impl Runtime {
    /// Installs `hooks` for the slots in `set`, replacing any previous hooks. Slots outside
    /// `set` are cleared.
    pub fn set_hooks(&self, hooks: impl RuntimeHooks, set: HookSet) {
        let hooks: std::rc::Rc<dyn RuntimeHooks> = std::rc::Rc::new(hooks);
        *self.shared().hooks().hooks.borrow_mut() = Some(std::rc::Rc::clone(&hooks));
        if set.panic {
            HOOKS_FOR_PANIC.with(|slot| *slot.borrow_mut() = Some(hooks));
        }
        self.install_hook_slots(set);
    }

    /// Removes the hooks and clears every slot they used.
    pub fn clear_hooks(&self) {
        self.install_hook_slots(HookSet::NONE);
        self.shared().hooks().hooks.borrow_mut().take();
        HOOKS_FOR_PANIC.with(|slot| slot.borrow_mut().take());
    }

    fn install_hook_slots(&self, set: HookSet) {
        // SAFETY: the callback block belongs to this VM; slots the binder itself uses
        // (interrupt, useratom, onallocate) are left alone.
        unsafe {
            let callbacks = ffi::lua_callbacks(self.stack().state_ptr());
            (*callbacks).panic = set.panic.then_some(on_panic as _);
            (*callbacks).userthread = set.user_thread.then_some(on_user_thread as _);
            (*callbacks).userfinalizer = set.user_finalizer.then_some(on_user_finalizer as _);
            (*callbacks).debugbreak = set.debug_break.then_some(on_debug_break as _);
            (*callbacks).debugstep = set.debug_step.then_some(on_debug_step as _);
            (*callbacks).debuginterrupt = set.debug_interrupt.then_some(on_debug_interrupt as _);
            (*callbacks).debugprotectederror = set.debug_protected_error.then_some(on_debug_protected_error as _);
            (*callbacks).preresume = set.pre_resume.then_some(on_pre_resume as _);
            (*callbacks).postresume = set.post_resume.then_some(on_post_resume as _);
            (*callbacks).onfree = set.on_free.then_some(on_free as _);
        }
    }
}
