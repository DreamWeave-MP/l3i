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
