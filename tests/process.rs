//! `@dream/process` through Luau: a child's exit code, its captured output and input, its
//! working directory and environment, and the capability it needs.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use std::time::{Duration, Instant};

use l3i::process::{ENVIRONMENT_CAPABILITY, ProcessExtension, SPAWN_CAPABILITY, STDIO_CAPABILITY};
use l3i::runtime::Limits;

fn runtime_granting(capabilities: &[&str], limits: Limits) -> Runtime {
    let mut policy = RuntimePolicy::new().compat_global("@dream/process", "process").limits(limits);
    for capability in capabilities {
        policy = policy.capability(capability);
    }
    let plan = RuntimePlan::builder().policy(policy).extension(ProcessExtension).finalize().unwrap();
    Runtime::from_plan(&plan).unwrap()
}

fn runtime_with(granted: bool) -> Runtime {
    let capabilities: &[&str] =
        if granted { &[SPAWN_CAPABILITY, ENVIRONMENT_CAPABILITY, STDIO_CAPABILITY] } else { &[] };
    runtime_granting(capabilities, Limits::default())
}

#[cfg(unix)]
#[test]
fn a_child_runs_with_arguments_input_and_environment() {
    runtime_with(true)
        .exec(
            "local result = process.run('sh', { '-c', 'printf \"%s|%s\" \"$1\" \"$GREETING\"; exit 3', 'sh', 'a b' }, \
               { env = { GREETING = 'hi' }, stdout = 'capture' }) \
             assert(not result.success and result.code == 3 and result.signal == nil, tostring(result.code)) \
             assert(result.stdout == 'a b|hi', result.stdout) \
             local echoed = process.run('cat', nil, { stdin = buffer.fromstring('in\\0put'), stdout = 'capture', stderr = 'null' }) \
             assert(echoed.success and echoed.code == 0 and echoed.stdout == 'in\\0put' and echoed.stderr == nil) \
             local where = process.run('pwd', {}, { cwd = '/', stdout = 'capture' }) \
             assert(where.stdout == '/\\n', where.stdout) \
             local errors = process.run('sh', { '-c', 'echo oops >&2' }, { stderr = 'capture' }) \
             assert(errors.success and errors.stderr == 'oops\\n' and errors.stdout == nil) \
             local killed = process.run('sh', { '-c', 'kill -9 $$' }) \
             assert(not killed.success and killed.code == nil and killed.signal == 9) \
             local missing, message, kind = process.run('dream-process-no-such-program') \
             assert(missing == nil and kind == 'notFound' and message:find('dream.process.run: dream%-process%-no%-such%-program'), message) \
             local ok, err = pcall(process.run, 'sh', { 1 }) assert(not ok and err:find('args%[1%]'), err) \
             ok, err = pcall(process.run, 'sh', {}, { stdout = 'file' }) assert(not ok and err:find('stdout'), err)",
        )
        .unwrap();
}

/// A child that exits without reading its input closes the pipe under the thread feeding it;
/// that write fails with EPIPE instead of raising SIGPIPE, which would kill a host that has
/// not ignored it. The test harness ignores SIGPIPE, so a counting handler stands in for the
/// default action while the run feeds a child that reads nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_child_that_ignores_its_input_raises_no_sigpipe() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RAISED: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn count(_: libc::c_int) {
        RAISED.fetch_add(1, Ordering::SeqCst);
    }
    // SAFETY: the action is zeroed, then given a handler that only touches an atomic (which is
    // async-signal-safe); the previous action is restored below.
    let previous = unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = count as *const () as libc::sighandler_t;
        let mut previous = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        assert_eq!(libc::sigaction(libc::SIGPIPE, &raw const action, previous.as_mut_ptr()), 0);
        previous.assume_init()
    };
    let result = runtime_with(true).exec(
        "local result = process.run('sh', { '-c', 'exit 0' }, { stdin = string.rep('x', 4 * 1024 * 1024) }) \
         assert(result.success, tostring(result.code))",
    );
    // SAFETY: `previous` is the action sigaction reported above.
    unsafe { libc::sigaction(libc::SIGPIPE, &raw const previous, std::ptr::null_mut()) };
    result.unwrap();
    assert_eq!(RAISED.load(Ordering::SeqCst), 0, "feeding the child raised SIGPIPE");
}

#[test]
fn running_and_the_environment_need_their_capabilities() {
    runtime_with(false)
        .exec(
            "local ok, err = pcall(process.run, 'sh', { '-c', 'exit 0' }) \
             assert(not ok and err:find(\"needs the 'process.spawn' capability\"), err) \
             ok, err = pcall(process.env, 'PATH') \
             assert(not ok and err:find(\"needs the 'process.environment' capability\"), err) \
             ok, err = pcall(process.write, 'stdout', '') \
             assert(not ok and err:find(\"needs the 'process.stdio' capability\"), err) \
             assert(type(process.isTerminal('stdout')) == 'boolean')",
        )
        .unwrap();
}

#[test]
fn the_environment_reads_variables() {
    // SAFETY: the integration binary's tests touch no other variable of this name.
    unsafe { std::env::set_var("DREAM_PROCESS_TEST", "caf\u{e9}") };
    runtime_with(true)
        .exec("assert(process.env('DREAM_PROCESS_TEST') == 'caf\\u{e9}') assert(process.env('DREAM_PROCESS_UNSET_VARIABLE') == nil)")
        .unwrap();
}

#[test]
fn writing_to_the_standard_streams_needs_its_capability() {
    runtime_granting(&[STDIO_CAPABILITY], Limits::default())
        .exec(
            "assert(process.write('stdout', '') == true and process.write('stderr', buffer.create(0)) == true) \
             local ok, err = pcall(process.write, 'stdin', 'x') assert(not ok and err:find('stream must be'), err)",
        )
        .unwrap();
}

#[cfg(unix)]
#[test]
fn a_child_inherits_the_environment_only_under_its_capability() {
    // SAFETY: the integration binary's tests touch no other variable of this name.
    unsafe { std::env::set_var("DREAM_PROCESS_INHERITED", "leaked") };
    let script = "local result = process.run('/bin/sh', { '-c', 'printf \"%s|%s\" \"$DREAM_PROCESS_INHERITED\" \"$GIVEN\"' }, \
                    { env = { GIVEN = 'given' }, stdout = 'capture' }) \
                  assert(result.stdout == EXPECTED, result.stdout)";
    runtime_granting(&[SPAWN_CAPABILITY], Limits::default())
        .exec(&format!("local EXPECTED = '|given' {script}"))
        .unwrap();
    runtime_granting(&[SPAWN_CAPABILITY, ENVIRONMENT_CAPABILITY], Limits::default())
        .exec(&format!("local EXPECTED = 'leaked|given' {script}"))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn a_child_still_running_at_the_execution_time_limit_is_killed() {
    let limits = Limits { execution_time: Duration::from_millis(100), memory_bytes: 0 };
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], limits);
    let started = Instant::now();
    let error = runtime.exec("process.run('/bin/sleep', { '10' }) while true do end").unwrap_err();
    assert!(matches!(error, l3i::Error::ExecutionTimeLimit(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "the child outlived the limit: {:?}", started.elapsed());
    runtime.exec("assert(process.run('/bin/sh', { '-c', 'exit 0' }).success)").unwrap();
}

#[test]
fn sleep_blocks_for_its_seconds_and_never_past_the_execution_time_limit() {
    let runtime = runtime_granting(&[], Limits::default());
    let started = Instant::now();
    runtime.exec("process.sleep(0.05) process.sleep(0)").unwrap();
    assert!(started.elapsed() >= Duration::from_millis(50));
    for bad in ["-1", "0/0", "math.huge", "'soon'"] {
        assert!(runtime.exec(&format!("process.sleep({bad})")).is_err(), "{bad}");
    }

    let limits = Limits { execution_time: Duration::from_millis(100), memory_bytes: 0 };
    let runtime = runtime_granting(&[], limits);
    let started = Instant::now();
    let error = runtime.exec("pcall(process.sleep, 10) local after = true").unwrap_err();
    assert!(matches!(error, l3i::Error::ExecutionTimeLimit(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "slept past the limit: {:?}", started.elapsed());
}
