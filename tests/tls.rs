//! `@dream/tls` over real loopback sockets with a test PKI made per run: verified TLS 1.3 and
//! 1.2 handshakes in both directions, ALPN and SNI, identity independent of the endpoint, every
//! way verification must fail closed, configuration errors, the TCP stream handed over whole,
//! clean close against truncation, buffered plaintext and backpressure under the poller,
//! handshake deadlines and broken peers, the platform store, and an independent rustls peer.

use std::io::{Read, Write};
use std::sync::{Arc, OnceLock};

use rustls::pki_types::pem::PemObject;

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::tcp::{CONNECT_CAPABILITY, LISTEN_CAPABILITY, TcpExtension};
use l3i::tls::TlsExtension;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    date_time_ymd,
};

/// One leaf: its certificate (or chain) and key, PEM.
#[derive(Clone)]
pub struct Leaf {
    pub chain: String,
    pub key: String,
}

pub struct Pki {
    pub ca: String,
    pub ca_der: Vec<u8>,
    /// test.local, localhost, 127.0.0.1 and ::1.
    pub leaf: Leaf,
    /// The same names, another key: a rotated certificate.
    pub rotated: Leaf,
    pub expired: Leaf,
    pub future: Leaf,
    /// Signed by a CA nobody trusts.
    pub stranger: Leaf,
    /// Signed by an intermediate: `chain` holds leaf and intermediate, `alone` only the leaf.
    pub chained: Leaf,
    pub alone: Leaf,
}

fn ca(name: &str) -> (CertificateParams, KeyPair, String, Vec<u8>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let cert = params.self_signed(&key).unwrap();
    (params, key, cert.pem(), cert.der().to_vec())
}

fn leaf(issuer: &Issuer<'_, KeyPair>, valid: Option<(i32, i32)>) -> Leaf {
    let key = KeyPair::generate().unwrap();
    let names = vec!["test.local".to_owned(), "localhost".to_owned(), "127.0.0.1".to_owned(), "::1".to_owned()];
    let mut params = CertificateParams::new(names).unwrap();
    params.distinguished_name.push(DnType::CommonName, "test.local");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    if let Some((from, to)) = valid {
        params.not_before = date_time_ymd(from, 1, 1);
        params.not_after = date_time_ymd(to, 1, 1);
    }
    Leaf { chain: params.signed_by(&key, issuer).unwrap().pem(), key: key.serialize_pem() }
}

pub fn pki() -> &'static Pki {
    static PKI: OnceLock<Pki> = OnceLock::new();
    PKI.get_or_init(|| {
        let (params, key, ca_pem, ca_der) = ca("l3i test CA");
        let issuer = Issuer::new(params, key);
        let (other_params, other_key, _, _) = ca("l3i stranger CA");
        let stranger_issuer = Issuer::new(other_params, other_key);
        let inter_key = KeyPair::generate().unwrap();
        let mut inter_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        inter_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        inter_params.distinguished_name.push(DnType::CommonName, "l3i test intermediate");
        inter_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        let inter_pem = inter_params.signed_by(&inter_key, &issuer).unwrap().pem();
        let inter_issuer = Issuer::new(inter_params, inter_key);
        let alone = leaf(&inter_issuer, None);
        Pki {
            ca: ca_pem,
            ca_der,
            leaf: leaf(&issuer, None),
            rotated: leaf(&issuer, None),
            expired: leaf(&issuer, Some((2000, 2001))),
            future: leaf(&issuer, Some((2100, 2101))),
            stranger: leaf(&stranger_issuer, None),
            chained: Leaf { chain: format!("{}{inter_pem}", alone.chain), key: alone.key.clone() },
            alone,
        }
    })
}

/// Script helpers: a connected pair, both handshakes driven through one poller, and a transfer.
pub const HELPERS: &str = r"
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

function connectedPair(options)
    local listener = assert(tcp.listen('127.0.0.1:0', options))
    local raw = assert(tcp.connect(listener.localAddress))
    local accepted = acceptOne(listener)
    assert(settle(raw) == true)
    return raw, accepted, listener
end

function serverConfig(leaf, extra)
    local options = { certChain = leaf.chain, privateKey = leaf.key }
    for k, v in extra or {} do options[k] = v end
    return assert(tls.serverConfig(options))
end

function trusting(extra)
    local options = { roots = { PKI.ca }, platform = false }
    for k, v in extra or {} do options[k] = v end
    return assert(tls.clientConfig(options))
end

-- Both handshakes through one poller: (clientDone, clientMessage, clientKind, serverDone, serverMessage, serverKind).
function handshakeBoth(client, server)
    local poller = tcp.poller()
    assert(poller:watch(client, 1, 'readwrite'))
    assert(poller:watch(server, 2, 'readwrite'))
    local result = { client = {}, server = {} }
    local function step(side, stream, token)
        local r = result[side]
        if r.done or r.kind then return end
        local ok, message, kind = stream:handshake()
        if ok then
            r.done = true
            -- An open session with room is always writable: stop asking.
            poller:modify(token, 'read')
        elseif ok == nil then
            r.message, r.kind = message, kind
        end
    end
    for _ = 1, 800 do
        step('client', client, 1)
        step('server', server, 2)
        local c, s = result.client, result.server
        if (c.done or c.kind) and (s.done or s.kind) then break end
        poller:wait(25)
        while poller:next() do end
    end
    poller:close()
    local c, s = result.client, result.server
    return c.done == true, c.message, c.kind, s.done == true, s.message, s.kind
end

function secured(leaf, clientOptions, serverExtra, clientConfigExtra)
    local raw, accepted, listener = connectedPair()
    local options = { serverName = 'test.local', config = trusting(clientConfigExtra) }
    for k, v in clientOptions or {} do options[k] = v end
    local client = assert(tls.client(raw, options))
    local server = assert(tls.server(accepted, serverConfig(leaf or PKI.leaf, serverExtra)))
    return client, server, listener, raw, accepted
end

-- Sends payload from one stream to another through a poller; returns what arrived.
function transfer(from, to, payload)
    local poller = tcp.poller()
    assert(poller:watch(from, 1, 'write'))
    assert(poller:watch(to, 2, 'read'))
    local sent, parts, got, flushed = 0, {}, 0, false
    local scratch = buffer.create(65536)
    for _ = 1, 100000 do
        if sent < #payload then
            local n, m, k = from:write(payload, sent)
            if n then sent += n else assert(k == 'wouldBlock', m) end
        elseif not flushed then
            local f, m = from:flush()
            assert(f ~= nil, m)
            if f then flushed = true poller:modify(1, 'read') end
        end
        while true do
            local n, m, k = to:readInto(scratch)
            if n == nil then assert(k == 'wouldBlock', m) break end
            assert(n > 0, 'end of stream mid-transfer')
            table.insert(parts, buffer.readstring(scratch, 0, n))
            got += n
        end
        if got == #payload and flushed then break end
        poller:wait(25)
        while poller:next() do end
    end
    poller:close()
    return table.concat(parts)
end

function readToEnd(stream)
    local poller = tcp.poller()
    assert(poller:watch(stream, 1, 'read'))
    local parts, scratch = {}, buffer.create(4096)
    for _ = 1, 400 do
        local n, m, k = stream:readInto(scratch)
        if n == 0 then poller:close() return table.concat(parts) end
        if n then table.insert(parts, buffer.readstring(scratch, 0, n))
        elseif k ~= 'wouldBlock' then poller:close() return nil, m, k
        else poller:wait(25) while poller:next() do end end
    end
    error('no end of stream')
end

function flushAll(stream)
    for _ = 1, 400 do
        local f, m, k = stream:flush()
        if f then return true end
        if f == nil then return nil, m, k end
        local poller = tcp.poller() poller:watch(stream, 1, 'write') poller:wait(25) poller:close()
    end
    error('never flushed')
end
";

pub fn runtime() -> Runtime {
    let policy = RuntimePolicy::new()
        .compat_global("@dream/tcp", "tcp")
        .compat_global("@dream/tls", "tls")
        .capability(LISTEN_CAPABILITY)
        .capability(CONNECT_CAPABILITY);
    let plan =
        RuntimePlan::builder().policy(policy).extension(TcpExtension).extension(TlsExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let pki = pki();
    let lua_leaf = |leaf: &Leaf| format!("{{ chain = [==[{}]==], key = [==[{}]==] }}", leaf.chain, leaf.key);
    runtime
        .exec(&format!(
            "PKI = {{ ca = [==[{}]==], leaf = {}, rotated = {}, expired = {}, future = {}, stranger = {}, chained = {}, alone = {} }}",
            pki.ca,
            lua_leaf(&pki.leaf),
            lua_leaf(&pki.rotated),
            lua_leaf(&pki.expired),
            lua_leaf(&pki.future),
            lua_leaf(&pki.stranger),
            lua_leaf(&pki.chained),
            lua_leaf(&pki.alone)
        ))
        .unwrap();
    runtime.exec(HELPERS).unwrap();
    runtime
}

#[test]
fn the_extension_requires_tcp_and_widens_its_watchable_handles() {
    let plan = RuntimePlan::builder().extension(TcpExtension).extension(TlsExtension).finalize().unwrap();
    let definitions = plan.type_definitions();
    assert!(definitions.contains("declare extern type dream_tls_Stream with"), "{definitions}");
    assert!(
        definitions
            .contains("export type dream_tcp_Watchable = dream_tcp_Listener | dream_tcp_Stream | dream_tls_Stream")
    );
    let error = RuntimePlan::builder().extension(TlsExtension).finalize().err().unwrap().to_string();
    assert!(error.contains("dream.tcp"), "{error}");
}

#[test]
fn a_verified_tls13_session_carries_bytes_both_ways_and_closes_cleanly() {
    runtime()
        .exec(
            r"
            local client, server, listener = secured(PKI.leaf, { alpn = { 'h2', 'http/1.1' } }, { alpn = { 'http/1.1' } })
            assert(client.state == 'handshaking' and client.handshaking)
            local cDone, cMessage, cKind, sDone, sMessage, sKind = handshakeBoth(client, server)
            assert(cDone and sDone, tostring(cMessage) .. ' / ' .. tostring(sMessage))
            assert(client.state == 'open' and server.state == 'open')
            assert(client.protocolVersion == '1.3' and server.protocolVersion == '1.3')
            assert(client.alpn == 'http/1.1' and server.alpn == 'http/1.1')
            assert(type(client.cipherSuite) == 'string' and client.cipherSuite:find('TLS13'), client.cipherSuite)
            -- The identity checked is the name given; the server sees it as SNI.
            assert(client.serverName == 'test.local' and server.serverName == 'test.local')
            assert(client.peerAddress == listener.localAddress)
            assert(client:handshake() == true)
            local payload = string.rep('\0binary\255payload', 70000)
            assert(transfer(client, server, payload) == payload, 'client to server')
            local reply = string.rep('reply\0', 1000)
            assert(transfer(server, client, reply) == reply, 'server to client')
            -- close_notify is a clean end of stream, and the other direction stays open.
            assert(client:shutdownWrite() ~= nil)
            assert(flushAll(client))
            assert(readToEnd(server) == '')
            assert(server:readInto(buffer.create(4)) == 0, 'end of stream stays 0')
            local late = 'after the half-close'
            assert(transfer(server, client, late) == late)
            local n, m, k = client:write('more')
            assert(n == nil and k == 'brokenPipe', m)
            assert(server:shutdownWrite() ~= nil and flushAll(server))
            assert(readToEnd(client) == '')
            client:close() client:close() server:close() listener:close()
            assert(client.closed and client.state == 'closed')
            local ok, err = pcall(client.readInto, client, buffer.create(1)) assert(not ok and err:find('closed'), err)
            ",
        )
        .unwrap();
}

#[test]
fn tls12_ip_identities_and_rotated_configs_verify() {
    runtime()
        .exec(
            r"
            -- TLS 1.2 when the server allows only it.
            local client, server = secured(PKI.leaf, nil, { versions = { '1.2' } })
            assert(handshakeBoth(client, server))
            assert(client.protocolVersion == '1.2' and server.protocolVersion == '1.2')
            assert(transfer(client, server, 'twelve') == 'twelve')
            client:close() server:close()
            -- An IP identity matches an IP SAN, whatever address the socket used.
            for _, identity in { '127.0.0.1', '::1', '[::1]' } do
                local c, s = secured(PKI.leaf, { serverName = identity })
                local done, message = handshakeBoth(c, s)
                assert(done, identity .. ': ' .. tostring(message))
                c:close() s:close()
            end
            -- New configs serve new sessions with the new certificate; both verify.
            local first = serverConfig(PKI.leaf)
            local second = serverConfig(PKI.rotated)
            for _, config in { first, second } do
                local raw, accepted = connectedPair()
                local c = assert(tls.client(raw, { serverName = 'localhost', config = trusting() }))
                local s = assert(tls.server(accepted, config))
                assert(handshakeBoth(c, s))
                c:close() s:close()
            end
            ",
        )
        .unwrap();
}

#[test]
fn verification_fails_closed_for_every_bad_certificate() {
    runtime()
        .exec(
            r"
            local cases = {
                { leaf = PKI.leaf, name = 'wrong.test', kind = 'certificateNameMismatch' },
                { leaf = PKI.leaf, name = '10.0.0.1', kind = 'certificateNameMismatch' },
                { leaf = PKI.expired, name = 'test.local', kind = 'certificateExpired' },
                { leaf = PKI.future, name = 'test.local', kind = 'certificateNotYetValid' },
                { leaf = PKI.stranger, name = 'test.local', kind = 'certificateUntrusted' },
                { leaf = PKI.alone, name = 'test.local', kind = 'certificateUntrusted' },
            }
            for _, case in cases do
                local client, server, listener, raw = secured(case.leaf, { serverName = case.name })
                local cDone, cMessage, cKind, sDone, sMessage, sKind = handshakeBoth(client, server)
                assert(not cDone and cKind == case.kind, `{case.name}: {tostring(cKind)} {tostring(cMessage)}`)
                -- The server hears the client's alert; nobody continues in plaintext.
                assert(not sDone and (sKind == 'tlsAlert' or sKind == 'connectionAborted' or sKind == 'connectionReset'), tostring(sKind) .. ' ' .. tostring(sMessage))
                assert(client.state == 'failed')
                local n, again, sameKind = client:readInto(buffer.create(8))
                assert(n == nil and sameKind == case.kind and again == cMessage, 'the failure persists')
                n, again, sameKind = client:write('x')
                assert(n == nil and sameKind == case.kind)
                local ok, err = pcall(function() tcp.poller():watch(client, 1, 'read') end)
                assert(not ok and err:find('closed or failed'), err)
                client:close() server:close() listener:close()
            end
            -- A chain with its intermediate verifies.
            local client, server = secured(PKI.chained)
            assert(handshakeBoth(client, server))
            client:close() server:close()
            -- ALPN with no protocol in common: the server refuses.
            local c, s = secured(PKI.leaf, { alpn = { 'http/1.1' } }, { alpn = { 'h2' } })
            local cDone, _, cKind, sDone, _, sKind = handshakeBoth(c, s)
            assert(not cDone and not sDone and sKind == 'noApplicationProtocol' and cKind == 'tlsAlert', tostring(sKind) .. ' ' .. tostring(cKind))
            ",
        )
        .unwrap();
}

#[test]
fn configurations_refuse_bad_input() {
    runtime()
        .exec(
            r"
            local config, message, kind = tls.serverConfig({ certChain = PKI.leaf.chain, privateKey = PKI.rotated.key })
            assert(config == nil and kind == 'keyMismatch', message)
            config, message, kind = tls.serverConfig({ certChain = '-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n', privateKey = PKI.leaf.key })
            assert(config == nil and kind == 'invalidCertificate', message)
            config, message, kind = tls.serverConfig({ certChain = PKI.leaf.chain, privateKey = 'not a key at all' })
            assert(config == nil and kind == 'invalidKey', message)
            assert(not message:find('not a key at all', 1, true), 'key bytes never appear in messages')
            config, message, kind = tls.serverConfig({ certChain = buffer.fromstring('garbage der'), privateKey = PKI.leaf.key })
            assert(config == nil, 'a DER certificate that does not parse is refused')
            config, message, kind = tls.clientConfig({ roots = { 'definitely not a certificate' }, platform = false })
            assert(config == nil and kind == 'invalidCertificate', message)
            for _, case in {
                { function() tls.serverConfig({ certChain = {}, privateKey = PKI.leaf.key }) end, 'certChain is empty' },
                { function() tls.serverConfig({ certChain = PKI.leaf.chain }) end, [[missing required option 'privateKey']] },
                { function() tls.serverConfig({ certChain = PKI.leaf.chain, privateKey = PKI.leaf.key, ocsp = 1 }) end, [[unknown option 'ocsp']] },
                { function() tls.serverConfig({ certChain = PKI.leaf.chain, privateKey = PKI.leaf.key, versions = { '1.1' } }) end, [[only '1.2' and '1.3']] },
                { function() tls.clientConfig({ platform = false }) end, 'needs roots' },
                { function() tls.clientConfig({ alpn = { '' } }) end, '1 to 255 bytes' },
                { function() tls.clientConfig({ insecure = true }) end, [[unknown option 'insecure']] },
            } do
                local ok, err = pcall(case[1])
                assert(not ok and string.find(err, case[2], 1, true), err)
            end
            ",
        )
        .unwrap();
}

#[test]
fn der_roots_and_bad_client_arguments() {
    let runtime = runtime();
    let der = pki().ca_der.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        let _ = write!(text, "\\x{byte:02x}");
        text
    });
    runtime
        .exec(&format!(
            r"
            local config = assert(tls.clientConfig({{ roots = {{ buffer.fromstring('{der}') }}, platform = false }}))
            local raw, accepted = connectedPair()
            local c = assert(tls.client(raw, {{ serverName = 'test.local', config = config }}))
            local s = assert(tls.server(accepted, serverConfig(PKI.leaf)))
            assert(handshakeBoth(c, s))
            c:close() s:close()
            local raw2, accepted2 = connectedPair()
            for _, case in {{
                {{ function() tls.client(raw2, {{}}) end, [[missing required option 'serverName']] }},
                {{ function() tls.client(raw2, {{ serverName = 'bad name!' }}) end, 'serverName' }},
                {{ function() tls.client(raw2, {{ serverName = 'test.local', bufferLimit = 10 }}) end, 'bufferLimit must be in' }},
                {{ function() tls.client(raw2, {{ serverName = 'test.local', handshakeTimeoutMs = 0 }}) end, 'handshakeTimeoutMs must be in' }},
                {{ function() tls.client(raw2, {{ serverName = 'test.local', skipVerify = true }}) end, [[unknown option 'skipVerify']] }},
                {{ function() tls.client({{}}, {{ serverName = 'test.local' }}) end, 'dream.tcp.Stream' }},
                {{ function() tls.server(accepted2, {{}}) end, 'dream.tls.ServerConfig' }},
            }} do
                local ok, err = pcall(case[1])
                assert(not ok and string.find(err, case[2], 1, true), err)
            end
            -- A refused call leaves the TCP stream usable.
            assert(raw2.state == 'connected')
            raw2:close() accepted2:close()
            "
        ))
        .unwrap();
}

#[test]
fn the_tcp_stream_is_handed_over_whole() {
    let runtime = runtime();
    runtime
        .exec(
            r"
            local raw, accepted, listener = connectedPair({ maxStreams = 4 })
            -- Not while watched, not while connecting.
            local poller = tcp.poller()
            assert(poller:watch(raw, 1, 'read'))
            local ok, err = pcall(tls.client, raw, { serverName = 'test.local', config = trusting() })
            assert(not ok and err:find('unwatch it'), err)
            poller:unwatch(1)
            local pending = assert(tcp.connect(listener.localAddress))
            ok, err = pcall(tls.client, pending, { serverName = 'test.local', config = trusting() })
            assert(not ok and err:find('finishConnect'), err)
            pending:close()
            assert(listener.streams == 1)
            client = assert(tls.client(raw, { serverName = 'test.local', config = trusting() }))
            server = assert(tls.server(accepted, serverConfig(PKI.leaf)))
            assert(raw.state == 'consumed' and accepted.state == 'consumed' and not raw.closed)
            for _, call in {
                function() raw:readInto(buffer.create(1)) end,
                function() raw:write('x') end,
                function() raw:shutdown('both') end,
                function() raw:finishConnect() end,
                function() poller:watch(raw, 2, 'read') end,
                function() tls.client(raw, { serverName = 'test.local', config = trusting() }) end,
                function() tls.server(accepted, serverConfig(PKI.leaf)) end,
            } do
                local ok2, err2 = pcall(call)
                assert(not ok2 and (err2:find('handed over') or err2:find('already handed over')), err2)
            end
            -- Closing the old handles does nothing to the session's socket.
            raw:close() accepted:close()
            assert(listener.streams == 1, 'the session carries the accepted stream in the count')
            poller:close()
            listenerHeld = listener
            raw, accepted = nil, nil
            ",
        )
        .unwrap();
    runtime.collect_garbage();
    runtime
        .exec(
            r"
            assert(handshakeBoth(client, server))
            assert(transfer(client, server, 'still connected') == 'still connected')
            server:close() server:close()
            assert(listenerHeld.streams == 0, 'released exactly once')
            client:close()
            ",
        )
        .unwrap();
}

#[test]
fn an_end_without_close_notify_is_truncation_not_end_of_stream() {
    runtime()
        .exec(
            r"
            local client, server = secured()
            assert(handshakeBoth(client, server))
            assert(transfer(server, client, 'partial') == 'partial')
            server:close() -- the socket closes with no close_notify
            local data, message, kind = readToEnd(client)
            assert(data == nil and kind == 'truncated', tostring(kind) .. ' ' .. tostring(message))
            assert(client.state == 'failed')
            local n, _, again = client:readInto(buffer.create(4))
            assert(n == nil and again == 'truncated')
            ",
        )
        .unwrap();
}

#[test]
fn buffered_plaintext_stays_readable_and_backpressure_is_bounded() {
    runtime()
        .exec(
            r"
            local client, server = secured(nil, { bufferLimit = 16384 })
            assert(handshakeBoth(client, server))
            -- Three records arrive; a tiny read leaves decrypted bytes inside the session.
            for _ = 1, 3 do assert(server:write(string.rep('r', 10000)) == 10000) end
            assert(flushAll(server))
            local poller = tcp.poller()
            assert(poller:watch(client, 1, 'read'))
            -- The records may wait out Nagle's algorithm on the server's side; poll until they come.
            local tiny = buffer.create(16)
            local n
            for _ = 1, 100 do
                n = client:readInto(tiny)
                if n then break end
                poller:wait(50)
            end
            assert(n == 16)
            -- The OS may have nothing new; the session's plaintext keeps the stream readable.
            assert(poller:wait(0) == 1 and poller:next() == 1)
            local total = 16
            while total < 30000 do
                local got, m, k = client:readInto(buffer.create(65536))
                if got then total += got else assert(k == 'wouldBlock', m) poller:wait(100) end
            end
            assert(total == 30000)
            local got, _, k = client:readInto(buffer.create(10))
            assert(got == nil and k == 'wouldBlock')
            assert(poller:wait(0) == 0, 'drained')
            -- Nothing is read on the other side: writes fill the socket, then the 16 KiB cap.
            local sent, chunk = 0, string.rep('w', 65536)
            for _ = 1, 10000 do
                local accepted, m, kind = client:write(chunk)
                if accepted == nil then assert(kind == 'wouldBlock' and m:find('send buffer is full'), m) break end
                sent += accepted
            end
            assert(client.wantsWrite and client:flush() == false)
            assert(sent < 64 * 1024 * 1024, sent)
            poller:modify(1, 'write')
            assert(poller:wait(0) == 0, 'not writable while the socket is full and the session buffer at its cap')
            -- Draining the peer lets everything through, exactly once.
            local received, scratch = 0, buffer.create(65536)
            local watcher = tcp.poller() assert(watcher:watch(server, 1, 'read'))
            while received < sent do
                local count, m, kind = server:readInto(scratch)
                if count then received += count else assert(kind == 'wouldBlock', m) client:flush() watcher:wait(25) end
            end
            assert(received == sent)
            assert(flushAll(client))
            watcher:close() poller:close() client:close() server:close()
            ",
        )
        .unwrap();
}

#[test]
fn broken_peers_and_deadlines_end_handshakes() {
    runtime()
        .exec(
            r"
            -- A peer that never answers: the deadline is reported by the poller and ends it.
            local raw, accepted = connectedPair()
            local client = assert(tls.client(raw, { serverName = 'test.local', config = trusting(), handshakeTimeoutMs = 120 }))
            assert(client:handshake() == false)
            local poller = tcp.poller()
            assert(poller:watch(client, 1, 'read'))
            local started = os.clock()
            assert(poller:wait(1000) == 1)
            local ok, message, kind = client:handshake()
            assert(ok == nil and kind == 'timedOut', message)
            accepted:close() poller:close()
            -- A peer that hangs up mid-handshake.
            raw, accepted = connectedPair()
            client = assert(tls.client(raw, { serverName = 'test.local', config = trusting() }))
            assert(client:handshake() == false)
            accepted:close()
            local done, why, what
            for _ = 1, 100 do
                done, why, what = client:handshake()
                if done ~= false then break end
                local p = tcp.poller() p:watch(client, 1, 'read') p:wait(25) p:close()
            end
            assert(done == nil and (what == 'connectionAborted' or what == 'connectionReset'), tostring(what) .. ' ' .. tostring(why))
            -- A peer that speaks something else.
            raw, accepted = connectedPair()
            client = assert(tls.client(raw, { serverName = 'test.local', config = trusting() }))
            assert(client:handshake() == false)
            assert(accepted:write('HTTP/1.1 400 Bad Request\r\n\r\n') > 0)
            for _ = 1, 100 do
                done, why, what = client:handshake()
                if done ~= false then break end
                local p = tcp.poller() p:watch(client, 1, 'read') p:wait(25) p:close()
            end
            assert(done == nil and what == 'tlsProtocol', tostring(what) .. ' ' .. tostring(why))
            accepted:close()
            ",
        )
        .unwrap();
}

#[test]
fn the_platform_store_is_used_by_default_and_fails_closed() {
    runtime()
        .exec(
            r"
            -- The test CA is in no platform store: untrusted, or the store itself is unusable.
            local raw, accepted = connectedPair()
            local client, message, kind = tls.client(raw, { serverName = 'test.local' })
            if client == nil then
                assert(kind == 'trustStoreUnavailable', message)
            else
                local server = assert(tls.server(accepted, serverConfig(PKI.leaf)))
                local done, why, what = handshakeBoth(client, server)
                assert(not done and what == 'certificateUntrusted', tostring(what) .. ' ' .. tostring(why))
                server:close()
            end
            -- The platform store with the test CA added.
            local config, cmessage, ckind = tls.clientConfig({ roots = PKI.ca })
            if config == nil then
                assert(ckind == 'trustStoreUnavailable', cmessage)
            else
                local raw2, accepted2 = connectedPair()
                local c = assert(tls.client(raw2, { serverName = 'test.local', config = config }))
                local s = assert(tls.server(accepted2, serverConfig(PKI.leaf)))
                local done, why = handshakeBoth(c, s)
                assert(done, why)
            end
            ",
        )
        .unwrap();
}

/// An independent peer: rustls's own blocking stream on a host thread serves the Luau client.
#[test]
fn a_plain_rustls_server_and_the_luau_client_agree() {
    let pki = pki();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let chain: Vec<_> =
        rustls::pki_types::CertificateDer::pem_slice_iter(pki.leaf.chain.as_bytes()).map(Result::unwrap).collect();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(pki.leaf.key.as_bytes()).unwrap();
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
    );
    let peer = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        let connection = rustls::ServerConnection::new(config).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut request = [0u8; 5];
        stream.read_exact(&mut request).unwrap();
        stream.write_all(b"pong:").unwrap();
        stream.write_all(&request).unwrap();
        stream.conn.send_close_notify();
        stream.flush().unwrap();
    });
    runtime()
        .exec(&format!(
            r"
            local raw = assert(tcp.connect('{address}'))
            assert(settle(raw) == true)
            local client = assert(tls.client(raw, {{ serverName = 'localhost', config = trusting() }}))
            local poller = tcp.poller()
            assert(poller:watch(client, 1, 'readwrite'))
            while true do
                local done, message = client:handshake()
                assert(done ~= nil, message)
                if done then break end
                poller:wait(100)
            end
            poller:modify(1, 'read')
            assert(client:write('ping!') == 5 and client:flush())
            poller:close()
            assert(readToEnd(client) == 'pong:ping!')
            client:close()
            "
        ))
        .unwrap();
    peer.join().unwrap();
}
