# Changelog

## Unreleased

- 5694ed9 - PERF: Add Criterion benchmarks for the hot paths and record the numbers
- 91c294c - FEAT: Add native code generation with Luau's lowering hooks, a Rust IR builder, and the writef32x3 lowering
- 2f50cda - FEAT: Let bound functions load and instantiate templates on their own scope, and document the memory limit as safepoint-polled
- 3c009a3 - FEAT: Add the OpenMW parity spike example covering every binding shape
- de89dbc - BREAK: Assign userdata tags per runtime at registration instead of on the type
- 340bfe9 - BREAK: Make atom catalogues per VM instead of process-wide
- b4ce182 - FIX: Lease the root stack per VM and make pinned values inert once their runtime closes
- 6d20416 - FEAT: Add the vector writef32x3 namecall shim for buffer writes
- 35b2cda - FEAT: Add the safepoint sampler, timed GC steps, stats frames, and profiler statistics primitives
- dd7d472 - FEAT: Add sandboxes with a frozen base environment, per-script instances, and compile-once templates
- b6475f7 - FIX: Give each Rust type a collision-free registry key instead of a hashed TypeId
- cdace28 - CLEANUP: Pin rustfmt to 120 columns and format the crate
- 7f338a8 - FEAT: Add runtime limits, memory categories, call scopes, and the interrupt watchdog
- 891cb6c - FEAT: Add Luau direct userdata access: atom catalogue, slot registry, typed direct callbacks, and direct fields
- a660222 - FEAT: Add array, keyed, and cursor iterator factories to MetatableBuilder
- 26b0f29 - FEAT: Add the component module contract and read-only tables
- 0586db3 - FIX: Read a method's receiver before counting its arguments, and report a missing argument once
- 2a7ffe1 - FEAT: Add untagged userdata, the full MetatableBuilder phase machine, and method-mode binding
- 3e1d090 - FEAT: Add the typed function binder with the bindfunction.hpp argument and return contracts
- 9c775bf - FEAT: Add protected calls from Rust: FunctionView::invoke and the pinned Function call family
- 18ea912 - FEAT: Add typed table access, checked optional reads, rawiter iteration, and the cold-tier Table API
- 4f42617 - FEAT: Add the conversion layer with the C++ numeric rules and first-class vectors and buffers
- 8eaabb9 - FEAT: Add registry-pinned Value, Table, and Function with the C++ reference semantics
- e0bf676 - CLEANUP: Remove ffi_extra.rs
- 8894526 - DOCS: Describe the crate's tiers and error model at the crate root
- a1ac8dd - FIX: Freeze the fast-flag policy from compile() too, and give the runtime builder OpenMW's creation order
- c2cd5bf - FIX: Make frame topology strictly nested and check thread identity and tag range before touching Luau
- 8ea3e64 - FIX: Make the stack model prove frame scoping and let Luau errors unwind through Rust frames
- 3827922 - FIX: Own the 254-tag Luau build inside build.rs instead of a .cargo/config.toml hosts would have to copy
- cab7b32 - FEAT: Add tagged userdata registration with inline Rust payloads and GC-driven Drop
- 7391657 - FEAT: Add debug-name validation and VM-lifetime interning for native closure names
- e6606a2 - FEAT: Enable Luau's if-local expression syntax, which OpenMW leaves off
- aef05c2 - BREAK: Drop mlua and drive Luau 0.740 directly through our own C API declarations
- 29f54cd - CHORE: Build against Luau 0.740, the exact release OpenMW pins, through a luau0-src semver shim
- f9de604 - FEAT: Add the borrowed stack layer: Stack, StackFrame, ValueView, and TableView
- e699970 - FEAT: Add the error type, the panic trampoline, and the Luau direct-access prototypes mlua-sys leaves out

