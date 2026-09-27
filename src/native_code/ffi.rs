//! C ABI of `csrc/codegen.cpp`: Luau's C++ code generation API and IrBuilder, flattened.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_uint, c_void};

use crate::raw::ffi::lua_State;

/// Opaque `Luau::CodeGen::IrBuilder`.
#[repr(C)]
pub struct db_ir_builder {
    _private: [u8; 0],
}

pub type db_vector_type_fn = unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> u8;
pub type db_vector_access_fn =
    unsafe extern "C" fn(*mut c_void, *mut db_ir_builder, *const c_char, usize, c_int, c_int, c_int) -> bool;
pub type db_vector_namecall_fn = unsafe extern "C" fn(
    *mut c_void,
    *mut db_ir_builder,
    *const c_char,
    usize,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
) -> bool;
pub type db_userdata_type_fn = unsafe extern "C" fn(*mut c_void, u8, *const c_char, usize) -> u8;
pub type db_userdata_metamethod_type_fn = unsafe extern "C" fn(*mut c_void, u8, u8, c_int) -> u8;
pub type db_userdata_access_fn =
    unsafe extern "C" fn(*mut c_void, *mut db_ir_builder, u8, *const c_char, usize, c_int, c_int, c_int) -> bool;
pub type db_userdata_metamethod_fn =
    unsafe extern "C" fn(*mut c_void, *mut db_ir_builder, u8, u8, c_int, u32, u32, c_int, c_int) -> bool;
pub type db_userdata_namecall_fn = unsafe extern "C" fn(
    *mut c_void,
    *mut db_ir_builder,
    u8,
    *const c_char,
    usize,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
) -> bool;
pub type db_remapper_fn = unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> u8;

/// Field order is ABI; mirrors `struct db_ir_hooks` in csrc/codegen.cpp.
#[repr(C)]
pub struct db_ir_hooks {
    pub context: *mut c_void,
    pub vector_access_type: Option<db_vector_type_fn>,
    pub vector_namecall_type: Option<db_vector_type_fn>,
    pub vector_access: Option<db_vector_access_fn>,
    pub vector_namecall: Option<db_vector_namecall_fn>,
    pub userdata_access_type: Option<db_userdata_type_fn>,
    pub userdata_metamethod_type: Option<db_userdata_metamethod_type_fn>,
    pub userdata_namecall_type: Option<db_userdata_type_fn>,
    pub userdata_access: Option<db_userdata_access_fn>,
    pub userdata_metamethod: Option<db_userdata_metamethod_fn>,
    pub userdata_namecall: Option<db_userdata_namecall_fn>,
}

/// Mirrors `struct db_compilation_options`.
#[repr(C)]
pub struct db_compilation_options {
    pub flags: c_uint,
    pub record_counters: bool,
    pub nop_padding: bool,
    pub userdata_types: *const *const c_char,
    pub hooks: *const db_ir_hooks,
}

/// Mirrors `struct db_compilation_stats`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct db_compilation_stats {
    pub bytecode_size_bytes: usize,
    pub native_code_size_bytes: usize,
    pub native_data_size_bytes: usize,
    pub native_metadata_size_bytes: usize,
    pub functions_total: u32,
    pub functions_compiled: u32,
    pub functions_bound: u32,
}

unsafe extern "C" {
    pub fn db_codegen_supported() -> c_int;
    pub fn db_codegen_create_shared_context(block_size: usize, max_total_size: usize) -> *mut c_void;
    pub fn db_codegen_destroy_shared_context(context: *mut c_void);
    pub fn db_codegen_create(L: *mut lua_State, context: *mut c_void);
    pub fn db_codegen_is_native_execution_enabled(L: *mut lua_State) -> c_int;
    pub fn db_codegen_set_native_execution_enabled(L: *mut lua_State, enabled: c_int);
    pub fn db_codegen_disable_native_execution_for_function(L: *mut lua_State, level: c_int);
    pub fn db_codegen_set_userdata_remapper(L: *mut lua_State, context: *mut c_void, remapper: Option<db_remapper_fn>);
    pub fn db_codegen_compile(
        L: *mut lua_State,
        idx: c_int,
        module_id: *const u8,
        options: *const db_compilation_options,
        out_stats: *mut db_compilation_stats,
    ) -> c_int;

    pub fn db_ir_inst(build: *mut db_ir_builder, cmd: u8, ops: *const u32, count: usize) -> u32;
    pub fn db_ir_undef(build: *mut db_ir_builder) -> u32;
    pub fn db_ir_const_int(build: *mut db_ir_builder, value: c_int) -> u32;
    pub fn db_ir_const_int64(build: *mut db_ir_builder, value: i64) -> u32;
    pub fn db_ir_const_uint(build: *mut db_ir_builder, value: c_uint) -> u32;
    pub fn db_ir_const_import(build: *mut db_ir_builder, value: c_uint) -> u32;
    pub fn db_ir_const_double(build: *mut db_ir_builder, value: f64) -> u32;
    pub fn db_ir_const_tag(build: *mut db_ir_builder, value: u8) -> u32;
    pub fn db_ir_cond(build: *mut db_ir_builder, condition: u8) -> u32;
    pub fn db_ir_block(build: *mut db_ir_builder, kind: u8) -> u32;
    pub fn db_ir_block_at_inst(build: *mut db_ir_builder, index: u32) -> u32;
    pub fn db_ir_fallback_block(build: *mut db_ir_builder, pcpos: u32) -> u32;
    pub fn db_ir_begin_block(build: *mut db_ir_builder, block: u32);
    pub fn db_ir_load_and_check_tag(build: *mut db_ir_builder, location: u32, tag: u8, fallback: u32);
    pub fn db_ir_vm_reg(build: *mut db_ir_builder, index: u8) -> u32;
    pub fn db_ir_vm_const(build: *mut db_ir_builder, index: u32) -> u32;
    pub fn db_ir_vm_upvalue(build: *mut db_ir_builder, index: u8) -> u32;
    pub fn db_ir_vm_exit(build: *mut db_ir_builder, pcpos: u32) -> u32;
    pub fn db_ir_in_terminated_block(build: *mut db_ir_builder) -> c_int;
}
