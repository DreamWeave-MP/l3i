//! Instruction, cycle, cache-miss, TLB-miss, and branch-miss counts per bound call, from the
//! CPU's own counters through `perf_event_open`. That is a Linux interface, so the harness lives
//! in `instructions/linux.rs` and this file is a stub elsewhere: `cargo bench --bench
//! instructions` prints why it measured nothing, and `cargo test --all-targets` still links.

#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;
#[cfg(target_os = "linux")]
#[path = "instructions/linux.rs"]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    eprintln!("the instructions bench reads Linux perf counters; nothing to measure on this platform");
}
