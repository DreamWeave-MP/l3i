//! The dream-net bridge over real localhost UDP: schema from Luau, a host-created server, a
//! script-created client, events both ways through buffers, stats, and the capability gate.

use std::time::Duration;

use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::net::{self, NetSchema};
use l3i::userdata;
use l3i::{Error, Runtime};

const PROTOCOL: u64 = 0xD4EA_4E70_0000_0002;

const SCHEMA: &str = "schema = net.schema{ version = 1, \
    channels = { { name = 'reliable', delivery = 'reliableOrdered' }, { name = 'state', delivery = 'unreliableUnordered', capacity = 256, overflow = 'dropOldest' } }, \
    events = { { name = 'Ping', channel = 'reliable', maxPayload = 64 }, { name = 'Pong', channel = 'reliable', maxPayload = 64 }, { name = 'Move', channel = 'state', maxPayload = 12, codecVersion = 2 } } }";

fn plan(transport: bool) -> std::rc::Rc<RuntimePlan> {
    let mut policy = RuntimePolicy::new().compat_global("@dream/net", "net");
    if transport {
        policy = policy.capability(net::TRANSPORT_CAPABILITY);
    }
    RuntimePlan::builder().policy(policy).finalize().unwrap()
}

/// The schema the script built, read back for the Rust-side server.
fn schema_of(runtime: &Runtime) -> dream_net::Schema {
    let value = runtime.global("schema").unwrap();
    let stack = runtime.stack();
    stack
        .with_frame(|frame| {
            let view = value.push_to(frame)?;
            userdata::check_receiver::<NetSchema>(view).map(|s| s.0.clone())
        })
        .unwrap()
}

#[test]
fn schema_builds_from_options_and_reports_ids() {
    let runtime = Runtime::from_plan(&plan(false)).unwrap();
    runtime.exec(SCHEMA).unwrap();
    runtime
        .exec(
            "assert(schema.version == 1) assert(schema.eventCount == 3 and schema.channelCount == 2) \
             assert(schema:eventId('Move') ~= nil and schema:eventId('Nope') == nil) \
             assert(schema:eventName(schema:eventId('Ping')) == 'Ping') \
             assert(schema:channelName(schema:channelId('state')) == 'state') \
             assert(schema:maxPayload(schema:eventId('Move')) == 12i) \
             local hi, lo = schema:fingerprintHalves() assert(hi ~= nil and lo ~= nil) \
             assert(#schema.fingerprint == 32) assert(tostring(schema):find('dream.net.Schema')) \
             assert(net.CONNECT_TOKEN_BYTES == 2048 and net.MAX_CHANNELS == 64)",
        )
        .unwrap();
    let schema = schema_of(&runtime);
    assert_eq!(schema.events().len(), 3);
    // Strict options.
    let error =
        runtime.exec("net.schema{ version = 1, channels = {}, events = {}, extra = 1 }").unwrap_err().to_string();
    assert!(error.contains("unknown option 'extra'"), "{error}");
    let error = runtime
        .exec("net.schema{ version = 1, channels = { { name = 'a', delivery = 'sometimes' } }, events = {} }")
        .unwrap_err()
        .to_string();
    assert!(error.contains("channels[1].delivery"), "{error}");
    let error = runtime
        .exec("net.schema{ version = 1, channels = {}, events = { { name = 'X', channel = 'missing', maxPayload = 4 } } }")
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown channel 'missing'"), "{error}");
}

#[test]
fn client_creation_is_gated_by_the_transport_capability() {
    let runtime = Runtime::from_plan(&plan(false)).unwrap();
    runtime.exec(SCHEMA).unwrap();
    let error = runtime.exec("net.client{ schema = schema }").unwrap_err().to_string();
    assert!(error.contains("network.transport"), "{error}");
    let error =
        runtime.exec("net.server{ schema = schema, address = '127.0.0.1:0', protocolId = 1 }").unwrap_err().to_string();
    assert!(error.contains("network.transport"), "{error}");
}

#[test]
fn events_flow_both_ways_over_localhost() {
    let runtime = Runtime::from_plan(&plan(true)).unwrap();
    runtime
        .exec(&format!(
            "{SCHEMA} server = net.server{{ schema = schema, address = '127.0.0.1:0', protocolId = {}i, maxClients = 4 }} \
             token = server:connectToken(77i, 30, 5) assert(buffer.len(token) == net.CONNECT_TOKEN_BYTES)",
            PROTOCOL as i64
        ))
        .unwrap();
    let error = runtime.exec("server:connectToken(77i, 0)").unwrap_err().to_string();
    assert!(error.contains("expiresInSeconds must be in [1, 86400]"), "{error}");

    runtime
        .exec(
            "client = net.client{ schema = schema } assert(client.status == 'disconnected') assert(not client.connected) \
             assert(client.rtt == nil) client:connect(token) \
             buf = buffer.create(64) log = {} pings, pongs, moves = 0, 0, 0 \
             function step() \
                 server:update() client:update() \
                 while true do \
                     local kind, peer, a, b, c = server:pollInto(buf) \
                     if not kind then break end \
                     table.insert(log, 'server:' .. kind) \
                     if kind == 'connected' then assert(a == 77i, 'client id') serverPeer = peer end \
                     if kind == 'message' then \
                         if a == schema:eventId('Ping') then pings += 1 assert(c == 4 and buffer.readstring(buf, 0, 4) == 'ping') \
                             server:sendEvent(peer, schema:eventId('Pong'), 'pong!') \
                         elseif a == schema:eventId('Move') then moves += 1 assert(b == schema:channelId('state')) \
                             assert(buffer.readf32(buf, 0) == 1.5) end \
                     end \
                 end \
                 while true do \
                     local kind, _, a, b, c = client:pollInto(buf) \
                     if not kind then break end \
                     table.insert(log, 'client:' .. kind) \
                     if kind == 'connected' then \
                         local out = buffer.create(16) buffer.writestring(out, 0, 'ping') client:sendEvent(schema:eventId('Ping'), out, 0, 4) \
                         local move = buffer.create(12) buffer.writef32(move, 0, 1.5) client:sendEvent(schema:eventId('Move'), move) \
                     end \
                     if kind == 'message' and a == schema:eventId('Pong') then pongs += 1 assert(buffer.readstring(buf, 0, c) == 'pong!') end \
                 end \
                 server:flush() client:flush() \
             end",
        )
        .unwrap();

    let step = runtime.load_function("return step").unwrap();
    let done = runtime.load_function("return function() return pongs >= 1 and moves >= 1 end").unwrap();
    let mut finished = false;
    for _ in 0..3000 {
        step.invoke::<(), ()>(&runtime.stack(), ()).unwrap();
        if done.invoke::<bool, ()>(&runtime.stack(), ()).unwrap() {
            finished = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(finished, "handshake or events did not complete");
    runtime
        .exec(
            "assert(pings == 1 and pongs == 1 and moves == 1, 'counts') assert(client.connected and client.status == 'connected') \
             assert(server.numConnected == 1) assert(#server:peers() == 1 and server:peers()[1] == serverPeer) \
             assert(server:clientId(serverPeer) == 77i) assert(server:clientAddress(serverPeer):find('127.0.0.1')) \
             assert(type(server:peerRtt(serverPeer)) == 'number') assert(type(client.rtt) == 'number') assert(client.packetLoss ~= nil) \
             assert(server:counters(serverPeer).eventsReceived >= 2) assert(client:counters().eventsSent >= 2) \
             assert(server:memoryUsage().total > 0 and client:memoryUsage().total >= 0) \
             assert(tostring(server):find('1 connected')) assert(tostring(client) == 'dream.net.Client(connected)') \
             assert(log[1] == 'server:connected' or log[1] == 'client:connected')",
        )
        .unwrap();
    // Misuse is a script error, not a panic.
    let error = runtime.exec("client:sendEvent(999, 'x')").unwrap_err().to_string();
    assert!(error.contains("dream.net"), "{error}");
    let error = runtime.exec("client:sendEvent(schema:eventId('Ping'), 'toolong', 0, 40)").unwrap_err().to_string();
    assert!(error.contains("exceeds the 7-byte buffer"), "{error}");
    let error = runtime.exec("server:sendEvent(12345i, schema:eventId('Ping'), 'x')").unwrap_err().to_string();
    assert!(error.contains("dream.net"), "{error}");
    // A too-small poll buffer reports the size it needed and consumes nothing: the same event
    // is still there for a big enough buffer afterwards.
    runtime.exec("server:sendEvent(serverPeer, schema:eventId('Pong'), string.rep('z', 40)) server:flush()").unwrap();
    let mut got = false;
    for _ in 0..3000 {
        runtime
            .exec(
                "client:update() local tiny = buffer.create(8) local ok, err = pcall(client.pollInto, client, tiny) \
                 if not ok then assert(string.find(err, 'too small'), err) \
                     local kind, _, a, _, c = client:pollInto(buf) assert(kind == 'message' and a == schema:eventId('Pong') and c == 40, kind) \
                     assert(buffer.readstring(buf, 0, 40) == string.rep('z', 40)) small = true end",
            )
            .unwrap();
        let small: bool = runtime
            .load_function("return function() return small == true end")
            .unwrap()
            .invoke(&runtime.stack(), ())
            .unwrap();
        if small {
            got = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(got, "expected a too-small poll buffer error followed by the event");
    runtime.exec("client:disconnect() assert(client.status == 'disconnected')").unwrap();
    let _ = Error::LuaErrorOnStack;
}

#[test]
fn the_plan_supplies_the_transport_clock() {
    use std::cell::Cell;
    use std::rc::Rc;
    let now = Rc::new(Cell::new(10.0f64));
    let reads = Rc::new(Cell::new(0u32));
    let clock: net::Clock = {
        let (now, reads) = (Rc::clone(&now), Rc::clone(&reads));
        Rc::new(move || {
            reads.set(reads.get() + 1);
            now.get()
        })
    };
    let policy = RuntimePolicy::new().compat_global("@dream/net", "net").capability(net::TRANSPORT_CAPABILITY);
    let plan = RuntimePlan::builder().policy(policy).network_clock(clock).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(&format!("{SCHEMA} client = net.client({{ schema = schema }}) client:update()")).unwrap();
    assert!(reads.get() > 0, "the client read the plan's clock");
    now.set(11.0);
    runtime.exec("client:update()").unwrap();
}
