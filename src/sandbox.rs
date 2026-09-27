//! Sandboxes: a frozen base environment, per-script instance environments, and compile-once
//! script templates (`Lua::State` in `components/lua/luastate.cpp`).
//!
//! The model, as OpenMW has it:
//! - A **base environment** copies every string-keyed global except `_G` and the environment
//!   escape hatches (`getfenv`, `setfenv`, `newproxy`), exposing tables as read-only views and
//!   replacing `getmetatable` with a tables-only version. `print` and `require` are left out
//!   because every instance gets its own. The base env is frozen and marked safe so Luau's
//!   import fast paths apply. The real `_G` is never frozen; `luaL_sandbox` is not used.
//! - An **instance** is a writable table whose frozen metatable indexes the base env, with
//!   `_G`, a named `print`, a `loaded` table of packages (function packages are factories
//!   called once with the instance's hidden data) and a `require` reading from it.
//! - A **template** is a chunk compiled once and loaded on a loader thread whose globals are an
//!   empty frozen table, then instantiated per script with `lua_clonefunction` + `lua_setfenv`.
//!
//! Hosts supply the log sink, the loader function behind `require`, and the packages; nothing
//! here knows about scripts, files, or OpenMW.

use std::collections::BTreeMap;
use std::ffi::{CString, c_int};

use crate::error::{Error, Result};
use crate::raw::protect::pop_error;
use crate::raw::{ffi, trampoline};
use crate::readonly;
use crate::runtime::{CallContext, CallKind, Runtime};
use crate::source::{CompileOptions, compile_raw};
use crate::stack::{Frame, Scope};
use crate::value::{Function, Table, Value};

/// Globals that never reach a sandbox: environment escape hatches, and the three names every
/// instance defines for itself.
const EXCLUDED_GLOBALS: [&str; 7] = ["_G", "getfenv", "setfenv", "newproxy", "getmetatable", "print", "require"];

/// The prelude: the generators every instance is built from. Runs once in the real globals with
/// the log sink, the raw metamethod reader, and the native `pairs`/`ipairs` as arguments;
/// leaks no globals.
const PRELUDE: &str = r#"
local writeToLog, rawMetamethod, nativePairs, nativeIpairs = ...
local function printToLog(...)
    local t = {}
    for i = 1, select('#', ...) do t[i] = tostring(select(i, ...)) end
    return writeToLog(table.concat(t, '\t'))
end
local function printGen(name) return function(...) return printToLog(name, ...) end end
local function requireGen(env, loaded, loadFn)
    return function(name)
        local p = loaded[name]
        if p == nil then
            local loader = loadFn(name, env)
            p = loader(name)
            loaded[name] = p
        end
        return p
    end
end
local function getSafeMetatable(v)
    if type(v) ~= 'table' then error('getmetatable is allowed only for tables', 2) end
    return getmetatable(v)
end
local function compatIterator(native, legacyName)
    return function(v)
        local kind = type(v)
        if kind == 'userdata' then
            local iter = rawMetamethod(v, legacyName)
            if iter ~= nil then return iter(v) end
            error('attempt to iterate a userdata value of type ' .. typeof(v) .. ' without iterator support', 2)
        end
        if kind == 'table' then
            local iter = rawMetamethod(v, legacyName)
            if iter ~= nil then return iter(v) end
        end
        return native(v)
    end
end
return printGen, requireGen, getSafeMetatable, compatIterator(nativePairs, '__pairs'), compatIterator(nativeIpairs, '__ipairs')
"#;

/// Host choices for a sandbox.
#[derive(Clone, Debug)]
pub struct SandboxOptions {
    /// Compile options for templates.
    pub compile_options: CompileOptions,
    /// Replace the global `pairs`/`ipairs` with versions honouring `__pairs`/`__ipairs` (stock
    /// Luau ignores both). Default on: read-only views and iterable userdata depend on it.
    pub compat_iterators: bool,
    /// Wrap `string.format` so `%s` applies `tostring` to any value, as LuaJIT does. Default off.
    pub compat_string_format: bool,
    /// Seed `math.random` once from the clock and make `math.randomseed` a no-op. Default off.
    pub neuter_randomseed: bool,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        SandboxOptions {
            compile_options: CompileOptions::default(),
            compat_iterators: true,
            compat_string_format: false,
            neuter_randomseed: false,
        }
    }
}

/// A prepared sandbox for one VM: the base environment and the generators.
pub struct Sandbox {
    base_env: Table,
    common_packages: BTreeMap<String, Value>,
    print_gen: Function,
    require_gen: Function,
    loader_thread: Value,
    compile_options: CompileOptions,
}

/// A compiled script, loaded once, instantiated many times.
#[derive(Debug)]
pub struct Template {
    closure: Function,
    chunk_name: String,
    #[cfg(feature = "jit")]
    native: Option<crate::native_code::NativeCodeResult>,
}

impl Template {
    pub fn chunk_name(&self) -> &str {
        &self.chunk_name
    }

    /// How native compilation of this template went, when the runtime has a generator.
    #[cfg(feature = "jit")]
    pub fn native_code(&self) -> Option<crate::native_code::NativeCodeResult> {
        self.native
    }
}

/// How to build one instance environment.
pub struct InstanceSpec<'a> {
    /// The instance's name; `print` prefixes its output with `name:`.
    pub name: &'a str,
    /// Per-instance packages, added after the common ones (and overriding them by name).
    /// Functions are factories called once with the hidden data.
    pub packages: &'a [(&'a str, &'a Value)],
    /// Passed to package factories; nil when absent.
    pub hidden_data: Option<&'a Value>,
    /// Behind `require`: called as `loader(name, env)` for a name missing from `loaded`, and
    /// must return a function that, called with `name`, produces the package.
    pub loader: &'a Function,
}

/// One script's environment.
pub struct Instance {
    pub env: Table,
    pub loaded: Table,
}

impl Runtime {
    /// Prepares a sandbox on this VM, installing the prelude and the chosen compatibility
    /// shims into the real globals. Call once, after every module the base env should expose is
    /// registered. `log` receives each `print` line, already tab-joined and prefixed.
    pub fn sandbox(&self, log: impl Fn(&str) + 'static, options: SandboxOptions) -> Result<Sandbox> {
        let root = self
            .debug_roots()
            .first()
            .copied()
            .ok_or_else(|| Error::logic("A sandbox needs at least one debug root for its internal functions"))?;
        if options.neuter_randomseed {
            self.exec("math.randomseed(os.time()); math.randomseed = function() end")?;
        }
        if options.compat_string_format {
            self.install_compat_string_format()?;
        }
        let write_to_log =
            self.bind_function(&format!("{root}.internal.writeToLog"), move |message: &str| log(message))?;
        let generated = self.stack().with_frame(|frame| {
            // SAFETY: the name is a static C string, valid until the VM closes.
            let raw_metamethod = unsafe {
                frame.push_c_function(compat_metamethod, c"dream_binder.internal.getCompatMetamethod".as_ptr())
            };
            let raw_metamethod = Function::from_value(Value::store(raw_metamethod)?)?;
            let pairs = Value::get_global(frame, "pairs")?;
            let ipairs = Value::get_global(frame, "ipairs")?;
            let chunk = self.load(frame, "=dream_binder.prelude", PRELUDE, &options.compile_options)?;
            let prelude = Function::from_value(Value::store(chunk)?)?;
            prelude.invoke_multi(frame, (&write_to_log, &raw_metamethod, &pairs, &ipairs))
        })?;
        let mut generated = generated.into_iter();
        let mut next = || {
            generated
                .next()
                .ok_or_else(|| Error::logic("The sandbox prelude returned too few values"))
                .and_then(Function::from_value)
        };
        let print_gen = next()?;
        let require_gen = next()?;
        let get_safe_metatable = next()?;
        let pairs = next()?;
        let ipairs = next()?;
        if options.compat_iterators {
            self.set_global("pairs", &pairs)?;
            self.set_global("ipairs", &ipairs)?;
        }
        let mut common_packages = BTreeMap::new();
        let base_env = self.build_base_env(&get_safe_metatable, &mut common_packages)?;
        let loader_thread = self.new_loader_thread()?;
        Ok(Sandbox {
            base_env,
            common_packages,
            print_gen,
            require_gen,
            loader_thread,
            compile_options: options.compile_options,
        })
    }

    fn install_compat_string_format(&self) -> Result<()> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            let state = frame.state();
            // SAFETY: balanced pushes on the frame; the closure captures the original formatter
            // as its only upvalue and is stored back into the (unfrozen) string library.
            unsafe {
                if ffi::lua_getglobal(state, c"string".as_ptr()) != ffi::LUA_TTABLE {
                    return Err(Error::logic("Luau string library is not available"));
                }
                if ffi::lua_getfield(state, -1, c"format".as_ptr()) != ffi::LUA_TFUNCTION {
                    return Err(Error::logic("Luau string.format is not available"));
                }
                ffi::lua_pushcclosure(state, compat_string_format, c"string.format".as_ptr(), 1);
                ffi::lua_setfield(state, -2, c"format".as_ptr());
            }
            Ok(())
        })
    }

    fn build_base_env(&self, get_safe_metatable: &Function, common: &mut BTreeMap<String, Value>) -> Result<Table> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            let env = frame.push_table(0, 64)?;
            frame.with_frame(|inner| {
                let state = inner.state();
                // SAFETY: the globals pseudo-index always names a table; the copy lives on `inner`.
                unsafe { ffi::lua_pushvalue(state, ffi::LUA_GLOBALSINDEX) };
                let globals = inner.top_value().as_table()?;
                globals.for_each(inner, |step, key, value| {
                    let Ok(name) = key.read::<&str>() else { return Ok(()) };
                    if EXCLUDED_GLOBALS.contains(&name) {
                        return Ok(());
                    }
                    let name = name.to_owned();
                    step.push_value(value)?;
                    if value.is_table() {
                        readonly::build_read_only_view(self, step, false)?;
                        common.insert(name.clone(), Value::store(step.top_value())?);
                    } else if value.is_userdata() {
                        common.insert(name.clone(), Value::store(step.top_value())?);
                    }
                    env.raw_set(step, &name)
                })
            })?;
            env.raw_set_value(frame, "getmetatable", get_safe_metatable)?;
            let state = frame.state();
            // SAFETY: the env table is on the frame; freezing and marking safe are plain flag writes.
            unsafe {
                ffi::lua_setreadonly(state, env.index(), 1);
                ffi::lua_setsafeenv(state, env.index(), 1);
            }
            Table::from_value(Value::store(env.value())?)
        })
    }

    /// A thread whose globals are an empty frozen table: chunks loaded on it capture no
    /// environment until `lua_setfenv` gives them one (`pushTemplateLoaderThread`).
    fn new_loader_thread(&self) -> Result<Value> {
        let stack = self.stack();
        stack.with_frame(|frame| {
            let state = frame.state();
            // SAFETY: lua_newthread pushes the thread on `state`; the loader's own stack is
            // touched only to replace its globals.
            unsafe {
                let loader = ffi::lua_newthread(state);
                ffi::lua_newtable(loader);
                ffi::lua_replace(loader, ffi::LUA_GLOBALSINDEX);
                ffi::lua_setreadonly(loader, ffi::LUA_GLOBALSINDEX, 1);
                ffi::lua_setsafeenv(loader, ffi::LUA_GLOBALSINDEX, 0);
            }
            Value::store(frame.top_value())
        })
    }
}

impl Sandbox {
    /// The frozen base environment every instance indexes.
    pub fn base_env(&self) -> &Table {
        &self.base_env
    }

    /// Packages every instance receives: the base env's tables and userdata plus those added
    /// with [`Sandbox::add_common_package`].
    pub fn common_packages(&self) -> &BTreeMap<String, Value> {
        &self.common_packages
    }

    /// Adds (or replaces) a package every instance receives. Tables are frozen in place; a
    /// function is a factory called per instance with the hidden data.
    pub fn add_common_package(&mut self, runtime: &Runtime, name: impl Into<String>, package: Value) -> Result<()> {
        require_same_vm(runtime, &package, "Common package")?;
        let package = if package.is_table() {
            let table = Table::from_value(package)?;
            readonly::make_read_only(runtime, &table)?;
            table.into_value()
        } else {
            package
        };
        self.common_packages.insert(name.into(), package);
        Ok(())
    }

    /// Builds one instance environment (`runInNewSandbox` up to the script call). Runs inside
    /// an initialization call scope.
    pub fn new_instance(&self, runtime: &Runtime, spec: &InstanceSpec<'_>) -> Result<Instance> {
        require_same_vm(runtime, spec.loader.value(), "Package loader")?;
        if let Some(hidden) = spec.hidden_data {
            require_same_vm(runtime, hidden, "Hidden data")?;
        }
        for (name, package) in spec.packages {
            require_same_vm(runtime, package, &format!("Package {name}"))?;
        }
        let _scope = runtime.call_scope(runtime.initialization_context(), CallKind::Initialization);
        let stack = runtime.stack();
        stack.with_frame(|frame| {
            let env = self.new_environment(frame)?;
            env.raw_set_value(frame, "_G", &env.value())?;
            let print = self.print_gen.invoke::<Function, _>(frame, (format!("{}:", spec.name),))?;
            env.raw_set_value(frame, "print", &print)?;

            let loaded = frame.push_table(0, self.common_packages.len() + spec.packages.len())?;
            let add_package = |name: &str, package: &Value| -> Result<()> {
                if package.is_function() {
                    let factory = Function::from_value(package.clone())?;
                    let mut produced = factory.invoke_multi(frame, (spec.hidden_data,))?;
                    match produced.drain(..).next() {
                        Some(value) => loaded.raw_set_value(frame, name, &value),
                        None => Ok(()),
                    }
                } else {
                    loaded.raw_set_value(frame, name, package)
                }
            };
            for (name, package) in &self.common_packages {
                add_package(name, package)?;
            }
            for (name, package) in spec.packages {
                add_package(name, package)?;
            }
            let require =
                self.require_gen.invoke::<Function, _>(frame, (&env.value(), &loaded.value(), spec.loader))?;
            env.raw_set_value(frame, "require", &require)?;
            // SAFETY: the env table is on the frame; marking safe is a flag write.
            unsafe { ffi::lua_setsafeenv(frame.state(), env.index(), 1) };
            Ok(Instance {
                env: Table::from_value(Value::store(env.value())?)?,
                loaded: Table::from_value(Value::store(loaded.value())?)?,
            })
        })
    }

    /// A writable table indexing the base env through a frozen, protected metatable
    /// (`newEnvironment`). Left on top of `frame`.
    fn new_environment<'f>(&self, frame: &'f Frame<'_>) -> Result<crate::stack::TableView<'f>> {
        let env = frame.push_table(0, 4)?;
        frame.with_frame(|inner| {
            let metatable = inner.push_table(0, 2)?;
            metatable.raw_set_value(inner, "__index", &self.base_env)?;
            metatable.raw_set_value(inner, "__metatable", &false)?;
            let state = inner.state();
            // SAFETY: metatable on top of `inner`, env below it on `frame`; setmetatable pops.
            unsafe {
                ffi::lua_setreadonly(state, metatable.index(), 1);
                ffi::lua_setmetatable(state, env.index());
            }
            Ok(())
        })?;
        Ok(env)
    }

    /// Compiles `source` once and loads it on the loader thread (`loadScriptTemplate`). Binary
    /// chunks are rejected. With the `jit` feature and a runtime built with native code, the
    /// closure is compiled natively and the outcome is kept on the template. Takes the root
    /// stack; from inside a bound function (a `require` loader, say) use
    /// [`Sandbox::load_template_in`] with the call's scope.
    pub fn load_template(&self, runtime: &Runtime, chunk_name: &str, source: &str) -> Result<Template> {
        self.load_template_in(&runtime.stack(), runtime, chunk_name, source)
    }

    /// [`Sandbox::load_template`] on an existing scope of this runtime's VM.
    pub fn load_template_in(
        &self,
        scope: &impl Scope,
        runtime: &Runtime,
        chunk_name: &str,
        source: &str,
    ) -> Result<Template> {
        if source.as_bytes().starts_with(b"\x1bLua") {
            return Err(Error::runtime(format!("Binary Lua/Luau chunks are not supported: {chunk_name}")));
        }
        let name = CString::new(chunk_name).map_err(|_| Error::logic("Chunk name cannot contain NUL"))?;
        let bytecode = compile_raw(source, &self.compile_options)?;
        let _scope = runtime.call_scope(runtime.initialization_context(), CallKind::Initialization);
        scope.with_frame(|frame| {
            let thread = self.loader_thread.push_to(frame)?;
            let state = frame.state();
            // SAFETY: the loader thread is pinned and belongs to this VM; luau_load leaves one
            // value on it which xmove transfers to the frame, so the loader stays empty.
            unsafe {
                let loader = ffi::lua_tothread(state, thread.index());
                if loader.is_null() {
                    return Err(Error::logic("The template loader thread is missing"));
                }
                let status = ffi::luau_load(loader, name.as_ptr(), bytecode.as_ptr().cast(), bytecode.len(), 0);
                ffi::lua_xmove(loader, state, 1);
                if status != ffi::LUA_OK {
                    return Err(pop_error(state, status));
                }
            }
            #[cfg(feature = "jit")]
            let native = match runtime.native_code() {
                Some(generator) => Some(generator.compile(frame, -1, &bytecode)?),
                None => None,
            };
            Ok(Template {
                closure: Function::from_value(Value::store(frame.top_value())?)?,
                chunk_name: chunk_name.to_owned(),
                #[cfg(feature = "jit")]
                native,
            })
        })
    }

    /// A fresh closure sharing the template's prototype, running in `env` (or in the real
    /// globals when `None`) (`instantiateScriptTemplate`). Takes the root stack; from inside a
    /// bound function use [`Sandbox::instantiate_in`].
    pub fn instantiate(&self, runtime: &Runtime, template: &Template, env: Option<&Table>) -> Result<Function> {
        self.instantiate_in(&runtime.stack(), template, env)
    }

    /// [`Sandbox::instantiate`] on an existing scope of the template's VM.
    pub fn instantiate_in(&self, scope: &impl Scope, template: &Template, env: Option<&Table>) -> Result<Function> {
        let state = scope.state();
        if !template.closure.value().belongs_to(state) {
            return Err(Error::logic("Script template belongs to a different Lua state"));
        }
        if let Some(env) = env
            && !env.value().is_valid_on(state)
        {
            return Err(Error::logic("Script environment belongs to a different Lua state"));
        }
        scope.with_frame(|frame| {
            template.closure.push_to(frame)?;
            let state = frame.state();
            // SAFETY: the template is a Lua closure (luau_load made it); clonefunction pushes the
            // clone, the original is removed, and setfenv pops the env.
            unsafe {
                ffi::lua_clonefunction(state, -1);
                ffi::lua_remove(state, -2);
                if let Some(env) = env {
                    env.push_to(frame)?;
                    if ffi::lua_setfenv(state, -2) == 0 {
                        return Err(Error::logic("Unable to set the script environment"));
                    }
                }
            }
            Function::from_value(Value::store(frame.top_value())?)
        })
    }

    /// Instantiates `template` in `instance` and runs it inside a script call scope for
    /// `context`, returning everything the chunk returned.
    pub fn run(
        &self,
        runtime: &Runtime,
        template: &Template,
        instance: &Instance,
        context: CallContext,
    ) -> Result<Vec<Value>> {
        let script = self.instantiate(runtime, template, Some(&instance.env))?;
        let _scope = runtime.call_scope(context, CallKind::ScriptCall);
        script.invoke_multi(&runtime.stack(), ())
    }
}

fn require_same_vm(runtime: &Runtime, value: &Value, what: &str) -> Result<()> {
    if !value.is_valid() || !value.belongs_to(runtime.stack().state_ptr()) {
        return Err(Error::logic(format!("{what} belongs to a different Lua state")));
    }
    Ok(())
}

/// `getCompatMetamethod(value, name)`: the raw metatable field `name` of `value`, ignoring
/// `__metatable` protection, or nil. Lets the prelude honour `__pairs`/`__ipairs` on protected
/// tables and userdata.
unsafe extern "C-unwind" fn compat_metamethod(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            if ffi::lua_type(state, 2) != ffi::LUA_TSTRING {
                return Err(Error::runtime("getCompatMetamethod expects a metamethod name"));
            }
            if ffi::lua_getmetatable(state, 1) == 0 {
                ffi::lua_pushnil(state);
                return Ok(1);
            }
            ffi::lua_pushvalue(state, 2);
            ffi::lua_rawget(state, -2);
            Ok(1)
        })
    }
}

fn is_string_format_flag(byte: u8) -> bool {
    matches!(byte, b'-' | b'+' | b' ' | b'#' | b'0')
}

/// `string.format` with LuaJIT's `%s` semantics: any non-string argument matched by `%s` is
/// passed through `tostring` first, then the original formatter (upvalue 1) runs.
unsafe extern "C-unwind" fn compat_string_format(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        trampoline::enter(state, || {
            let top = ffi::lua_gettop(state);
            let mut length = 0usize;
            let data = ffi::lua_tolstring(state, 1, &mut length);
            if data.is_null() {
                return Err(crate::diagnostics::type_error_at(crate::stack::ValueView::resolve(state, 1), 1, "string"));
            }
            let format = std::slice::from_raw_parts(data.cast::<u8>(), length).to_vec();
            let mut argument: c_int = 1;
            let mut position = 0usize;
            while position < format.len() {
                let byte = format[position];
                position += 1;
                if byte != b'%' {
                    continue;
                }
                if position >= format.len() {
                    break;
                }
                if format[position] == b'%' {
                    position += 1;
                    continue;
                }
                argument += 1;
                if format[position] == b'*' {
                    position += 1;
                    continue;
                }
                while position < format.len() && is_string_format_flag(format[position]) {
                    position += 1;
                }
                let mut digits = 0;
                while digits < 2 && position < format.len() && format[position].is_ascii_digit() {
                    position += 1;
                    digits += 1;
                }
                if position < format.len() && format[position] == b'.' {
                    position += 1;
                    let mut digits = 0;
                    while digits < 2 && position < format.len() && format[position].is_ascii_digit() {
                        position += 1;
                        digits += 1;
                    }
                }
                if position >= format.len() {
                    break;
                }
                let indicator = format[position];
                position += 1;
                if indicator == b's' && argument <= top && ffi::lua_isstring(state, argument) == 0 {
                    ffi::luaL_tolstring(state, argument, std::ptr::null_mut());
                    ffi::lua_replace(state, argument);
                }
            }
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_insert(state, 1);
            {
                let _lua_call = crate::runtime::shared::LuaCall::enter(state);
                ffi::lua_call(state, top, ffi::LUA_MULTRET);
            }
            Ok(ffi::lua_gettop(state))
        })
    }
}
