//! Source compilation: an explicit `CompileOptions` always travels with the source, never
//! Luau's silent defaults (`components/luau/compileoptions.hpp`, `bytecode.cpp`). And
//! [`LoadScope`], loading a chunk on the thread of any scope with the runtime's native code
//! policy applied.

use std::ffi::{CString, c_char, c_int};
use std::ptr;

use crate::error::{Error, Result};
use crate::raw::ffi;
use crate::stack::Scope;
use crate::value::{Function, Value};

unsafe extern "C" {
    /// L3i surface syntax pass. Returns malloc-owned rewritten source, or null when unchanged.
    fn l3i_rewrite_surface_syntax(source: *const c_char, size: usize, outsize: *mut usize) -> *mut c_char;
}

fn rewrite_surface_syntax(source: &str) -> Option<Vec<u8>> {
    let mut size = 0usize;
    // SAFETY: source is a valid byte range for the call. The returned allocation, when non-null,
    // has `size` bytes and uses C malloc; copy it before releasing it with the same CRT `free`.
    let rewritten = unsafe { l3i_rewrite_surface_syntax(source.as_ptr().cast(), source.len(), &mut size) };
    if rewritten.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(rewritten.cast::<u8>(), size) }.to_vec();
    unsafe { ffi::free(rewritten.cast()) };
    Some(bytes)
}

/// Luau compiler policy. Defaults match OpenMW: optimisation 2, line info and function names,
/// no type information, no coverage.
#[derive(Clone)]
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
    /// Libraries whose members the compiler may ask about through `library_members`
    /// (`librariesWithKnownMembers`); the compiler folds constant members and specialises on
    /// member types.
    pub known_libraries: Vec<CString>,
    /// Answers the compiler's member queries for the known libraries.
    pub library_members: Option<std::rc::Rc<dyn LibraryMembers>>,
}

/// Compile-time knowledge about a library's members (`lua_LibraryMemberTypeCallback` and
/// `lua_LibraryMemberConstantCallback`).
pub trait LibraryMembers {
    /// The bytecode type of `library.member`, as a `LuauBytecodeType` value
    /// (`native_code::ir::bytecode_type` constants), or `None` for unknown.
    fn member_type(&self, library: &str, member: &str) -> Option<u8> {
        let _ = (library, member);
        None
    }
    /// The constant value of `library.member`, if it is one.
    fn member_constant(&self, library: &str, member: &str) -> Option<CompileConstant> {
        let _ = (library, member);
        None
    }
}

/// A constant the compiler may fold in place of a library member access.
#[derive(Clone, Debug, PartialEq)]
pub enum CompileConstant {
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    Vector(f32, f32, f32),
    String(String),
}

thread_local! {
    static ACTIVE_MEMBERS: std::cell::RefCell<Option<std::rc::Rc<dyn LibraryMembers>>> = const { std::cell::RefCell::new(None) };
    /// String constants handed to the compiler are borrowed, not copied, until compilation ends.
    static CONSTANT_STRINGS: std::cell::RefCell<Vec<Box<str>>> = const { std::cell::RefCell::new(Vec::new()) };
}

unsafe fn member_names<'a>(library: *const c_char, member: *const c_char) -> Option<(&'a str, &'a str)> {
    if library.is_null() || member.is_null() {
        return None;
    }
    // SAFETY: the compiler passes NUL-terminated names valid for the call.
    unsafe { Some((std::ffi::CStr::from_ptr(library).to_str().ok()?, std::ffi::CStr::from_ptr(member).to_str().ok()?)) }
}

unsafe extern "C" fn member_type_callback(library: *const c_char, member: *const c_char) -> c_int {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    let Some((library, member)) = (unsafe { member_names(library, member) }) else { return -1 };
    ACTIVE_MEMBERS.with(|active| {
        active.borrow().as_ref().and_then(|members| members.member_type(library, member)).map_or(-1, c_int::from)
    })
}

unsafe extern "C" fn member_constant_callback(
    library: *const c_char,
    member: *const c_char,
    constant: *mut ffi::lua_CompileConstant,
) {
    let _guard = crate::raw::trampoline::AbortOnPanic::new();
    let Some((library, member)) = (unsafe { member_names(library, member) }) else { return };
    let value = ACTIVE_MEMBERS
        .with(|active| active.borrow().as_ref().and_then(|members| members.member_constant(library, member)));
    // SAFETY: `constant` is the compiler's slot for this query.
    unsafe {
        match value {
            None => {}
            Some(CompileConstant::Nil) => ffi::luau_set_compile_constant_nil(constant),
            Some(CompileConstant::Boolean(b)) => ffi::luau_set_compile_constant_boolean(constant, c_int::from(b)),
            Some(CompileConstant::Number(n)) => ffi::luau_set_compile_constant_number(constant, n),
            Some(CompileConstant::Integer(i)) => ffi::luau_set_compile_constant_integer64(constant, i),
            Some(CompileConstant::Vector(x, y, z)) => ffi::luau_set_compile_constant_vector(constant, x, y, z, 0.0),
            Some(CompileConstant::String(text)) => {
                let kept: Box<str> = text.into_boxed_str();
                ffi::luau_set_compile_constant_string(constant, kept.as_ptr().cast(), kept.len());
                CONSTANT_STRINGS.with(|strings| strings.borrow_mut().push(kept));
            }
        }
    }
}

impl std::fmt::Debug for CompileOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompileOptions")
            .field("optimization_level", &self.optimization_level)
            .field("debug_level", &self.debug_level)
            .field("type_info_level", &self.type_info_level)
            .field("coverage_level", &self.coverage_level)
            .field("vector_lib", &self.vector_lib)
            .field("vector_ctor", &self.vector_ctor)
            .field("vector_type", &self.vector_type)
            .field("mutable_globals", &self.mutable_globals)
            .field("userdata_types", &self.userdata_types)
            .field("disabled_builtins", &self.disabled_builtins)
            .field("known_libraries", &self.known_libraries)
            .field("library_members", &self.library_members.is_some())
            .finish()
    }
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
            known_libraries: Vec::new(),
            library_members: None,
        }
    }
}

fn c_ptr(value: Option<&CString>) -> *const c_char {
    value.map_or(ptr::null(), |s| s.as_ptr())
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
/// Luau's message (without a chunk name) rather than as error bytecode.
pub fn compile(source: &str, options: &CompileOptions) -> Result<Vec<u8>> {
    let bytes = compile_raw(source, options)?;
    if bytes.first() == Some(&0) {
        return Err(Error::runtime(String::from_utf8_lossy(&bytes[1..]).into_owned()));
    }
    Ok(bytes)
}

/// Returns Luau's textual disassembly for `source`, compiled with the same explicit options as
/// [`compile`]. The output includes every function's bytecode instructions, source lines, locals,
/// and constants.
///
/// # Errors
///
/// Returns the Luau parse or compile error, or an error if the disassembly is not UTF-8.
pub fn disassemble(source: &str, options: &CompileOptions) -> Result<String> {
    let output = compile_native(source, options, true)?;
    if output.first() == Some(&0) {
        return Err(Error::runtime(String::from_utf8_lossy(&output[1..]).into_owned()));
    }
    String::from_utf8(output).map_err(|_| Error::runtime("Luau disassembler returned invalid UTF-8"))
}

/// Compiles to bytecode, returning Luau's error bytecode (leading NUL byte) as-is so that
/// `luau_load` reports the failure with the chunk name, as OpenMW's `loadBytecode` does.
pub(crate) fn compile_raw(source: &str, options: &CompileOptions) -> Result<Vec<u8>> {
    compile_native(source, options, false)
}

/// Runs either the bytecode compiler or its textual disassembler with identical option setup.
fn compile_native(source: &str, options: &CompileOptions, disassemble: bool) -> Result<Vec<u8>> {
    // Several flags change emitted bytecode; standalone compilation must see the same policy
    // a Runtime would.
    crate::flags::initialize()?;
    let mut mutable_globals = Vec::new();
    let mut userdata_types = Vec::new();
    let mut disabled_builtins = Vec::new();
    let mut known_libraries = Vec::new();
    let with_members = options.library_members.is_some() && !options.known_libraries.is_empty();
    let mut raw = ffi::lua_CompileOptions {
        optimizationLevel: c_int::from(options.optimization_level),
        debugLevel: c_int::from(options.debug_level),
        typeInfoLevel: c_int::from(options.type_info_level),
        coverageLevel: c_int::from(options.coverage_level),
        vectorLib: c_ptr(options.vector_lib.as_ref()),
        vectorCtor: c_ptr(options.vector_ctor.as_ref()),
        vectorType: c_ptr(options.vector_type.as_ref()),
        vectorPrecision: 0,
        mutableGlobals: c_array(&options.mutable_globals, &mut mutable_globals),
        userdataTypes: c_array(&options.userdata_types, &mut userdata_types),
        librariesWithKnownMembers: if with_members {
            c_array(&options.known_libraries, &mut known_libraries)
        } else {
            ptr::null()
        },
        libraryMemberTypeCb: with_members.then_some(member_type_callback as ffi::lua_LibraryMemberTypeCallback),
        libraryMemberConstantCb: with_members
            .then_some(member_constant_callback as ffi::lua_LibraryMemberConstantCallback),
        disabledBuiltins: c_array(&options.disabled_builtins, &mut disabled_builtins),
    };

    let rewritten = rewrite_surface_syntax(source);
    let compile_source = rewritten.as_deref().unwrap_or(source.as_bytes());

    let mut size = 0usize;
    // SAFETY: every pointer in `raw` outlives this call (the CStrings and the pointer arrays
    // are locals of this function). The surface pass owns its Vec through the call, and
    // luau_compile never raises; it reports failure in-band.
    if with_members {
        ACTIVE_MEMBERS.with(|active| active.borrow_mut().clone_from(&options.library_members));
    }
    let compile = if disassemble { ffi::l3i_luau_disassemble } else { ffi::luau_compile };
    let bytecode = unsafe { compile(compile_source.as_ptr().cast(), compile_source.len(), &mut raw, &mut size) };
    if with_members {
        ACTIVE_MEMBERS.with(|active| active.borrow_mut().take());
        CONSTANT_STRINGS.with(|strings| strings.borrow_mut().clear());
    }
    if bytecode.is_null() {
        return Err(Error::runtime("Luau compiler returned no bytecode"));
    }
    // SAFETY: the selected compiler function returned `size` valid bytes, owned by us until `free`.
    let bytes = unsafe { std::slice::from_raw_parts(bytecode.cast::<u8>(), size) }.to_vec();
    unsafe { ffi::free(bytecode.cast()) };
    Ok(bytes)
}

/// Loading chunks on the thread of any scope: a bound function's `Call`, a frame of a host
/// thread, the root stack. Implemented for every [`Scope`].
///
/// A chunk loads on the scope's own thread, so Luau resolves its builtin imports (`math.sqrt`,
/// `string.format`, ...) against that thread's globals when they are marked safe
/// (`Runtime::sandbox_globals`, `Thread::sandbox`), and the chunk takes the fast import path
/// from its first run; a template loaded on the sandbox's loader thread resolves against the
/// base environment instead. With the `jit` feature and a runtime built with native code, the
/// loaded chunk is compiled according to the runtime's mode: always under `Eager`, when it is
/// marked `--!native` under `Annotated`, never under `Off`. Without a generator the chunk simply
/// loads. The chunk runs in the thread's globals until the host sets another environment.
pub trait LoadScope: Scope + Sized {
    /// Loads `bytecode` (from [`compile`], or a cache of it) under `chunk_name` and pins the
    /// chunk as a `Function`. A chunk name starting with `@` or `=` is stripped of that prefix
    /// in debug records, as Luau does. A load failure is `Error::Runtime` with Luau's message;
    /// a chunk name containing NUL is a logic error.
    fn load_bytecode(&self, chunk_name: &str, bytecode: &[u8]) -> Result<Function> {
        let name = CString::new(chunk_name).map_err(|_| Error::logic("Chunk name cannot contain NUL"))?;
        self.with_frame(|frame| {
            let state = frame.state();
            // SAFETY: the frame's thread is live; the bytecode and name outlive the call.
            // luau_load reports failure by status and leaves the message on top of the frame.
            let status = unsafe { ffi::luau_load(state, name.as_ptr(), bytecode.as_ptr().cast(), bytecode.len(), 0) };
            if status != ffi::LUA_OK {
                // SAFETY: the message is on the frame's thread.
                return Err(unsafe { crate::raw::protect::pop_error(state, status) });
            }
            #[cfg(feature = "jit")]
            if let Some(generator) = crate::native_code::NativeCodeGen::for_scope(frame) {
                generator.compile(frame, -1, bytecode)?;
            }
            Function::from_value(Value::store(frame.top_value())?)
        })
    }

    /// Compiles `source` with `options` and loads it as [`LoadScope::load_bytecode`] does. A
    /// compile error is `Error::Runtime` carrying Luau's message with the chunk name, as a
    /// failed `luau_load` reports it.
    fn load_source(&self, chunk_name: &str, source: &str, options: &CompileOptions) -> Result<Function> {
        let bytecode = compile_raw(source, options)?;
        self.load_bytecode(chunk_name, &bytecode)
    }
}

impl<S: Scope> LoadScope for S {}

#[cfg(test)]
mod comprehension_tests {
    use super::rewrite_surface_syntax;

    #[test]
    fn leaves_ordinary_luau_untouched() {
        assert!(rewrite_surface_syntax("local x = values[1]").is_none());
        assert!(rewrite_surface_syntax("local t = { 1, 2, 3 }").is_none());
    }

    #[test]
    fn lowers_dense_list_comprehension() {
        let lowered = rewrite_surface_syntax("return [for x in values => x * x]").expect("rewritten");
        let text = String::from_utf8(lowered).unwrap();
        assert!(text.contains("table.create"), "{text}");
        assert!(text.contains("for __l3i_comp_7_g0_i = 1"), "{text}");
        assert!(text.contains("local x = __l3i_comp_7_g0_src[__l3i_comp_7_g0_i]"), "{text}");
        assert!(text.contains("= x * x"), "{text}");
    }

    #[test]
    fn lowers_filter_in_one_loop() {
        let lowered = rewrite_surface_syntax("return [for x in values if x.active => x.id]").expect("rewritten");
        let text = String::from_utf8(lowered).unwrap();
        assert!(text.contains("if x.active then"), "{text}");
        assert!(text.contains("+= 1"), "{text}");
        assert_eq!(text.matches("for __l3i_comp_7_g0_i").count(), 1, "{text}");
    }

    #[test]
    fn projections_are_nil_checked_once() {
        let lowered = rewrite_surface_syntax("return [for x in values => maybe(x)]").expect("rewritten");
        let text = String::from_utf8(lowered).unwrap();
        assert!(text.contains("local __l3i_comp_7_value = maybe(x)"), "{text}");
        assert!(text.contains("comprehension projection produced nil"), "{text}");
        assert_eq!(text.matches("maybe(x)").count(), 1, "{text}");
    }

    #[test]
    fn length_form_fuses_without_result_table() {
        let lowered = rewrite_surface_syntax("return #[for x in values if x.active => effect(x)]").expect("rewritten");
        let text = String::from_utf8(lowered).unwrap();
        assert!(!text.contains("table.create"), "{text}");
        assert!(!text.contains("_out"), "{text}");
        assert!(text.contains("local __l3i_comp_8_value = effect(x)"), "{text}");
        assert!(text.contains("return __l3i_comp_8_n"), "{text}");
    }

    #[test]
    fn ignores_for_inside_nested_expression_and_strings() {
        let lowered = rewrite_surface_syntax(
            "return [for x in values if g({ ok = true }, x) => f({ label = 'for x in nope' }, x)]",
        )
        .expect("rewritten");
        let text = String::from_utf8(lowered).unwrap();
        assert!(text.contains("'for x in nope'"), "{text}");
        assert!(text.contains("f({ label = 'for x in nope' }, x)"), "{text}");
    }
}
