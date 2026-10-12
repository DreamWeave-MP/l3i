//! Every bench in one binary. Each bench file is a module here rather than its own target
//! because under cross-language thin LTO every binary pays a full link, which is what made
//! `cargo test --all-targets` fifteen links instead of one. The Criterion groups keep their
//! names, so `cargo bench -- comprehension_dense` still selects by group; the benches with their
//! own harness (`instructions`, `intern`, `syntax`, `intl`, `tcp`, `dns_tls`) run when the
//! positional filter is absent or is contained in their name.

// The perf_event_open shim and the counting allocator are shared: one copy each.
#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;
#[cfg(any(feature = "intl", feature = "tcp", feature = "tls"))]
mod counting_alloc;

#[cfg(any(feature = "intl", feature = "tcp", feature = "tls"))]
#[global_allocator]
static GLOBAL: counting_alloc::Counting = counting_alloc::Counting;

mod comprehension;
mod hot_paths;
mod instructions;
mod packed_quat;
mod patterns;
mod raster;
mod udp;

#[cfg(feature = "bytes")]
mod bytes;
#[cfg(feature = "data")]
mod data;
#[cfg(all(feature = "dns", feature = "tls"))]
mod dns_tls;
#[cfg(feature = "intern")]
mod intern;
#[cfg(feature = "intl")]
mod intl;
#[cfg(feature = "soft-render")]
mod soft_render;
#[cfg(all(feature = "syntax", feature = "analysis"))]
mod syntax;
#[cfg(feature = "tcp")]
mod tcp;

fn main() {
    // Criterion filters its groups by the positional argument itself; the harnesses below only
    // see the same word.
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let wanted = |name: &str| filter.as_deref().is_none_or(|filter| name.contains(filter));

    hot_paths::benches();
    udp::benches();
    packed_quat::benches();
    raster::benches();
    comprehension::benches();
    patterns::benches();
    #[cfg(feature = "soft-render")]
    soft_render::benches();
    #[cfg(feature = "bytes")]
    bytes::benches();
    #[cfg(feature = "data")]
    data::benches();
    criterion::Criterion::default().configure_from_args().final_summary();

    if wanted("instructions") {
        instructions::main();
    }
    #[cfg(feature = "intern")]
    if wanted("intern") {
        intern::main();
    }
    #[cfg(all(feature = "syntax", feature = "analysis"))]
    if wanted("syntax") {
        syntax::main();
    }
    #[cfg(feature = "intl")]
    if wanted("intl") {
        intl::main();
    }
    #[cfg(feature = "tcp")]
    if wanted("tcp") {
        tcp::main();
    }
    #[cfg(all(feature = "dns", feature = "tls"))]
    if wanted("dns_tls") {
        dns_tls::main();
    }
}
