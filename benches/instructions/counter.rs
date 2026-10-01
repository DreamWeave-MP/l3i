//! `perf_event_open` by hand: one hardware counter of this process, user space only. Shared
//! by the benches that count instructions instead of timing (`instructions`, `intern`).

#![allow(dead_code, clippy::cast_possible_truncation)]

use std::ffi::{c_int, c_long, c_ulong, c_void};

#[repr(C)]
pub struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    bp_addr: u64,
    bp_len: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved_2: u16,
    aux_sample_size: u32,
    reserved_3: u32,
    sig_data: u64,
    config3: u64,
}

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn read(fd: c_int, buffer: *mut c_void, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}

const SYS_PERF_EVENT_OPEN: c_long = 298;
pub const PERF_TYPE_HARDWARE: u32 = 0;
pub const PERF_TYPE_HW_CACHE: u32 = 3;
pub const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
pub const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
pub const PERF_COUNT_HW_BRANCH_MISSES: u64 = 5;
/// `PERF_TYPE_HW_CACHE` configs: `cache | (op << 8) | (result << 16)` with op READ and result MISS.
pub const CACHE_READ_MISS: u64 = 1 << 16;
pub const PERF_COUNT_HW_CACHE_L1D: u64 = 0;
pub const PERF_COUNT_HW_CACHE_L1I: u64 = 1;
pub const PERF_COUNT_HW_CACHE_LL: u64 = 2;
pub const PERF_COUNT_HW_CACHE_DTLB: u64 = 3;
pub const PERF_COUNT_HW_CACHE_ITLB: u64 = 4;
const PERF_EVENT_IOC_ENABLE: c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: c_ulong = 0x2403;
/// `disabled | exclude_kernel | exclude_hv`.
const FLAGS: u64 = 1 | (1 << 5) | (1 << 6);

pub struct Counter(c_int);

impl Counter {
    pub fn open(kind: u32, config: u64) -> Option<Counter> {
        let mut attr = PerfEventAttr {
            kind,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: FLAGS,
            wakeup_events: 0,
            bp_type: 0,
            bp_addr: 0,
            bp_len: 0,
            branch_sample_type: 0,
            sample_regs_user: 0,
            sample_stack_user: 0,
            clockid: 0,
            sample_regs_intr: 0,
            aux_watermark: 0,
            sample_max_stack: 0,
            reserved_2: 0,
            aux_sample_size: 0,
            reserved_3: 0,
            sig_data: 0,
            config3: 0,
        };
        // SAFETY: a well-formed attribute block for this process and any CPU.
        let fd =
            unsafe { syscall(SYS_PERF_EVENT_OPEN, &raw mut attr, 0 as c_int, -1 as c_int, -1 as c_int, 0 as c_ulong) };
        (fd >= 0).then_some(Counter(fd as c_int))
    }

    pub fn measure(&self, body: &mut dyn FnMut()) -> u64 {
        // SAFETY: valid descriptor from `open`; the read buffer is eight bytes.
        unsafe {
            ioctl(self.0, PERF_EVENT_IOC_RESET, 0);
            ioctl(self.0, PERF_EVENT_IOC_ENABLE, 0);
            body();
            ioctl(self.0, PERF_EVENT_IOC_DISABLE, 0);
            let mut value: u64 = 0;
            let got = read(self.0, (&raw mut value).cast(), 8);
            assert_eq!(got, 8, "perf counter read");
            value
        }
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours.
        unsafe { close(self.0) };
    }
}
