//! Luau 0.740 C API, transcribed from `VM/include/lua.h`, `VM/include/lualib.h`,
//! `Compiler/include/luacode.h`, `CodeGen/include/luacodegen.h`, and the two helpers
//! `luau0-src` adds in `Custom/`.
//!
//! Luau is built with C++ exceptions (not longjmp), so every entry point that can raise is
//! declared `extern "C-unwind"`. Names and argument order follow the headers exactly so the
//! C++ binder's code reads across one to one.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

// ---------------------------------------------------------------------------------------------
// luaconf.h values the VM was compiled with (see build.rs)
// ---------------------------------------------------------------------------------------------

pub const LUAI_MAXCSTACK: c_int = 8000;
pub const LUA_MINSTACK: c_int = 20;
pub const LUA_IDSIZE: usize = 256;
pub const LUA_MEMORY_CATEGORIES: c_int = 256;
pub const LUA_VECTOR_SIZE: usize = 3;

pub const LUA_MULTRET: c_int = -1;

pub const LUA_REGISTRYINDEX: c_int = -LUAI_MAXCSTACK - 2000;
pub const LUA_ENVIRONINDEX: c_int = -LUAI_MAXCSTACK - 2001;
pub const LUA_GLOBALSINDEX: c_int = -LUAI_MAXCSTACK - 2002;

#[inline]
pub const fn lua_upvalueindex(i: c_int) -> c_int {
    LUA_GLOBALSINDEX - i
}

#[inline]
pub const fn lua_ispseudo(i: c_int) -> bool {
    i <= LUA_REGISTRYINDEX
}

// lua_Status
pub const LUA_OK: c_int = 0;
pub const LUA_YIELD: c_int = 1;
pub const LUA_ERRRUN: c_int = 2;
pub const LUA_ERRSYNTAX: c_int = 3;
pub const LUA_ERRMEM: c_int = 4;
pub const LUA_ERRERR: c_int = 5;
pub const LUA_BREAK: c_int = 6;

// lua_CoStatus
pub const LUA_CORUN: c_int = 0;
pub const LUA_COSUS: c_int = 1;
pub const LUA_CONOR: c_int = 2;
pub const LUA_COFIN: c_int = 3;
pub const LUA_COERR: c_int = 4;

// lua_Type (LUA_VECTOR_DOUBLE == 0 layout)
pub const LUA_TNONE: c_int = -1;
pub const LUA_TNIL: c_int = 0;
pub const LUA_TBOOLEAN: c_int = 1;
pub const LUA_TLIGHTUSERDATA: c_int = 2;
pub const LUA_TNUMBER: c_int = 3;
pub const LUA_TINTEGER: c_int = 4;
pub const LUA_TVECTOR: c_int = 5;
pub const LUA_TSTRING: c_int = 6;
pub const LUA_TTABLE: c_int = 7;
pub const LUA_TFUNCTION: c_int = 8;
pub const LUA_TUSERDATA: c_int = 9;
pub const LUA_TTHREAD: c_int = 10;
pub const LUA_TBUFFER: c_int = 11;
pub const LUA_TCLASS: c_int = 12;
pub const LUA_TOBJECT: c_int = 13;

// lua_GCOp
pub const LUA_GCSTOP: c_int = 0;
pub const LUA_GCRESTART: c_int = 1;
pub const LUA_GCCOLLECT: c_int = 2;
pub const LUA_GCCOUNT: c_int = 3;
pub const LUA_GCCOUNTB: c_int = 4;
pub const LUA_GCISRUNNING: c_int = 5;
pub const LUA_GCSTEP: c_int = 6;
pub const LUA_GCSETGOAL: c_int = 7;
pub const LUA_GCSETSTEPMUL: c_int = 8;
pub const LUA_GCSETSTEPSIZE: c_int = 9;
pub const LUA_GCISPAUSED: c_int = 10;

pub const LUA_NOREF: c_int = -1;
pub const LUA_REFNIL: c_int = 0;

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

#[repr(C)]
pub struct lua_State {
    _opaque: [u8; 0],
}

pub type lua_Number = f64;
pub type lua_Integer = c_int;
pub type lua_Unsigned = c_uint;
pub type LUA_VECTOR_TYPE = f32;

pub type lua_CFunction = unsafe extern "C-unwind" fn(L: *mut lua_State) -> c_int;
pub type lua_Continuation = unsafe extern "C-unwind" fn(L: *mut lua_State, status: c_int) -> c_int;
pub type lua_Alloc = unsafe extern "C" fn(ud: *mut c_void, ptr: *mut c_void, osize: usize, nsize: usize) -> *mut c_void;
pub type lua_CageAlloc =
    unsafe extern "C" fn(ud: *mut c_void, ptr: *mut c_void, osize: usize, nsize: usize, r#type: c_int) -> *mut c_void;

/// Runs during GC traversal: must not touch the Lua API and must not unwind.
pub type lua_Destructor = unsafe extern "C" fn(L: *mut lua_State, userdata: *mut c_void);
pub type lua_UserdataMark = unsafe extern "C" fn(L: *mut lua_State, ud: *mut c_void);
pub type lua_EmbedderMark = unsafe extern "C" fn(L: *mut lua_State, r#ref: c_int);
/// `markref` is null for the reset call at the start of a cycle.
pub type lua_EmbedderGc = unsafe extern "C" fn(L: *mut lua_State, markref: Option<lua_EmbedderMark>);
pub type lua_CategoryName = unsafe extern "C" fn(L: *mut lua_State, memcat: u8) -> *const c_char;

/// `void (*)(lua_State*, void* data, int atom, uint16_t* cachedslot, int utag)`
pub type lua_UserdataDirectAccess =
    unsafe extern "C-unwind" fn(L: *mut lua_State, data: *mut c_void, atom: c_int, cachedslot: *mut u16, utag: c_int);
/// `int (*)(lua_State*, void* data, int atom, uint16_t* cachedslot, int utag)`
pub type lua_UserdataDirectNamecall = unsafe extern "C-unwind" fn(
    L: *mut lua_State,
    data: *mut c_void,
    atom: c_int,
    cachedslot: *mut u16,
    utag: c_int,
) -> c_int;
/// `void (*)(void* ud, void* result)`; write `result` with `lua_userdatadirectfield_set*`.
pub type lua_UserdataDirectFieldGet = unsafe extern "C" fn(ud: *mut c_void, result: *mut c_void);

pub type lua_Hook = unsafe extern "C-unwind" fn(L: *mut lua_State, ar: *mut lua_Debug);
pub type lua_Coverage = unsafe extern "C" fn(
    context: *mut c_void,
    function: *const c_char,
    linedefined: c_int,
    depth: c_int,
    hits: *const c_int,
    size: usize,
);
pub type lua_CounterFunction = unsafe extern "C" fn(context: *mut c_void, function: *const c_char, linedefined: c_int);
pub type lua_CounterValue = unsafe extern "C" fn(context: *mut c_void, kind: c_int, line: c_int, hits: u64);

#[repr(C)]
pub struct lua_Debug {
    pub name: *const c_char,
    pub what: *const c_char,
    pub source: *const c_char,
    pub short_src: *const c_char,
    pub linedefined: c_int,
    pub currentline: c_int,
    pub protoid: c_int,
    pub bytecodeid: c_int,
    pub nupvals: u8,
    pub nparams: u8,
    pub isvararg: c_char,
    pub userdata: *mut c_void,
    pub ssbuf: [c_char; LUA_IDSIZE],
}

/// Field order is ABI; it mirrors `struct lua_Callbacks` in lua.h exactly.
#[repr(C)]
pub struct lua_Callbacks {
    pub userdata: *mut c_void,
    pub interrupt: Option<unsafe extern "C-unwind" fn(L: *mut lua_State, gc: c_int)>,
    pub panic: Option<unsafe extern "C-unwind" fn(L: *mut lua_State, errcode: c_int)>,
    pub userthread: Option<unsafe extern "C" fn(LP: *mut lua_State, L: *mut lua_State)>,
    pub useratom: Option<unsafe extern "C" fn(L: *mut lua_State, s: *const c_char, l: usize) -> i16>,
    pub userfinalizer: Option<unsafe extern "C" fn(L: *mut lua_State, co: *mut lua_State)>,
    pub debugbreak: Option<unsafe extern "C-unwind" fn(L: *mut lua_State, ar: *mut lua_Debug)>,
    pub debugstep: Option<unsafe extern "C-unwind" fn(L: *mut lua_State, ar: *mut lua_Debug)>,
    pub debuginterrupt: Option<unsafe extern "C-unwind" fn(L: *mut lua_State, ar: *mut lua_Debug)>,
    pub debugprotectederror: Option<unsafe extern "C-unwind" fn(L: *mut lua_State)>,
    pub onallocate: Option<
        unsafe extern "C" fn(
            L: *mut lua_State,
            block: *mut c_void,
            osize: usize,
            nsize: usize,
            memcat: u8,
            tt: c_int,
            tag: c_int,
        ),
    >,
    pub preresume: Option<unsafe extern "C" fn(L: *mut lua_State)>,
    pub postresume: Option<unsafe extern "C" fn(L: *mut lua_State)>,
    pub onfree: Option<unsafe extern "C" fn(L: *mut lua_State, block: *mut c_void)>,
}

#[repr(C)]
pub struct luaL_Reg {
    pub name: *const c_char,
    pub func: Option<lua_CFunction>,
}

/// `lua_CompileOptions` from luacode.h. Pointer members are borrowed for the duration of one
/// `luau_compile` call.
#[repr(C)]
pub struct lua_CompileOptions {
    pub optimizationLevel: c_int,
    pub debugLevel: c_int,
    pub typeInfoLevel: c_int,
    pub coverageLevel: c_int,
    pub vectorLib: *const c_char,
    pub vectorCtor: *const c_char,
    pub vectorType: *const c_char,
    pub vectorPrecision: c_int,
    pub mutableGlobals: *const *const c_char,
    pub userdataTypes: *const *const c_char,
    pub librariesWithKnownMembers: *const *const c_char,
    pub libraryMemberTypeCb: Option<lua_LibraryMemberTypeCallback>,
    pub libraryMemberConstantCb: Option<lua_LibraryMemberConstantCallback>,
    pub disabledBuiltins: *const *const c_char,
}

pub type lua_CompileConstant = *mut c_void;
pub type lua_LibraryMemberTypeCallback = unsafe extern "C" fn(library: *const c_char, member: *const c_char) -> c_int;
pub type lua_LibraryMemberConstantCallback =
    unsafe extern "C" fn(library: *const c_char, member: *const c_char, constant: *mut lua_CompileConstant);

// ---------------------------------------------------------------------------------------------
// lua.h
// ---------------------------------------------------------------------------------------------

unsafe extern "C-unwind" {
    // state manipulation
    pub fn lua_newstate(allocator: lua_Alloc, ud: *mut c_void) -> *mut lua_State;
    pub fn lua_close(L: *mut lua_State);
    pub fn lua_newthread(L: *mut lua_State) -> *mut lua_State;
    pub fn lua_mainthread(L: *mut lua_State) -> *mut lua_State;
    pub fn lua_resetthread(L: *mut lua_State);
    pub fn lua_isthreadreset(L: *mut lua_State) -> c_int;

    // basic stack manipulation
    pub fn lua_absindex(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_gettop(L: *mut lua_State) -> c_int;
    pub fn lua_settop(L: *mut lua_State, idx: c_int);
    pub fn lua_pushvalue(L: *mut lua_State, idx: c_int);
    pub fn lua_remove(L: *mut lua_State, idx: c_int);
    pub fn lua_insert(L: *mut lua_State, idx: c_int);
    pub fn lua_replace(L: *mut lua_State, idx: c_int);
    pub fn lua_checkstack(L: *mut lua_State, sz: c_int) -> c_int;
    pub fn lua_rawcheckstack(L: *mut lua_State, sz: c_int);
    pub fn lua_xmove(from: *mut lua_State, to: *mut lua_State, n: c_int);
    pub fn lua_xpush(from: *mut lua_State, to: *mut lua_State, idx: c_int);

    // access functions (stack -> C)
    pub fn lua_isnumber(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_isstring(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_isinteger64(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_iscfunction(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_isLfunction(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_isuserdata(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_type(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_typename(L: *mut lua_State, tp: c_int) -> *const c_char;
    pub fn lua_equal(L: *mut lua_State, idx1: c_int, idx2: c_int) -> c_int;
    pub fn lua_rawequal(L: *mut lua_State, idx1: c_int, idx2: c_int) -> c_int;
    pub fn lua_lessthan(L: *mut lua_State, idx1: c_int, idx2: c_int) -> c_int;
    pub fn lua_tonumberx(L: *mut lua_State, idx: c_int, isnum: *mut c_int) -> f64;
    pub fn lua_tointegerx(L: *mut lua_State, idx: c_int, isnum: *mut c_int) -> c_int;
    pub fn lua_tounsignedx(L: *mut lua_State, idx: c_int, isnum: *mut c_int) -> c_uint;
    pub fn lua_tovector(L: *mut lua_State, idx: c_int) -> *const LUA_VECTOR_TYPE;
    pub fn lua_toboolean(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tointeger64(L: *mut lua_State, idx: c_int, isinteger: *mut c_int) -> i64;
    pub fn lua_tolstring(L: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    pub fn lua_tostringatom(L: *mut lua_State, idx: c_int, atom: *mut c_int) -> *const c_char;
    pub fn lua_tolstringatom(L: *mut lua_State, idx: c_int, len: *mut usize, atom: *mut c_int) -> *const c_char;
    pub fn lua_namecallatom(L: *mut lua_State, atom: *mut c_int) -> *const c_char;
    pub fn lua_objlen(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tocfunction(L: *mut lua_State, idx: c_int) -> Option<lua_CFunction>;
    pub fn lua_tolightuserdata(L: *mut lua_State, idx: c_int) -> *mut c_void;
    pub fn lua_tolightuserdatatagged(L: *mut lua_State, idx: c_int, tag: c_int) -> *mut c_void;
    pub fn lua_touserdata(L: *mut lua_State, idx: c_int) -> *mut c_void;
    pub fn lua_touserdatatagged(L: *mut lua_State, idx: c_int, tag: c_int) -> *mut c_void;
    pub fn lua_userdatatag(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_lightuserdatatag(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tothread(L: *mut lua_State, idx: c_int) -> *mut lua_State;
    pub fn lua_tobuffer(L: *mut lua_State, idx: c_int, len: *mut usize) -> *mut c_void;
    pub fn lua_topointer(L: *mut lua_State, idx: c_int) -> *const c_void;

    // push functions (C -> stack)
    pub fn lua_pushnil(L: *mut lua_State);
    pub fn lua_pushnumber(L: *mut lua_State, n: f64);
    pub fn lua_pushinteger(L: *mut lua_State, n: c_int);
    pub fn lua_pushinteger64(L: *mut lua_State, n: i64);
    pub fn lua_pushunsigned(L: *mut lua_State, n: c_uint);
    pub fn lua_pushvector(L: *mut lua_State, x: LUA_VECTOR_TYPE, y: LUA_VECTOR_TYPE, z: LUA_VECTOR_TYPE);
    pub fn lua_pushlstring(L: *mut lua_State, s: *const c_char, l: usize);
    pub fn lua_pushstring(L: *mut lua_State, s: *const c_char);
    pub fn lua_pushfstringL(L: *mut lua_State, fmt: *const c_char, ...) -> *const c_char;
    pub fn lua_pushcclosurek(
        L: *mut lua_State,
        r#fn: lua_CFunction,
        debugname: *const c_char,
        nup: c_int,
        cont: Option<lua_Continuation>,
    );
    pub fn lua_pushboolean(L: *mut lua_State, b: c_int);
    pub fn lua_pushthread(L: *mut lua_State) -> c_int;
    pub fn lua_pushlightuserdatatagged(L: *mut lua_State, p: *mut c_void, tag: c_int);
    pub fn lua_newuserdatatagged(L: *mut lua_State, sz: usize, tag: c_int) -> *mut c_void;
    pub fn lua_newuserdatataggedwithmetatable(L: *mut lua_State, sz: usize, tag: c_int) -> *mut c_void;
    pub fn lua_newuserdatadtor(L: *mut lua_State, sz: usize, dtor: lua_Destructor) -> *mut c_void;
    pub fn lua_newbuffer(L: *mut lua_State, sz: usize) -> *mut c_void;

    // get functions (Lua -> stack)
    pub fn lua_gettable(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_getfield(L: *mut lua_State, idx: c_int, k: *const c_char) -> c_int;
    pub fn lua_rawgetfield(L: *mut lua_State, idx: c_int, k: *const c_char) -> c_int;
    pub fn lua_rawget(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_rawgeti(L: *mut lua_State, idx: c_int, n: c_int) -> c_int;
    pub fn lua_rawgetptagged(L: *mut lua_State, idx: c_int, p: *mut c_void, tag: c_int) -> c_int;
    pub fn lua_createtable(L: *mut lua_State, narr: c_int, nrec: c_int);
    pub fn lua_setreadonly(L: *mut lua_State, idx: c_int, enabled: c_int);
    pub fn lua_getreadonly(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_setsafeenv(L: *mut lua_State, idx: c_int, enabled: c_int);
    pub fn lua_getmetatable(L: *mut lua_State, objindex: c_int) -> c_int;
    pub fn lua_getfenv(L: *mut lua_State, idx: c_int);

    // set functions (stack -> Lua)
    pub fn lua_settable(L: *mut lua_State, idx: c_int);
    pub fn lua_setfield(L: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_rawsetfield(L: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_rawset(L: *mut lua_State, idx: c_int);
    pub fn lua_rawseti(L: *mut lua_State, idx: c_int, n: c_int);
    pub fn lua_rawsetptagged(L: *mut lua_State, idx: c_int, p: *mut c_void, tag: c_int);
    pub fn lua_setmetatable(L: *mut lua_State, objindex: c_int) -> c_int;
    pub fn lua_setfenv(L: *mut lua_State, idx: c_int) -> c_int;

    // load and call
    pub fn luau_load(
        L: *mut lua_State,
        chunkname: *const c_char,
        data: *const c_char,
        size: usize,
        env: c_int,
    ) -> c_int;
    pub fn lua_call(L: *mut lua_State, nargs: c_int, nresults: c_int);
    pub fn lua_pcall(L: *mut lua_State, nargs: c_int, nresults: c_int, errfunc: c_int) -> c_int;
    pub fn lua_cpcall(L: *mut lua_State, func: lua_CFunction, ud: *mut c_void) -> c_int;
    pub fn lua_callyieldable(L: *mut lua_State, nargs: c_int, nresults: c_int) -> c_int;
    pub fn lua_pcallyieldable(L: *mut lua_State, nargs: c_int, nresults: c_int, errfunc: c_int) -> c_int;

    // coroutines
    pub fn lua_yield(L: *mut lua_State, nresults: c_int) -> c_int;
    pub fn lua_break(L: *mut lua_State) -> c_int;
    pub fn lua_resume(L: *mut lua_State, from: *mut lua_State, narg: c_int) -> c_int;
    pub fn lua_resumeerror(L: *mut lua_State, from: *mut lua_State) -> c_int;
    pub fn lua_status(L: *mut lua_State) -> c_int;
    pub fn lua_isyieldable(L: *mut lua_State) -> c_int;
    pub fn lua_getthreaddata(L: *mut lua_State) -> *mut c_void;
    pub fn lua_setthreaddata(L: *mut lua_State, data: *mut c_void);
    pub fn lua_costatus(L: *mut lua_State, co: *mut lua_State) -> c_int;

    // garbage collection and memory
    pub fn lua_gc(L: *mut lua_State, what: c_int, data: c_int) -> c_int;
    pub fn lua_memorydump(L: *mut lua_State, file: *mut c_void, categoryName: Option<lua_CategoryName>);
    pub fn lua_setmemcat(L: *mut lua_State, category: c_int);
    pub fn lua_totalbytes(L: *mut lua_State, category: c_int) -> usize;
    pub fn lua_allocationrate(L: *mut lua_State) -> i64;

    // miscellaneous
    pub fn lua_error(L: *mut lua_State) -> !;
    pub fn lua_next(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_rawiter(L: *mut lua_State, idx: c_int, iter: c_int) -> c_int;
    pub fn lua_concat(L: *mut lua_State, n: c_int);
    pub fn lua_setpointerencodekey(L: *mut lua_State, a: u64, b: u64, c: u64, d: u64);
    pub fn lua_encodepointer(L: *mut lua_State, p: usize) -> usize;
    pub fn lua_clock() -> f64;

    // tagged userdata
    pub fn lua_setuserdatatag(L: *mut lua_State, idx: c_int, tag: c_int);
    pub fn lua_setuserdatadtor(L: *mut lua_State, tag: c_int, dtor: Option<lua_Destructor>);
    pub fn lua_getuserdatadtor(L: *mut lua_State, tag: c_int) -> Option<lua_Destructor>;
    pub fn lua_setuserdatamark(L: *mut lua_State, tag: c_int, markfn: Option<lua_UserdataMark>);
    pub fn lua_setembeddergc(L: *mut lua_State, r#fn: Option<lua_EmbedderGc>);
    pub fn lua_weakref(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_weakunref(L: *mut lua_State, r#ref: c_int) -> c_int;
    pub fn lua_getweakref(L: *mut lua_State, r#ref: c_int) -> c_int;
    pub fn lua_setuserdatametatable(L: *mut lua_State, tag: c_int);
    pub fn lua_getuserdatametatable(L: *mut lua_State, tag: c_int);
    pub fn lua_getuserdataname(L: *mut lua_State, tag: c_int) -> *const c_char;

    // direct userdata access
    pub fn lua_registeruserdatadirectaccess(
        L: *mut lua_State,
        tag: c_int,
        get: Option<lua_UserdataDirectAccess>,
        set: Option<lua_UserdataDirectAccess>,
        namecall: Option<lua_UserdataDirectNamecall>,
    ) -> c_int;
    pub fn lua_registeruserdatadirectfieldget(
        L: *mut lua_State,
        tag: c_int,
        field: *const c_char,
        r#fn: lua_UserdataDirectFieldGet,
    );
    pub fn lua_userdatadirectfield_setnumber(result: *mut c_void, n: f64);
    pub fn lua_userdatadirectfield_setvector(
        result: *mut c_void,
        x: LUA_VECTOR_TYPE,
        y: LUA_VECTOR_TYPE,
        z: LUA_VECTOR_TYPE,
    );
    pub fn lua_userdatadirectfield_setboolean(result: *mut c_void, b: c_int);
    pub fn lua_userdatadirectfield_setinteger64(result: *mut c_void, n: i64);
    pub fn lua_userdatadirectfield_setnil(result: *mut c_void);

    pub fn lua_setlightuserdataname(L: *mut lua_State, tag: c_int, name: *const c_char);
    pub fn lua_getlightuserdataname(L: *mut lua_State, tag: c_int) -> *const c_char;

    pub fn lua_clonefunction(L: *mut lua_State, idx: c_int);
    pub fn lua_usesexport(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_cleartable(L: *mut lua_State, idx: c_int);
    pub fn lua_clonetable(L: *mut lua_State, idx: c_int);
    pub fn lua_getallocf(L: *mut lua_State, ud: *mut *mut c_void) -> lua_Alloc;

    // reference system
    pub fn lua_ref(L: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_unref(L: *mut lua_State, r#ref: c_int) -> c_int;

    // debug API
    pub fn lua_callhook(L: *mut lua_State, hook: lua_Hook, userdata: *mut c_void);
    pub fn lua_stackdepth(L: *mut lua_State) -> c_int;
    pub fn lua_getinfo(L: *mut lua_State, level: c_int, what: *const c_char, ar: *mut lua_Debug) -> c_int;
    pub fn lua_getargument(L: *mut lua_State, level: c_int, n: c_int) -> c_int;
    pub fn lua_getlocal(L: *mut lua_State, level: c_int, n: c_int) -> *const c_char;
    pub fn lua_setlocal(L: *mut lua_State, level: c_int, n: c_int) -> *const c_char;
    pub fn lua_getupvalue(L: *mut lua_State, funcindex: c_int, n: c_int) -> *const c_char;
    pub fn lua_setupvalue(L: *mut lua_State, funcindex: c_int, n: c_int) -> *const c_char;
    pub fn lua_hascustomexecution(L: *mut lua_State, level: c_int) -> c_int;
    pub fn lua_incustomexecution(L: *mut lua_State, level: c_int) -> c_int;
    pub fn lua_singlestep(L: *mut lua_State, enabled: c_int);
    pub fn lua_breakpoint(L: *mut lua_State, funcindex: c_int, line: c_int, enabled: c_int) -> c_int;
    pub fn lua_atbreakpoint(L: *mut lua_State) -> c_int;
    pub fn lua_debugtrace(L: *mut lua_State) -> *const c_char;
    pub fn lua_getcoverage(L: *mut lua_State, funcindex: c_int, context: *mut c_void, callback: lua_Coverage);
    pub fn lua_getcounters(
        L: *mut lua_State,
        funcindex: c_int,
        context: *mut c_void,
        functionvisit: lua_CounterFunction,
        countervisit: lua_CounterValue,
    );

    pub fn lua_callbacks(L: *mut lua_State) -> *mut lua_Callbacks;
    pub fn lua_setbuffercage(L: *mut lua_State, alloc: lua_CageAlloc, ud: *mut c_void);
}

// ---------------------------------------------------------------------------------------------
// lualib.h
// ---------------------------------------------------------------------------------------------

unsafe extern "C-unwind" {
    pub fn luaL_register(L: *mut lua_State, libname: *const c_char, l: *const luaL_Reg);
    pub fn luaL_getmetafield(L: *mut lua_State, obj: c_int, e: *const c_char) -> c_int;
    pub fn luaL_callmeta(L: *mut lua_State, obj: c_int, e: *const c_char) -> c_int;
    pub fn luaL_typeerrorL(L: *mut lua_State, narg: c_int, tname: *const c_char) -> !;
    pub fn luaL_argerrorL(L: *mut lua_State, narg: c_int, extramsg: *const c_char) -> !;
    pub fn luaL_checklstring(L: *mut lua_State, numArg: c_int, l: *mut usize) -> *const c_char;
    pub fn luaL_optlstring(L: *mut lua_State, numArg: c_int, def: *const c_char, l: *mut usize) -> *const c_char;
    pub fn luaL_checknumber(L: *mut lua_State, numArg: c_int) -> f64;
    pub fn luaL_optnumber(L: *mut lua_State, nArg: c_int, def: f64) -> f64;
    pub fn luaL_checkboolean(L: *mut lua_State, narg: c_int) -> c_int;
    pub fn luaL_optboolean(L: *mut lua_State, narg: c_int, def: c_int) -> c_int;
    pub fn luaL_checkinteger(L: *mut lua_State, numArg: c_int) -> c_int;
    pub fn luaL_checkinteger64(L: *mut lua_State, numArg: c_int) -> i64;
    pub fn luaL_optinteger(L: *mut lua_State, nArg: c_int, def: c_int) -> c_int;
    pub fn luaL_optinteger64(L: *mut lua_State, nArg: c_int, def: i64) -> i64;
    pub fn luaL_checkunsigned(L: *mut lua_State, numArg: c_int) -> c_uint;
    pub fn luaL_optunsigned(L: *mut lua_State, numArg: c_int, def: c_uint) -> c_uint;
    pub fn luaL_checkvector(L: *mut lua_State, narg: c_int) -> *const LUA_VECTOR_TYPE;
    pub fn luaL_optvector(L: *mut lua_State, narg: c_int, def: *const LUA_VECTOR_TYPE) -> *const LUA_VECTOR_TYPE;
    pub fn luaL_checkstack(L: *mut lua_State, sz: c_int, msg: *const c_char);
    pub fn luaL_checktype(L: *mut lua_State, narg: c_int, t: c_int);
    pub fn luaL_checkany(L: *mut lua_State, narg: c_int);
    pub fn luaL_newmetatable(L: *mut lua_State, tname: *const c_char) -> c_int;
    pub fn luaL_checkudata(L: *mut lua_State, ud: c_int, tname: *const c_char) -> *mut c_void;
    pub fn luaL_checkudatatagged(L: *mut lua_State, ud: c_int, tag: c_int) -> *mut c_void;
    pub fn luaL_checkbuffer(L: *mut lua_State, narg: c_int, len: *mut usize) -> *mut c_void;
    pub fn luaL_where(L: *mut lua_State, lvl: c_int);
    pub fn luaL_errorL(L: *mut lua_State, fmt: *const c_char, ...) -> !;
    pub fn luaL_checkoption(L: *mut lua_State, narg: c_int, def: *const c_char, lst: *const *const c_char) -> c_int;
    pub fn luaL_tolstring(L: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    pub fn luaL_newstate() -> *mut lua_State;
    pub fn luaL_findtable(L: *mut lua_State, idx: c_int, fname: *const c_char, szhint: c_int) -> *const c_char;
    pub fn luaL_typename(L: *mut lua_State, idx: c_int) -> *const c_char;
    pub fn luaL_traceback(L: *mut lua_State, L1: *mut lua_State, msg: *const c_char, level: c_int);

    pub fn luaopen_base(L: *mut lua_State) -> c_int;
    pub fn luaopen_coroutine(L: *mut lua_State) -> c_int;
    pub fn luaopen_table(L: *mut lua_State) -> c_int;
    pub fn luaopen_os(L: *mut lua_State) -> c_int;
    pub fn luaopen_string(L: *mut lua_State) -> c_int;
    pub fn luaopen_bit32(L: *mut lua_State) -> c_int;
    pub fn luaopen_buffer(L: *mut lua_State) -> c_int;
    pub fn luaopen_utf8(L: *mut lua_State) -> c_int;
    pub fn luaopen_class(L: *mut lua_State) -> c_int;
    pub fn luaopen_math(L: *mut lua_State) -> c_int;
    pub fn luaopen_debug(L: *mut lua_State) -> c_int;
    pub fn luaopen_vector(L: *mut lua_State) -> c_int;
    pub fn luaopen_integer(L: *mut lua_State) -> c_int;
    pub fn luaL_openlibs(L: *mut lua_State);
    pub fn luaL_sandbox(L: *mut lua_State);
    pub fn luaL_sandboxthread(L: *mut lua_State);
}

// ---------------------------------------------------------------------------------------------
// luacode.h and luacodegen.h
// ---------------------------------------------------------------------------------------------

unsafe extern "C" {
    /// Failed compilation returns bytecode whose first byte is 0 followed by the message. The
    /// returned buffer comes from `malloc` and is released with [`free`].
    pub fn luau_compile(
        source: *const c_char,
        size: usize,
        options: *mut lua_CompileOptions,
        outsize: *mut usize,
    ) -> *mut c_char;
    /// Compiles L3i surface source, embedding original-source locations in bytecode.
    /// Uses [`luau_compile`]'s malloc/free and leading-NUL error conventions. No C++
    /// exception escapes; allocation failure returns null with output size zero.
    pub fn l3i_luau_compile(
        source: *const c_char,
        size: usize,
        options: *mut lua_CompileOptions,
        outsize: *mut usize,
    ) -> *mut c_char;
    /// Disassembles through the same lowering, mapping and compilation pipeline as
    /// [`l3i_luau_compile`], with identical error and ownership conventions.
    pub fn l3i_luau_disassemble(
        source: *const c_char,
        size: usize,
        options: *mut lua_CompileOptions,
        outsize: *mut usize,
    ) -> *mut c_char;
    pub fn luau_set_compile_constant_nil(constant: *mut lua_CompileConstant);
    pub fn luau_set_compile_constant_boolean(constant: *mut lua_CompileConstant, b: c_int);
    pub fn luau_set_compile_constant_number(constant: *mut lua_CompileConstant, n: f64);
    pub fn luau_set_compile_constant_integer64(constant: *mut lua_CompileConstant, l: i64);
    pub fn luau_set_compile_constant_vector(constant: *mut lua_CompileConstant, x: f32, y: f32, z: f32, w: f32);
    pub fn luau_set_compile_constant_vectord(constant: *mut lua_CompileConstant, x: f64, y: f64, z: f64, w: f64);
    pub fn luau_set_compile_constant_string(constant: *mut lua_CompileConstant, s: *const c_char, l: usize);

    /// C `free`, for buffers `luau_compile` returns.
    pub fn free(p: *mut c_void);

    // Coroutine finalizers (experimental in Luau 0.740; needs the DebugLuauCoroutineFinally flag).
    pub fn lua_hasfinalizers(L: *mut lua_State) -> c_int;
    pub fn lua_pushfinalizerfunction(L: *mut lua_State);
    pub fn lua_addfinalizer(L: *mut lua_State, co: *mut lua_State, idx: c_int);

    // lualib.h string buffers
    pub fn luaL_buffinit(L: *mut lua_State, B: *mut luaL_Strbuf);
    pub fn luaL_buffinitsize(L: *mut lua_State, B: *mut luaL_Strbuf, size: usize) -> *mut c_char;
    pub fn luaL_prepbuffsize(B: *mut luaL_Strbuf, size: usize) -> *mut c_char;
    pub fn luaL_addlstring(B: *mut luaL_Strbuf, s: *const c_char, l: usize);
    pub fn luaL_addvalue(B: *mut luaL_Strbuf);
    pub fn luaL_addvalueany(B: *mut luaL_Strbuf, idx: c_int);
    pub fn luaL_pushresult(B: *mut luaL_Strbuf);
    pub fn luaL_pushresultsize(B: *mut luaL_Strbuf, size: usize);

    // Inliner/include/luajitinliner.h
    pub fn luau_enable_jit_inliner(L: *mut lua_State);
    pub fn luau_disable_jit_inliner(L: *mut lua_State);

    // Require/include/Luau/Require.h
    pub fn luarequire_pushrequire(
        L: *mut lua_State,
        config_init: luarequire_Configuration_init,
        ctx: *mut c_void,
    ) -> c_int;
    pub fn luaopen_require(L: *mut lua_State, config_init: luarequire_Configuration_init, ctx: *mut c_void);
    pub fn luarequire_pushproxyrequire(
        L: *mut lua_State,
        config_init: luarequire_Configuration_init,
        ctx: *mut c_void,
    ) -> c_int;
    pub fn luarequire_registermodule(L: *mut lua_State) -> c_int;
    pub fn luarequire_clearcacheentry(L: *mut lua_State) -> c_int;
    pub fn luarequire_clearcache(L: *mut lua_State) -> c_int;
    pub fn luarequire_lockplaceholder(L: *mut lua_State, idx: c_int);
    pub fn luarequire_populateplaceholder(L: *mut lua_State, placeholderIdx: c_int, resultIdx: c_int);
    pub fn luarequire_createplaceholder(L: *mut lua_State);

    // l3i csrc/extra.cpp
    pub fn luau_setfflag(name: *const c_char, value: c_int) -> c_int;
    pub fn luau_getfflag(name: *const c_char) -> c_int;
    pub fn luau_setfint(name: *const c_char, value: c_int) -> c_int;
    pub fn luau_getfint(name: *const c_char, out: *mut c_int) -> c_int;
    pub fn luau_visitfflags(context: *mut c_void, visit: unsafe extern "C" fn(*mut c_void, *const c_char, c_int));
    pub fn lua_getmetatablepointer(L: *mut lua_State, objindex: c_int) -> *const c_void;
    pub fn l3i_read_scalar(L: *mut lua_State, idx: c_int, number: *mut f64, integer: *mut i64) -> c_int;
    pub fn l3i_touserdata_tag(L: *mut lua_State, idx: c_int, tag: *mut c_int) -> *mut c_void;
    pub fn l3i_native_enter(
        L: *mut lua_State,
        top: *mut c_int,
        threaddata: *mut *mut c_void,
        base: *mut *const crate::convert::RawValue,
    ) -> *mut c_void;
    pub fn l3i_call_base(L: *mut lua_State) -> *const crate::convert::RawValue;
    pub fn l3i_stack_slot(L: *mut lua_State, idx: c_int) -> *const crate::convert::RawValue;
    pub fn l3i_read_vector(L: *mut lua_State, idx: c_int, out: *mut f32) -> c_int;
    pub fn l3i_direct_enter(
        L: *mut lua_State,
        threaddata: *mut *mut c_void,
        base: *mut *const crate::convert::RawValue,
    ) -> c_int;
    pub fn lua_gcdump(L: *mut lua_State, file: *mut c_void, categoryName: Option<lua_CategoryName>);

    // C runtime, for the heap dump files.
    pub fn fopen(path: *const c_char, mode: *const c_char) -> *mut c_void;
    pub fn fclose(file: *mut c_void) -> c_int;
}

/// `luaL_Strbuf` (`luaL_Buffer`): a growable string builder; `LUA_BUFFERSIZE` inline bytes.
#[repr(C)]
pub struct luaL_Strbuf {
    pub p: *mut c_char,
    pub end: *mut c_char,
    pub L: *mut lua_State,
    pub storage: *mut c_void,
    pub buffer: [c_char; LUA_BUFFERSIZE],
}

pub const LUA_BUFFERSIZE: usize = 512;

/// Light userdata tags run `0..LUA_LUTAG_LIMIT`.
pub const LUA_LUTAG_LIMIT: c_int = 128;

pub type luarequire_NavigateResult = c_int;
pub const NAVIGATE_SUCCESS: c_int = 0;
pub const NAVIGATE_AMBIGUOUS: c_int = 1;
pub const NAVIGATE_NOT_FOUND: c_int = 2;
pub type luarequire_WriteResult = c_int;
pub const WRITE_SUCCESS: c_int = 0;
pub const WRITE_BUFFER_TOO_SMALL: c_int = 1;
pub const WRITE_FAILURE: c_int = 2;
pub type luarequire_ConfigStatus = c_int;
pub const CONFIG_ABSENT: c_int = 0;
pub const CONFIG_AMBIGUOUS: c_int = 1;
pub const CONFIG_PRESENT_JSON: c_int = 2;
pub const CONFIG_PRESENT_LUAU: c_int = 3;

/// Field order is ABI; mirrors `struct luarequire_Configuration` in Require.h exactly.
#[repr(C)]
pub struct luarequire_Configuration {
    pub is_require_allowed: Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char) -> bool>,
    pub reset:
        Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char) -> luarequire_NavigateResult>,
    pub jump_to_alias:
        Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char) -> luarequire_NavigateResult>,
    pub to_alias_override:
        Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char) -> luarequire_NavigateResult>,
    pub to_alias_fallback:
        Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char) -> luarequire_NavigateResult>,
    pub to_parent: Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void) -> luarequire_NavigateResult>,
    pub to_child:
        Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char) -> luarequire_NavigateResult>,
    pub is_module_present: Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void) -> bool>,
    pub get_chunkname: Option<
        unsafe extern "C-unwind" fn(
            *mut lua_State,
            *mut c_void,
            *mut c_char,
            usize,
            *mut usize,
        ) -> luarequire_WriteResult,
    >,
    pub get_loadname: Option<
        unsafe extern "C-unwind" fn(
            *mut lua_State,
            *mut c_void,
            *mut c_char,
            usize,
            *mut usize,
        ) -> luarequire_WriteResult,
    >,
    pub get_cache_key: Option<
        unsafe extern "C-unwind" fn(
            *mut lua_State,
            *mut c_void,
            *mut c_char,
            usize,
            *mut usize,
        ) -> luarequire_WriteResult,
    >,
    pub get_config_status: Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void) -> luarequire_ConfigStatus>,
    pub get_alias: Option<
        unsafe extern "C-unwind" fn(
            *mut lua_State,
            *mut c_void,
            *const c_char,
            *mut c_char,
            usize,
            *mut usize,
        ) -> luarequire_WriteResult,
    >,
    pub get_config: Option<
        unsafe extern "C-unwind" fn(
            *mut lua_State,
            *mut c_void,
            *mut c_char,
            usize,
            *mut usize,
        ) -> luarequire_WriteResult,
    >,
    pub get_luau_config_timeout: Option<unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void) -> c_int>,
    pub load: Option<
        unsafe extern "C-unwind" fn(*mut lua_State, *mut c_void, *const c_char, *const c_char, *const c_char) -> c_int,
    >,
}

pub type luarequire_Configuration_init = unsafe extern "C" fn(*mut luarequire_Configuration);

#[cfg(feature = "jit")]
unsafe extern "C" {
    pub fn luau_codegen_supported() -> c_int;
    pub fn luau_codegen_create(L: *mut lua_State);
    pub fn luau_codegen_compile(L: *mut lua_State, idx: c_int);
}

// ---------------------------------------------------------------------------------------------
// Macros from lua.h and lualib.h, as inline functions
// ---------------------------------------------------------------------------------------------

/// # Safety
/// `L` is a live state; `n` is not greater than the current top.
#[inline]
pub unsafe fn lua_pop(L: *mut lua_State, n: c_int) {
    unsafe { lua_settop(L, -n - 1) }
}

/// # Safety
/// `L` is a live state.
#[inline]
pub unsafe fn lua_newtable(L: *mut lua_State) {
    unsafe { lua_createtable(L, 0, 0) }
}

/// # Safety
/// `L` is a live state and `idx` is an acceptable index.
#[inline]
pub unsafe fn lua_tonumber(L: *mut lua_State, idx: c_int) -> f64 {
    unsafe { lua_tonumberx(L, idx, std::ptr::null_mut()) }
}

/// # Safety
/// `L` is a live state and `idx` is an acceptable index.
#[inline]
pub unsafe fn lua_tostring(L: *mut lua_State, idx: c_int) -> *const c_char {
    unsafe { lua_tolstring(L, idx, std::ptr::null_mut()) }
}

/// # Safety
/// `L` is a live state and `idx` is an acceptable index.
#[inline]
pub unsafe fn lua_isnil(L: *mut lua_State, idx: c_int) -> bool {
    unsafe { lua_type(L, idx) == LUA_TNIL }
}

/// # Safety
/// `L` is a live state and `idx` is an acceptable index.
#[inline]
pub unsafe fn lua_istable(L: *mut lua_State, idx: c_int) -> bool {
    unsafe { lua_type(L, idx) == LUA_TTABLE }
}

/// # Safety
/// `L` is a live state and `idx` is an acceptable index.
#[inline]
pub unsafe fn lua_isfunction(L: *mut lua_State, idx: c_int) -> bool {
    unsafe { lua_type(L, idx) == LUA_TFUNCTION }
}

/// `lua_pushcfunction(L, fn, debugname)`: no upvalues, no continuation. `debugname` is
/// borrowed by Luau for the closure's lifetime.
///
/// # Safety
/// `L` is a live state; `debugname` is null or valid until the VM closes.
#[inline]
pub unsafe fn lua_pushcfunction(L: *mut lua_State, r#fn: lua_CFunction, debugname: *const c_char) {
    unsafe { lua_pushcclosurek(L, r#fn, debugname, 0, None) }
}

/// `lua_pushcclosure(L, fn, debugname, nup)`.
///
/// # Safety
/// As `lua_pushcfunction`, with `nup` values on the stack.
#[inline]
pub unsafe fn lua_pushcclosure(L: *mut lua_State, r#fn: lua_CFunction, debugname: *const c_char, nup: c_int) {
    unsafe { lua_pushcclosurek(L, r#fn, debugname, nup, None) }
}

/// # Safety
/// `L` is a live state.
#[inline]
pub unsafe fn lua_pushlightuserdata(L: *mut lua_State, p: *mut c_void) {
    unsafe { lua_pushlightuserdatatagged(L, p, 0) }
}

/// # Safety
/// `L` is a live state and `idx` is a table index.
#[inline]
pub unsafe fn lua_rawgetp(L: *mut lua_State, idx: c_int, p: *mut c_void) -> c_int {
    unsafe { lua_rawgetptagged(L, idx, p, 0) }
}

/// # Safety
/// `L` is a live state, `idx` is a table index, and a value is on top of the stack.
#[inline]
pub unsafe fn lua_rawsetp(L: *mut lua_State, idx: c_int, p: *mut c_void) {
    unsafe { lua_rawsetptagged(L, idx, p, 0) }
}

/// # Safety
/// `L` is a live state and `s` is a NUL-terminated string.
#[inline]
pub unsafe fn lua_setglobal(L: *mut lua_State, s: *const c_char) {
    unsafe { lua_setfield(L, LUA_GLOBALSINDEX, s) }
}

/// # Safety
/// `L` is a live state and `s` is a NUL-terminated string.
#[inline]
pub unsafe fn lua_getglobal(L: *mut lua_State, s: *const c_char) -> c_int {
    unsafe { lua_getfield(L, LUA_GLOBALSINDEX, s) }
}

/// `lua_getref(L, ref)`: pushes the pinned value.
///
/// # Safety
/// `L` is a live state and `r` came from `lua_ref` on the same VM.
#[inline]
pub unsafe fn lua_getref(L: *mut lua_State, r: c_int) -> c_int {
    unsafe { lua_rawgeti(L, LUA_REGISTRYINDEX, r) }
}

/// `luaL_getmetatable(L, name)`.
///
/// # Safety
/// `L` is a live state and `name` is a NUL-terminated string.
#[inline]
pub unsafe fn luaL_getmetatable(L: *mut lua_State, name: *const c_char) -> c_int {
    unsafe { lua_getfield(L, LUA_REGISTRYINDEX, name) }
}
