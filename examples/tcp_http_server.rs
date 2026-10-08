//! An HTTP/1.1 server written in Luau over `@dream/tcp` (`tcp_http_server.luau`), run by a
//! host that grants only `network.tcp.listen`:
//!
//! ```text
//! cargo run --example tcp_http_server --features tcp -- [address] [responses]
//! curl -i http://127.0.0.1:8080/
//! ```
//!
//! It serves until it has sent `responses` responses, or forever without one. A
//! demonstration of the transport, not a production server.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::tcp::{LISTEN_CAPABILITY, TcpExtension};

const SERVER: &str = include_str!("tcp_http_server.luau");

fn main() -> l3i::Result<()> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:8080".to_owned());
    let responses: f64 = args.next().map_or(0.0, |count| count.parse().expect("responses is a number"));
    let policy = RuntimePolicy::new().capability(LISTEN_CAPABILITY);
    let plan = RuntimePlan::builder().policy(policy).extension(TcpExtension).finalize()?;
    let runtime = Runtime::from_plan(&plan)?;
    runtime.exec(&format!("HttpServer = (function()\n{SERVER}\nend)()"))?;
    runtime.set_global("address", address.as_str())?;
    runtime.exec(
        r"
        server = assert(HttpServer.new(address, function(method, path)
            if method ~= 'GET' then return 405, 'text/plain', 'GET only\n' end
            if path == '/' then
                return 200, 'text/html; charset=utf-8', '<!doctype html><title>l3i</title><p>Served by Luau over @dream/tcp.</p>\n'
            end
            return 404, 'text/plain', 'not found\n'
        end))
        print('listening on http://' .. server.address)
        ",
    )?;
    let step = runtime.load_function("return function() server:step(250) return server.served end")?;
    loop {
        let served: f64 = step.invoke(&runtime.stack(), ())?;
        if responses > 0.0 && served >= responses {
            break;
        }
    }
    runtime.exec("server:close()")
}
