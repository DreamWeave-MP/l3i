//! Host IR lowering hooks (`Luau::CodeGen::HostIrHooks`) written in Rust.
//!
//! Luau asks two kinds of question while compiling a function: what bytecode type an operation
//! on a vector or annotated userdata produces (so the rest of the function specialises on it),
//! and whether the host wants to lower the operation to IR itself. A `*_type` answer of
//! [`bytecode_type::ANY`] and a lowering answer of `false` mean "not mine", and Luau falls back
//! to its generic path. Lowering must obey Luau's contract for the hook (see CodeGenOptions.h):
//! check tags, take VM exits to `pcpos` on failure, read operands before writing results.
//!
//! Hooks run inside the code generator: no Lua API, no panics (a panic aborts the process).

use std::ffi::{CStr, c_char, c_int, c_void};

use super::ffi;
use super::ir::{HostMetamethod, IrBuilder, IrOp, bytecode_type};

/// What a `vector:method(...)` or `userdata:method(...)` namecall looks like at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamecallSite {
    /// Register of the first argument and of the results.
    pub arg_res_reg: c_int,
    /// Register holding the receiver.
    pub source_reg: c_int,
    /// Argument count including the receiver.
    pub params: c_int,
    /// Result count, or `LUA_MULTRET` (-1).
    pub results: c_int,
    /// Bytecode position for VM exits.
    pub pcpos: c_int,
}

/// A field access `value.member` at the access site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessSite {
    pub result_reg: c_int,
    pub source_reg: c_int,
    pub pcpos: c_int,
}

/// A metamethod operation on userdata operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetamethodSite {
    pub lhs_type: u8,
    pub rhs_type: u8,
    pub result_reg: c_int,
    pub lhs: IrOp,
    pub rhs: IrOp,
    pub method: HostMetamethod,
    pub pcpos: c_int,
}

/// Lowering hooks. Every method has a "not mine" default; implement the ones the host lowers.
/// `userdata type` values are `bytecode_type::TAGGED_USERDATA_BASE + index` into the
/// compilation's userdata type names.
pub trait NativeCodeHooks: 'static {
    fn vector_access_type(&self, member: &str) -> u8 {
        let _ = member;
        bytecode_type::ANY
    }
    fn vector_namecall_type(&self, member: &str) -> u8 {
        let _ = member;
        bytecode_type::ANY
    }
    fn vector_access(&self, build: &mut IrBuilder<'_>, member: &str, site: AccessSite) -> bool {
        let _ = (build, member, site);
        false
    }
    fn vector_namecall(&self, build: &mut IrBuilder<'_>, member: &str, site: NamecallSite) -> bool {
        let _ = (build, member, site);
        false
    }
    fn userdata_access_type(&self, userdata_type: u8, member: &str) -> u8 {
        let _ = (userdata_type, member);
        bytecode_type::ANY
    }
    fn userdata_metamethod_type(&self, lhs_type: u8, rhs_type: u8, method: HostMetamethod) -> u8 {
        let _ = (lhs_type, rhs_type, method);
        bytecode_type::ANY
    }
    fn userdata_namecall_type(&self, userdata_type: u8, member: &str) -> u8 {
        let _ = (userdata_type, member);
        bytecode_type::ANY
    }
    fn userdata_access(&self, build: &mut IrBuilder<'_>, userdata_type: u8, member: &str, site: AccessSite) -> bool {
        let _ = (build, userdata_type, member, site);
        false
    }
    fn userdata_metamethod(&self, build: &mut IrBuilder<'_>, site: MetamethodSite) -> bool {
        let _ = (build, site);
        false
    }
    fn userdata_namecall(
        &self,
        build: &mut IrBuilder<'_>,
        userdata_type: u8,
        member: &str,
        site: NamecallSite,
    ) -> bool {
        let _ = (build, userdata_type, member, site);
        false
    }
}

/// Several hook sets asked in order; the first one that claims an operation wins.
pub(crate) struct HookChain {
    pub(crate) hooks: Vec<Box<dyn NativeCodeHooks>>,
}

impl HookChain {
    fn first_type(&self, mut ask: impl FnMut(&dyn NativeCodeHooks) -> u8) -> u8 {
        self.hooks
            .iter()
            .map(|hooks| ask(hooks.as_ref()))
            .find(|&kind| kind != bytecode_type::ANY)
            .unwrap_or(bytecode_type::ANY)
    }

    fn first_lowering(&self, mut lower: impl FnMut(&dyn NativeCodeHooks) -> bool) -> bool {
        self.hooks.iter().any(|hooks| lower(hooks.as_ref()))
    }

    /// The C hook table pointing at this chain. The chain must outlive every compilation that
    /// uses the table.
    pub(crate) fn table(&self) -> ffi::db_ir_hooks {
        ffi::db_ir_hooks {
            context: (self as *const HookChain).cast_mut().cast(),
            vector_access_type: Some(vector_access_type),
            vector_namecall_type: Some(vector_namecall_type),
            vector_access: Some(vector_access),
            vector_namecall: Some(vector_namecall),
            userdata_access_type: Some(userdata_access_type),
            userdata_metamethod_type: Some(userdata_metamethod_type),
            userdata_namecall_type: Some(userdata_namecall_type),
            userdata_access: Some(userdata_access),
            userdata_metamethod: Some(userdata_metamethod),
            userdata_namecall: Some(userdata_namecall),
        }
    }
}

unsafe fn chain<'a>(context: *mut c_void) -> &'a HookChain {
    // SAFETY: `context` is the chain pointer `HookChain::table` stored, alive for the compilation.
    unsafe { &*context.cast_const().cast::<HookChain>() }
}

unsafe fn member<'a>(text: *const c_char, length: usize) -> &'a str {
    if text.is_null() {
        return "";
    }
    // SAFETY: Luau passes the interned member string and its length.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), length) };
    std::str::from_utf8(bytes).unwrap_or("")
}

unsafe fn with_builder<R>(raw: *mut ffi::db_ir_builder, body: impl FnOnce(&mut IrBuilder<'_>) -> R) -> R {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    // SAFETY: Luau hands us its live IrBuilder for the duration of the hook.
    let mut build = unsafe { IrBuilder::from_raw(raw) };
    body(&mut build)
}

unsafe extern "C" fn vector_access_type(context: *mut c_void, text: *const c_char, length: usize) -> u8 {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    chain.first_type(|hooks| hooks.vector_access_type(member))
}

unsafe extern "C" fn vector_namecall_type(context: *mut c_void, text: *const c_char, length: usize) -> u8 {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    chain.first_type(|hooks| hooks.vector_namecall_type(member))
}

unsafe extern "C" fn vector_access(
    context: *mut c_void,
    build: *mut ffi::db_ir_builder,
    text: *const c_char,
    length: usize,
    result_reg: c_int,
    source_reg: c_int,
    pcpos: c_int,
) -> bool {
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    let site = AccessSite { result_reg, source_reg, pcpos };
    unsafe { with_builder(build, |build| chain.first_lowering(|hooks| hooks.vector_access(build, member, site))) }
}

unsafe extern "C" fn vector_namecall(
    context: *mut c_void,
    build: *mut ffi::db_ir_builder,
    text: *const c_char,
    length: usize,
    arg_res_reg: c_int,
    source_reg: c_int,
    params: c_int,
    results: c_int,
    pcpos: c_int,
) -> bool {
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    let site = NamecallSite { arg_res_reg, source_reg, params, results, pcpos };
    unsafe { with_builder(build, |build| chain.first_lowering(|hooks| hooks.vector_namecall(build, member, site))) }
}

unsafe extern "C" fn userdata_access_type(context: *mut c_void, kind: u8, text: *const c_char, length: usize) -> u8 {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    chain.first_type(|hooks| hooks.userdata_access_type(kind, member))
}

unsafe extern "C" fn userdata_metamethod_type(context: *mut c_void, lhs: u8, rhs: u8, method: c_int) -> u8 {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    let chain = unsafe { chain(context) };
    let Some(method) = host_metamethod(method) else { return bytecode_type::ANY };
    chain.first_type(|hooks| hooks.userdata_metamethod_type(lhs, rhs, method))
}

unsafe extern "C" fn userdata_namecall_type(context: *mut c_void, kind: u8, text: *const c_char, length: usize) -> u8 {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    chain.first_type(|hooks| hooks.userdata_namecall_type(kind, member))
}

unsafe extern "C" fn userdata_access(
    context: *mut c_void,
    build: *mut ffi::db_ir_builder,
    kind: u8,
    text: *const c_char,
    length: usize,
    result_reg: c_int,
    source_reg: c_int,
    pcpos: c_int,
) -> bool {
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    let site = AccessSite { result_reg, source_reg, pcpos };
    unsafe {
        with_builder(build, |build| chain.first_lowering(|hooks| hooks.userdata_access(build, kind, member, site)))
    }
}

unsafe extern "C" fn userdata_metamethod(
    context: *mut c_void,
    build: *mut ffi::db_ir_builder,
    lhs_type: u8,
    rhs_type: u8,
    result_reg: c_int,
    lhs: u32,
    rhs: u32,
    method: c_int,
    pcpos: c_int,
) -> bool {
    let chain = unsafe { chain(context) };
    let Some(method) = host_metamethod(method) else { return false };
    let site = MetamethodSite { lhs_type, rhs_type, result_reg, lhs: IrOp(lhs), rhs: IrOp(rhs), method, pcpos };
    unsafe { with_builder(build, |build| chain.first_lowering(|hooks| hooks.userdata_metamethod(build, site))) }
}

unsafe extern "C" fn userdata_namecall(
    context: *mut c_void,
    build: *mut ffi::db_ir_builder,
    kind: u8,
    text: *const c_char,
    length: usize,
    arg_res_reg: c_int,
    source_reg: c_int,
    params: c_int,
    results: c_int,
    pcpos: c_int,
) -> bool {
    let (chain, member) = unsafe { (chain(context), member(text, length)) };
    let site = NamecallSite { arg_res_reg, source_reg, params, results, pcpos };
    unsafe {
        with_builder(build, |build| chain.first_lowering(|hooks| hooks.userdata_namecall(build, kind, member, site)))
    }
}

/// `HostMetamethod` from its C++ enumerator value, generated in the same order.
fn host_metamethod(value: c_int) -> Option<HostMetamethod> {
    const ALL: [HostMetamethod; 13] = [
        HostMetamethod::Add,
        HostMetamethod::Sub,
        HostMetamethod::Mul,
        HostMetamethod::Div,
        HostMetamethod::Idiv,
        HostMetamethod::Mod,
        HostMetamethod::Pow,
        HostMetamethod::Minus,
        HostMetamethod::Equal,
        HostMetamethod::LessThan,
        HostMetamethod::LessEqual,
        HostMetamethod::Length,
        HostMetamethod::Concat,
    ];
    usize::try_from(value).ok().and_then(|index| ALL.get(index).copied())
}

/// The userdata remapper: script type annotations are resolved to indexes into the
/// compilation's `userdata_types` list (0xff for unknown), matching the bytecode compiler.
pub(crate) unsafe extern "C" fn remap_userdata_type(context: *mut c_void, name: *const c_char, length: usize) -> u8 {
    let _guard = crate::raw::trampoline::AbortOnPanic;
    // SAFETY: `context` is the NativeCodeGen's type-name list; Luau passes the annotation text.
    let names = unsafe { &*context.cast_const().cast::<Vec<std::ffi::CString>>() };
    let requested = unsafe { std::slice::from_raw_parts(name.cast::<u8>(), length) };
    names
        .iter()
        .position(|candidate| candidate.as_bytes() == requested)
        .and_then(|index| u8::try_from(index).ok())
        .unwrap_or(0xff)
}

#[allow(dead_code)]
fn _cstr_is_used(_: &CStr) {}
