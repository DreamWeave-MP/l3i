+++
title = "The raw C API"
description = "The ffi module: every Luau 0.740 C entry point declared by hand; TAG_LIMIT and LUAU_VERSION; the rules for hand-written lua_CFunctions."
weight = 270

[extra]
kind = "api"
+++

Module `l3i::ffi` and the two constants at the crate root. [Safety](@/docs/safety.md) explains
the error model these rules come from; [Building](@/docs/building.md) covers the Luau build they
depend on.

## ffi

{{ api_signature(value="pub mod ffi") }}

The raw Luau C API for hand-written native functions, transcribed from Luau 0.740's headers.
Everything here is `unsafe`; prefer the safe layers. Names, argument order and types follow the
headers exactly, so the C++ binder's code reads across one to one.

| Header | Declares |
|---|---|
| `VM/include/luaconf.h` | The values the VM was compiled with: `LUAI_MAXCSTACK` 8000, `LUA_MINSTACK` 20, `LUA_IDSIZE` 256, `LUA_MEMORY_CATEGORIES` 256, `LUA_VECTOR_SIZE` 3 |
| `VM/include/lua.h` | The status codes (`LUA_OK`, `LUA_YIELD`, `LUA_ERRRUN`, `LUA_ERRSYNTAX`, `LUA_ERRMEM`, `LUA_ERRERR`, `LUA_BREAK`), coroutine statuses, type tags (`LUA_TNONE` to `LUA_TBUFFER`, including `LUA_TINTEGER` and `LUA_TVECTOR`), the pseudo-indexes and `lua_upvalueindex`, `lua_State`, `lua_Debug`, `lua_Callbacks`, the callback types (`lua_CFunction`, `lua_Continuation`, `lua_Alloc`, `lua_CageAlloc`, `lua_Destructor`, `lua_UserdataMark`, `lua_EmbedderMark`, `lua_EmbedderGc`, `lua_CategoryName`, `lua_UserdataDirectAccess`, `lua_UserdataDirectNamecall`, `lua_UserdataDirectFieldGet`, `lua_Hook`, `lua_Coverage`, `lua_CounterFunction`, `lua_CounterValue`), and every function |
| `VM/include/lualib.h` | `luaL_Reg`, `luaL_Strbuf` and `LUA_BUFFERSIZE`, `LUA_LUTAG_LIMIT`, the `luaopen_*` openers, `luaL_openlibs`, `luaL_sandbox`, `luaL_sandboxthread`, the `luaL_check*`/`luaL_opt*` argument helpers, `luaL_errorL`, `luaL_typeerrorL`, `luaL_traceback`, `luaL_findtable`, `luaL_newmetatable`, the string buffer functions |
| `Compiler/include/luacode.h` | `lua_CompileOptions`, `lua_CompileConstant`, the library member callback types, `luau_compile`, `luau_set_compile_constant_*` |
| `CodeGen/include/luacodegen.h` | `luau_codegen_supported`, `luau_codegen_create`, `luau_codegen_compile` |
| `Inliner/include/luajitinliner.h` | `luau_enable_jit_inliner`, `luau_disable_jit_inliner` |
| `Require/include/Luau/Require.h` | `luarequire_Configuration` and its result types, `luaopen_require`, `luarequire_pushrequire`, `luarequire_pushproxyrequire`, `luarequire_registermodule`, `luarequire_clearcacheentry`, `luarequire_clearcache`, the placeholder functions |
| `csrc/extra.cpp` (l3i's own shims) | Fast-flag access (`luau_setfflag`, `luau_getfflag`, `luau_setfint`, `luau_getfint`, `luau_visitfflags`), `lua_getmetatablepointer`, and the value-layout readers the binder's hot paths use (`l3i_read_scalar`, `l3i_read_vector`, `l3i_touserdata_tag`, `l3i_native_enter`, `l3i_direct_enter`, `l3i_call_base`, `l3i_stack_slot`) |

The macros of `lua.h` and `lualib.h` (`lua_pop`, `lua_newtable`, `lua_isfunction`,
`lua_tostring`, `lua_tonumber`, `lua_pushcfunction`, `lua_pushcclosure`, `lua_getref`,
`lua_setglobal`, `lua_getglobal`, `luaL_getmetatable`) are inline functions with the same names. C `free` and `fopen`/`fclose` are declared for the buffers
`luau_compile` returns and for the dump functions.

Every function in those headers is declared, with one exception: the varargs
`lua_pushvfstring`, which takes a C `va_list` and has no Rust spelling. `lua_pushfstringL` and
`luaL_errorL`, which take C varargs, are declared and callable with matching arguments.

Luau is built with C++ exceptions, not `longjmp`, so every entry point that can raise is declared
`extern "C-unwind"`; the callbacks Luau documents as non-raising (`lua_Alloc`, `lua_Destructor`,
the mark and counter callbacks) are plain `extern "C"`.

## TAG_LIMIT

{{ api_signature(value="const TAG_LIMIT: u8 = 254") }}

The number of userdata tags the linked Luau VM was compiled with (`LUA_UTAG_LIMIT`). l3i builds
Luau with 254, the most Luau can address; `build.rs` is the single owner of that define and the
constant mirrors it through the `L3I_TAG_LIMIT` build variable. Valid runtime tags are
`1..TAG_LIMIT`; tag 0 is Luau's untagged default and is never registered. Nothing else is
reserved: which tag a type gets is the host's or the planner's choice, per runtime.

## LUAU_VERSION

{{ api_signature(value='const LUAU_VERSION: &str = "0.740"') }}

The Luau release the linked VM was built from: the release OpenMW pins, as the `luau/` git
submodule. The flag policy names this version in its error when a flag does not exist.

## Hand-written native functions

A `lua_CFunction` written against `ffi` runs under Luau's rules, not Rust's:

- **It is `unsafe extern "C-unwind"`.** Luau calls it with the running thread; the function
  reads its arguments at `1..=lua_gettop(L)` and returns how many results it pushed.
- **Luau errors unwind through Rust frames.** A `luaL_errorL`, `lua_error`, or any API call that
  raises (an out-of-memory allocation, a `__index` metamethod that errors) throws a C++ exception
  that unwinds through the Rust frames to Luau's `pcall`. Destructors run. Keep no Rust value
  that must observe the raise alive across a raising call, and write the function so that every
  raise happens after the values it would skip have been dropped.
- **Never put `catch_unwind` between a Luau raise and Luau's `pcall`.** Rust aborts on a
  foreign exception crossing `catch_unwind`. Host-level code, where no Lua call is above the
  Rust frame, routes raising API calls through `lua_pcall`; that is what the safe layers do with
  `Frame::is_host_level`.
- **Panics abort.** A Rust panic inside a native call cannot unwind into Luau's C++ frames, so
  the binder's trampolines turn one into `std::process::abort`. Do not panic in a native
  function; return an error instead.
- **GC destructors never call the Lua API.** A `lua_Destructor` runs while the collector sweeps;
  it drops its payload and nothing more, and a panic there aborts too.
- **Prefer `native::enter`.** It wraps the body with a `Stack`, turns a returned `Err` into the
  Lua error after every Rust value has dropped, and installs the panic guard, so the rules above
  hold by construction. `MetatableBuilder::add_native_method` and `Runtime::register_library`
  are the registration paths for such functions.

```rust
use std::ffi::c_int;
use l3i::ffi;
use l3i::userdata::tagged;

unsafe extern "C-unwind" fn make_probe(state: *mut ffi::lua_State) -> c_int {
    unsafe {
        l3i::native::enter(state, |stack| {
            let value = ffi::lua_tonumber(state, 1);
            tagged::push(stack, Probe::new(value))?;
            Ok(1)
        })
    }
}
```
