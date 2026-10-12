//! The integration tests, linked as one binary: with cross-language LTO every test binary
//! pays a full link-time codegen, so one link instead of seventeen keeps `cargo test` fast.
//! Run one file's tests with `cargo test <file>::`.

// The shared CI runs `-W clippy::pedantic -D warnings`; tests compare floats exactly on purpose
// and read like the C++ contracts they port.
#![allow(
    clippy::float_cmp,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::items_after_statements,
    clippy::needless_pass_by_value
)]

/// Whether work grows no worse than linearly from `small` to four times it, where `fastest(size)`
/// is the quickest of a few runs at that size. Wall time on a shared runner is noisy in one
/// direction only: a sample can be slowed, never sped up. So one slow sample proves nothing, and
/// the bound has to fail on every attempt to fail. Quadratic growth is sixteen times for four
/// times the input and fails all of them. `Err` holds what each attempt measured.
fn grows_linearly(mut fastest: impl FnMut(usize) -> std::time::Duration, small: usize) -> Result<(), String> {
    use std::time::Duration;
    const ATTEMPTS: usize = 5;
    // Below this a run is mostly its fixed costs, and a ratio of two of those says nothing.
    let floor = Duration::from_millis(5);
    let mut measured = Vec::with_capacity(ATTEMPTS);
    for _ in 0..ATTEMPTS {
        let (few, many) = (fastest(small).max(floor), fastest(small * 4).max(floor));
        if many < few * 10 {
            return Ok(());
        }
        measured.push((few, many));
    }
    Err(format!("{measured:?} for {small} and {} on every attempt", small * 4))
}

#[test]
fn grows_linearly_fails_only_what_is_slow_on_every_attempt() {
    use std::time::Duration;
    let ms = |count: usize| Duration::from_millis(count as u64);
    // Four times the input for four times the time passes; for sixteen times, never.
    assert!(grows_linearly(ms, 100).is_ok());
    assert!(grows_linearly(|size| ms(size * size / 100), 100).is_err());
    // A sample slowed once is noise, and the attempt after it decides.
    let mut samples = 0;
    let slowed_once = |size| {
        samples += 1;
        if samples == 2 { ms(1_000_000) } else { ms(size) }
    };
    assert!(grows_linearly(slowed_once, 100).is_ok());
    assert_eq!(samples, 4);
}

#[cfg(feature = "analysis")]
mod analysis;
#[cfg(feature = "bytes")]
mod bytes;
mod call;
mod comprehension;
#[cfg(feature = "data")]
mod data;
mod debug;
mod direct;
#[cfg(feature = "dns")]
mod dns;
mod extension;
#[cfg(feature = "fs")]
mod fs;
#[cfg(all(feature = "dns", feature = "tls"))]
mod https;
#[cfg(feature = "intern")]
mod intern;
#[cfg(feature = "intl")]
mod intl;
mod iterator;
mod libraries;
mod lowering;
mod memory;
mod metatable;
mod module;
#[cfg(feature = "jit")]
mod native_code;
mod patterns;
mod primitives;
#[cfg(feature = "process")]
mod process;
mod quat;
mod raster;
mod require;
mod sandbox;
#[cfg(feature = "soft-render")]
mod soft_render;
mod source;
#[cfg(feature = "syntax")]
mod syntax;
mod tagged;
#[cfg(feature = "tcp")]
mod tcp;
mod thread;
#[cfg(feature = "tls")]
mod tls;
#[cfg(all(feature = "analysis", feature = "soft-render"))]
mod typed_definitions;
mod udp;
mod untagged;
mod vector_writer;
mod watchdog;
