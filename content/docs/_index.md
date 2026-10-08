+++
title = "Documentation"
description = "How l3i puts a safe, scoped Rust layer over Luau 0.741: the stack tiers, userdata, modules and sandboxes, runtime options, direct access, the rest of the VM, the safety model, and the complete Rust API."
template = "docs/section.html"
page_template = "docs/page.html"
sort_by = "weight"

[extra]
docs_root = true
docs_project_name = "l3i"
docs_short_title = "l3i docs"
docs_project_path = "@/home/index.md"
docs_repository_url = "https://github.com/DreamWeave-MP/l3i/tree/main/content/docs"
docs_sidebar_label = "Documentation"
hide_child_cards = true
kind = "guide"
+++

l3i is a Rust binder for Luau 0.741. Its author wrote one in C++ first, for OpenMW:
[`components/luau`](https://gitlab.com/magicaldave1/openmw/-/tree/f69579c8d54fb2ae4d7a79a4d7033205effaec0e/components/luau) on a branch of his fork, which is not part of OpenMW and may never
be. That is "the C++ binder" these pages compare against, and
[Where the fast paths came from](@/docs/performance.md#where-the-fast-paths-came-from) points
at its lines; l3i is not affiliated with the OpenMW project. It owns its Luau
build, declares the C API by hand, and puts a scoped layer over it: frame-bound stack views,
registry-pinned owned values, a typed function binder that reads arguments straight from stack
slots, tagged and untagged userdata, Luau's direct userdata access, sandboxed script instances,
and watchdog and profiler plumbing. The host owns the `Runtime`; component crates register
bindings into it.

Tags, atoms, type names and debug-name roots are host data. The crate ships the mechanism and
never a catalogue, so the same Rust type can be tag 8 in one runtime, tag 17 in another, and
untagged in a third.

## Learn it

- **[Start here](@/docs/start-here.md)**: the crate, the toolchain it insists on, and a first
  program that exposes a Rust type to a script.
- **[Stack and values](@/docs/stack-and-values.md)**: the three tiers, frames and views, owned
  values, and what the typed binder accepts and returns.
- **[Userdata](@/docs/userdata.md)**: tagged and untagged registration, the metatable builder,
  receivers, and the storage wrappers.

## Use it

- **[Modules and sandboxes](@/docs/modules-and-sandboxes.md)**: frozen package tables,
  read-only views, per-script environments, templates, and `require`.
- **[Runtime options](@/docs/runtime-options.md)**: the builder, the watchdog, memory categories,
  call scopes, the collector, the profiler, and fast flags.
- **[Direct access and atoms](@/docs/direct-access.md)**: atom catalogues, dispatch plans,
  cache validation, direct handlers, and direct fields.
- **[Extensions](@/docs/extensions.md)**: describing a crate's whole Luau surface as a plan that
  instantiates any number of runtimes.
- **[Primitives](@/docs/primitives.md)**: bytes, strict options, exact integers, packed scalars,
  sequences and streams.
- **[Built-in extensions](@/docs/builtin-extensions.md)**: `dream.udp`, `dream.raster`,
  `dream.quat`, and `dream.soft_render`.
- **[Native code](@/docs/native-code.md)**: Luau's code generator with lowering hooks written in
  Rust.
- **[Coroutines, debugging and the rest of the VM](@/docs/vm.md)**: threads, the debug API,
  memory controls, libraries, `require`, and the analysis feature.

## Look it up

- **[Safety model](@/docs/safety.md)**: what the crate guarantees, the Luau facts it builds on,
  and where it diverges from the C++ binder on purpose.
- **[Building](@/docs/building.md)**: the submodule, what `build.rs` compiles, and the defines it
  owns.
- **[Performance](@/docs/performance.md)**: instructions and cycles per call, and the benchmark
  suites.
- **[Rust API](@/docs/api/_index.md)**: every public type, function and constant.
