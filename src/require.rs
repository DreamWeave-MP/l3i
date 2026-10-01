//! Luau's require-by-string runtime (`Require/include/Luau/Require.h`).
//!
//! Luau resolves `require("./path")` by walking a navigator the host provides: reset to the
//! requiring module, step to parents and children, ask whether a module is present, and load
//! it. The host implements [`RequireNavigator`] over whatever its module space is (files, a VFS,
//! an archive, generated code); the binder mounts it through Luau's own implementation, which
//! supplies caching, cyclic-require placeholders, `.luaurc` alias handling, and the error
//! messages scripts see.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_void};

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::runtime::Runtime;
use crate::stack::Scope;
use crate::value::{Function, Value};

/// Outcome of a navigation step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Navigate {
    Success,
    Ambiguous,
    NotFound,
}

/// Whether the module at the navigator's position has a configuration file, and its syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigStatus {
    Absent,
    Ambiguous,
    Json,
    Luau,
}

/// How to run a module once Luau has resolved it.
pub enum Load {
    /// The loader pushed this many results onto the requiring thread.
    Results(c_int),
    /// The requiring thread should yield; it is resumed later with the module result pushed.
    Yield,
}

/// The host's module space. Luau drives it through these calls; each holds an implicit
/// "current position" the navigator maintains between `reset` and `load`.
pub trait RequireNavigator: 'static {
    /// Whether `requirer_chunkname` may call `require` at all.
    fn is_require_allowed(&self, requirer_chunkname: &str) -> bool {
        let _ = requirer_chunkname;
        true
    }
    /// Points the position at the module whose chunk name is given.
    fn reset(&self, requirer_chunkname: &str) -> Navigate;
    /// Points the position at an aliased module given its path from a configuration file.
    fn jump_to_alias(&self, path: &str) -> Navigate {
        let _ = path;
        Navigate::NotFound
    }
    /// A chance to resolve `@alias` before configuration files are consulted.
    fn to_alias_override(&self, alias: &str) -> Option<Navigate> {
        let _ = alias;
        None
    }
    /// A last chance to resolve `@alias` after configuration files failed.
    fn to_alias_fallback(&self, alias: &str) -> Option<Navigate> {
        let _ = alias;
        None
    }
    fn to_parent(&self) -> Navigate;
    fn to_child(&self, name: &str) -> Navigate;
    /// Whether the position names a loadable module.
    fn is_module_present(&self) -> bool;
    /// The chunk name the module runs under (visible to the debug library).
    fn chunkname(&self) -> Option<String>;
    /// The name passed to `load`.
    fn loadname(&self) -> Option<String>;
    /// The key Luau caches the module result under.
    fn cache_key(&self) -> Option<String>;
    fn config_status(&self) -> ConfigStatus {
        ConfigStatus::Absent
    }
    /// The configuration file's contents at the position (Luau parses it); consulted when
    /// `config_status` is not `Absent`.
    fn config(&self) -> Option<String> {
        None
    }
    /// Milliseconds allowed for a Luau-syntax configuration file; `None` for Luau's default.
    fn luau_config_timeout_ms(&self) -> Option<i32> {
        None
    }
    /// Runs the module: compile `path`/`loadname` under `chunkname`, push its results on the
    /// scope, and say how many (or that the requirer should yield). An error raises into the
    /// requiring script.
    fn load(&self, scope: &impl_scope::Requirer<'_>, path: &str, chunkname: &str, loadname: &str) -> Result<Load>;
}

/// The requiring thread as a scope, for `load`.
pub mod impl_scope {
    pub use crate::bind::Call as Requirer;
}

pub(crate) type NavigatorSlot = RefCell<Option<Box<Box<dyn RequireNavigator>>>>;

unsafe fn navigator<'a>(ctx: *mut c_void) -> &'a dyn RequireNavigator {
    // SAFETY: `ctx` is the inner Box the runtime keeps alive for the VM's life.
    unsafe { &**ctx.cast_const().cast::<Box<dyn RequireNavigator>>() }
}

unsafe fn text<'a>(pointer: *const c_char) -> &'a str {
    if pointer.is_null() {
        return "";
    }
    // SAFETY: Luau passes NUL-terminated strings valid for the call.
    unsafe { CStr::from_ptr(pointer) }.to_str().unwrap_or("")
}

fn navigate(result: Navigate) -> ffi::luarequire_NavigateResult {
    match result {
        Navigate::Success => ffi::NAVIGATE_SUCCESS,
        Navigate::Ambiguous => ffi::NAVIGATE_AMBIGUOUS,
        Navigate::NotFound => ffi::NAVIGATE_NOT_FOUND,
    }
}

/// Copies `value` into Luau's buffer, reporting the size it needs when it does not fit.
unsafe fn write(
    value: Option<String>,
    buffer: *mut c_char,
    size: usize,
    size_out: *mut usize,
) -> ffi::luarequire_WriteResult {
    let Some(value) = value else { return ffi::WRITE_FAILURE };
    unsafe {
        *size_out = value.len();
        if value.len() > size {
            return ffi::WRITE_BUFFER_TOO_SMALL;
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), buffer.cast::<u8>(), value.len());
        ffi::WRITE_SUCCESS
    }
}

unsafe extern "C-unwind" fn is_require_allowed(_: *mut ffi::lua_State, ctx: *mut c_void, chunk: *const c_char) -> bool {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigator(ctx).is_require_allowed(text(chunk)) }
}

unsafe extern "C-unwind" fn reset(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    chunk: *const c_char,
) -> ffi::luarequire_NavigateResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigate(navigator(ctx).reset(text(chunk))) }
}

unsafe extern "C-unwind" fn jump_to_alias(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    path: *const c_char,
) -> ffi::luarequire_NavigateResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigate(navigator(ctx).jump_to_alias(text(path))) }
}

unsafe extern "C-unwind" fn to_alias_override(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    alias: *const c_char,
) -> ffi::luarequire_NavigateResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigator(ctx).to_alias_override(text(alias)).map_or(ffi::NAVIGATE_NOT_FOUND, navigate) }
}

unsafe extern "C-unwind" fn to_alias_fallback(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    alias: *const c_char,
) -> ffi::luarequire_NavigateResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigator(ctx).to_alias_fallback(text(alias)).map_or(ffi::NAVIGATE_NOT_FOUND, navigate) }
}

unsafe extern "C-unwind" fn to_parent(_: *mut ffi::lua_State, ctx: *mut c_void) -> ffi::luarequire_NavigateResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigate(navigator(ctx).to_parent()) }
}

unsafe extern "C-unwind" fn to_child(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    name: *const c_char,
) -> ffi::luarequire_NavigateResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigate(navigator(ctx).to_child(text(name))) }
}

unsafe extern "C-unwind" fn is_module_present(_: *mut ffi::lua_State, ctx: *mut c_void) -> bool {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigator(ctx).is_module_present() }
}

unsafe extern "C-unwind" fn get_chunkname(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    buffer: *mut c_char,
    size: usize,
    out: *mut usize,
) -> ffi::luarequire_WriteResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { write(navigator(ctx).chunkname(), buffer, size, out) }
}

unsafe extern "C-unwind" fn get_loadname(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    buffer: *mut c_char,
    size: usize,
    out: *mut usize,
) -> ffi::luarequire_WriteResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { write(navigator(ctx).loadname(), buffer, size, out) }
}

unsafe extern "C-unwind" fn get_cache_key(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    buffer: *mut c_char,
    size: usize,
    out: *mut usize,
) -> ffi::luarequire_WriteResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { write(navigator(ctx).cache_key(), buffer, size, out) }
}

unsafe extern "C-unwind" fn get_config_status(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
) -> ffi::luarequire_ConfigStatus {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    match unsafe { navigator(ctx).config_status() } {
        ConfigStatus::Absent => ffi::CONFIG_ABSENT,
        ConfigStatus::Ambiguous => ffi::CONFIG_AMBIGUOUS,
        ConfigStatus::Json => ffi::CONFIG_PRESENT_JSON,
        ConfigStatus::Luau => ffi::CONFIG_PRESENT_LUAU,
    }
}

unsafe extern "C-unwind" fn get_config(
    _: *mut ffi::lua_State,
    ctx: *mut c_void,
    buffer: *mut c_char,
    size: usize,
    out: *mut usize,
) -> ffi::luarequire_WriteResult {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { write(navigator(ctx).config(), buffer, size, out) }
}

unsafe extern "C-unwind" fn get_luau_config_timeout(_: *mut ffi::lua_State, ctx: *mut c_void) -> c_int {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    unsafe { navigator(ctx).luau_config_timeout_ms().unwrap_or(2000) }
}

unsafe extern "C-unwind" fn load(
    state: *mut ffi::lua_State,
    ctx: *mut c_void,
    path: *const c_char,
    chunkname: *const c_char,
    loadname: *const c_char,
) -> c_int {
    // Runs on the requiring thread as a native call: errors raise into the script.
    unsafe {
        crate::raw::trampoline::enter(state, || {
            let call = crate::bind::Call::from_raw(state);
            match navigator(ctx).load(&call, text(path), text(chunkname), text(loadname))? {
                Load::Results(count) => Ok(count),
                Load::Yield => Ok(-1),
            }
        })
    }
}

unsafe extern "C" fn configure(config: *mut ffi::luarequire_Configuration) {
    // SAFETY: Luau hands us its configuration struct to fill.
    unsafe {
        *config = ffi::luarequire_Configuration {
            is_require_allowed: Some(is_require_allowed),
            reset: Some(reset),
            jump_to_alias: Some(jump_to_alias),
            to_alias_override: Some(to_alias_override),
            to_alias_fallback: Some(to_alias_fallback),
            to_parent: Some(to_parent),
            to_child: Some(to_child),
            is_module_present: Some(is_module_present),
            get_chunkname: Some(get_chunkname),
            get_loadname: Some(get_loadname),
            get_cache_key: Some(get_cache_key),
            get_config_status: Some(get_config_status),
            get_alias: None,
            get_config: Some(get_config),
            get_luau_config_timeout: Some(get_luau_config_timeout),
            load: Some(load),
        };
    }
}

impl Runtime {
    fn install_navigator(&self, navigator: impl RequireNavigator) -> *mut c_void {
        let boxed: Box<Box<dyn RequireNavigator>> = Box::new(Box::new(navigator));
        let ctx = (&*boxed as *const Box<dyn RequireNavigator>).cast_mut().cast::<c_void>();
        *self.shared().require_navigator().borrow_mut() = Some(boxed);
        ctx
    }

    /// Installs `navigator` and registers Luau's `require` as a global (`luaopen_require`).
    /// Replaces any navigator installed before; closures Luau already created keep the old one
    /// alive only through the runtime, so install once at setup.
    pub fn install_require(&self, navigator: impl RequireNavigator) -> Result<()> {
        let ctx = self.install_navigator(navigator);
        // SAFETY: the navigator box lives in the shared block for the VM's life.
        unsafe { ffi::luaopen_require(self.stack().state_ptr(), configure, ctx) };
        Ok(())
    }

    /// Installs `navigator` and returns Luau's `require` closure pinned, without registering it
    /// globally (for sandboxes that hand it to instances themselves).
    pub fn require_function(&self, navigator: impl RequireNavigator) -> Result<Function> {
        let ctx = self.install_navigator(navigator);
        let stack = self.stack();
        stack.with_frame(|frame| {
            // SAFETY: pushes one closure.
            unsafe { ffi::luarequire_pushrequire(frame.state(), configure, ctx) };
            Function::from_value(Value::store(frame.top_value())?)
        })
    }

    /// A `proxyrequire(path, chunkname)` closure over the installed navigator: resolves `path`
    /// as if required from the module `chunkname`.
    pub fn proxy_require_function(&self) -> Result<Function> {
        let ctx = {
            let slot = self.shared().require_navigator().borrow();
            let Some(boxed) = slot.as_ref() else { return Err(Error::logic("No require navigator is installed")) };
            (&**boxed as *const Box<dyn RequireNavigator>).cast_mut().cast::<c_void>()
        };
        let stack = self.stack();
        stack.with_frame(|frame| {
            unsafe { ffi::luarequire_pushproxyrequire(frame.state(), configure, ctx) };
            Function::from_value(Value::store(frame.top_value())?)
        })
    }

    /// Registers `value` as the permanent result of requiring the alias `path`
    /// (`luarequire_registermodule`).
    pub fn register_require_module(&self, path: &str, value: &Value) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            frame.push_string(path);
            value.push_to(frame)?;
            // SAFETY: the two arguments are on top; registermodule consumes them and may raise.
            unsafe {
                frame.raising(2, 0, |state| {
                    ffi::luarequire_registermodule(state);
                    0
                })
            }
        })?;
        let mut modules = self.shared().require_modules().borrow_mut();
        if !modules.iter().any(|module| module == path) {
            modules.push(path.to_owned());
        }
        Ok(())
    }

    /// Every path registered with [`Self::register_require_module`], in registration order: the
    /// plan's modules and whatever the host registered after, the namespace `require` resolves
    /// without a navigator.
    pub fn registered_require_modules(&self) -> Vec<String> {
        self.shared().require_modules().borrow().clone()
    }

    /// Drops one cached module result by its cache key (`luarequire_clearcacheentry`).
    pub fn clear_require_cache_entry(&self, cache_key: &str) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            frame.push_string(cache_key);
            unsafe {
                frame.raising(1, 0, |state| {
                    ffi::luarequire_clearcacheentry(state);
                    0
                })
            }
        })
    }

    /// Drops every cached module result (`luarequire_clearcache`).
    pub fn clear_require_cache(&self) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| unsafe {
            frame.raising(0, 0, |state| {
                ffi::luarequire_clearcache(state);
                0
            })
        })
    }
}

/// Placeholder support for cyclic requires, for use from [`RequireNavigator::load`].
pub trait RequirePlaceholders: Scope + Sized {
    /// Creates a locked placeholder table for the module being loaded and caches it, so a cycle
    /// back to this module sees the placeholder (`luarequire_createplaceholder`).
    fn create_require_placeholder(&self) {
        unsafe { ffi::luarequire_createplaceholder(self.state()) }
    }

    /// Locks the table at `index` with error-raising metamethods (`luarequire_lockplaceholder`).
    fn lock_require_placeholder(&self, index: c_int) {
        unsafe { ffi::luarequire_lockplaceholder(self.state(), index) }
    }

    /// Copies the finished module table at `result` into the placeholder at `placeholder`,
    /// replacing its lock (`luarequire_populateplaceholder`).
    fn populate_require_placeholder(&self, placeholder: c_int, result: c_int) {
        unsafe { ffi::luarequire_populateplaceholder(self.state(), placeholder, result) }
    }
}

impl<S: Scope> RequirePlaceholders for S {}
