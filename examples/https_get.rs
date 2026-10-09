//! Fetches a page over HTTPS with the Luau client in `https_client.luau`: the OS resolver, the
//! platform trust store, and nothing but L3i's modules underneath. A manual smoke test against
//! a real server, never part of the test suite:
//!
//! ```text
//! cargo run --example https_get --features dns,tls -- example.com /
//! ```

use l3i::Runtime;
use l3i::dns::{DnsExtension, RESOLVE_CAPABILITY};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::tcp::{CONNECT_CAPABILITY, TcpExtension};
use l3i::tls::TlsExtension;

const CLIENT: &str = include_str!("https_client.luau");

fn main() -> l3i::Result<()> {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "example.com".to_owned());
    let path = args.next().unwrap_or_else(|| "/".to_owned());
    let policy = RuntimePolicy::new().capability(CONNECT_CAPABILITY).capability(RESOLVE_CAPABILITY);
    let plan = RuntimePlan::builder()
        .policy(policy)
        .extension(TcpExtension)
        .extension(TlsExtension)
        .extension(DnsExtension::default())
        .finalize()?;
    let runtime = Runtime::from_plan(&plan)?;
    runtime.exec(&format!("HttpsClient = (function()\n{CLIENT}\nend)()"))?;
    runtime.set_global("HOST", host.as_str())?;
    runtime.set_global("PATH", path.as_str())?;
    runtime.exec(
        r"
        local started = os.clock()
        local secure, endpoint = HttpsClient.connect(HOST, 443, { timeoutMs = 15000 })
        if not secure then error(`connect: {endpoint}`) end
        local connected = os.clock()
        print(`connected to {endpoint} as {secure.serverName}: TLS {secure.protocolVersion}, {secure.cipherSuite}, ALPN {secure.alpn}`)
        local response, message = HttpsClient.request(secure, 'GET', HOST, PATH, { timeoutMs = 15000 })
        if not response then error(`request: {message}`) end
        print(`HTTP {response.status} {response.reason}: {#response.body} bytes, content-type {response.headers['content-type']}`)
        print(string.format('connect + handshake %.0f ms, request %.0f ms', (connected - started) * 1000, (os.clock() - connected) * 1000))
        HttpsClient.close(secure)
        ",
    )
}
