//! The dream-net bridge hot paths over localhost: `sendEvent` from Luau, the receive phase
//! (`update` + `pollInto`), and a round trip, all through the extension-planned handles.

// The shared CI runs `-W clippy::pedantic -D warnings`.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::missing_panics_doc,
    clippy::semicolon_if_nothing_returned
)]

use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::stack::Scope;
use l3i::udp::{self, Server, UdpSchema};
use l3i::value::Function;
use l3i::{Runtime, userdata};

const PROTOCOL: u64 = 0xD4EA_4E70_0000_0003;
const EVENTS: u64 = 256;

fn connected_runtime() -> (Runtime, Function) {
    let policy = RuntimePolicy::new().compat_global("@dream/udp", "udp").capability(udp::TRANSPORT_CAPABILITY);
    let plan = RuntimePlan::builder().policy(policy).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime
        .exec(
            "schema = udp.schema{ version = 1, channels = { { name = 'state', delivery = 'unreliableUnordered', capacity = 1024, overflow = 'dropOldest' } }, \
             events = { { name = 'Move', channel = 'state', maxPayload = 32 } } }",
        )
        .unwrap();
    let schema = {
        let value = runtime.global("schema").unwrap();
        let stack = runtime.stack();
        stack
            .with_frame(|frame| userdata::check_receiver::<UdpSchema>(value.push_to(frame)?).map(|s| s.0.clone()))
            .unwrap()
    };
    let key = dream_net::generate_key();
    let config = dream_net::ServerConfig {
        public_address: "127.0.0.1:0".parse().unwrap(),
        protocol_id: PROTOCOL,
        max_clients: 2,
        transport: dream_net::TransportConfig::default(),
    };
    let clock = udp::monotonic_clock();
    let server = dream_net::Server::new(config, &key, schema, clock()).unwrap();
    let address = server.address();
    let token = dream_net::generate_connect_token(
        &[address],
        &[address],
        300,
        30,
        1,
        PROTOCOL,
        &key,
        &[0u8; dream_net::USER_DATA_BYTES],
    )
    .unwrap();
    {
        let stack = runtime.stack();
        let frame = stack.frame();
        Server::push(&frame, server, clock).unwrap();
        frame.set_global("server").unwrap();
        frame.push(&token[..]).unwrap();
        frame.set_global("token").unwrap();
    }
    runtime
        .exec(
            "client = udp.client{ schema = schema } client:connect(token) buf = buffer.create(64) out = buffer.create(32) buffer.writef32(out, 0, 1.5) \
             move = schema:eventId('Move') \
             function pump() server:update() client:update() \
                 while server:pollInto(buf) do end while client:pollInto(buf) do end server:flush() client:flush() end",
        )
        .unwrap();
    let pump = runtime.load_function("return pump").unwrap();
    let connected =
        runtime.load_function("return function() return client.connected and server.numConnected == 1 end").unwrap();
    for _ in 0..5000 {
        pump.invoke::<(), ()>(&runtime.stack(), ()).unwrap();
        if connected.invoke::<bool, ()>(&runtime.stack(), ()).unwrap() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(connected.invoke::<bool, ()>(&runtime.stack(), ()).unwrap(), "bench client did not connect");
    (runtime, pump)
}

fn udp_bridge(c: &mut Criterion) {
    let (runtime, _pump) = connected_runtime();
    let mut group = c.benchmark_group("udp_bridge");
    group.throughput(Throughput::Elements(EVENTS));
    let send = runtime
        .load_function(&format!(
            "return function() for i = 1, {EVENTS} do client:sendEvent(move, out, 0, 12) end client:flush() end"
        ))
        .unwrap();
    let drain =
        runtime.load_function("return function() server:update() while server:pollInto(buf) do end end").unwrap();
    let round_trip = runtime
        .load_function(&format!(
            "return function() for i = 1, {EVENTS} do client:sendEvent(move, out, 0, 12) end client:flush() \
             local got = 0 for attempt = 1, 64 do server:update() while server:pollInto(buf) do got += 1 end if got >= {EVENTS} then break end end return got end"
        ))
        .unwrap();
    let idle = runtime.load_function("return function() server:update() client:update() while server:pollInto(buf) do end while client:pollInto(buf) do end end").unwrap();
    // Every function is loaded before the root stack lease, which `load_function` also needs.
    let stack = runtime.stack();
    group.bench_function("client sendEvent + flush", |b| {
        b.iter(|| {
            send.invoke::<(), ()>(&stack, ()).unwrap();
            // Drain the server so queues never fill.
            drain.invoke::<(), ()>(&stack, ()).unwrap();
        })
    });
    group.bench_function("client send, server update + pollInto", |b| {
        b.iter(|| round_trip.invoke::<f64, ()>(&stack, ()).unwrap())
    });
    group.throughput(Throughput::Elements(1));
    group.bench_function("idle update + empty pollInto, both ends", |b| {
        b.iter(|| idle.invoke::<(), ()>(&stack, ()).unwrap())
    });
    group.finish();
}

fn configure() -> Criterion {
    Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3))
}

criterion_group! { name = benches; config = configure(); targets = udp_bridge }
criterion_main!(benches);
