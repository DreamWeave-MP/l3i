//! `@dream/dns` and `@dream/tls` from Luau, one thread, loopback.
//!
//! - DNS: a literal (no worker), a lookup through the worker pool with an instant resolver (the
//!   pool's own round trip), the same woken through a `@dream/tcp` poller, `localhost` through
//!   the OS resolver cold and warm, saturation, and CPU while requests are pending.
//! - TLS against plain TCP on the same path: connection setup with and without a handshake,
//!   a 16-byte round trip, throughput by write size, a read with nothing ready, configuration
//!   construction, and what a live connection holds.
//! - Native allocations are Rust heap calls (this bench's counting allocator); VM bytes are
//!   what Luau's heap holds after a full collection; CPU is the thread's schedstat (Linux).
//!
//! `cargo bench --features dns,tls --bench dns_tls`. Linux, `perf_event_paranoid` at 2 or lower
//! for instruction counts.

#![allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::items_after_statements,
    clippy::similar_names
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::dns::{DnsExtension, Lookup, LookupError, RESOLVE_CAPABILITY, Resolver, ResolverConfig};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::memory::GcControl;
use l3i::tcp::{CONNECT_CAPABILITY, LISTEN_CAPABILITY, TcpExtension};
use l3i::tls::TlsExtension;
use l3i::value::Function;

#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;

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
    println!("| {label} | {instructions} | {:.1} | {:.2} | {extra} |", cost.nanos / 1000.0, cost.allocations);
}

fn header(title: &str) {
    println!("\n{title}\n");
    println!("| Operation | instr/op | µs/op | native allocs/op | note |");
    println!("|---|---:|---:|---:|---|");
}

fn cpu_nanos() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// Instant answers for `bench.test`; `slow.test` waits for the gate.
struct Bench {
    gate: (Mutex<bool>, Condvar),
}

impl Lookup for Bench {
    fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, LookupError> {
        if host == "slow.test" {
            let mut open = self.gate.0.lock().unwrap();
            while !*open {
                open = self.gate.1.wait(open).unwrap();
            }
        }
        Ok(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            "[::1]:0".parse::<SocketAddr>().map(|a| SocketAddr::new(a.ip(), port)).unwrap(),
        ])
    }
}

const DNS_CHUNK: &str = r"
local dns = require('@dream/dns')
local tcp = require('@dream/tcp')
local cases = {}
local held = {}
function cases.literal(n) for i = 1, n do assert(dns.resolve('127.0.0.1', 80)):take() end end
function cases.pooled(n)
    for i = 1, n do
        local r = assert(dns.resolve('bench.test', 80))
        assert(r:wait(1000))
        assert(r:take())
    end
end
function cases.polled(n)
    local poller = tcp.poller()
    for i = 1, n do
        local r = assert(dns.resolve('bench.test', 80))
        assert(poller:watch(r, 1, 'read'))
        while poller:wait(1000) == 0 do end
        assert(r:take())
        poller:unwatch(1)
    end
    poller:close()
end
function cases.system(n)
    for i = 1, n do
        local r = assert(dns.resolve('localhost', 80))
        assert(r:wait(5000))
        assert(r:take())
    end
end
function cases.saturate()
    local count = 0
    while true do
        local r, message, kind = dns.resolve('slow.test', 80)
        if not r then assert(kind == 'limitReached', message) return count end
        table.insert(held, r)
        count += 1
    end
end
function cases.idle(n, ms)
    local poller = tcp.poller()
    for i, r in held do poller:watch(r, i, 'read') end
    for i = 1, n do poller:wait(ms) end
    poller:close()
end
function cases.release() for _, r in held do r:close() end held = {} end
return function(case, a, b) return cases[case](a, b) end
";

const TLS_CHUNK: &str = r"
local tcp = require('@dream/tcp')
local tls = require('@dream/tls')
local listener = assert(tcp.listen('127.0.0.1:0', { backlog = 1024, maxStreams = 65536, noDelay = true }))
local clientConfig = assert(tls.clientConfig({ roots = { CA }, platform = false }))
local serverConfig = assert(tls.serverConfig({ certChain = CHAIN, privateKey = KEY }))
local function tcpPair()
    local c = assert(tcp.connect(listener.localAddress, { noDelay = true }))
    local s
    repeat s = listener:accept() until s
    while c:finishConnect() == false do end
    return c, s
end
local function handshake(c, s)
    for _ = 1, 100000 do
        local a, am = c:handshake()
        local b, bm = s:handshake()
        assert(a ~= nil and b ~= nil, tostring(am) .. ' ' .. tostring(bm))
        if a and b then return end
    end
    error('the handshake stalled')
end
local function tlsPair()
    local c, s = tcpPair()
    local sc = assert(tls.client(c, { serverName = 'test.local', config = clientConfig }))
    local ss = assert(tls.server(s, serverConfig))
    handshake(sc, ss)
    return sc, ss
end
local secureClient, secureServer = tlsPair()
local plainClient, plainServer = tcpPair()
local src = buffer.create(65536)
local dst = buffer.create(65536)
local small = buffer.create(16)
local kept = {}
local cases = {}
function cases.tcpSetup(n) for i = 1, n do local c, s = tcpPair() c:close() s:close() end end
function cases.tlsSetup(n) for i = 1, n do local c, s = tlsPair() c:close() s:close() end end
local function pump(c, s, total, size, flush)
    local sent, got = 0, 0
    while got < total do
        if sent < total then
            local n = c:write(src, 0, math.min(size, total - sent))
            if n then sent += n end
            if flush then c:flush() end
        end
        while true do
            local n = s:readInto(dst, 0, 65536)
            if not n then break end
            got += n
        end
    end
end
function cases.tcpPump(total, size) pump(plainClient, plainServer, total, size, false) end
function cases.tlsPump(total, size) pump(secureClient, secureServer, total, size, true) end
local function roundTrip(c, s, n, flush)
    for i = 1, n do
        c:write(small) if flush then c:flush() end
        local got = 0
        while got < 16 do local r = s:readInto(dst, 0, 16 - got) if r then got += r end end
        s:write(small) if flush then s:flush() end
        got = 0
        while got < 16 do local r = c:readInto(dst, 0, 16 - got) if r then got += r end end
    end
end
function cases.tcpRoundTrip(n) roundTrip(plainClient, plainServer, n, false) end
function cases.tlsRoundTrip(n) roundTrip(secureClient, secureServer, n, true) end
function cases.tlsReadEmpty(n) for i = 1, n do secureServer:readInto(dst, 0, 16) end end
function cases.tcpReadEmpty(n) for i = 1, n do plainServer:readInto(dst, 0, 16) end end
function cases.clientConfig(n) for i = 1, n do assert(tls.clientConfig({ roots = { CA }, platform = false })) end end
function cases.platformConfig(n) for i = 1, n do assert(tls.clientConfig()) end end
function cases.serverConfig(n) for i = 1, n do assert(tls.serverConfig({ certChain = CHAIN, privateKey = KEY })) end end
function cases.keepTls(n) for i = 1, n do local c, s = tlsPair() kept[i] = { c, s } end end
function cases.keepTcp(n) for i = 1, n do local c, s = tcpPair() kept[i] = { c, s } end end
function cases.drop() for _, pair in kept do pair[1]:close() pair[2]:close() end kept = {} end
return function(case, a, b) return cases[case](a, b) end
";

fn run(runtime: &Runtime, chunk: &Function, case: &str, a: f64, b: f64) -> f64 {
    chunk.invoke::<Option<f64>, _>(&runtime.stack(), (case, a, b)).unwrap().unwrap_or(0.0)
}

fn heap(runtime: &Runtime) -> f64 {
    f64::from(runtime.gc(GcControl::Count)) * 1024.0 + f64::from(runtime.gc(GcControl::CountRemainder))
}

fn dns(instructions: &Instructions) {
    let lookup = Arc::new(Bench { gate: (Mutex::new(false), Condvar::new()) });
    let resolver = Resolver::new(ResolverConfig { workers: 4, queue: 64 }, Arc::clone(&lookup) as Arc<dyn Lookup>);
    let policy = RuntimePolicy::new().capability(RESOLVE_CAPABILITY);
    let plan = RuntimePlan::builder()
        .policy(policy)
        .extension(TcpExtension)
        .extension(DnsExtension::new(resolver.clone()).max_requests(4096))
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let chunk = runtime.load_function(DNS_CHUNK).unwrap();
    header("DNS: one request, resolved and taken");
    const N: u64 = 2000;
    row(
        "literal `127.0.0.1` (no worker)",
        measure(instructions, N, || {
            run(&runtime, &chunk, "literal", N as f64, 0.0);
        }),
        "",
    );
    row(
        "instant lookup on the pool, `wait` + `take`",
        measure(instructions, N, || {
            run(&runtime, &chunk, "pooled", N as f64, 0.0);
        }),
        "queue, worker hand-off, condition variable",
    );
    row(
        "instant lookup on the pool, woken through a poller",
        measure(instructions, N, || {
            run(&runtime, &chunk, "polled", N as f64, 0.0);
        }),
        "mio::Waker wake and the poller's scan",
    );
    let cold = Instant::now();
    let system_runtime = {
        let plan = RuntimePlan::builder()
            .policy(RuntimePolicy::new().capability(RESOLVE_CAPABILITY))
            .extension(TcpExtension)
            .extension(DnsExtension::default())
            .finalize()
            .unwrap();
        Runtime::from_plan(&plan).unwrap()
    };
    let system = system_runtime.load_function(DNS_CHUNK).unwrap();
    run(&system_runtime, &system, "system", 1.0, 0.0);
    let cold = cold.elapsed();
    row(
        "`localhost` through the OS resolver, warm",
        measure(instructions, 50, || {
            run(&system_runtime, &system, "system", 50.0, 0.0);
        }),
        &format!("first, cold: {:.0} µs (thread start included)", cold.as_secs_f64() * 1e6),
    );
    let admitted = run(&runtime, &chunk, "saturate", 0.0, 0.0);
    println!(
        "\nSaturation: {admitted} requests admitted with 4 workers blocked and a queue of 64 before `limitReached`."
    );
    let (cpu, wall) = (cpu_nanos(), Instant::now());
    run(&runtime, &chunk, "idle", 4.0, 250.0);
    if let (Some(before), Some(after)) = (cpu, cpu_nanos()) {
        println!(
            "Idle: {admitted} pending requests watched, 4 × `wait(250)`: {:.0} ms wall, {:.2} ms CPU.",
            wall.elapsed().as_secs_f64() * 1e3,
            Duration::from_nanos(after - before).as_secs_f64() * 1e3
        );
    }
    run(&runtime, &chunk, "release", 0.0, 0.0);
    *lookup.gate.0.lock().unwrap() = true;
    lookup.gate.1.notify_all();
    println!("Threads started by the pool: {} (its maximum is 4).", resolver.load().1);
}

fn tls(instructions: &Instructions) {
    let pki = pki();
    let policy = RuntimePolicy::new().capability(LISTEN_CAPABILITY).capability(CONNECT_CAPABILITY);
    let plan =
        RuntimePlan::builder().policy(policy).extension(TcpExtension).extension(TlsExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.set_global("CA", pki.0.as_str()).unwrap();
    runtime.set_global("CHAIN", pki.1.as_str()).unwrap();
    runtime.set_global("KEY", pki.2.as_str()).unwrap();
    let chunk = runtime.load_function(TLS_CHUNK).unwrap();

    header("Connections: connect, accept, close both ends; with a TLS 1.3 handshake (ECDSA P-256, reused configs)");
    const PAIRS: u64 = 200;
    let tcp = measure(instructions, PAIRS, || {
        run(&runtime, &chunk, "tcpSetup", PAIRS as f64, 0.0);
    });
    row("TCP pair", tcp, "");
    let secure = measure(instructions, PAIRS, || {
        run(&runtime, &chunk, "tlsSetup", PAIRS as f64, 0.0);
    });
    row("TCP pair + both handshakes", secure, &format!("handshakes: {:.0} µs", (secure.nanos - tcp.nanos) / 1000.0));

    header("A 16-byte round trip (client writes, server reads and answers)");
    const TRIPS: u64 = 20_000;
    row(
        "TCP",
        measure(instructions, TRIPS, || {
            run(&runtime, &chunk, "tcpRoundTrip", TRIPS as f64, 0.0);
        }),
        "",
    );
    row(
        "TLS (write + flush, readInto)",
        measure(instructions, TRIPS, || {
            run(&runtime, &chunk, "tlsRoundTrip", TRIPS as f64, 0.0);
        }),
        "",
    );
    const EMPTY: u64 = 200_000;
    row(
        "TCP `readInto`, nothing ready",
        measure(instructions, EMPTY, || {
            run(&runtime, &chunk, "tcpReadEmpty", EMPTY as f64, 0.0);
        }),
        "",
    );
    row(
        "TLS `readInto`, nothing ready",
        measure(instructions, EMPTY, || {
            run(&runtime, &chunk, "tlsReadEmpty", EMPTY as f64, 0.0);
        }),
        "one recv, no decryption",
    );

    println!("\nThroughput: one thread writes and drains the peer (TLS: write, flush, readInto)\n");
    println!("| Write size | TCP MiB/s | TLS MiB/s | TLS native allocs per MiB |");
    println!("|---:|---:|---:|---:|");
    for kib in [1usize, 4, 16, 64] {
        let total: usize = if kib == 1 { 16 << 20 } else { 64 << 20 };
        let plain = measure(instructions, 1, || {
            run(&runtime, &chunk, "tcpPump", total as f64, (kib * 1024) as f64);
        });
        let secure = measure(instructions, 1, || {
            run(&runtime, &chunk, "tlsPump", total as f64, (kib * 1024) as f64);
        });
        let mib = total as f64 / f64::from(1 << 20);
        println!(
            "| {kib} KiB | {:.0} | {:.0} | {:.1} |",
            mib / (plain.nanos / 1e9),
            mib / (secure.nanos / 1e9),
            secure.allocations / mib
        );
    }

    header("Configuration construction");
    row(
        "`tls.clientConfig` over one explicit root",
        measure(instructions, 200, || {
            run(&runtime, &chunk, "clientConfig", 200.0, 0.0);
        }),
        "",
    );
    row(
        "`tls.serverConfig` (chain and P-256 key parsed, matched)",
        measure(instructions, 200, || {
            run(&runtime, &chunk, "serverConfig", 200.0, 0.0);
        }),
        "",
    );
    let platform = Instant::now();
    let built = chunk.invoke::<Option<f64>, _>(&runtime.stack(), ("platformConfig", 1.0, 0.0));
    match built {
        Ok(_) => println!(
            "| `tls.clientConfig()` over the platform store | - | {:.1} | - | loads the system CA bundle once per config |",
            platform.elapsed().as_secs_f64() * 1e6
        ),
        Err(error) => {
            println!("| `tls.clientConfig()` over the platform store | - | - | - | unavailable here: {error} |");
        }
    }

    println!("\nRetention per connected pair (client and server), live after a full collection\n");
    println!("| Held | VM bytes | native bytes |");
    println!("|---|---:|---:|");
    for (label, case) in [("TCP pair", "keepTcp"), ("TLS pair after the handshake", "keepTls")] {
        runtime.gc(GcControl::Collect);
        let (vm, native) = (heap(&runtime), LIVE.load(Ordering::Relaxed));
        run(&runtime, &chunk, case, 64.0, 0.0);
        runtime.gc(GcControl::Collect);
        println!(
            "| {label} | {:.0} | {:.0} |",
            (heap(&runtime) - vm) / 64.0,
            (LIVE.load(Ordering::Relaxed) as f64 - native as f64) / 64.0
        );
        run(&runtime, &chunk, "drop", 0.0, 0.0);
    }
}

/// A CA, and a leaf for test.local with its key, PEM.
fn pki() -> (String, String, String) {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, "l3i bench CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["test.local".to_owned()]).unwrap().signed_by(&key, &issuer).unwrap();
    (ca.pem(), leaf.pem(), key.serialize_pem())
}

fn main() {
    let instructions = Instructions::open();
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(model) = cpuinfo.lines().find(|line| line.starts_with("model name"))
    {
        println!("{}", model.trim_start_matches("model name").trim_start_matches([' ', '\t', ':']));
    }
    dns(&instructions);
    tls(&instructions);
}
