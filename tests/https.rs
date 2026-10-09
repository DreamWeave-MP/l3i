//! DNS → TCP → verified TLS → HTTP/1.1, all in Luau through L3i's own modules: the example
//! client (`examples/https_client.luau`) resolves a name through a deterministic fake resolver,
//! races an unreachable address, a refused IPv6 one and the real IPv4 server, verifies the
//! certificate for the name it asked for, and fetches; the example server
//! (`examples/https_server.luau`) answers it, and answers independent rustls clients over
//! bounded keep-alive.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::dns::{DnsExtension, Lookup, RESOLVE_CAPABILITY, Resolver, ResolverConfig};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::tcp::{CONNECT_CAPABILITY, LISTEN_CAPABILITY, TcpExtension};
use l3i::tls::TlsExtension;
use rustls::pki_types::pem::PemObject;

use crate::dns::Mock;
use crate::tls::{Leaf, pki};

const CLIENT: &str = include_str!("../examples/https_client.luau");
const SERVER: &str = include_str!("../examples/https_server.luau");

const ROUTES: &str = r"
function(method, path)
    if path == '/hello' then return 200, 'text/plain', 'hello from luau over tls\n' end
    if path == '/big' then return 200, 'application/octet-stream', string.rep('0123456789abcdef', 65536) end
    return 404, 'text/plain', 'not found\n'
end";

/// A Luau HTTPS server on its own thread and runtime, until `stop`.
fn serve(leaf: Leaf, stop: Arc<AtomicBool>) -> (SocketAddr, std::thread::JoinHandle<f64>) {
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = server_runtime();
        runtime.set_global("CHAIN", leaf.chain.as_str()).unwrap();
        runtime.set_global("KEY", leaf.key.as_str()).unwrap();
        runtime
            .exec(&format!(
                "config = assert(tls.serverConfig({{ certChain = CHAIN, privateKey = KEY, alpn = {{ 'http/1.1' }} }})) \
                 server = assert(HttpsServer.new('127.0.0.1:0', config, {ROUTES}, {{ maxRequestsPerConnection = 3 }}))"
            ))
            .unwrap();
        let address: String = runtime.eval("return server.address").unwrap();
        sender.send(address.parse::<SocketAddr>().unwrap()).unwrap();
        let step = runtime.load_function("return function() server:step(20) return server.served end").unwrap();
        let mut served = 0.0;
        while !stop.load(Ordering::SeqCst) {
            served = step.invoke::<f64, ()>(&runtime.stack(), ()).unwrap();
        }
        runtime.exec("server:close()").unwrap();
        served
    });
    (receiver.recv_timeout(Duration::from_secs(10)).unwrap(), thread)
}

fn server_runtime() -> Runtime {
    let policy = RuntimePolicy::new()
        .compat_global("@dream/tcp", "tcp")
        .compat_global("@dream/tls", "tls")
        .capability(LISTEN_CAPABILITY);
    let plan =
        RuntimePlan::builder().policy(policy).extension(TcpExtension).extension(TlsExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(&format!("HttpsServer = (function()\n{SERVER}\nend)()")).unwrap();
    runtime
}

/// A client runtime whose resolver maps test names to fixed addresses.
fn client_runtime(table: HashMap<&'static str, Vec<&str>>) -> Runtime {
    let table = table
        .into_iter()
        .map(|(name, ips)| (name, Ok(ips.iter().map(|ip| SocketAddr::new(ip.parse().unwrap(), 0)).collect())))
        .collect();
    let mock = Arc::new(Mock { table, ..Mock::default() });
    let resolver = Resolver::new(ResolverConfig { workers: 2, queue: 8 }, mock as Arc<dyn Lookup>);
    let policy = RuntimePolicy::new()
        .compat_global("@dream/tcp", "tcp")
        .compat_global("@dream/tls", "tls")
        .compat_global("@dream/dns", "dns")
        .capability(CONNECT_CAPABILITY)
        .capability(RESOLVE_CAPABILITY);
    let plan = RuntimePlan::builder()
        .policy(policy)
        .extension(TcpExtension)
        .extension(TlsExtension)
        .extension(DnsExtension::new(resolver))
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(&format!("HttpsClient = (function()\n{CLIENT}\nend)()")).unwrap();
    runtime.set_global("CA", pki().ca.as_str()).unwrap();
    runtime.exec("trust = assert(tls.clientConfig({ roots = { CA }, platform = false }))").unwrap();
    runtime
}

#[test]
fn a_luau_client_resolves_races_verifies_and_fetches_from_a_luau_server() {
    let stop = Arc::new(AtomicBool::new(false));
    let (address, server) = serve(pki().leaf.clone(), Arc::clone(&stop));
    // 192.0.2.1 never answers (or is unreachable at once), nothing listens on [::1] at this
    // port; only the IPv4 loopback is the server.
    let mut table = HashMap::new();
    table.insert("test.local", vec!["192.0.2.1", "::1", "127.0.0.1"]);
    table.insert("wrong.test", vec!["127.0.0.1"]);
    let client = client_runtime(table);
    client.set_global("PORT", &f64::from(address.port())).unwrap();
    let started = Instant::now();
    client
        .exec(
            r"
            local secure, endpoint = HttpsClient.connect('test.local', PORT, { tlsConfig = trust, timeoutMs = 10000, attemptDelayMs = 100 })
            assert(secure, endpoint)
            assert(endpoint == `127.0.0.1:{PORT}`, endpoint)
            assert(secure.serverName == 'test.local' and secure.alpn == 'http/1.1' and secure.protocolVersion == '1.3')
            local response = assert(HttpsClient.request(secure, 'GET', 'test.local', '/hello'))
            assert(response.status == 200 and response.body == 'hello from luau over tls\n', response.body)
            assert(response.headers['connection'] == 'close' and response.headers['content-type'] == 'text/plain')
            HttpsClient.close(secure)
            -- A large body crosses many TLS records and partial reads and writes.
            local again = assert(HttpsClient.connect('test.local', PORT, { tlsConfig = trust, attemptDelayMs = 100 }))
            local big = assert(HttpsClient.request(again, 'GET', 'test.local', '/big'))
            assert(big.status == 200 and #big.body == 1048576 and big.body == string.rep('0123456789abcdef', 65536))
            HttpsClient.close(again)
            local missing = assert(HttpsClient.connect('test.local', PORT, { tlsConfig = trust, attemptDelayMs = 100 }))
            assert(assert(HttpsClient.request(missing, 'GET', 'test.local', '/nope')).status == 404)
            HttpsClient.close(missing)
            -- The identity is the name asked for: the same server under another name is refused.
            local refused, message, kind = HttpsClient.connect('wrong.test', PORT, { tlsConfig = trust })
            assert(refused == nil and kind == 'certificateNameMismatch', message)
            -- A name the resolver does not know.
            local unknown, why, failure = HttpsClient.connect('nowhere.test', PORT, { tlsConfig = trust })
            assert(unknown == nil and failure == 'notFound', why)
            ",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(20));
    stop.store(true, Ordering::SeqCst);
    assert_eq!(server.join().unwrap(), 3.0);
}

/// One keep-alive connection from an independent rustls client: `count` requests in turn.
fn keep_alive_client(address: SocketAddr, count: usize) -> Vec<(String, Vec<u8>)> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from_pem_slice(pki().ca.as_bytes()).unwrap()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("test.local").unwrap();
    let connection = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let socket = std::net::TcpStream::connect(address).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut stream = rustls::StreamOwned::new(connection, socket);
    let mut responses = Vec::new();
    let mut pending = Vec::new();
    for _ in 0..count {
        stream.write_all(b"GET /hello HTTP/1.1\r\nHost: test.local\r\n\r\n").unwrap();
        stream.flush().unwrap();
        // Read one response: the head, then Content-Length bytes.
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                break end;
            }
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "the server closed early");
            pending.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8(pending[..head_end].to_vec()).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_owned))
            .unwrap()
            .parse()
            .unwrap();
        pending.drain(..head_end + 4);
        while pending.len() < length {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            pending.extend_from_slice(&chunk[..read]);
        }
        responses.push((head, pending.drain(..length).collect()));
    }
    // The server closes after its request limit with close_notify: a clean end of stream.
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert_eq!(rest.len(), 0, "nothing after the last response");
    responses
}

#[test]
fn a_luau_server_serves_concurrent_keep_alive_clients_within_its_limits() {
    let runtime = server_runtime();
    let leaf = &pki().leaf;
    runtime.set_global("CHAIN", leaf.chain.as_str()).unwrap();
    runtime.set_global("KEY", leaf.key.as_str()).unwrap();
    runtime
        .exec(&format!(
            "config = assert(tls.serverConfig({{ certChain = CHAIN, privateKey = KEY }})) \
             server = assert(HttpsServer.new('127.0.0.1:0', config, {ROUTES}, {{ maxRequestsPerConnection = 3, maxConnections = 8 }}))"
        ))
        .unwrap();
    let address: String = runtime.eval("return server.address").unwrap();
    let address: SocketAddr = address.parse().unwrap();
    let clients: Vec<_> = (0..4).map(|_| std::thread::spawn(move || keep_alive_client(address, 3))).collect();
    let step = runtime.load_function("return function() server:step(20) return server.served end").unwrap();
    let started = Instant::now();
    while !clients.iter().all(std::thread::JoinHandle::is_finished) {
        step.invoke::<f64, ()>(&runtime.stack(), ()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(30), "the server stopped answering");
    }
    for client in clients {
        let responses = client.join().unwrap();
        assert_eq!(responses.len(), 3);
        for (index, (head, body)) in responses.iter().enumerate() {
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
            assert_eq!(body, b"hello from luau over tls\n");
            let connection = if index < 2 { "keep-alive" } else { "close" };
            assert!(head.contains(&format!("Connection: {connection}")), "{head}");
        }
    }
    runtime.exec("assert(server.served == 12, server.served) server:close()").unwrap();
}
