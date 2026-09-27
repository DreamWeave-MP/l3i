//! Memory and collector controls beyond the watchdog: GC tuning, allocation rate, heap dumps,
//! the buffer cage, embedder-side GC integration (userdata marks, embedder GC, weak references),
//! light userdata with tags and names, raw tag operations, and coroutine finalizers.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::rc::Weak;

use crate::convert::{FromView, Push};
use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::{Scope, Type, ValueView};
use crate::thread::Thread;
use crate::userdata::{RuntimeTag, Userdata};

/// `lua_gc` commands. Sizes are in kilobytes where Luau's are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcControl {
    Stop,
    Restart,
    Collect,
    /// Heap size in KB.
    Count,
    /// Remainder of the heap size in bytes.
    CountRemainder,
    IsRunning,
    /// One incremental step of the given size in KB (0 for a basic step); returns 1 when a cycle ended.
    Step(c_int),
    /// Target heap growth percentage before a new cycle (Luau's `goal`).
    SetGoal(c_int),
    SetStepMultiplier(c_int),
    SetStepSize(c_int),
    IsPaused,
}

impl Runtime {
    /// Drives the collector (`lua_gc`) and returns its answer.
    pub fn gc(&self, control: GcControl) -> c_int {
        let (what, data) = match control {
            GcControl::Stop => (ffi::LUA_GCSTOP, 0),
            GcControl::Restart => (ffi::LUA_GCRESTART, 0),
            GcControl::Collect => (ffi::LUA_GCCOLLECT, 0),
            GcControl::Count => (ffi::LUA_GCCOUNT, 0),
            GcControl::CountRemainder => (ffi::LUA_GCCOUNTB, 0),
            GcControl::IsRunning => (ffi::LUA_GCISRUNNING, 0),
            GcControl::Step(size) => (ffi::LUA_GCSTEP, size),
            GcControl::SetGoal(goal) => (ffi::LUA_GCSETGOAL, goal),
            GcControl::SetStepMultiplier(value) => (ffi::LUA_GCSETSTEPMUL, value),
            GcControl::SetStepSize(value) => (ffi::LUA_GCSETSTEPSIZE, value),
            GcControl::IsPaused => (ffi::LUA_GCISPAUSED, 0),
        };
        // SAFETY: live state; lua_gc is always permitted from the host.
        unsafe { ffi::lua_gc(self.stack().state_ptr(), what, data) }
    }

    /// Bytes allocated per second, as Luau estimates it (`lua_allocationrate`); `-1` until the
    /// collector has enough history to estimate.
    pub fn allocation_rate(&self) -> i64 {
        unsafe { ffi::lua_allocationrate(self.stack().state_ptr()) }
    }

    /// Luau's high-resolution clock in seconds (`lua_clock`).
    pub fn clock() -> f64 {
        unsafe { ffi::lua_clock() }
    }

    /// Encodes a pointer with this VM's pointer-encoding key, as `tostring` does for addresses.
    pub fn encode_pointer(&self, pointer: usize) -> usize {
        unsafe { ffi::lua_encodepointer(self.stack().state_ptr(), pointer) }
    }

    /// Writes Luau's memory dump (object counts and sizes per category) to `path`.
    pub fn memory_dump(&self, path: &Path) -> Result<()> {
        self.with_dump_file(path, |state, file| unsafe { ffi::lua_memorydump(state, file, None) })
    }

    /// Writes Luau's full heap graph dump to `path`; `category_names` labels memory categories by
    /// index where given.
    pub fn gc_dump(&self, path: &Path, category_names: Option<&'static [&'static CStr]>) -> Result<()> {
        CATEGORY_NAMES.with(|names| names.set(category_names));
        let result =
            self.with_dump_file(path, |state, file| unsafe { ffi::lua_gcdump(state, file, Some(category_name)) });
        CATEGORY_NAMES.with(|names| names.set(None));
        result
    }

    fn with_dump_file(&self, path: &Path, dump: impl FnOnce(*mut ffi::lua_State, *mut c_void)) -> Result<()> {
        let path =
            CString::new(path.to_string_lossy().as_bytes()).map_err(|_| Error::logic("Dump path contains NUL"))?;
        // SAFETY: fopen/fclose are the C runtime's; the FILE is closed after the dump.
        unsafe {
            let file = ffi::fopen(path.as_ptr(), c"w".as_ptr());
            if file.is_null() {
                return Err(Error::runtime(format!("Cannot open {} for writing", path.to_string_lossy())));
            }
            dump(self.stack().state_ptr(), file);
            ffi::fclose(file);
        }
        Ok(())
    }

    /// Names light userdata `tag` (`lua_setlightuserdataname`), as `typeof` reports it. Tags run
    /// `0..LUA_LUTAG_LIMIT`.
    pub fn set_light_userdata_name(&self, tag: c_int, name: &str) -> Result<()> {
        if !(0..ffi::LUA_LUTAG_LIMIT).contains(&tag) {
            return Err(Error::logic(format!("Light userdata tag {tag} is outside 0..{}", ffi::LUA_LUTAG_LIMIT)));
        }
        let name = CString::new(name).map_err(|_| Error::logic("Light userdata name contains NUL"))?;
        // SAFETY: the name is copied into an interned string.
        unsafe { ffi::lua_setlightuserdataname(self.stack().state_ptr(), tag, name.as_ptr()) };
        Ok(())
    }

    pub fn light_userdata_name(&self, tag: c_int) -> Option<String> {
        if !(0..ffi::LUA_LUTAG_LIMIT).contains(&tag) {
            return None;
        }
        // SAFETY: returns null or an interned string.
        unsafe {
            let name = ffi::lua_getlightuserdataname(self.stack().state_ptr(), tag);
            (!name.is_null()).then(|| CStr::from_ptr(name).to_string_lossy().into_owned())
        }
    }

    /// A weak reference to the value at `view`: it does not keep the value alive. The host keeps
    /// it alive by marking it from [`EmbedderGc::mark_reachable`].
    pub fn weak_ref(&self, view: ValueView<'_>) -> Result<WeakRef> {
        if view.type_of() == Type::None {
            return Err(Error::logic("Cannot take a weak reference to a nonexistent value"));
        }
        // SAFETY: the slot exists on a live thread of this VM.
        let id = unsafe { ffi::lua_weakref(view.state(), view.index()) };
        Ok(WeakRef { id, vm: unsafe { crate::runtime::vm_lifetime(view.state()) } })
    }

    /// Installs the embedder half of Luau's cross-heap GC integration.
    pub fn set_embedder_gc(&self, gc: impl EmbedderGc) {
        *self.shared().embedder_gc().borrow_mut() = Some(Box::new(gc));
        // SAFETY: a plain callback write on this VM.
        unsafe { ffi::lua_setembeddergc(self.stack().state_ptr(), Some(embedder_gc_callback)) };
    }

    pub fn clear_embedder_gc(&self) {
        unsafe { ffi::lua_setembeddergc(self.stack().state_ptr(), None) };
        self.shared().embedder_gc().borrow_mut().take();
    }

    /// Enables Luau's experimental coroutine finalizers (`DebugLuauCoroutineFinally`).
    pub fn enable_coroutine_finalizers(&self) -> Result<()> {
        // SAFETY: luau_setfflag walks the static flag list.
        if unsafe { ffi::luau_setfflag(c"DebugLuauCoroutineFinally".as_ptr(), 1) } == 0 {
            return Err(Error::logic("This Luau build has no DebugLuauCoroutineFinally flag"));
        }
        Ok(())
    }

    /// The `finalize(coroutine)` function that runs a finished coroutine's finalizers
    /// (`lua_pushfinalizerfunction`), pinned.
    pub fn finalizer_function(&self) -> Result<crate::value::Function> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            // SAFETY: pushes one C closure on the frame.
            unsafe { ffi::lua_pushfinalizerfunction(frame.state()) };
            crate::value::Function::from_value(crate::value::Value::store(frame.top_value())?)
        })
    }
}

thread_local! {
    static CATEGORY_NAMES: std::cell::Cell<Option<&'static [&'static CStr]>> = const { std::cell::Cell::new(None) };
}

unsafe extern "C" fn category_name(_: *mut ffi::lua_State, memcat: u8) -> *const c_char {
    CATEGORY_NAMES.with(|names| {
        names.get().and_then(|names| names.get(usize::from(memcat))).map_or(std::ptr::null(), |name| name.as_ptr())
    })
}

/// A caged allocator for Luau buffers (`lua_setbuffercage`): every `buffer` allocation goes
/// through it, so the host can place buffers in a guarded region or account for them exactly.
/// Installed through [`crate::runtime::RuntimeBuilder::buffer_cage`] before any buffer exists.
pub trait BufferCage: 'static {
    /// The `lua_Alloc` contract: `nsize == 0` frees `ptr` and returns null; `ptr` null with
    /// `nsize > 0` allocates; otherwise reallocates. `kind` is Luau's opaque allocation type.
    fn allocate(&self, ptr: *mut c_void, old_size: usize, new_size: usize, kind: c_int) -> *mut c_void;
}

pub(crate) unsafe extern "C" fn cage_callback(
    ud: *mut c_void,
    ptr: *mut c_void,
    osize: usize,
    nsize: usize,
    kind: c_int,
) -> *mut c_void {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    // SAFETY: `ud` is the runtime's boxed cage, alive as long as the VM.
    let cage = unsafe { &**ud.cast_const().cast::<Box<dyn BufferCage>>() };
    cage.allocate(ptr, osize, nsize, kind)
}

/// The embedder side of cross-heap marking (`lua_setembeddergc`): each GC cycle Luau first
/// asks the embedder to reset its bookkeeping, then, once it has marked userdata through
/// [`UserdataMark`] callbacks, asks it to mark every weak reference reachable from marked
/// native objects. No Lua API may be used from these methods.
pub trait EmbedderGc: 'static {
    fn reset(&self);
    fn mark_reachable(&self, mark: &mut dyn FnMut(&WeakRef));
}

unsafe extern "C" fn embedder_gc_callback(state: *mut ffi::lua_State, markref: Option<ffi::lua_EmbedderMark>) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    // SAFETY: called by the collector on a live VM; Shared outlives it.
    let Some(shared) = (unsafe { crate::runtime::shared_for(state) }) else { return };
    let Ok(slot) = shared.embedder_gc().try_borrow() else { return };
    let Some(gc) = slot.as_ref() else { return };
    match markref {
        None => gc.reset(),
        Some(mark) => gc.mark_reachable(&mut |weak: &WeakRef| unsafe { mark(state, weak.id) }),
    }
}

/// A userdata mark callback for `T` (`lua_setuserdatamark`): the collector calls
/// [`UserdataMark::mark`] when it marks a `T` reachable, so the host can note that the native
/// object behind it is alive. No Lua API may be used here.
pub trait UserdataMark<T: Userdata>: 'static {
    fn mark(value: &T);
}

unsafe extern "C" fn mark_thunk<T: Userdata, M: UserdataMark<T>>(_: *mut ffi::lua_State, ud: *mut c_void) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    // SAFETY: Luau calls the mark registered for T's tag only on userdata of that tag.
    M::mark(unsafe { &*ud.cast::<T>() });
}

/// Registers `M` as the mark callback for `T`, which must be registered tagged in this runtime.
pub fn set_userdata_mark<T: Userdata, M: UserdataMark<T>>(runtime: &Runtime) -> Result<()> {
    let stack = runtime.stack();
    let Some(tag) = crate::userdata::tagged::tag_of::<T>(&stack) else {
        return Err(Error::logic(format!("'{}' is not tagged in this runtime; marks need a tag", T::NAME)));
    };
    // SAFETY: a callback write for a tag this runtime owns.
    unsafe { ffi::lua_setuserdatamark(stack.state_ptr(), c_int::from(tag), Some(mark_thunk::<T, M>)) };
    Ok(())
}

/// An embedder-managed weak reference (`lua_weakref`). Released explicitly; a leaked one costs a
/// registry slot until the VM closes.
#[derive(Debug)]
pub struct WeakRef {
    id: c_int,
    vm: Weak<()>,
}

impl WeakRef {
    /// Pushes the referenced value if it is still alive and returns a view of it; `None` (and
    /// nothing pushed) once it was collected.
    pub fn get<'s>(&self, scope: &'s impl Scope) -> Option<ValueView<'s>> {
        if self.vm.strong_count() == 0 {
            return None;
        }
        // SAFETY: the VM is open; lua_getweakref pushes the value or nil.
        unsafe {
            if ffi::lua_getweakref(scope.state(), self.id) == ffi::LUA_TNIL {
                ffi::lua_pop(scope.state(), 1);
                return None;
            }
        }
        Some(scope.top_value())
    }

    /// Releases the reference slot.
    pub fn release(self, scope: &impl Scope) {
        if self.vm.strong_count() > 0 {
            unsafe { ffi::lua_weakunref(scope.state(), self.id) };
        }
    }

    pub fn id(&self) -> c_int {
        self.id
    }
}

/// A light userdata: a raw pointer with a tag (`lua_pushlightuserdatatagged`). Tag 0 is the
/// plain light userdata Lua knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightUserdata {
    pub pointer: *mut c_void,
    pub tag: c_int,
}

impl Push for LightUserdata {
    fn push_into<'s, S: Scope>(&self, scope: &'s S) -> Result<ValueView<'s>> {
        if !(0..ffi::LUA_LUTAG_LIMIT).contains(&self.tag) {
            return Err(Error::logic(format!(
                "Light userdata tag {} is outside 0..{}",
                self.tag,
                ffi::LUA_LUTAG_LIMIT
            )));
        }
        unsafe { ffi::lua_pushlightuserdatatagged(scope.state(), self.pointer, self.tag) };
        Ok(scope.top_value())
    }
}

impl<'v> FromView<'v> for LightUserdata {
    const EXPECTED: &'static str = "light userdata";

    fn from_view(view: ValueView<'v>) -> Result<LightUserdata> {
        if !view.is_light_userdata() {
            return Err(view.type_error(Type::LightUserdata));
        }
        // SAFETY: the slot holds a light userdata.
        unsafe {
            Ok(LightUserdata {
                pointer: ffi::lua_tolightuserdata(view.state(), view.index()),
                tag: ffi::lua_lightuserdatatag(view.state(), view.index()),
            })
        }
    }

    fn matches(view: ValueView<'v>) -> bool {
        view.is_light_userdata()
    }
}

/// Raw tag operations that bypass the registration contract. For hosts that manage a tag's
/// meaning themselves.
pub mod raw {
    use super::*;

    /// Changes the tag of the full userdata at `view` (`lua_setuserdatatag`).
    ///
    /// # Safety
    /// Every check in this crate trusts a tag to identify the payload type registered for it;
    /// the caller must guarantee the payload is what the new tag's type expects (or that the new
    /// tag has no registered type), and that no destructor mismatch results.
    pub unsafe fn set_userdata_tag(view: ValueView<'_>, tag: RuntimeTag) -> Result<()> {
        if !view.is_userdata() {
            return Err(Error::logic("set_userdata_tag needs a full userdata"));
        }
        unsafe { ffi::lua_setuserdatatag(view.state(), view.index(), c_int::from(tag)) };
        Ok(())
    }

    /// Allocates a tagged userdata of `size` bytes with no metatable (`lua_newuserdatatagged`) and
    /// returns the uninitialised payload; the value is on top of `scope`.
    ///
    /// # Safety
    /// The caller must initialise the payload before Luau can observe it and must ensure the
    /// tag's destructor (if any) can run on that payload.
    pub unsafe fn new_userdata_tagged<'s>(
        scope: &'s impl Scope,
        size: usize,
        tag: RuntimeTag,
    ) -> Result<(*mut c_void, ValueView<'s>)> {
        let data = unsafe { ffi::lua_newuserdatatagged(scope.state(), size, c_int::from(tag)) };
        if data.is_null() {
            return Err(Error::runtime("Unable to allocate userdata"));
        }
        Ok((data, scope.top_value()))
    }
}

impl Thread {
    /// Registers the function at `callback` to run when this coroutine is finalized
    /// (`lua_addfinalizer`; enable with [`Runtime::enable_coroutine_finalizers`] first). The
    /// coroutine must be live: started and suspended, or running. Fails for the main thread, a
    /// fresh thread (Luau reports it as finished), or a dead coroutine.
    pub fn add_finalizer(&self, scope: &impl Scope, callback: ValueView<'_>) -> Result<()> {
        if !callback.is_function() {
            return Err(Error::logic("A finalizer must be a function"));
        }
        let co = self.state_ptr();
        scope.with_frame(|frame| {
            frame.push_value(callback)?;
            // SAFETY: the callback is on top; lua_addfinalizer raises for the main thread or a
            // dead coroutine, which the protected call turns into an error.
            unsafe {
                frame.raising(1, 0, |state| {
                    ffi::lua_addfinalizer(state, co, 1);
                    ffi::lua_pop(state, 1);
                    0
                })
            }
        })
    }

    /// True when finalizers are registered on this coroutine.
    pub fn has_finalizers(&self) -> bool {
        self.value().is_valid() && unsafe { ffi::lua_hasfinalizers(self.state_ptr()) != 0 }
    }
}

/// Reads every boolean fast flag registered in this build with its current value.
pub fn fast_flags() -> Vec<(String, bool)> {
    let mut flags: Vec<(String, bool)> = Vec::new();
    unsafe extern "C" fn visit(context: *mut c_void, name: *const c_char, value: c_int) {
        // SAFETY: context is the Vec passed below; name is a static string.
        unsafe {
            (*context.cast::<Vec<(String, bool)>>())
                .push((CStr::from_ptr(name).to_string_lossy().into_owned(), value != 0));
        }
    }
    // SAFETY: walks the static flag list.
    unsafe { ffi::luau_visitfflags((&mut flags as *mut Vec<(String, bool)>).cast(), visit) };
    flags
}

/// Reads one boolean fast flag; `None` when this build has no such flag.
pub fn fast_flag(name: &str) -> Option<bool> {
    let name = CString::new(name).ok()?;
    match unsafe { ffi::luau_getfflag(name.as_ptr()) } {
        -1 => None,
        value => Some(value != 0),
    }
}

/// Sets one boolean fast flag. Flags are process-wide and frozen by the crate's policy once a
/// runtime exists; use this for Luau's `Debug*` flags only.
pub fn set_fast_flag(name: &str, value: bool) -> Result<()> {
    let name = CString::new(name).map_err(|_| Error::logic("Flag name contains NUL"))?;
    if unsafe { ffi::luau_setfflag(name.as_ptr(), c_int::from(value)) } == 0 {
        return Err(Error::logic(format!("This Luau build has no fast flag named {}", name.to_string_lossy())));
    }
    Ok(())
}

/// Reads one integer fast flag (`FInt`).
pub fn fast_int(name: &str) -> Option<c_int> {
    let name = CString::new(name).ok()?;
    let mut out = 0;
    (unsafe { ffi::luau_getfint(name.as_ptr(), &mut out) } != 0).then_some(out)
}

/// Sets one integer fast flag (`FInt`).
pub fn set_fast_int(name: &str, value: c_int) -> Result<()> {
    let name = CString::new(name).map_err(|_| Error::logic("Flag name contains NUL"))?;
    if unsafe { ffi::luau_setfint(name.as_ptr(), value) } == 0 {
        return Err(Error::logic(format!("This Luau build has no integer fast flag named {}", name.to_string_lossy())));
    }
    Ok(())
}

pub(crate) type EmbedderGcSlot = RefCell<Option<Box<dyn EmbedderGc>>>;
