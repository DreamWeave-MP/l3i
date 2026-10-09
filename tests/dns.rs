//! `@dream/dns` through Luau: literals, the OS resolver on `localhost`, a deterministic mock
//! resolver for ordering, failures, the bounded queue, cancellation and timeouts racing a slow
//! lookup, one-shot results, the per-runtime request limit, collection, and the capability.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::dns::{DnsExtension, Lookup, LookupError, RESOLVE_CAPABILITY, Resolver, ResolverConfig};
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::runtime::{CallContext, CallKind, Limits, MemoryCategory};

/// Answers from a table; names starting with `slow` wait until the gate opens.
#[derive(Default)]
pub struct Mock {
    pub table: HashMap<&'static str, Result<Vec<SocketAddr>, LookupError>>,
    pub gate: Arc<(Mutex<bool>, Condvar)>,
    pub calls: AtomicUsize,
}

impl Mock {
    pub fn open(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

impl Lookup for Mock {
    fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, LookupError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if host.starts_with("slow") {
            let (open, changed) = &*self.gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
        }
        match self.table.get(host) {
            Some(Ok(addresses)) => Ok(addresses.iter().map(|a| SocketAddr::new(a.ip(), port)).collect()),
            Some(Err(error)) => Err(error.clone()),
            None => Err(LookupError::NotFound),
        }
    }
}

fn addresses(list: &[&str]) -> Vec<SocketAddr> {
    list.iter().map(|a| a.parse().unwrap()).collect()
}

pub fn mock() -> Arc<Mock> {
    let mut table = HashMap::new();
    table.insert("multi.test", Ok(addresses(&["[2001:db8::1]:0", "192.0.2.1:0", "[2001:db8::1]:0", "192.0.2.2:0"])));
    table.insert("slow.test", Ok(addresses(&["192.0.2.9:0"])));
    table.insert("empty.test", Ok(Vec::new()));
    table.insert("again.test", Err(LookupError::TemporaryFailure));
    table.insert("weird.test", Err(LookupError::Other("the resolver is confused".to_owned())));
    table.insert("xn--bcher-kva.test", Ok(addresses(&["192.0.2.3:0"])));
    Arc::new(Mock { table, ..Mock::default() })
}

fn runtime_over(resolver: Resolver, grants: &[&str], max_requests: u32, limits: Option<Limits>) -> Runtime {
    let mut policy = RuntimePolicy::new().compat_global("@dream/dns", "dns");
    for grant in grants {
        policy = policy.capability(grant);
    }
    if let Some(limits) = limits {
        policy = policy.limits(limits);
    }
    let extension = DnsExtension::new(resolver).max_requests(max_requests);
    let plan = RuntimePlan::builder().policy(policy).extension(extension).finalize().unwrap();
    Runtime::from_plan(&plan).unwrap()
}

fn mocked(workers: usize, queue: usize) -> (Runtime, Arc<Mock>, Resolver) {
    let mock = mock();
    let resolver = Resolver::new(ResolverConfig { workers, queue }, Arc::clone(&mock) as Arc<dyn Lookup>);
    (runtime_over(resolver.clone(), &[RESOLVE_CAPABILITY], 64, None), mock, resolver)
}

fn error_of(runtime: &Runtime, source: &str) -> String {
    runtime.exec(source).unwrap_err().to_string()
}

#[test]
fn the_module_is_typed_and_resolving_needs_its_own_capability() {
    let plan = RuntimePlan::builder().extension(DnsExtension::default()).finalize().unwrap();
    let definitions = plan.type_definitions();
    assert!(definitions.contains("declare extern type dream_dns_Request with"), "{definitions}");
    assert!(definitions.contains("export type dream_dns_ErrorKind ="), "{definitions}");
    let (_, mock, resolver) = mocked(1, 4);
    for grants in [&[][..], &["network.tcp.connect", "network.tcp.listen", "network.transport"][..]] {
        let runtime = runtime_over(resolver.clone(), grants, 64, None);
        let error = error_of(&runtime, "dns.resolve('multi.test', 80)");
        assert!(error.contains("needs the 'network.dns.resolve' capability"), "{error}");
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn literals_complete_at_once_and_arguments_are_checked() {
    let (runtime, mock, _) = mocked(1, 4);
    runtime
        .exec(
            r"
            local r = assert(dns.resolve('127.0.0.1', 80))
            assert(r.status == 'ready')
            local list = assert(r:take())
            assert(#list == 1 and list[1] == '127.0.0.1:80')
            assert(r.status == 'consumed')
            local ok, err = pcall(r.take, r) assert(not ok and err:find('already taken'), err)
            assert(assert(dns.resolve('::1', 443)):take()[1] == '[::1]:443')
            assert(assert(dns.resolve('[::1]', 8443)):take()[1] == '[::1]:8443')
            for _, case in {
                { function() dns.resolve('example.test', 0) end, 'port must be in [1, 65535]' },
                { function() dns.resolve('example.test', 65536) end, 'port must be in [1, 65535]' },
                { function() dns.resolve('example.test', 1.5) end, '' },
                { function() dns.resolve('exa\0mple.test', 80) end, 'NUL' },
                { function() dns.resolve(string.rep('a', 64) .. '.test', 80) end, '1 to 63' },
                { function() dns.resolve('example.test:80', 80) end, 'IDNA' },
                { function() dns.resolve('example.test', 80, { timeoutMs = 0 }) end, 'timeoutMs must be in [1, 60000]' },
                { function() dns.resolve('example.test', 80, { maxAddresses = 65 }) end, 'maxAddresses must be in [1, 64]' },
                { function() dns.resolve('example.test', 80, { ttl = 5 }) end, [[unknown option 'ttl']] },
            } do
                local ok, err = pcall(case[1])
                assert(not ok and string.find(err, case[2], 1, true), err)
            end
            ",
        )
        .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0, "literals and refused arguments never reach the resolver");
}

#[test]
fn the_system_resolver_answers_localhost() {
    let runtime = runtime_over(Resolver::system(), &[RESOLVE_CAPABILITY], 64, None);
    runtime
        .exec(
            r"
            local r = assert(dns.resolve('localhost', 8080, { timeoutMs = 10000 }))
            assert(r:wait(10000), r.status)
            local list, message, kind = r:take()
            assert(list, message)
            local loopback = false
            for _, address in list do
                assert(address:match(':8080$'), address)
                if address == '127.0.0.1:8080' or address == '[::1]:8080' then loopback = true end
            end
            assert(loopback, table.concat(list, ' '))
            local missing = assert(dns.resolve('no-such-host.invalid', 80, { timeoutMs = 10000 }))
            missing:wait(10000)
            local none, why, failure = missing:take()
            assert(none == nil and (failure == 'notFound' or failure == 'temporaryFailure' or failure == 'timedOut'), tostring(failure) .. ' ' .. tostring(why))
            ",
        )
        .unwrap();
}

#[test]
fn results_keep_the_resolvers_order_both_families_and_no_duplicates() {
    let (runtime, _, _) = mocked(2, 8);
    runtime
        .exec(
            r"
            local r = assert(dns.resolve('Multi.Test', 443))
            assert(r.host == 'Multi.Test' and r.asciiHost == 'multi.test' and r.port == 443)
            assert(r:wait(5000))
            local list = assert(r:take())
            assert(#list == 3 and list[1] == '[2001:db8::1]:443' and list[2] == '192.0.2.1:443' and list[3] == '192.0.2.2:443', table.concat(list, ' '))
            local cut = assert(dns.resolve('multi.test', 1, { maxAddresses = 1 }))
            cut:wait(5000)
            assert(#assert(cut:take()) == 1)
            local idn = assert(dns.resolve('Bücher.test', 80))
            assert(idn.asciiHost == 'xn--bcher-kva.test' and idn.host == 'Bücher.test')
            idn:wait(5000)
            assert(idn:take()[1] == '192.0.2.3:80')
            for name, expected in { ['empty.test'] = 'notFound', ['nowhere.test'] = 'notFound', ['again.test'] = 'temporaryFailure', ['weird.test'] = 'other' } do
                local failing = assert(dns.resolve(name, 80))
                assert(failing:wait(5000))
                assert(failing.status == 'failed')
                local none, message, kind = failing:take()
                assert(none == nil and kind == expected and message:find(name, 1, true), `{name}: {kind} {message}`)
                local ok, err = pcall(failing.take, failing) assert(not ok and err:find('already taken'))
            end
            ",
        )
        .unwrap();
}

#[test]
fn the_queue_is_bounded_and_cancellation_and_timeouts_race_a_slow_lookup_cleanly() {
    let (runtime, mock, resolver) = mocked(1, 1);
    runtime
        .exec(
            r"
            running = assert(dns.resolve('slow.test', 80, { timeoutMs = 60000 }))
            ",
        )
        .unwrap();
    // Wait until the worker holds the slow lookup, so the next one queues behind it.
    let started = Instant::now();
    while mock.calls.load(Ordering::SeqCst) == 0 {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1));
    }
    runtime
        .exec(
            r"
            queued = assert(dns.resolve('multi.test', 80))
            local refused, message, kind = dns.resolve('multi.test', 81)
            assert(refused == nil and kind == 'limitReached' and message:find('queued lookups'), message)
            local n, why, k = queued:take()
            assert(n == nil and k == 'wouldBlock', why)
            -- Cancelled before it starts: withdrawn from the queue.
            queued:cancel()
            assert(queued.status == 'cancelled')
            local none, cancelledMessage, cancelled = queued:take()
            assert(none == nil and cancelled == 'cancelled', cancelledMessage)
            -- A deadline passes while the request still waits for a worker.
            timed = assert(dns.resolve('slow.test', 80, { timeoutMs = 60 }))
            ",
        )
        .unwrap();
    assert_eq!(resolver.load().0, 1, "only the timed request waits; the cancelled one left the queue");
    let started = Instant::now();
    runtime.exec("assert(timed:wait(5000) == true) assert(timed.status == 'timedOut')").unwrap();
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(40) && waited < Duration::from_secs(2), "{waited:?}");
    runtime
        .exec(
            r"
            local none, message, kind = timed:take()
            assert(none == nil and kind == 'timedOut', message)
            assert(message:find('timed out'), message)
            -- Cancelled while its OS call runs: the late result is thrown away.
            running:cancel()
            assert(running.status == 'cancelled')
            ",
        )
        .unwrap();
    mock.open();
    std::thread::sleep(Duration::from_millis(50));
    runtime
        .exec(
            r"
            assert(running.status == 'cancelled', running.status)
            local none, _, kind = running:take()
            assert(none == nil and kind == 'cancelled')
            ",
        )
        .unwrap();
    assert_eq!(resolver.load(), (0, 1), "one worker started, nothing left queued");
    // The timed-out request left the queue before any worker ran it.
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);

    // A deadline passes while the OS call runs: the request times out on time, and the result
    // that arrives later is thrown away.
    let (runtime, mock, _) = mocked(1, 1);
    let started = Instant::now();
    runtime
        .exec("slow = assert(dns.resolve('slow.test', 80, { timeoutMs = 60 })) assert(slow:wait(5000)) assert(slow.status == 'timedOut')")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1, "the lookup was running");
    mock.open();
    std::thread::sleep(Duration::from_millis(50));
    runtime
        .exec("assert(slow.status == 'timedOut') local n, _, k = slow:take() assert(n == nil and k == 'timedOut')")
        .unwrap();
}

#[test]
fn requests_are_limited_per_runtime_and_released_by_take_close_and_collection() {
    let mock = mock();
    mock.open();
    let resolver = Resolver::new(ResolverConfig { workers: 2, queue: 8 }, Arc::clone(&mock) as Arc<dyn Lookup>);
    let runtime = runtime_over(resolver.clone(), &[RESOLVE_CAPABILITY], 2, None);
    runtime
        .exec(
            r"
            a = assert(dns.resolve('multi.test', 80))
            b = assert(dns.resolve('127.0.0.1', 80))
            local refused, message, kind = dns.resolve('multi.test', 82)
            assert(refused == nil and kind == 'limitReached' and message:find('2 requests are open'), message)
            assert(b:take())
            c = assert(dns.resolve('multi.test', 83))
            a:close() a:close()
            assert(a.closed and a.status == 'closed')
            local ok, err = pcall(a.take, a) assert(not ok and err:find('closed'), err)
            d = assert(dns.resolve('multi.test', 84))
            c, d = nil, nil
            ",
        )
        .unwrap();
    runtime.collect_garbage();
    runtime.exec("e = assert(dns.resolve('multi.test', 85)) f = assert(dns.resolve('multi.test', 86))").unwrap();
    // A request collected while queued leaves the queue.
    let (gated, gated_mock, gated_resolver) = mocked(1, 4);
    gated.exec("hold = assert(dns.resolve('slow.test', 1))").unwrap();
    let started = Instant::now();
    while gated_mock.calls.load(Ordering::SeqCst) == 0 {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1));
    }
    gated.exec("local lost = assert(dns.resolve('multi.test', 2))").unwrap();
    assert_eq!(gated_resolver.load().0, 1);
    gated.collect_garbage();
    assert_eq!(gated_resolver.load().0, 0, "the collected request was withdrawn");
    gated_mock.open();
}

#[test]
fn a_wait_never_outlasts_the_execution_time_limit() {
    let (_, mock, resolver) = mocked(1, 4);
    let limits = Limits { execution_time: Duration::from_millis(60), memory_bytes: 0 };
    let runtime = runtime_over(resolver, &[RESOLVE_CAPABILITY], 64, Some(limits));
    let waiting = runtime
        .load_function(
            "return function() local r = assert(dns.resolve('slow.test', 1, { timeoutMs = 60000 })) while true do r:wait(1000) end end",
        )
        .unwrap();
    let started = Instant::now();
    let error = {
        let _scope = runtime.call_scope(CallContext { id: 1, category: MemoryCategory(0) }, CallKind::ScriptCall);
        waiting.invoke::<(), _>(&runtime.stack(), ()).unwrap_err().to_string()
    };
    assert!(error.contains("execution time limit exceeded"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(900), "{:?}", started.elapsed());
    mock.open();
}
