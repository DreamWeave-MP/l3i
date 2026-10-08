//! `@dream/tcp` over real loopback sockets on ephemeral ports: the capability gates, IPv4 and
//! IPv6 addresses, connects settled by the socket error, echo with binary payloads across
//! several clients, end of stream against would-block, partial writes under backpressure,
//! half-close, level-triggered readiness and token reuse, bounded and fair waits, the watchdog,
//! and sockets released by close and by collection.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::runtime::{CallContext, CallKind, Limits, MemoryCategory};
use l3i::tcp::{CONNECT_CAPABILITY, LISTEN_CAPABILITY, Listener, PUBLIC_CAPABILITY, Stream, TcpExtension};

/// Script helpers: each waits on its own poller, so the handle must not be watched elsewhere.
const HELPERS: &str = r"
function settle(stream)
    local poller = tcp.poller()
    assert(poller:watch(stream, 1, 'write'))
    for _ = 1, 200 do
        local ok, message, kind = stream:finishConnect()
        if ok ~= false then poller:close() return ok, message, kind end
        poller:wait(25)
    end
    error('the connect never settled')
end

function acceptOne(listener)
    local poller = tcp.poller()
    assert(poller:watch(listener, 1, 'read'))
    for _ = 1, 200 do
        local stream, peer, kind = listener:accept()
        if stream then poller:close() return stream, peer end
        assert(kind == 'wouldBlock', peer)
        poller:wait(25)
    end
    error('nothing to accept')
end

function pair(address)
    local listener = assert(tcp.listen(address or '127.0.0.1:0'))
    local client = assert(tcp.connect(listener.localAddress))
    local server, peer = acceptOne(listener)
    assert(settle(client) == true)
    return client, server, listener, peer
end

function readExactly(stream, n)
    local out = buffer.create(n)
    local got = 0
    local poller = tcp.poller()
    assert(poller:watch(stream, 1, 'read'))
    for _ = 1, 400 do
        if got == n then break end
        local count, message, kind = stream:readInto(out, got, n - got)
        if count == 0 then error(`end of stream after {got} of {n} bytes`) end
        if count then got += count else assert(kind == 'wouldBlock', message) poller:wait(25) end
    end
    poller:close()
    assert(got == n, `read {got} of {n} bytes`)
    return buffer.tostring(out)
end

function readToEnd(stream)
    local parts = {}
    local scratch = buffer.create(4096)
    local poller = tcp.poller()
    assert(poller:watch(stream, 1, 'read'))
    for _ = 1, 400 do
        local count, message, kind = stream:readInto(scratch)
        if count == 0 then poller:close() return table.concat(parts) end
        if count then table.insert(parts, buffer.readstring(scratch, 0, count))
        else assert(kind == 'wouldBlock', message) poller:wait(25) end
    end
    error('no end of stream')
end

function writeAll(stream, data)
    local total = if type(data) == 'string' then #data else buffer.len(data)
    local at = 0
    local poller = tcp.poller()
    assert(poller:watch(stream, 1, 'write'))
    for _ = 1, 400 do
        if at == total then break end
        local count, message, kind = stream:write(data, at)
        if count then at += count else assert(kind == 'wouldBlock', message) poller:wait(25) end
    end
    poller:close()
    assert(at == total, `wrote {at} of {total} bytes`)
end
";

fn runtime(grants: &[&str]) -> Runtime {
    runtime_with(grants, None)
}

fn runtime_with(grants: &[&str], limits: Option<Limits>) -> Runtime {
    let mut policy = RuntimePolicy::new().compat_global("@dream/tcp", "tcp");
    for grant in grants {
        policy = policy.capability(grant);
    }
    if let Some(limits) = limits {
        policy = policy.limits(limits);
    }
    let plan = RuntimePlan::builder().policy(policy).extension(TcpExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(HELPERS).unwrap();
    runtime
}

fn both() -> Runtime {
    runtime(&[LISTEN_CAPABILITY, CONNECT_CAPABILITY])
}

fn error_of(runtime: &Runtime, source: &str) -> String {
    runtime.exec(source).unwrap_err().to_string()
}

#[test]
fn the_extension_registers_its_module_and_typed_definitions() {
    let plan = RuntimePlan::builder().extension(TcpExtension).finalize().unwrap();
    assert!(plan.modules().iter().any(|module| module.path == "@dream/tcp"));
    let definitions = plan.type_definitions();
    for declared in [
        "declare extern type dream_tcp_Stream with",
        "declare extern type dream_tcp_Listener with",
        "declare extern type dream_tcp_Poller with",
        "type dream_tcp_ErrorKind =",
    ] {
        assert!(definitions.contains(declared), "{declared} missing:\n{definitions}");
    }
    let runtime = runtime(&[]);
    runtime
        .exec("assert(tcp.MAX_WAIT_MS == 60000) assert(tostring(tcp.poller()) == 'dream.tcp.Poller(0 watched)')")
        .unwrap();
}

#[test]
fn nothing_is_granted_unless_the_policy_says_so() {
    let none = runtime(&[]);
    let error = error_of(&none, "tcp.listen('127.0.0.1:0')");
    assert!(error.contains("needs the 'network.tcp.listen' capability"), "{error}");
    let error = error_of(&none, "tcp.connect('127.0.0.1:9')");
    assert!(error.contains("needs the 'network.tcp.connect' capability"), "{error}");
    none.exec("local poller = tcp.poller() assert(poller:wait(0) == 0) poller:close()").unwrap();

    // The UDP transport's capability grants nothing here.
    let udp = runtime(&[l3i::udp::TRANSPORT_CAPABILITY]);
    assert!(error_of(&udp, "tcp.listen('127.0.0.1:0')").contains("network.tcp.listen"));
    assert!(error_of(&udp, "tcp.connect('127.0.0.1:9')").contains("network.tcp.connect"));

    // Listening does not grant connecting, and loopback is all it grants.
    let listen = runtime(&[LISTEN_CAPABILITY]);
    listen.exec("local l = assert(tcp.listen('127.0.0.1:0')) l:close() local l6 = tcp.listen('[::1]:0') if l6 then l6:close() end").unwrap();
    assert!(error_of(&listen, "tcp.connect('127.0.0.1:9')").contains("network.tcp.connect"));
    for wildcard in ["0.0.0.0:0", "[::]:0", "192.0.2.1:0"] {
        let error = error_of(&listen, &format!("tcp.listen('{wildcard}')"));
        assert!(error.contains("not a loopback address") && error.contains("network.tcp.public"), "{error}");
    }
    // Public widens listening; it is not listening by itself.
    let public = runtime(&[PUBLIC_CAPABILITY]);
    assert!(error_of(&public, "tcp.listen('0.0.0.0:0')").contains("network.tcp.listen"));
    let wide = runtime(&[LISTEN_CAPABILITY, PUBLIC_CAPABILITY]);
    wide.exec("local l = assert(tcp.listen('0.0.0.0:0')) assert(l.localAddress:match('^0%.0%.0%.0:%d+$')) l:close()")
        .unwrap();

    // Connecting does not grant listening; a stream accepted from a listener the host made
    // needs no capability at all.
    let connect = runtime(&[CONNECT_CAPABILITY]);
    assert!(error_of(&connect, "tcp.listen('127.0.0.1:0')").contains("network.tcp.listen"));
    let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = host.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(b"from the host side").unwrap();
    });
    let none = runtime(&[]);
    {
        let stack = none.stack();
        stack
            .with_frame(|frame| {
                Listener::push(frame, Listener::from_std(host, 4).unwrap())?;
                frame.set_global("hostListener")
            })
            .unwrap();
    }
    client.join().unwrap();
    none.exec("local s = acceptOne(hostListener) assert(readExactly(s, 18) == 'from the host side') s:close()")
        .unwrap();
}

#[test]
fn ipv4_and_ipv6_loopback_report_their_addresses() {
    let runtime = both();
    runtime
        .exec(
            r"
            local client, server, listener, peer = pair('127.0.0.1:0')
            assert(listener.localAddress:match('^127%.0%.0%.1:%d+$') and not listener.localAddress:match(':0$'))
            assert(client.peerAddress == listener.localAddress)
            assert(server.peerAddress == client.localAddress and peer == client.localAddress)
            assert(server.localAddress == listener.localAddress)
            assert(client.state == 'connected' and server.state == 'connected' and not client.closed)
            assert(tostring(client) == `dream.tcp.Stream({listener.localAddress}, connected)`)
            assert(tostring(listener) == `dream.tcp.Listener({listener.localAddress})`)
            client:close() server:close() listener:close()
            ",
        )
        .unwrap();
    // IPv6 when the host has it.
    let has_ipv6 = std::net::TcpListener::bind("[::1]:0").is_ok();
    if has_ipv6 {
        runtime
            .exec(
                r"
                local client, server, listener = pair('[::1]:0')
                assert(listener.localAddress:match('^%[::1%]:%d+$'), listener.localAddress)
                assert(server.peerAddress == client.localAddress and client.peerAddress == listener.localAddress)
                writeAll(client, 'six') assert(readExactly(server, 3) == 'six')
                client:close() server:close() listener:close()
                ",
            )
            .unwrap();
    } else {
        eprintln!("no IPv6 loopback on this host; skipping the IPv6 half");
    }
    for bad in ["localhost:80", "127.0.0.1", "127.0.0.1:99999", "", "::1:80", "example.com:443"] {
        let error = error_of(&runtime, &format!("tcp.connect('{bad}')"));
        assert!(error.contains("is not a numeric address and port"), "{bad}: {error}");
        let error = error_of(&runtime, &format!("tcp.listen('{bad}')"));
        assert!(error.contains("is not a numeric address and port"), "{bad}: {error}");
    }
}

#[test]
fn a_connect_is_settled_by_the_socket_error() {
    // A port nothing listens on: bound, read, released.
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let runtime = both();
    runtime
        .exec(&format!(
            r"
            local stream, message, kind = tcp.connect('127.0.0.1:{closed_port}')
            if stream then
                local ok, why, failure = settle(stream)
                assert(ok == nil and failure == 'connectionRefused', tostring(why))
                assert(stream.state == 'failed')
                -- Every transfer after a failed connect reports the same failure.
                local n, again, sameKind = stream:readInto(buffer.create(4))
                assert(n == nil and sameKind == 'connectionRefused' and again == why, again)
                n, again, sameKind = stream:write('x')
                assert(n == nil and sameKind == 'connectionRefused')
                local done, _, k = stream:finishConnect() assert(done == nil and k == 'connectionRefused')
                stream:close() assert(stream.state == 'closed')
            else
                assert(kind == 'connectionRefused', message)
            end

            -- A pending connect: transfers report wouldBlock until it settles, never EOF.
            local listener = assert(tcp.listen('127.0.0.1:0'))
            local client = assert(tcp.connect(listener.localAddress))
            assert(client.state == 'connecting')
            local n, why, k = client:readInto(buffer.create(8))
            assert(n == nil and k == 'wouldBlock', why)
            assert(settle(client) == true and client.state == 'connected')
            assert(client:finishConnect() == true)
            local server = acceptOne(listener)
            writeAll(client, 'ok') assert(readExactly(server, 2) == 'ok')
            client:close() server:close() listener:close()
            "
        ))
        .unwrap();
}

#[test]
fn an_echo_server_serves_several_clients_binary_payloads() {
    let runtime = both();
    runtime
        .exec(
            r"
            local listener = assert(tcp.listen('127.0.0.1:0', { backlog = 16, noDelay = true }))
            local poller = tcp.poller()
            assert(poller:watch(listener, 0, 'read'))
            local clients, payloads, sent, received = {}, {}, {}, {}
            for i = 1, 4 do
                clients[i] = assert(tcp.connect(listener.localAddress, { noDelay = i % 2 == 0 }))
                payloads[i] = string.rep('\0' .. string.char(i) .. 'echo\255\r\n', 3000 * i)
                sent[i], received[i] = 0, {}
                assert(poller:watch(clients[i], i, 'readwrite'))
            end
            local servers, nextToken = {}, 100
            local scratch = buffer.create(8192)
            local function complete()
                for i = 1, 4 do
                    if #table.concat(received[i]) < #payloads[i] then return false end
                end
                return true
            end
            for _ = 1, 4000 do
                if complete() then break end
                poller:wait(50)
                while true do
                    local token, readable, writable = poller:next()
                    if token == nil then break end
                    if token == 0 then
                        while true do
                            local stream, peer, kind = listener:accept()
                            if not stream then assert(kind == 'wouldBlock', peer) break end
                            nextToken += 1
                            servers[nextToken] = { stream = stream, pending = '' }
                            assert(poller:watch(stream, nextToken, 'read'))
                        end
                    elseif token <= 4 then
                        local client = clients[token]
                        if writable and sent[token] < #payloads[token] then
                            local n, message, kind = client:write(payloads[token], sent[token])
                            if n then sent[token] += n else assert(kind == 'wouldBlock', message) end
                            if sent[token] == #payloads[token] then poller:modify(token, 'read') end
                        end
                        if readable then
                            while true do
                                local n, message, kind = client:readInto(scratch)
                                if n == nil then assert(kind == 'wouldBlock', message) break end
                                assert(n > 0, 'the server closed early')
                                table.insert(received[token], buffer.readstring(scratch, 0, n))
                            end
                        end
                    else
                        local s = servers[token]
                        if readable then
                            while true do
                                local n, message, kind = s.stream:readInto(scratch)
                                if n == nil then assert(kind == 'wouldBlock', message) break end
                                if n == 0 then break end
                                s.pending ..= buffer.readstring(scratch, 0, n)
                            end
                        end
                        if #s.pending > 0 then
                            local n, message, kind = s.stream:write(s.pending)
                            if n then s.pending = string.sub(s.pending, n + 1) else assert(kind == 'wouldBlock', message) end
                        end
                        poller:modify(token, if #s.pending > 0 then 'readwrite' else 'read')
                    end
                end
            end
            assert(complete(), 'the echo did not complete')
            for i = 1, 4 do assert(table.concat(received[i]) == payloads[i], `payload {i} differs`) end
            assert(listener.streams == 4 and poller.watching == 9)
            for _, s in servers do s.stream:close() end
            for _, c in clients do c:close() end
            assert(poller.watching == 1 and listener.streams == 0)
            poller:close() listener:close()
            ",
        )
        .unwrap();
}

#[test]
fn end_of_stream_is_never_would_block_and_windows_are_checked() {
    let runtime = both();
    runtime
        .exec(
            r"
            local client, server, listener = pair()
            local n, message, kind = server:readInto(buffer.create(16))
            assert(n == nil and kind == 'wouldBlock' and message:find('no bytes are ready'), message)
            assert(client:write('') == 0 and client:write(buffer.create(0)) == 0)
            assert(client:write('abcdef', 1, 1) == 1)
            assert(readExactly(server, 1) == 'b')
            -- A read lands at the offset and touches nothing else.
            local target = buffer.create(8) buffer.fill(target, 0, 0x2a)
            writeAll(client, 'xy')
            local poller = tcp.poller() poller:watch(server, 1, 'read') poller:wait(1000) poller:close()
            assert(server:readInto(target, 3, 2) == 2)
            assert(buffer.tostring(target) == '***xy***')
            client:shutdown('write')
            assert(readToEnd(server) == '')
            -- End of stream stays end of stream.
            assert(server:readInto(buffer.create(4)) == 0)
            assert(server:readInto(buffer.create(4)) == 0)
            for _, case in {
                { function() server:readInto(buffer.create(0)) end, 'no room to read into' },
                { function() server:readInto(buffer.create(8), 8) end, 'no room to read into' },
                { function() server:readInto(buffer.create(8), 9) end, 'offset 9 past the end of the buffer (size 8)' },
                { function() server:readInto(buffer.create(8), 2, 7) end, 'length 7 does not fit the buffer (space 6 after offset 2)' },
                { function() server:readInto(buffer.create(8), -1) end, 'offset -1 is negative' },
                { function() server:readInto(buffer.create(8), 0.5) end, '' },
                { function() server:readInto('text') end, '' },
                { function() client:write(buffer.create(4), 2, 5) end, 'length 5 does not fit the data' },
                { function() client:write('abc', 4) end, 'offset 4 past the end of the data' },
                { function() client:write(42) end, '' },
                { function() server:shutdown('sideways') end, [[expected 'read', 'write' or 'both']] },
            } do
                local ok, err = pcall(case[1])
                assert(not ok and string.find(err, case[2], 1, true), err)
            end
            client:close() server:close() listener:close()
            ",
        )
        .unwrap();
}

#[test]
fn writes_are_partial_under_backpressure_and_nothing_is_queued_behind_them() {
    let runtime = both();
    runtime
        .exec(
            r"
            local client, server, listener = pair()
            -- 65511 = 251 * 261: byte k of the stream is k % 251 whatever the write sizes.
            local pattern = buffer.create(65511)
            for i = 0, 65510 do buffer.writeu8(pattern, i, i % 251) end
            local sent, blocked = 0, false
            for _ = 1, 100000 do
                local at = sent % 65511
                local n, message, kind = client:write(pattern, at)
                if n == nil then
                    assert(kind == 'wouldBlock' and message:find('send buffer is full'), message)
                    blocked = true
                    break
                end
                sent += n
            end
            assert(blocked, 'the receiver never pushed back')
            assert(sent > 0 and sent < 64 * 1024 * 1024, `{sent} bytes accepted with nobody reading`)
            -- A blocked stream is not writable until the receiver drains.
            local poller = tcp.poller()
            assert(poller:watch(client, 1, 'write'))
            assert(poller:wait(0) == 0)
            -- Drain everything and check every byte.
            local scratch = buffer.create(65536)
            local got = 0
            local reader = tcp.poller() assert(reader:watch(server, 2, 'read'))
            for _ = 1, 100000 do
                if got == sent then break end
                local n, message, kind = server:readInto(scratch)
                if n == nil then assert(kind == 'wouldBlock', message) reader:wait(25)
                else
                    assert(n > 0)
                    for j = 0, n - 1 do
                        if buffer.readu8(scratch, j) ~= (got + j) % 251 then error(`byte {got + j} is wrong`) end
                    end
                    got += n
                end
            end
            assert(got == sent, `received {got} of {sent}`)
            -- Nothing beyond what write reported was sent.
            assert(server:readInto(scratch) == nil)
            -- And the writer is writable again.
            assert(poller:wait(1000) == 1)
            local token, readable, writable = poller:next()
            assert(token == 1 and writable and not readable)
            poller:close() reader:close() client:close() server:close() listener:close()
            ",
        )
        .unwrap();
}

#[test]
fn a_half_close_ends_one_direction_only() {
    let runtime = both();
    runtime
        .exec(
            r"
            local client, server, listener = pair()
            writeAll(client, 'request')
            assert(client:shutdown('write') == true)
            -- The peer of a half-closed stream sees the end of stream, flagged as closed.
            local poller = tcp.poller()
            assert(poller:watch(server, 5, 'read'))
            assert(poller:wait(1000) == 1)
            local token, readable, writable, closed = poller:next()
            assert(token == 5 and readable and not writable and closed)
            poller:unwatch(5)
            assert(readToEnd(server) == 'request')
            -- ...but can still answer, and the half-closed side still reads.
            writeAll(server, 'response')
            server:shutdown('write')
            assert(readToEnd(client) == 'response')
            -- A write after shutdown('write') is the OS's refusal, not an exception.
            local n, message, kind = client:write('late')
            assert(n == nil and kind ~= 'wouldBlock', message)
            client:close() server:close() listener:close() poller:close()
            ",
        )
        .unwrap();
}

#[test]
fn readiness_is_level_triggered_and_tokens_never_go_stale() {
    let runtime = both();
    runtime
        .exec(
            r"
            local listener = assert(tcp.listen('127.0.0.1:0'))
            local poller = tcp.poller()
            assert(poller:watch(listener, 7, 'read'))
            local a = assert(tcp.connect(listener.localAddress))
            local b = assert(tcp.connect(listener.localAddress))
            assert(poller:wait(1000) >= 1)
            local token, readable = poller:next()
            assert(token == 7 and readable and poller:next() == nil)
            -- Not drained: still reported, with no new OS notification.
            assert(poller:wait(0) == 1 and poller:next() == 7)
            -- Drained to wouldBlock (both connections may need a moment to arrive).
            local accepted = {}
            for _ = 1, 100 do
                local s, peer, kind = listener:accept()
                if s then table.insert(accepted, s)
                elseif #accepted == 2 then assert(kind == 'wouldBlock') break
                else poller:wait(20) end
            end
            assert(#accepted == 2)
            assert(poller:wait(0) == 0, 'a drained listener is not ready')
            local server = accepted[1]
            assert(settle(a) == true and settle(b) == true)

            -- A partial read leaves the stream readable; a would-block read clears it.
            assert(poller:watch(server, 8, 'read'))
            writeAll(a, 'abc')
            assert(poller:wait(1000) == 1 and poller:next() == 8)
            assert(server:readInto(buffer.create(1)) == 1)
            assert(poller:wait(0) == 1 and poller:next() == 8)
            assert(server:readInto(buffer.create(8)) == 2)
            assert(poller:wait(0) == 1, 'readable until a read says otherwise')
            assert(server:readInto(buffer.create(8)) == nil)
            assert(poller:wait(0) == 0)

            -- modify changes what is reported, without a system call.
            poller:modify(8, 'write')
            assert(poller:wait(1000) == 1)
            local t, r, w, c = poller:next()
            assert(t == 8 and not r and w and not c)
            poller:modify(8, 'read')
            assert(poller:wait(0) == 0)

            -- An event queued before unwatch is never handed out.
            poller:modify(8, 'readwrite')
            assert(poller:wait(0) == 1)
            poller:unwatch(8)
            assert(poller:next() == nil)
            -- Nor one queued for a token reused by another handle.
            assert(poller:watch(server, 8, 'write'))
            assert(poller:wait(0) == 1)
            poller:unwatch(8)
            assert(poller:watch(accepted[2], 8, 'read'))
            assert(poller:next() == nil, 'the old watch under token 8 must not leak into the new one')
            -- Nor one for a handle closed after the wait.
            assert(poller:watch(server, 9, 'write'))
            assert(poller:wait(0) == 1)
            assert(poller.watching == 3)
            server:close()
            assert(poller.watching == 2 and poller:next() == nil)

            for _, case in {
                { function() poller:watch(a, 7, 'read') end, 'token 7 is already watching a handle' },
                { function() poller:watch(accepted[2], 10, 'read') end, 'already watched' },
                { function() tcp.poller():watch(listener, 1, 'read') end, 'already watched' },
                { function() poller:watch(b, 10, 'write') poller:watch(b, 11, 'write') end, 'already watched' },
                { function() poller:watch(server, 12, 'read') end, 'the handle is closed' },
                { function() poller:modify(7, 'write') end, 'a listener is only ever readable' },
                { function() poller:modify(10, 'sometimes') end, [[interest must be 'read', 'write' or 'readwrite']] },
                { function() poller:modify(99, 'read') end, 'token 99 is not watching a handle' },
                { function() poller:unwatch(99) end, 'token 99 is not watching a handle' },
                { function() poller:watch({}, 13, 'read') end, 'dream.tcp.Listener or dream.tcp.Stream' },
                { function() poller:watch(a, -1, 'read') end, 'token must be a whole number' },
                { function() poller:watch(a, 2 ^ 60, 'read') end, 'token must be a whole number' },
                { function() poller:watch(a, 1.5, 'read') end, '' },
            } do
                local ok, err = pcall(case[1])
                assert(not ok and string.find(err, case[2], 1, true), err)
            end
            poller:close()
            assert(poller.closed and listener.closed == false)
            -- A closed poller's handles may be watched again.
            local again = tcp.poller()
            assert(again:watch(listener, 1, 'read') and again:watch(b, 2, 'write'))
            again:close() a:close() b:close() accepted[2]:close() listener:close()
            ",
        )
        .unwrap();
}

#[test]
fn waits_are_bounded_and_events_are_shared_fairly() {
    let runtime = both();
    let started = Instant::now();
    runtime.exec("idle = tcp.poller() assert(idle:wait(0) == 0)").unwrap();
    assert!(started.elapsed() < Duration::from_millis(50), "wait(0) does not wait");
    let started = Instant::now();
    runtime.exec("assert(idle:wait(120) == 0)").unwrap();
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(110) && waited < Duration::from_millis(1000), "{waited:?}");
    for (source, expected) in [
        ("idle:wait(-1)", "timeout must be in [0, 1000]"),
        ("idle:wait(1001)", "timeout must be in [0, 1000]"),
        ("idle:wait(0 / 0)", "timeout must be in [0, 1000]"),
        ("tcp.poller({ maxWaitMs = 10 }):wait(11)", "timeout must be in [0, 10]"),
        ("tcp.poller({ maxWaitMs = 60001 })", "maxWaitMs must be in [0, 60000]"),
        ("tcp.poller({ maxEvents = 0 })", "maxEvents must be in [1, 65536]"),
        ("tcp.poller({ maxWatches = 70000 })", "maxWatches must be in [1, 65536]"),
        ("tcp.poller({ forever = true })", "unknown option 'forever'"),
        (
            "local p = tcp.poller({ maxWatches = 1 }) p:watch(tcp.listen('127.0.0.1:0'), 1, 'read') p:watch(tcp.listen('127.0.0.1:0'), 2, 'read')",
            "maxWatches (1)",
        ),
    ] {
        let error = error_of(&runtime, source);
        assert!(error.contains(expected), "{source}: {error}");
    }
    runtime.exec("assert(tcp.poller({ maxWaitMs = 60000 }):wait(0) == 0)").unwrap();

    // Eight always-writable streams, three events per wait: every token comes round.
    runtime
        .exec(
            r"
            local listener = assert(tcp.listen('127.0.0.1:0'))
            local poller = tcp.poller({ maxEvents = 3 })
            local streams = {}
            for i = 1, 8 do
                streams[i] = assert(tcp.connect(listener.localAddress))
                assert(settle(streams[i]) == true)
                assert(poller:watch(streams[i], i, 'write'))
            end
            local seen, count = {}, 0
            for _ = 1, 3 do
                assert(poller:wait(1000) == 3)
                while true do
                    local token = poller:next()
                    if not token then break end
                    if not seen[token] then seen[token] = true count += 1 end
                end
            end
            assert(count == 8, `{count} of 8 tokens reported in three waits of three`)
            poller:close() listener:close()
            for _, s in streams do s:close() end
            ",
        )
        .unwrap();
}

#[test]
fn a_wait_never_outlasts_the_execution_time_limit() {
    let limits = Limits { execution_time: Duration::from_millis(60), memory_bytes: 0 };
    let runtime = runtime_with(&[], Some(limits));
    let waiting =
        runtime.load_function("return function() local p = tcp.poller() while true do p:wait(1000) end end").unwrap();
    let started = Instant::now();
    let error = {
        let _scope = runtime.call_scope(CallContext { id: 1, category: MemoryCategory(0) }, CallKind::ScriptCall);
        waiting.invoke::<(), _>(&runtime.stack(), ()).unwrap_err().to_string()
    };
    assert!(error.contains("execution time limit exceeded"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(900), "{:?}", started.elapsed());
}

#[test]
fn close_and_collection_release_sockets_and_registrations() {
    let runtime = both();
    runtime
        .exec(
            r"
            client, server, listener = pair()
            poller = tcp.poller()
            assert(poller:watch(server, 1, 'read'))
            assert(poller:watch(listener, 2, 'read'))
            server = nil
            ",
        )
        .unwrap();
    runtime.collect_garbage();
    runtime.collect_garbage();
    runtime
        .exec(
            r"
            -- The collected stream's socket is closed: the peer reads the end of stream, and
            -- its watch is gone.
            assert(readToEnd(client) == '')
            assert(poller.watching == 1, poller.watching)
            assert(listener.streams == 0)
            client:close() client:close()
            assert(client.closed and client.state == 'closed' and client.peerAddress ~= nil)
            for _, call in {
                function() client:readInto(buffer.create(1)) end,
                function() client:write('x') end,
                function() client:finishConnect() end,
                function() client:shutdown('both') end,
                function() listener:close() listener:accept() end,
                function() poller:close() poller:wait(0) end,
                function() poller:watch(listener, 3, 'read') end,
            } do
                local ok, err = pcall(call)
                assert(not ok and err:find('closed'), err)
            end
            listener:close() poller:close()
            assert(listener.closed and poller.closed)
            ",
        )
        .unwrap();
    // A collected poller leaves its handles free to be watched again.
    runtime
        .exec(
            r"
            listener = assert(tcp.listen('127.0.0.1:0'))
            local p = tcp.poller()
            assert(p:watch(listener, 1, 'read'))
            ",
        )
        .unwrap();
    runtime.collect_garbage();
    runtime.exec("local p = tcp.poller() assert(p:watch(listener, 1, 'read')) p:close() listener:close()").unwrap();

    // maxStreams: the limit refuses without accepting, and freeing a stream makes the waiting
    // connection reportable again.
    runtime
        .exec(
            r"
            local listener = assert(tcp.listen('127.0.0.1:0', { maxStreams = 1 }))
            local first = assert(tcp.connect(listener.localAddress))
            local second = assert(tcp.connect(listener.localAddress))
            local accepted = acceptOne(listener)
            assert(listener.streams == 1)
            local poller = tcp.poller()
            assert(poller:watch(listener, 1, 'read'))
            -- Registering reports the connection already waiting.
            assert(poller:wait(1000) == 1 and poller:next() == 1)
            local s, message, kind = listener:accept()
            assert(s == nil and kind == 'limitReached' and message:find('maxStreams'), message)
            assert(poller:wait(0) == 0, 'refusing at the limit clears readiness')
            accepted:close()
            assert(listener.streams == 0)
            assert(poller:wait(0) == 1 and poller:next() == 1)
            local later = assert(listener:accept())
            later:close() first:close() second:close() poller:close() listener:close()
            ",
        )
        .unwrap();
}

#[test]
fn bad_arguments_are_script_errors() {
    let runtime = both();
    for (source, expected) in [
        ("tcp.listen(5)", ""),
        ("tcp.listen('127.0.0.1:0', { bogus = 1 })", "unknown option 'bogus'"),
        ("tcp.listen('127.0.0.1:0', { backlog = 0 })", "backlog must be in [1, 65535]"),
        ("tcp.listen('127.0.0.1:0', { maxStreams = 0 })", "maxStreams must be in [1, 65536]"),
        ("tcp.listen('127.0.0.1:0', { reuseAddress = 'yes' })", "reuseAddress"),
        ("tcp.connect('127.0.0.1:1', { timeout = 5 })", "unknown option 'timeout'"),
        ("tcp.connect('127.0.0.1:1', 5)", ""),
        ("tcp.poller(5)", ""),
    ] {
        let error = error_of(&runtime, source);
        assert!(error.contains(expected), "{source}: {error}");
    }
}

/// The host side of a stream: a script can be handed one without the connect capability.
#[test]
fn a_host_stream_carries_bytes_both_ways() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"ping\0").unwrap();
        let mut reply = [0u8; 5];
        stream.read_exact(&mut reply).unwrap();
        reply
    });
    let stream = std::net::TcpStream::connect(address).unwrap();
    let runtime = runtime(&[]);
    {
        let stack = runtime.stack();
        stack
            .with_frame(|frame| {
                Stream::push(frame, Stream::from_std(stream).unwrap())?;
                frame.set_global("host")
            })
            .unwrap();
    }
    runtime
        .exec("assert(host.state == 'connected') assert(readExactly(host, 5) == 'ping\\0') writeAll(host, 'pong\\0') host:close()")
        .unwrap();
    assert_eq!(&peer.join().unwrap(), b"pong\0");
}
