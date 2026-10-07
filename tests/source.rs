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
fn builder_type_assertion_has_no_runtime_cost() {
    let source = "local src = values local out = table.create(#src) TYPE \
                  for i = 1, #src do local value = src[i] * 2 \
                  if value == nil then error('nil') end out[i] = value end return out";
    for optimization_level in 0..=2 {
        let options = CompileOptions { optimization_level, debug_level: 0, ..CompileOptions::default() };
        assert_eq!(
            compile(&source.replace("TYPE", ":: typeof({})"), &options).unwrap(),
            compile(&source.replace("TYPE", ""), &options).unwrap(),
            "type-only builder must not allocate or change executable bytecode"
        );
    }
}

#[test]
fn dense_comprehension_bytecode_exposes_guard_and_register_cost() {
    let options = CompileOptions::default();
    let surface = disassemble("return [for x in values => x * 2]", &options).unwrap();
    let handwritten = disassemble(
        "local src = values local n = #src local out = table.create(n) \
         for i = 1, n do local x = src[i] local value = x * 2 \
         if value == nil then error('L3i comprehension projection produced nil; filter nil explicitly') end \
         out[i] = value end return out",
        &options,
    )
    .unwrap();
    // Luau retains an unused prototype for the flattened wrapper. Inspect the entry function,
    // not that dead prototype, when counting executed instructions.
    let entry = surface.rsplit("Function ").next().unwrap();
    assert!(!entry.contains("CLOSURE") && !entry.contains("CAPTURE"), "{surface}");
    // The only runtime calls are table.create and the cold nil-error path, never the wrapper.
    assert_eq!(entry.matches(": CALL ").count(), 2, "{surface}");
    assert!(entry.contains("JUMPXEQKNIL"), "{surface}");
    println!("dense surface:\n{surface}\ndense handwritten nil-checked:\n{handwritten}");
}

#[test]
fn disassembly_returns_compile_errors_instead_of_an_error_listing() {
    let error = disassemble("local =", &CompileOptions::default()).unwrap_err().to_string();
    assert!(error.contains(":1:") && error.contains("Expected identifier"), "{error}");
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
    assert!(listing.contains("GETUPVAL"), "{listing}");
    assert!(!listing.contains("SETTABLE"), "{listing}");
    assert!(listing.contains("MULK") && listing.contains("JUMPXEQKNIL"), "{listing}");
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
}

#[test]
fn comprehension_sum_reducer_fuses_without_materializing_a_table() {
    let source = "return sum[for x in values if x % 2 == 0 => x * 2]";
    let listing = disassemble(source, &CompileOptions::default()).unwrap();

    assert!(!listing.contains("NEWTABLE"), "{listing}");
    assert!(!listing.contains("SETLIST"), "{listing}");
    assert!(!listing.contains("NEWCLOSURE"), "{listing}");
    assert!(!listing.contains("DUPCLOSURE"), "{listing}");
    assert!(!listing.contains("table.create"), "{listing}");
    assert!(listing.contains("GETUPVAL"), "{listing}");
    assert!(!listing.contains("SETTABLE"), "{listing}");
    assert!(listing.contains("JUMPXEQKNIL"), "{listing}");
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
}

#[test]
fn numeric_range_sum_is_a_direct_numeric_loop() {
    let listing = disassemble("return sum[for i in range(1, limit, 2) => i * 3]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
    assert!(!listing.contains("range") && !listing.contains("FORGPREP") && !listing.contains("CLOSURE"), "{listing}");
    assert!(!listing.contains("NEWTABLE") && !listing.contains("SETTABLE"), "{listing}");
}

#[test]
fn enumerate_comprehensions_are_direct_indexed_loops() {
    let listing =
        disassemble("return sum[for i, value in enumerate(values) => i + value]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
    assert!(
        !listing.contains("enumerate") && !listing.contains("FORGPREP") && !listing.contains("CLOSURE"),
        "{listing}"
    );
    assert!(!listing.contains("NEWTABLE") && !listing.contains("SETLIST"), "{listing}");
}

#[test]
fn zip_comprehensions_are_direct_indexed_loops() {
    let listing =
        disassemble("return sum[for a, b in zipShortest(left, right) => a + b]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
    assert!(
        !listing.contains("zipShortest") && !listing.contains("FORGPREP") && !listing.contains("CLOSURE"),
        "{listing}"
    );
    assert!(!listing.contains("NEWTABLE") && !listing.contains("SETLIST"), "{listing}");
}

#[test]
fn slices_fuse_without_materializing_the_slice() {
    let listing = disassemble("return sum[for x in values[2:limit] => x]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
    assert!(
        !listing.contains("table.move") && !listing.contains("FORGPREP") && !listing.contains("CLOSURE"),
        "{listing}"
    );
    assert!(!listing.contains("NEWTABLE") && !listing.contains("SETLIST"), "{listing}");
}

#[test]
fn buffer_slice_sums_fuse_to_direct_byte_reads() {
    let listing = disassemble("return sum[for byte in values[2:limit] => byte]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("'readu8'") && listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{listing}");
    assert!(!listing.contains("'copy'") && !listing.contains("NEWTABLE") && !listing.contains("SETLIST"), "{listing}");
}

#[test]
fn standalone_table_slices_use_bulk_copy() {
    let listing = disassemble("return values[2:limit]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("table.move"), "{listing}");
    assert!(!listing.contains("FORNPREP") && !listing.contains("FORNLOOP"), "{listing}");
}

#[test]
fn standalone_buffer_slices_use_bulk_copy() {
    let listing = disassemble("return values[2:limit]", &CompileOptions::default()).unwrap();
    assert!(listing.contains("'buffer'") && listing.contains("'copy'") && listing.contains("table.move"), "{listing}");
    assert!(!listing.contains("FORNPREP") && !listing.contains("FORNLOOP"), "{listing}");
}

#[test]
fn surface_compile_errors_and_disassembly_use_original_lines() {
    let source = "local xs = {1}\nlocal ys = [for x in xs => x * 2]\n\nlocal broken =\n    [for y in ys => y + * 2]";
    let options = CompileOptions::default();
    let error = compile(source, &options).unwrap_err().to_string();
    let listing_error = disassemble(source, &options).unwrap_err().to_string();
    assert_eq!(error, listing_error);
    assert!(error.starts_with(":5:"), "{error}");
    assert!(error.contains("Expected identifier"), "{error}");
}

#[test]
fn mapped_runtime_errors_attribute_copied_and_synthetic_operations() {
    for consumer in ["", "#"] {
        for projection in ["nil", "missing.field"] {
            let source = format!(
                "local xs = {{1}}\nlocal result = {consumer}[\n    for x in xs\n    if x > 0\n    => {projection}\n]\nreturn result"
            );
            let runtime = l3i::Runtime::new().unwrap();
            let error = runtime.exec(&source).unwrap_err().to_string();
            assert!(error.contains("exec:5:"), "{error}");
            assert!(
                if projection == "nil" {
                    error.contains("projection produced nil")
                } else {
                    error.contains("attempt to index nil")
                },
                "{error}"
            );
        }
    }
    let runtime = l3i::Runtime::new().unwrap();
    let error = runtime
        .exec("local values = {1}\nlocal xs = [for x in values => x]\n\nlocal broken = nil\nreturn broken.field")
        .unwrap_err()
        .to_string();
    assert!(error.contains("exec:5:"), "{error}");
}

#[test]
fn slice_validation_errors_point_to_the_original_source_and_bound() {
    let runtime = l3i::Runtime::new().unwrap();
    let first = runtime.exec("local values = {1, 2, 3}\nreturn values[\n    1.5:\n    2\n]").unwrap_err().to_string();
    assert!(first.contains("exec:3:") && first.contains("slice first bound"), "{first}");

    let source = runtime
        .exec("local source = 'text'\nreturn sum[for x in source[\n    1:\n    2\n] => x]")
        .unwrap_err()
        .to_string();
    assert!(source.contains("exec:2:") && source.contains("slice source"), "{source}");
}

#[test]
fn parser_secondary_references_and_eof_use_original_source() {
    let source = "local xs = [\n for x in {1}\n => x\n]\nfunction broken()\n return xs";
    for suffix in ["", "\n"] {
        let source = format!("{source}{suffix}");
        let expected_line = if suffix.is_empty() { 6 } else { 7 };
        let error = compile(&source, &CompileOptions::default()).unwrap_err().to_string();
        assert_eq!(error, disassemble(&source, &CompileOptions::default()).unwrap_err().to_string());
        assert!(error.starts_with(&format!(":{expected_line}:")), "{error}");
        assert!(error.contains("to close 'function' at line 5"), "{error}");
    }
    let same_line = "local xs = [for x in {1} => x]; function broken() return xs";
    let column = same_line.find("function broken").unwrap() + 1;
    let error = compile(same_line, &CompileOptions::default()).unwrap_err().to_string();
    assert!(error.contains(&format!("to close 'function' at line 1, column {column}")), "{error}");
}

#[test]
fn coordinate_templates_never_rewrite_quoted_user_input() {
    let payload = "refers to a class and cannot be used as a variable name on line 2";
    let source = format!("local xs = [\n for x in {{1}}\n => x\n]\nlocal \"{payload}\"\n");
    let compiled = compile(&source, &CompileOptions::default()).unwrap_err().to_string();
    let listing = disassemble(&source, &CompileOptions::default()).unwrap_err().to_string();
    assert_eq!(compiled, listing);
    assert!(compiled.contains(payload), "{compiled}");
}

#[test]
fn unfinished_surface_forms_never_compile_or_execute_recovery_holes() {
    let options = CompileOptions::default();
    for fragment in [
        "[for",
        "[for x",
        "[for x in",
        "[for x in xs",
        "[for x in xs if",
        "[for x in xs =>",
        "[for x in xs => x.",
        "[for x in xs => x",
        "sum[for x in xs =>",
        "#[for x in xs =>",
        "xs[:2]",
        "xs[1:]",
        "[for x in xs[:2] => x]",
        "[for x in xs[1:] => x]",
    ] {
        let source = format!("return {fragment}");
        let error = compile(&source, &options).unwrap_err().to_string();
        assert_eq!(error, disassemble(&source, &options).unwrap_err().to_string());
        assert!(error.starts_with(":1:"), "{fragment}: {error}");
        assert!(!error.contains("__l3i_comp_"), "{fragment}: {error}");
    }
    let runtime = l3i::Runtime::new().unwrap();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let record = calls.clone();
    let effect = runtime.bind_function("dreamweave.effect", move || record.set(record.get() + 1)).unwrap();
    runtime.set_global("effect", &effect).unwrap();
    let error = runtime.exec("effect()\nreturn [for x in {1} =>").unwrap_err().to_string();
    assert!(error.contains("expected projection expression"), "{error}");
    assert_eq!(calls.get(), 0, "strict compilation cannot run any recovery scaffolding");
}

#[test]
fn min_max_any_all_reducers_compile_to_bare_loops() {
    for op in ["min", "max", "any", "all"] {
        let listing =
            disassemble(&format!("return {op}[for x in values if x > 1 => x * 2]"), &CompileOptions::default())
                .unwrap();
        assert!(listing.contains("FORNPREP") && listing.contains("FORNLOOP"), "{op}: {listing}");
        assert!(
            !listing.contains("NEWTABLE") && !listing.contains("SETLIST") && !listing.contains("table.create"),
            "{op}: {listing}"
        );
        assert!(!listing.contains("FORGPREP") && !listing.contains("NEWCLOSURE"), "{op}: {listing}");
    }
}
