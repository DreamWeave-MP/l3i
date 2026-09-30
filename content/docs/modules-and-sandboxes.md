+++
title = "Modules and sandboxes"
description = "Frozen package tables through LuauModule, read-only tables and views, OpenMW's sandbox prelude, per-script instances, compile-once templates, and Luau's require over a host navigator."
weight = 40

[extra]
kind = "guide"
+++

## Modules

A component crate knows how to expose itself to Luau but never owns the VM. It implements
`module::LuauModule`; the host owns the `Runtime`, decides which modules exist, and calls
`Runtime::register_module::<M>()`. The result is a frozen package table the host places
wherever its script environment wants it: a global, a `require` loader, a sandbox package list.

```rust
use std::cell::Cell;

use l3i::bind::{Call, StackResults};
use l3i::module::{LuauModule, ModuleBuilder};
use l3i::userdata::{Userdata, tagged};
use l3i::value::Table;
use l3i::{Result, Runtime};

struct Asset {
    name: String,
    loads: Cell<u32>,
}

unsafe impl Userdata for Asset {
    const NAME: &'static str = "dreamweave.assets.Asset";
}

struct AssetsModule;

impl LuauModule for AssetsModule {
    const NAME: &'static str = "dreamweave.assets";

    fn register(_runtime: &Runtime, module: &mut ModuleBuilder<'_>) -> Result<()> {
        module.userdata::<Asset>(Some(30), |ty| {
            ty.property("name", |asset: &Asset| asset.name.clone())?;
            ty.method("load", |asset: &Asset| {
                asset.loads.set(asset.loads.get() + 1);
                asset.loads.get()
            })
        })?;
        module.function("open", |call: &Call, name: &str| -> Result<StackResults> {
            tagged::push(call, Asset { name: name.to_owned(), loads: Cell::new(0) })?;
            Ok(StackResults)
        })?;
        module.set("VERSION", &3i32)?;
        module.metamethod("__tostring", |_: Table| "dreamweave.assets package")?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let runtime = Runtime::new()?;
    let assets = runtime.register_module::<AssetsModule>()?;
    runtime.set_global("assets", &assets)?;
    runtime.exec("local a = assets.open('tree.nif') assert(a.name == 'tree.nif' and a:load() == 1)")?;
    runtime.exec("assert(assets.VERSION == 3 and tostring(assets) == 'dreamweave.assets package')")?;
    Ok(())
}
```

`ModuleBuilder` is the `PackageBuilder` of the C++ binder: every function it binds is named
`<module>.<key>` under the host's debug roots, so `assets.open(5)` fails with
`dreamweave.assets.open: bad argument #1 (expected string)`. `NAME` is a dot-separated package
path rooted at one of the runtime's debug roots; `Runtime::module(path)` starts a builder by
hand and refuses a path outside them.

| `ModuleBuilder` | Does |
|---|---|
| `function(key, callable)` | Binds `callable` as `<path>.<key>` and stores it |
| `set(key, &value)` | Stores any pushable value |
| `userdata::<T>(Some(tag) or None, configure)` | Registers a userdata type, tagged or untagged, and configures its metatable |
| `metamethod(name, callable)`, `set_metafield(name, &value)` | The package's own metatable, created on first use |
| `path()`, `table()` | The package path and the table being built |
| `finish()` | Freezes the package and its metatable |

After `finish`, nothing can be added from scripts (`attempt to modify a readonly table`) or from
Rust except through `readonly::set_read_only_field`. `Runtime::set_global(name, &value)` and
`Runtime::global(name)` place and fetch globals.

## Read-only tables

`readonly` provides OpenMW's two flavours:

| Function | Effect |
|---|---|
| `make_read_only(&runtime, &table)` | Freezes the table in place (`lua_setreadonly`) |
| `make_strict_read_only(&runtime, &table)` | Freezes it with the shared strict metatable: a missing key raises `Key not found: <key>`. The table must not already be frozen or have a metatable |
| `make_read_only_view(&runtime, &table)` | A frozen proxy whose `__index` is the backing table, with `__pairs`, `__iter`, `__ipairs` and `__len` that iterate and measure the backing table without exposing it, and `__metatable = false`. A view of a view is the same view |
| `make_strict_read_only_view(&runtime, &table)` | The same, raising `Key not found` for a missing key |
| `set_read_only_field(&runtime, &table, key, &value)` | Sets a key on a frozen table, or on the backing table of a view, and restores the frozen state; for tables the host owns |
| `make_frozen_package(&runtime, &package, &to_string)` | A metatable with only `__tostring`, then frozen; what `ModuleBuilder::finish` does |

Writes to a backing table stay visible through its view. Stock Luau's `pairs` and `ipairs`
ignore `__pairs` and `__ipairs`; the generic `for` honours `__iter`, and the sandbox prelude
below replaces `pairs` and `ipairs` with versions that honour them.

## Sandboxes

`Runtime::sandbox(log, options)` prepares a sandbox on the VM, as OpenMW's `Lua::State` does.
Call it once, after every module the base environment should expose is registered. `log`
receives each `print` line, already tab-joined and prefixed with the instance's name.

- The **base environment** copies every string-keyed global except `_G` and the environment
  escape hatches (`getfenv`, `setfenv`, `newproxy`), exposes tables as read-only views, and
  replaces `getmetatable` with a tables-only version. `print` and `require` are left out
  because every instance gets its own. The base environment is frozen and marked safe so Luau's
  import fast paths apply. The real `_G` is never frozen.
- An **instance** is a writable table whose frozen metatable indexes the base environment, with
  `_G`, a named `print`, a `loaded` table of packages, and a `require` reading from it.
- A **template** is a chunk compiled once and loaded on a loader thread whose globals are the
  base environment, then instantiated per script with `lua_clonefunction` plus `lua_setfenv`.
  Because the base environment is marked safe, Luau resolves the chunk's builtin imports
  (`math.sqrt`, a module placed as a global, ...) against it when the template loads, and every
  instance, whose environment is marked safe too, takes the fast import path from its first run.
  The trade is Luau's own: a global the base environment holds cannot be shadowed by a write
  from another chunk into the instance; a chunk that assigns the name itself compiles its reads
  as lookups, and `print` and `require`, which the base environment leaves out, resolve per
  instance. OpenMW loads templates on a thread with empty, unsafe globals and looks every import
  up at run time.

`SandboxOptions` holds the compile options for templates and three compatibility switches:
`compat_iterators` (on by default: `pairs` and `ipairs` honouring `__pairs` and `__ipairs`, which
read-only views and iterable userdata depend on), `compat_string_format` (off: `string.format`
applies `tostring` to any `%s` argument, as LuaJIT does), and `neuter_randomseed` (off: `math.random`
is seeded once from the clock and `math.randomseed` becomes a no-op).

```rust
use l3i::runtime::{CallContext, MemoryCategory};
use l3i::sandbox::{InstanceSpec, SandboxOptions};
use l3i::{Result, Runtime};

fn main() -> Result<()> {
    let runtime = Runtime::new()?;
    let sandbox = runtime.sandbox(|line| println!("[script] {line}"), SandboxOptions::default())?;
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end")?;
    let instance = sandbox.new_instance(
        &runtime,
        &InstanceSpec { name: "player", packages: &[], hidden_data: None, loader: &loader },
    )?;
    let template = sandbox.load_template(
        &runtime,
        "player.lua",
        "counter = (counter or 0) + 1 print('run', counter) return counter",
    )?;
    let context = CallContext { id: 1, category: MemoryCategory(3) };
    sandbox.run(&runtime, &template, &instance, context)?;
    let results = sandbox.run(&runtime, &template, &instance, context)?;
    let count = results[0].with_value(&runtime.stack(), |_, view| view.read::<i32>())?;
    assert_eq!(count, 2);
    Ok(())
}
```

It prints `[script] player:\trun\t1` and then `[script] player:\trun\t2`. The instance kept its
own `counter`; the real globals never saw it, and a second instance would start at 1.

| Call | Does |
|---|---|
| `Sandbox::base_env()` | The frozen base environment every instance indexes |
| `Sandbox::common_packages()`, `add_common_package(&runtime, name, value)` | Packages every instance receives: the base environment's tables and userdata plus those added. A table is frozen in place; a function is a factory called once per instance with the hidden data |
| `Sandbox::new_instance(&runtime, &spec)` | One instance, built inside an initialization call scope |
| `Sandbox::load_template(&runtime, chunk_name, source)` | Compiles once and loads on the loader thread, with its imports resolved against the base environment. Binary chunks are rejected; a syntax error is `Err` with Luau's message |
| `Sandbox::instantiate(&runtime, &template, env)` | A fresh closure sharing the template's prototype, running in `env`, or in the real globals for `None` |
| `Sandbox::run(&runtime, &template, &instance, context)` | Instantiates in the instance and runs it inside a script call scope, returning everything the chunk returned |
| `Template::chunk_name()` | The name the template was loaded under |

`InstanceSpec` names the instance (`print` prefixes its output with `name:`), lists per-instance
packages that override the common ones by name, passes optional hidden data to package
factories, and names the loader behind `require`: called as `loader(name, env)` for a name
missing from `loaded`, it must return a function that, called with `name`, produces the package.
An `Instance` exposes its `env` and `loaded` tables.

### Loaders written in Rust

`load_template` and `instantiate` take the root stack, which is suspended while a script call
is running. A `require` loader bound with `bind_function` runs inside that call, so it uses
`Sandbox::load_template_in(scope, chunk_name, source)` and
`Sandbox::instantiate_in(scope, &template, env)` with the call as the scope. Neither needs the
`Runtime`: the initialization context and the native code generator live with the VM, so a
loader captures only the sandbox:

```rust
use std::rc::Rc;

use l3i::bind::{Call, StackResults};
use l3i::sandbox::{Sandbox, SandboxOptions};
use l3i::value::{Function, Table, Value};
use l3i::{Result, Runtime};

fn loader(runtime: &Rc<Runtime>, sandbox: &Rc<Sandbox>) -> Result<Function> {
    let sandbox = Rc::clone(sandbox);
    runtime.bind_function("dreamweave.loader", move |call: &Call, name: &str, env: Value| -> Result<StackResults> {
        if name != "util" {
            return Err(l3i::Error::runtime(format!("module '{name}' not found")));
        }
        let env = Table::from_value(env)?;
        let source = "return function(name) return { twice = function(x) return x * 2 end } end";
        let template = sandbox.load_template_in(call, "util.lua", source)?;
        let factory = sandbox.instantiate_in(call, &template, Some(&env))?;
        factory.invoke::<Function, _>(call, ())?.value().push_to_scope(call)?;
        Ok(StackResults)
    })
}

fn main() -> Result<()> {
    let runtime = Rc::new(Runtime::new()?);
    let sandbox = Rc::new(runtime.sandbox(|line| println!("{line}"), SandboxOptions::default())?);
    let _loader = loader(&runtime, &sandbox)?;
    Ok(())
}
```

## Luau's own sandbox

`Runtime::sandbox_globals()` (also `sandbox_luau()`) is `luaL_sandbox`: every library table and
the globals table become read-only, the globals table is marked safe, and the string metatable
gets read-only protection. After it, `Thread::sandbox(&scope)` gives a coroutine its own
writable globals table that proxies the frozen main globals, which is how a chunk loaded on that
thread writes without touching the real globals. `Runtime::load_with_env` loads one chunk with
its globals resolving through a table of the host's choosing instead.

## `require` over a host navigator

Luau resolves `require("./path")` by walking a navigator the host provides: reset to the
requiring module, step to parents and children, ask whether a module is present, and load it.
The host implements `require::RequireNavigator` over whatever its module space is (files, a VFS,
an archive, generated code), and the binder mounts it through Luau's own implementation, which
supplies caching, cyclic-require placeholders, `.luaurc` alias handling, and the error messages
scripts see.

| Method | Meaning |
|---|---|
| `reset(requirer_chunkname)` | Point the position at the requiring module |
| `to_parent()`, `to_child(name)` | One navigation step; each returns `Navigate::Success`, `Ambiguous` or `NotFound` |
| `is_module_present()` | Whether the position names a loadable module |
| `chunkname()`, `loadname()`, `cache_key()` | The chunk name the module runs under, the name passed to `load`, and the key Luau caches the result under |
| `load(scope, path, chunkname, loadname)` | Run the module on the requiring thread and return `Load::Results(n)` or `Load::Yield`; an error raises into the requiring script |
| `is_require_allowed`, `jump_to_alias`, `to_alias_override`, `to_alias_fallback`, `config_status`, `config`, `luau_config_timeout_ms` | Optional, with defaults: permission, `@alias` resolution, and `.luaurc` contents |

`Runtime::install_require(navigator)` registers Luau's `require` as a global;
`require_function(navigator)` returns the closure pinned for a sandbox to hand to instances
itself. `proxy_require_function()` is a `proxyrequire(path, chunkname)` closure that resolves a
path as if required from a named module. `register_require_module(path, &value)` makes a value
the permanent result of requiring an alias, and `clear_require_cache_entry(key)` and
`clear_require_cache()` drop cached results. A module that is loaded twice in a cycle sees a
locked placeholder table; the `require::RequirePlaceholders` trait on any scope creates,
locks and populates one from inside `load`.
