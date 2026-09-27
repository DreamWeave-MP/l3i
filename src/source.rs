//! Source compilation: an explicit `CompileOptions` always travels with the source, never
//! Luau's silent defaults (`components/luau/compileoptions.hpp`, `bytecode.cpp`).

use std::ffi::{CString, c_char, c_int};
use std::ptr;

use crate::error::{Error, Result};
use crate::raw::ffi;

/// Luau compiler policy. Defaults match OpenMW: optimisation 2, line info and function names,
/// no type information, no coverage.
#[derive(Clone, Debug)]
pub struct CompileOptions {
    pub optimization_level: u8,
    pub debug_level: u8,
    pub type_info_level: u8,
    pub coverage_level: u8,
    /// Alternative global vector constructor `vector_lib.vector_ctor`, in addition to `vector.create`.
    pub vector_lib: Option<CString>,
    pub vector_ctor: Option<CString>,
    /// Alternative vector type name for type tables, in addition to `vector`.
    pub vector_type: Option<CString>,
    pub mutable_globals: Vec<CString>,
    pub userdata_types: Vec<CString>,
    pub disabled_builtins: Vec<CString>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            optimization_level: 2,
            debug_level: 1,
            type_info_level: 0,
            coverage_level: 0,
            vector_lib: None,
            vector_ctor: None,
            vector_type: None,
            mutable_globals: Vec::new(),
            userdata_types: Vec::new(),
            disabled_builtins: Vec::new(),
        }
    }
}

fn c_ptr(value: &Option<CString>) -> *const c_char {
    value.as_ref().map_or(ptr::null(), |s| s.as_ptr())
}

/// NUL-terminated array of C strings, or null when empty (Luau treats null as "none").
fn c_array(values: &[CString], storage: &mut Vec<*const c_char>) -> *const *const c_char {
    if values.is_empty() {
        return ptr::null();
    }
    storage.extend(values.iter().map(|s| s.as_ptr()));
    storage.push(ptr::null());
    storage.as_ptr()
}

/// Compiles Luau source to bytecode. A compile error is returned as `Error::Runtime` carrying
/// Luau's message rather than as error bytecode.
pub fn compile(source: &str, options: &CompileOptions) -> Result<Vec<u8>> {
    // Several flags change emitted bytecode; standalone compilation must see the same policy
    // a Runtime would.
    crate::flags::initialize()?;
    let mut mutable_globals = Vec::new();
    let mut userdata_types = Vec::new();
    let mut disabled_builtins = Vec::new();
    let mut raw = ffi::lua_CompileOptions {
        optimizationLevel: c_int::from(options.optimization_level),
        debugLevel: c_int::from(options.debug_level),
        typeInfoLevel: c_int::from(options.type_info_level),
        coverageLevel: c_int::from(options.coverage_level),
        vectorLib: c_ptr(&options.vector_lib),
        vectorCtor: c_ptr(&options.vector_ctor),
        vectorType: c_ptr(&options.vector_type),
        vectorPrecision: 0,
        mutableGlobals: c_array(&options.mutable_globals, &mut mutable_globals),
        userdataTypes: c_array(&options.userdata_types, &mut userdata_types),
        librariesWithKnownMembers: ptr::null(),
        libraryMemberTypeCb: None,
        libraryMemberConstantCb: None,
        disabledBuiltins: c_array(&options.disabled_builtins, &mut disabled_builtins),
    };

    let mut size = 0usize;
    // SAFETY: every pointer in `raw` outlives this call (the CStrings and the pointer arrays
    // are locals of this function). luau_compile never raises; it reports failure in-band.
    let bytecode = unsafe { ffi::luau_compile(source.as_ptr().cast(), source.len(), &mut raw, &mut size) };
    if bytecode.is_null() {
        return Err(Error::runtime("Luau compiler returned no bytecode"));
    }
    // SAFETY: luau_compile returned `size` valid bytes at `bytecode`, owned by us until `free`.
    let bytes = unsafe { std::slice::from_raw_parts(bytecode.cast::<u8>(), size) };
    let result = if bytes.first() == Some(&0) {
        Err(Error::runtime(String::from_utf8_lossy(&bytes[1..]).into_owned()))
    } else {
        Ok(bytes.to_vec())
    };
    unsafe { ffi::free(bytecode.cast()) };
    result
}
