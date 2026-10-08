//! `@dream/tcp` over loopback, single-threaded and nonblocking: throughput by transfer size,
//! the cost of one bound call, connection setup and teardown, poller waits and dispatch at 1,
//! 32 and 256 connections, memory per handle and per watch, and CPU time while idle and while
//! a writer is blocked.
//!
//! - Luau rows drive the extension from an interpreted loop; `loop only` is the loop itself
//!   and the `state` getter the floor of any bound member, neither subtracted.
//! - Rust rows do the same transfer with nonblocking `std::net` sockets: the OS and loopback
//!   cost without the VM or the binding.
//! - Native allocations are Rust heap calls (this bench's counting allocator); VM bytes are
//!   what Luau's heap grew by.
//! - CPU is this thread's run time from `/proc/thread-self/schedstat` (Linux only).
//!
//! `cargo bench --features tcp --bench tcp`. Linux, `perf_event_paranoid` at 2 or lower for
//! instruction counts; elsewhere the timings print.

#![allow(clippy::cast_precision_loss, clippy::too_many_lines, clippy::missing_panics_doc)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::memory::GcControl;
use l3i::tcp::{CONNECT_CAPABILITY, LISTEN_CAPABILITY, TcpExtension};
use l3i::value::Function;

#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;

/// The system allocator, counting calls and bytes.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System` unchanged; the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const ROUNDS: usize = 5;

/// Instructions, when perf counters are available.
struct Instructions {
    #[cfg(target_os = "linux")]
    counter: Option<counter::Counter>,
}

impl Instructions {
    fn open() -> Instructions {
        Instructions {
            #[cfg(target_os = "linux")]
            counter: counter::Counter::open(counter::PERF_TYPE_HARDWARE, counter::PERF_COUNT_HW_INSTRUCTIONS),
        }
    }

    fn count(&self, body: &mut dyn FnMut()) -> Option<u64> {
        #[cfg(target_os = "linux")]
        if let Some(counter) = &self.counter {
            return Some(counter.measure(body));
        }
        body();
        None
    }
}

/// Per operation, the minimum over the rounds.
#[derive(Clone, Copy)]
struct Cost {
    instructions: Option<f64>,
    nanos: f64,
    allocations: f64,
}

fn measure(instructions: &Instructions, n: u64, mut body: impl FnMut()) -> Cost {
    let mut best = Cost { instructions: None, nanos: f64::MAX, allocations: f64::MAX };
    for _ in 0..ROUNDS {
        let counted = instructions.count(&mut body);
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        body();
        let nanos = start.elapsed().as_nanos() as f64;
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
        let n = n as f64;
        best.instructions = match (best.instructions, counted) {
            (Some(old), Some(new)) => Some(old.min(new as f64 / n)),
            (_, new) => new.map(|new| new as f64 / n),
        };
        best.nanos = best.nanos.min(nanos / n);
        best.allocations = best.allocations.min(allocations as f64 / n);
    }
    best
}

fn row(label: &str, cost: Cost, extra: &str) {
    let instructions = cost.instructions.map_or_else(|| "-".to_owned(), |count| format!("{count:.0}"));
    println!("| {label} | {instructions} | {:.1} | {:.2} | {extra} |", cost.nanos, cost.allocations);
}

fn header(title: &str, extra: &str) {
    println!("\n{title}\n");
    println!("| Operation | instr/op | ns/op | native allocs/op | {extra} |");
    println!("|---|---:|---:|---:|---|");
}

/// This thread's CPU time so far, in nanoseconds.
fn cpu_nanos() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

const CHUNK: &str = r"
local tcp = require('@dream/tcp')
local function settle(stream)
    local p = tcp.poller() p:watch(stream, 1, 'write')
    while stream:finishConnect() == false do p:wait(10) end
    p:close()
end
local function accept(listener)
    local p = tcp.poller() p:watch(listener, 1, 'read')
    while true do
        local s = listener:accept()
        if s then p:close() return s end
        p:wait(10)
    end
end
local listener = assert(tcp.listen('127.0.0.1:0', { backlog = 1024, maxStreams = 65536 }))
local function pair()
    local c = assert(tcp.connect(listener.localAddress))
    local s = accept(listener)
    settle(c)
    return c, s
end
local client, server = pair()
local src = buffer.create(65536)
local dst = buffer.create(65536)
local small = buffer.create(16)
local fleet, ready, idle = {}, nil, nil
local kept = {}
local cases = {}
function cases.loop(n) for i = 1, n do end end
function cases.getter(n) for i = 1, n do local _ = server.state end end
function cases.readEmpty(n) for i = 1, n do server:readInto(dst, 0, 16) end end
function cases.writeRead(n)
    for i = 1, n do
        client:write(small)
        server:readInto(dst, 0, 16)
    end
end
function cases.pump(total, size)
    local sent, got = 0, 0
    while got < total do
        if sent < total then
            local n = client:write(src, 0, math.min(size, total - sent))
            if n then sent += n end
        end
        while true do
            local n = server:readInto(dst, 0, size)
            if not n then break end
            got += n
        end
    end
end
function cases.acceptClose(n)
    for i = 1, n do
        local c, s = pair()
        c:close() s:close()
    end
end
function cases.fleet(count)
    if ready then ready:close() idle:close() end
    for _, p in fleet do p[1]:close() p[2]:close() end
    fleet = {}
    ready, idle = tcp.poller({ maxEvents = 256 }), tcp.poller({ maxEvents = 256 })
    for i = 1, count do
        local c, s = pair()
        fleet[i] = { c, s }
        ready:watch(c, i, 'write')
        idle:watch(s, i, 'read')
    end
end
function cases.dispatchReady(n)
    for i = 1, n do
        ready:wait(0)
        while ready:next() do end
    end
end
function cases.dispatchIdle(n) for i = 1, n do idle:wait(0) end end
function cases.idleWait(n, ms) for i = 1, n do idle:wait(ms) end end
function cases.blockedWait(n, ms)
    local p = tcp.poller() p:watch(client, 1, 'write')
    for i = 1, n do
        -- A writer with bytes to send writes until the socket refuses, then waits.
        while client:write(src) do end
        p:wait(ms)
    end
    p:close()
end
function cases.drain()
    while server:readInto(dst) do end
end
function cases.keepPairs(n) for i = 1, n do local c, s = pair() kept[i] = { c, s } end end
function cases.keepListeners(n) for i = 1, n do kept[i] = tcp.listen('127.0.0.1:0') end end
function cases.watchKept(n)
    local p = tcp.poller({ maxWatches = 65536 })
    kept.poller = p
    for i = 1, n do p:watch(kept[i][1], i, 'readwrite') end
end
function cases.drop()
    if kept.poller then kept.poller:close() kept.poller = nil end
    for _, k in kept do
        if type(k) == 'table' then k[1]:close() k[2]:close() else k:close() end
    end
    kept = {}
end
return function(case, a, b) cases[case](a, b) end
";

fn runtime() -> (Runtime, Function) {
    let policy = RuntimePolicy::new().capability(LISTEN_CAPABILITY).capability(CONNECT_CAPABILITY);
    let plan = RuntimePlan::builder().policy(policy).extension(TcpExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let run = runtime.load_function(CHUNK).unwrap();
    (runtime, run)
}

fn run(runtime: &Runtime, run: &Function, case: &str, a: f64, b: f64) {
    run.invoke::<(), _>(&runtime.stack(), (case, a, b)).unwrap();
}

fn heap(runtime: &Runtime) -> f64 {
    f64::from(runtime.gc(GcControl::Count)) * 1024.0 + f64::from(runtime.gc(GcControl::CountRemainder))
}

/// A connected nonblocking std pair.
fn std_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    client.set_nonblocking(true).unwrap();
    server.set_nonblocking(true).unwrap();
    (client, server)
}

fn std_pump(client: &mut TcpStream, server: &mut TcpStream, total: usize, size: usize, src: &[u8], dst: &mut [u8]) {
    let (mut sent, mut got) = (0, 0);
    while got < total {
        if sent < total {
            match client.write(&src[..size.min(total - sent)]) {
                Ok(n) => sent += n,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => panic!("{error}"),
            }
        }
        loop {
            match server.read(&mut dst[..size]) {
                Ok(n) => got += n,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
    }
}

fn throughput(instructions: &Instructions, runtime: &Runtime, chunk: &Function) {
    println!("\nThroughput: one thread writes a range and drains the peer, loopback\n");
    println!("| Transfer size | Luau MiB/s | Rust std MiB/s | Luau ns per write | native allocs per MiB (Luau) |");
    println!("|---:|---:|---:|---:|---:|");
    let (mut client, mut server) = std_pair();
    let src = vec![7u8; 65536];
    let mut dst = vec![0u8; 65536];
    for kib in [1usize, 4, 16, 64] {
        let size = kib * 1024;
        let total = if kib == 1 { 32 << 20 } else { 128 << 20 };
        let luau = measure(instructions, 1, || run(runtime, chunk, "pump", total as f64, size as f64));
        let native = measure(instructions, 1, || std_pump(&mut client, &mut server, total, size, &src, &mut dst));
        let mib = total as f64 / f64::from(1 << 20);
        let writes = total as f64 / size as f64;
        println!(
            "| {kib} KiB | {:.0} | {:.0} | {:.0} | {:.2} |",
            mib / (luau.nanos / 1e9),
            mib / (native.nanos / 1e9),
            luau.nanos / writes,
            luau.allocations / mib
        );
    }
}

fn calls(instructions: &Instructions, runtime: &Runtime, chunk: &Function) {
    const N: u64 = 200_000;
    const PAIRS: u64 = 500;
    header("Luau: one bound call, warm", "note");
    for (case, label, note) in [
        ("loop", "loop only", "the interpreted loop"),
        ("getter", "`stream.state`", "the floor of a bound member"),
        ("readEmpty", "`readInto` with nothing ready", "`nil, message, 'wouldBlock'`: one `recv`, borrowed message"),
        ("writeRead", "`write` 16 B + `readInto` 16 B", "two syscalls; the bytes arrive at once on loopback"),
    ] {
        row(label, measure(instructions, N, || run(runtime, chunk, case, N as f64, 0.0)), note);
    }
    let (mut client, mut server) = std_pair();
    let mut scratch = [0u8; 16];
    row(
        "Rust std: `write` 16 B + `read` 16 B",
        measure(instructions, N, || {
            for _ in 0..N {
                client.write_all(&[1; 16]).unwrap();
                assert_eq!(server.read(&mut scratch).unwrap(), 16);
            }
        }),
        "the same two syscalls, no VM",
    );
    header("Connections: connect, accept, settle and close both ends", "note");
    row(
        "Luau, through `@dream/tcp`",
        measure(instructions, PAIRS, || run(runtime, chunk, "acceptClose", PAIRS as f64, 0.0)),
        "three short-lived pollers per pair in the helpers",
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    row(
        "Rust std, blocking",
        measure(instructions, PAIRS, || {
            for _ in 0..PAIRS {
                let client = TcpStream::connect(address).unwrap();
                let (server, _) = listener.accept().unwrap();
                drop((client, server));
            }
        }),
        "",
    );
}

fn dispatch(instructions: &Instructions, runtime: &Runtime, chunk: &Function) {
    const WAITS: u64 = 20_000;
    header("Poller: `wait(0)` and draining `next()`", "per event");
    for count in [1u32, 32, 256] {
        run(runtime, chunk, "fleet", f64::from(count), 0.0);
        let ready = measure(instructions, WAITS, || run(runtime, chunk, "dispatchReady", WAITS as f64, 0.0));
        row(
            &format!("{count} watched, all writable: wait + {count} `next()`"),
            ready,
            &format!("{:.1} ns", ready.nanos / f64::from(count)),
        );
        let idle = measure(instructions, WAITS, || run(runtime, chunk, "dispatchIdle", WAITS as f64, 0.0));
        row(&format!("{count} watched, none ready: wait(0)"), idle, "");
    }
}

fn cpu(runtime: &Runtime, chunk: &Function) {
    println!("\nCPU while waiting (256 idle connections watched for read; one writer blocked on a full send buffer)\n");
    println!("| Situation | wall ms | CPU ms | CPU % |");
    println!("|---|---:|---:|---:|");
    let line = |label: &str, case: &str, n: f64, ms: f64| {
        let (cpu, wall) = (cpu_nanos(), Instant::now());
        run(runtime, chunk, case, n, ms);
        let wall = wall.elapsed();
        match (cpu, cpu_nanos()) {
            (Some(before), Some(after)) => {
                let cpu = Duration::from_nanos(after - before);
                println!(
                    "| {label} | {:.0} | {:.2} | {:.3} |",
                    wall.as_secs_f64() * 1e3,
                    cpu.as_secs_f64() * 1e3,
                    cpu.as_secs_f64() / wall.as_secs_f64() * 100.0
                );
            }
            _ => println!("| {label} | {:.0} | - | - |", wall.as_secs_f64() * 1e3),
        }
    };
    line("idle: 4 × `wait(250)`", "idleWait", 4.0, 250.0);
    line("blocked writer: 10 × (write until `wouldBlock`, `wait(100)` for write)", "blockedWait", 10.0, 100.0);
    run(runtime, chunk, "drain", 0.0, 0.0);
}

fn memory(runtime: &Runtime, chunk: &Function) {
    const KEPT: f64 = 256.0;
    println!("\nRetention: per handle and per watch, live after a full collection\n");
    println!("| Held | VM bytes each | native bytes each |");
    println!("|---|---:|---:|");
    let line = |label: &str, case: &str| {
        runtime.gc(GcControl::Collect);
        let (vm, native) = (heap(runtime), LIVE.load(Ordering::Relaxed));
        run(runtime, chunk, case, KEPT, 0.0);
        runtime.gc(GcControl::Collect);
        let vm = (heap(runtime) - vm) / KEPT;
        let native = (LIVE.load(Ordering::Relaxed) as f64 - native as f64) / KEPT;
        println!("| {label} | {vm:.0} | {native:.0} |");
    };
    line("a connected pair (two streams, the helpers' pollers closed)", "keepPairs");
    line("a watch (one stream in one poller)", "watchKept");
    run(runtime, chunk, "drop", 0.0, 0.0);
    line("a listener", "keepListeners");
    run(runtime, chunk, "drop", 0.0, 0.0);
    runtime.gc(GcControl::Collect);
}

fn main() {
    let instructions = Instructions::open();
    let (runtime, chunk) = runtime();
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(model) = cpuinfo.lines().find(|line| line.starts_with("model name"))
    {
        println!("{}", model.trim_start_matches("model name").trim_start_matches([' ', '\t', ':']));
    }
    throughput(&instructions, &runtime, &chunk);
    calls(&instructions, &runtime, &chunk);
    dispatch(&instructions, &runtime, &chunk);
    cpu(&runtime, &chunk);
    memory(&runtime, &chunk);
}
