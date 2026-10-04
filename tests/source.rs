use l3i::source::{CompileOptions, compile, disassemble};

#[test]
fn textual_disassembly_lists_bytecode_functions_lines_locals_and_constants() {
    let source = "local function add(base)\n return function(delta) return base + delta end\nend\nreturn add(40)(2)";
    let options = CompileOptions::default();
    compile(source, &options).unwrap();
    let listing = disassemble(source, &options).unwrap();

    assert!(listing.contains("Function 0"), "{listing}");
    assert!(listing.contains("Function 1"), "{listing}");
    assert!(listing.contains("GETUPVAL"), "{listing}");
    assert!(listing.contains("ADD"), "{listing}");
    assert!(listing.contains("40"), "constants should be shown: {listing}");
    assert!(listing.contains("add"), "debug names should be shown: {listing}");
}

#[test]
fn disassembly_returns_compile_errors_instead_of_an_error_listing() {
    let error = disassemble("local =", &CompileOptions::default()).unwrap_err().to_string();
    assert!(error.contains("parse error"), "{error}");
}

#[test]
fn comprehension_wrapper_is_inlineable_at_l3i_optimization_level() {
    let source = "local values = { 1, 2, 3, 4 } return [for x in values => x * 2]";
    let options = CompileOptions::default();
    assert_eq!(options.optimization_level, 2);
    let listing = disassemble(source, &options).unwrap();

    // The surface lowering deliberately uses an IIFE because L3i enables
    // LuauCompileIifeInline. There must be no runtime closure allocation for the wrapper.
    assert!(!listing.contains("NEWCLOSURE"), "{listing}");
    assert!(!listing.contains("DUPCLOSURE"), "{listing}");
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
}
#[test]
fn comprehension_length_fuses_without_materializing_a_table() {
    // Supply the input externally: an input literal legitimately emits NEWTABLE/SETLIST.
    let source = "return #[for x in values if x % 2 == 0 => x * 2]";
    let listing = disassemble(source, &CompileOptions::default()).unwrap();

    assert!(!listing.contains("NEWTABLE"), "{listing}");
    assert!(!listing.contains("SETLIST"), "{listing}");
    assert!(!listing.contains("NEWCLOSURE"), "{listing}");
    assert!(!listing.contains("DUPCLOSURE"), "{listing}");
    assert!(!listing.contains("table.create"), "{listing}");
    assert!(!listing.contains("SETTABLE"), "{listing}");
    assert!(listing.contains("MULK") && listing.contains("JUMPXEQKNIL"), "{listing}");
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
}
