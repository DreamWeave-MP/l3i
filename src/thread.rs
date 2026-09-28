//! Coroutines: Lua threads driven from the host (`lua_newthread`, `lua_resume`, `lua_yield`,
//! `lua_break`, `lua_resetthread`, `lua_costatus`, thread data, `luaL_sandboxthread`).
//!
//! A [`Thread`] is a pinned Lua thread. The host starts it with a function and arguments, then
//! resumes it with the values the coroutine's `yield` receives. Each step reports what the
//! coroutine did ([`Resume`]); an error inside it comes back as `Err` and leaves the thread in
//! its error status, as `coroutine.resume` would report. Bound functions yield by returning
//! [`crate::bind::Yield`] and request a debugger break by returning [`crate::bind::Break`].

use std::ffi::{c_int, c_void};

use crate::call::PushArgs;
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::runtime::shared::LuaCall;
use crate::stack::{Scope, Stack, same_vm};
use crate::value::{Function, Value};

/// What a resume step did.
#[derive(Debug)]
pub enum Resume {
    /// The coroutine yielded these values and can be resumed.
    Yielded(Vec<Value>),
    /// The coroutine's function returned these values; the thread is finished.
    Finished(Vec<Value>),
    /// The coroutine hit a `lua_break` (a breakpoint or a bound function returning `Break`).
    Break,
}

/// `lua_status` of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Not started, or finished normally.
    Ok,
    Yielded,
    Break,
    /// Died with a runtime, syntax, memory, or error-handler error (`LUA_ERR*` code).
    Error(c_int),
}

/// `lua_costatus` of a thread as seen from another thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoroutineStatus {
    Running,
    Suspended,
    /// Active but not running: it resumed another coroutine.
    Normal,
    Finished,
    FinishedWithError,
}

/// A pinned Lua thread.
pub struct Thread {
    pin: Value,
    state: *mut ffi::lua_State,
}

impl Runtime {
    /// A new thread sharing this VM's globals and registry.
    pub fn new_thread(&self) -> Result<Thread> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            // SAFETY: lua_newthread pushes the thread; the frame pops it after pinning.
            let state = unsafe { ffi::lua_newthread(frame.state()) };
            if state.is_null() {
                return Err(Error::runtime("Unable to allocate a Lua thread"));
            }
            Ok(Thread { pin: Value::store(frame.top_value())?, state })
        })
    }
}

impl Thread {
    /// The thread pinned as a value (a `thread`-typed Lua value).
    pub fn value(&self) -> &Value {
        &self.pin
    }

    pub(crate) fn state_ptr(&self) -> *mut ffi::lua_State {
        self.state
    }

    fn require_live(&self, scope: &impl Scope) -> Result<()> {
        if !self.pin.is_valid() {
            return Err(Error::logic("Cannot use a Lua thread whose runtime has closed"));
        }
        if !same_vm(self.state, scope.state()) {
            return Err(Error::logic("Lua thread belongs to a different VM"));
        }
        Ok(())
    }

    pub fn status(&self) -> ThreadStatus {
        if !self.pin.is_valid() {
            return ThreadStatus::Error(ffi::LUA_ERRRUN);
        }
        // SAFETY: the pin proves the VM is open and the thread reachable.
        match unsafe { ffi::lua_status(self.state) } {
            ffi::LUA_OK => ThreadStatus::Ok,
            ffi::LUA_YIELD => ThreadStatus::Yielded,
            ffi::LUA_BREAK => ThreadStatus::Break,
            code => ThreadStatus::Error(code),
        }
    }

    /// The coroutine status of this thread relative to `scope`'s thread.
    pub fn coroutine_status(&self, scope: &impl Scope) -> Result<CoroutineStatus> {
        self.require_live(scope)?;
        // SAFETY: both threads are live and in one VM.
        Ok(match unsafe { ffi::lua_costatus(scope.state(), self.state) } {
            ffi::LUA_CORUN => CoroutineStatus::Running,
            ffi::LUA_COSUS => CoroutineStatus::Suspended,
            ffi::LUA_CONOR => CoroutineStatus::Normal,
            ffi::LUA_COFIN => CoroutineStatus::Finished,
            _ => CoroutineStatus::FinishedWithError,
        })
    }

    /// Starts the coroutine: `function` runs on this thread with `args` until it yields,
    /// returns, breaks, or fails. The thread must be idle: fresh, finished normally, or reset.
    pub fn start<A: PushArgs>(&self, scope: &impl Scope, function: &Function, args: A) -> Result<Resume> {
        self.require_live(scope)?;
        if !function.value().belongs_to(scope.state()) {
            return Err(Error::logic("Coroutine function belongs to a different VM"));
        }
        // SAFETY: the thread is live; lua_gettop and lua_status only read. The arguments go on
        // the coroutine's own stack through a root lease on that thread, which refuses to
        // coexist with a live `with_stack` on it.
        unsafe {
            if ffi::lua_status(self.state) != ffi::LUA_OK || ffi::lua_gettop(self.state) != 0 {
                return Err(Error::logic("Lua thread is already running or holds values; reset it before starting"));
            }
            let own = Stack::lease_root(self.state);
            ffi::lua_getref(self.state, function.value().reference_id());
            args.push_all(&own)?;
            drop(own);
        }
        self.step(scope, A::COUNT)
    }

    /// Resumes a yielded coroutine with `args` as the results of its `yield`.
    pub fn resume<A: PushArgs>(&self, scope: &impl Scope, args: A) -> Result<Resume> {
        self.require_live(scope)?;
        if self.status() != ThreadStatus::Yielded && self.status() != ThreadStatus::Break {
            return Err(Error::logic("Lua thread is not suspended in a yield or break"));
        }
        // SAFETY: arguments go on the suspended thread's own stack, as Luau expects.
        unsafe {
            let own = Stack::lease_root(self.state);
            args.push_all(&own)?;
        }
        self.step(scope, A::COUNT)
    }

    /// Resumes a yielded coroutine by raising `message` inside it, as if its `yield` failed.
    pub fn resume_with_error(&self, scope: &impl Scope, message: &str) -> Result<Resume> {
        self.require_live(scope)?;
        if self.status() != ThreadStatus::Yielded {
            return Err(Error::logic("Lua thread is not suspended in a yield"));
        }
        // SAFETY: the error object goes on the coroutine's stack before lua_resumeerror.
        let status = unsafe {
            ffi::lua_pushlstring(self.state, message.as_ptr().cast(), message.len());
            let _call = LuaCall::enter(scope.state());
            ffi::lua_resumeerror(self.state, scope.state())
        };
        self.collect(status)
    }

    fn step(&self, scope: &impl Scope, nargs: c_int) -> Result<Resume> {
        // SAFETY: the function and arguments are on the coroutine's stack; `from` is a live
        // thread of the same VM.
        let status = unsafe {
            let _call = LuaCall::enter(scope.state());
            ffi::lua_resume(self.state, scope.state(), nargs)
        };
        self.collect(status)
    }

    fn collect(&self, status: c_int) -> Result<Resume> {
        // SAFETY: after lua_resume the coroutine's stack holds its yielded/returned values or
        // the error object; everything is pinned or read, then popped.
        unsafe {
            match status {
                ffi::LUA_OK | ffi::LUA_YIELD => {
                    let count = ffi::lua_gettop(self.state);
                    let own = Stack::lease_root(self.state);
                    let mut values = Vec::with_capacity(count as usize);
                    for index in 1..=count {
                        values.push(Value::store(own.at(index))?);
                    }
                    drop(own);
                    ffi::lua_settop(self.state, 0);
                    Ok(if status == ffi::LUA_OK { Resume::Finished(values) } else { Resume::Yielded(values) })
                }
                ffi::LUA_BREAK => Ok(Resume::Break),
                error => {
                    let message = crate::raw::protect::pop_error(self.state, error);
                    Err(Error::runtime(format!("Lua error: {}", message_text(&message))))
                }
            }
        }
    }

    /// Returns a finished or failed thread to the fresh state so it can be started again.
    pub fn reset(&self) -> Result<()> {
        if !self.pin.is_valid() {
            return Err(Error::logic("Cannot reset a Lua thread whose runtime has closed"));
        }
        // SAFETY: lua_resetthread is permitted on a thread that is not currently running.
        unsafe { ffi::lua_resetthread(self.state) };
        Ok(())
    }

    /// True when the thread has been reset (or is fresh) and holds no function.
    pub fn is_reset(&self) -> bool {
        self.pin.is_valid() && unsafe { ffi::lua_isthreadreset(self.state) != 0 }
    }

    /// Runs `body` with the root stack of this thread, for pushing arguments or reading values
    /// between resumes. Never call into Lua on a suspended thread through it.
    ///
    /// # Panics
    /// If a root stack on this thread is already alive (a nested `with_stack`, or a `start` or
    /// `resume` in progress): the lease is per Lua thread.
    pub fn with_stack<R>(&self, scope: &impl Scope, body: impl FnOnce(&Stack<'_>) -> Result<R>) -> Result<R> {
        self.require_live(scope)?;
        // SAFETY: the thread is live and not running (the host holds `scope`'s thread).
        let stack = unsafe { Stack::lease_root(self.state) };
        body(&stack)
    }

    /// `luaL_sandboxthread`: gives this thread its own writable globals table that proxies the
    /// (frozen) main globals.
    pub fn sandbox(&self, scope: &impl Scope) -> Result<()> {
        self.require_live(scope)?;
        // SAFETY: live thread; the helper only touches this thread's globals slot.
        unsafe { ffi::luaL_sandboxthread(self.state) };
        Ok(())
    }

    /// Host data attached to the thread; null when none. (Luau's own `lua_setthreaddata` slot
    /// holds the binder's per-thread record; the host's pointer lives inside it.)
    pub fn data(&self) -> *mut c_void {
        if !self.pin.is_valid() {
            return std::ptr::null_mut();
        }
        // SAFETY: the pin proves the VM is open, so the record is live.
        unsafe { crate::runtime::shared::thread_record(self.state) }
            .map_or(std::ptr::null_mut(), |record| record.host_data.get())
    }

    /// Attaches host data to the thread. The binder only stores the pointer.
    ///
    /// # Safety
    /// Whatever the host later reads through [`Thread::data`] must stay valid for as long as
    /// the pointer is attached; the binder never dereferences it.
    pub unsafe fn set_data(&self, data: *mut c_void) -> Result<()> {
        if !self.pin.is_valid() {
            return Err(Error::logic("Cannot set data on a Lua thread whose runtime has closed"));
        }
        match unsafe { crate::runtime::shared::thread_record(self.state) } {
            Some(record) => {
                record.host_data.set(data);
                Ok(())
            }
            None => Err(Error::logic("Lua thread has no runtime record")),
        }
    }
}

fn message_text(error: &Error) -> String {
    match error {
        Error::Runtime(text) | Error::Logic(text) | Error::Permission(text) => text.clone(),
        Error::LuaErrorOnStack => "error object left on the stack".to_owned(),
    }
}

impl crate::bind::Call<'_> {
    /// True when the running function may yield (it is inside a coroutine with no C boundary).
    pub fn is_yieldable(&self) -> bool {
        unsafe { ffi::lua_isyieldable(self.state()) != 0 }
    }
}
