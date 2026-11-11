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
    let fed = RAISED.load(Ordering::SeqCst);
    // The same for a started child: the handle's writer thread meets a pipe nobody reads.
    let started = runtime_with(true).exec(
        "local child = assert(process.start('sh', { '-c', 'exit 0' }, { stdin = 'pipe' })) \
         assert(assert(child:wait(5000)).success) \
         assert(child:write(string.rep('x', 200000))) \
         local polls = 0 \
         repeat \
             process.sleep(0.01) \
             polls = polls + 1 \
             local written, _, kind = child:write('x') \
         until (not written and kind ~= 'wouldBlock') or polls > 300 \
         assert(polls <= 300, 'the writer never met the closed pipe') \
         child:close()",
    );
    // SAFETY: `previous` is the action sigaction reported above.
    unsafe { libc::sigaction(libc::SIGPIPE, &raw const previous, std::ptr::null_mut()) };
    result.unwrap();
    started.unwrap();
    assert_eq!(fed, 0, "feeding the child raised SIGPIPE");
    assert_eq!(RAISED.load(Ordering::SeqCst), 0, "writing to a started child that had exited raised SIGPIPE");
}

#[test]
fn running_and_the_environment_need_their_capabilities() {
    runtime_with(false)
        .exec(
            "local ok, err = pcall(process.run, 'sh', { '-c', 'exit 0' }) \
             assert(not ok and err:find(\"needs the 'process.spawn' capability\"), err) \
             ok, err = pcall(process.env, 'PATH') \
             assert(not ok and err:find(\"needs the 'process.environment' capability\"), err) \
             ok, err = pcall(process.knownFolder, 'home') \
             assert(not ok and err:find(\"needs the 'process.environment' capability\"), err) \
             ok, err = pcall(process.write, 'stdout', '') \
             assert(not ok and err:find(\"needs the 'process.stdio' capability\"), err) \
             assert(type(process.isTerminal('stdout')) == 'boolean') \
             assert(type(process.platform()) == 'string')",
        )
        .unwrap();
}

/// The platform is Rust's name for it, and the home folder the one the OS's own variables name:
/// `HOME` on Unix, `USERPROFILE` on Windows (which every Windows session sets). Only Windows
/// names a Documents folder.
#[test]
fn the_platform_and_its_known_folders() {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
    let documents =
        if cfg!(windows) { "assert(documents and #documents > 0, 'no documents')" } else { "assert(documents == nil)" };
    runtime_with(true)
        .exec(&format!(
            "assert(process.platform() == {platform:?}, process.platform()) \
             assert(process.knownFolder('home') == {home:?}, process.knownFolder('home')) \
             local documents = process.knownFolder('documents') {documents} \
             local ok, err = pcall(process.knownFolder, 'desktop') assert(not ok and err:find(\"name must be 'home' or 'documents'\"), err)",
            platform = std::env::consts::OS,
        ))
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

/// `PATH` is the one variable a child keeps without the environment capability, so a program is
/// found where the host would find it, by its name alone; `clearEnv` takes it away too.
#[cfg(unix)]
#[test]
fn a_child_keeps_the_host_path_unless_its_environment_is_cleared() {
    let expected = format!("PATH={}\n", std::env::var("PATH").unwrap());
    runtime_granting(&[SPAWN_CAPABILITY], Limits::default())
        .exec(&format!(
            "local kept = assert(process.run('env', nil, {{ stdout = 'capture' }})) \
             assert(kept.stdout == {expected:?}, kept.stdout) \
             local cleared = assert(process.run('/usr/bin/env', nil, {{ clearEnv = true, stdout = 'capture' }})) \
             assert(cleared.stdout == '', cleared.stdout)"
        ))
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

/// A wait the deadline cuts short is the time limit, even when the script does nothing after it:
/// Luau checks for the watchdog at its next instruction, and a script that returns has none.
#[test]
fn a_wait_the_deadline_cuts_short_fails_the_call_it_ends() {
    let limits = Limits { execution_time: Duration::from_millis(100), memory_bytes: 0 };
    let runtime = runtime_granting(&[], limits);
    let started = Instant::now();
    let error = runtime.exec("process.sleep(10)").unwrap_err();
    assert!(matches!(error, l3i::Error::ExecutionTimeLimit(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "slept past the limit: {:?}", started.elapsed());
    runtime.exec("local after = true").unwrap();
}

/// A process the child started can hold a captured pipe open after the child is gone; the run
/// still ends at the time limit, without waiting for it.
#[cfg(unix)]
#[test]
fn a_capture_a_grandchild_holds_open_ends_at_the_time_limit() {
    let limits = Limits { execution_time: Duration::from_millis(100), memory_bytes: 0 };
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], limits);
    for script in ["sleep 10; echo late", "sleep 10 & echo early"] {
        let started = Instant::now();
        let error = runtime
            .exec(&format!("process.run('/bin/sh', {{ '-c', '{script}' }}, {{ stdout = 'capture' }})"))
            .unwrap_err();
        assert!(matches!(error, l3i::Error::ExecutionTimeLimit(_)), "{script}: {error:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{script}: waited {:?}", started.elapsed());
    }
}

#[test]
fn stdin_child() {
    let Ok(mode) = std::env::var("L3I_STDIN_TEST_MODE") else { return };
    let runtime = runtime_granting(&[STDIO_CAPABILITY], Limits::default());
    if mode == "blocked" {
        runtime.exec("local input = assert(process.openStdin()) local v, m, k = input:readLine() assert(v == nil and k == 'wouldBlock', m)").unwrap();
        drop(runtime);
        return;
    }
    if mode == "shutdown" {
        // The script keeps its handle; only the shutdown can free the claim for the next runtime.
        runtime.exec("held = assert(process.openStdin())").unwrap();
        runtime.shutdown();
        let next = runtime_granting(&[STDIO_CAPABILITY], Limits::default());
        next.exec("assert(process.openStdin()):close()").unwrap();
        runtime
            .exec(
                r"
            local value, message, kind = held:read(1)
            assert(value == nil and message:find('closed', 1, true), message)
            ",
            )
            .unwrap();
        return;
    }
    if mode == "oversized" {
        runtime
            .exec(
                r"
            local input = assert(process.openStdin())
            local function line()
                while true do
                    local value, message, kind = input:readLine(4)
                    if kind == 'wouldBlock' then input:wait(1000)
                    else return value, kind end
                end
            end
            -- Ten bytes: two refusals of five each, then the empty tail ends the skip.
            local value, kind = line()
            assert(value == nil and kind == 'invalidInput', kind)
            value, kind = line()
            assert(value == nil and kind == 'invalidInput', kind)
            assert(line() == '')
            assert(line() == 'next')
            ",
            )
            .unwrap();
        return;
    }
    if mode == "deadline" {
        drop(runtime);
        let runtime = runtime_granting(
            &[STDIO_CAPABILITY],
            Limits { execution_time: Duration::from_millis(50), memory_bytes: 0 },
        );
        let error =
            runtime.exec("local input = assert(process.openStdin()) input:read(1) input:wait(60000)").unwrap_err();
        assert!(matches!(error, l3i::Error::ExecutionTimeLimit(_)), "{error:?}");
        return;
    }
    runtime
        .exec(
            r"
        local input = assert(process.openStdin())
        local other, message, kind = process.openStdin()
        assert(other == nil and kind == 'wouldBlock', message)
        local function line()
            while true do
                local value, message, kind = input:readLine(64)
                if kind == 'wouldBlock' then input:wait(1000)
                else assert(message == nil, message) return value end
            end
        end
        assert(line() == 'hello')
        assert(line() == '')
        assert(line() == string.char(255, 0))
        assert(line() == 'last')
        assert(line() == nil)
        input:close()
        input:close()
        local next = assert(process.openStdin())
        assert(next:read(1) == nil)
        next:close()
    ",
        )
        .unwrap();
}

/// The child alone reads controlled stdin; a deadline kills any hung child. In blocked cases
/// the parent deliberately holds the pipe open until the child has destroyed its runtime.
#[test]
fn stdin_through_luau_handles_pipes_eof_and_blocked_teardown() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for mode in ["lines", "blocked", "deadline", "shutdown", "oversized"] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process::stdin_child", "--nocapture"])
            .env("L3I_STDIN_TEST_MODE", mode)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take();
        if mode == "lines" {
            pipe.as_mut().unwrap().write_all(b"hello\r\n\n\xff\0\nlast").unwrap();
            drop(pipe.take());
        }
        if mode == "oversized" {
            pipe.as_mut().unwrap().write_all(b"aaaaaaaaaa\nnext\n").unwrap();
            drop(pipe.take());
        }
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "stdin child {mode}: {status}");
                break;
            }
            if Instant::now() >= until {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("stdin child {mode} hung");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn stdin_claim_requires_stdio_before_touching_the_stream() {
    runtime_with(false)
        .exec(
            r"
        local ok, message = pcall(process.openStdin)
        assert(not ok and message:find('process.stdio', 1, true), message)
    ",
        )
        .unwrap();
}

/// Refuses waits, the way a frame-driven host refuses what would stall it, and
/// counts what it was asked: the first operation must reach it before the stream is touched.
struct Refusing(std::rc::Rc<std::cell::RefCell<Vec<l3i::process::input::Operation>>>);

impl l3i::process::input::Policy for Refusing {
    fn check(&self, operation: l3i::process::input::Operation) -> l3i::Result<()> {
        use l3i::process::input::Operation;
        self.0.borrow_mut().push(operation);
        match operation {
            Operation::Open => Ok(()),
            Operation::Wait => Err(l3i::Error::permission("this host does not wait")),
        }
    }
}

fn runtime_with_policy(granted: bool, policy: std::rc::Rc<dyn l3i::process::input::Policy>) -> Runtime {
    let mut runtime_policy = RuntimePolicy::new().compat_global("@dream/process", "process");
    if granted {
        runtime_policy = runtime_policy.capability(STDIO_CAPABILITY);
    }
    let plan = RuntimePlan::builder()
        .policy(runtime_policy)
        .extension(ProcessExtension.with_input_policy(policy))
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn stdin_policy_child() {
    use l3i::process::input::Operation;
    if std::env::var_os("L3I_STDIN_POLICY_TEST").is_none() {
        return;
    }
    let seen = std::rc::Rc::default();
    let runtime = runtime_with_policy(true, std::rc::Rc::new(Refusing(std::rc::Rc::clone(&seen))));
    runtime
        .exec(
            r"
        local input = assert(process.openStdin())
        local ok, message = pcall(input.wait, input, 0)
        assert(not ok and message:find('this host does not wait', 1, true), message)
        local line, problem, kind = input:readLine()
        assert(line == nil and kind == 'wouldBlock', problem)
        input:close()
        input:close()
        ",
        )
        .unwrap();
    assert_eq!(*seen.borrow(), [Operation::Open, Operation::Wait]);
}

#[test]
fn stdin_policy_judges_every_operation_and_never_blocks_close() {
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process::stdin_policy_child", "--nocapture"])
        .env("L3I_STDIN_POLICY_TEST", "1")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let pipe = child.stdin.take();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "stdin policy child: {status}");
            break;
        }
        if Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("stdin policy child hung");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(pipe);
}

#[test]
fn stdin_policy_cannot_grant_what_the_runtime_withheld() {
    let seen = std::rc::Rc::default();
    runtime_with_policy(false, std::rc::Rc::new(Refusing(std::rc::Rc::clone(&seen))))
        .exec(
            r"
        local ok, message = pcall(process.openStdin)
        assert(not ok and message:find('process.stdio', 1, true), message)
        ",
        )
        .unwrap();
    assert!(seen.borrow().is_empty());
}

/// `timeoutMs` ends a child that would run longer, reports `timeout`, and leaves nothing running:
/// the child writes its own pid first, then becomes the sleep.
#[cfg(unix)]
#[test]
fn a_timeout_kills_and_reaps_the_child() {
    let pid_file = std::env::temp_dir().join(format!("l3i-process-timeout-{}", std::process::id()));
    let pid_file_text = pid_file.to_string_lossy().into_owned();
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(&format!(
            r#"local result, message, kind = process.run('sh', {{ '-c', 'echo $$ > "$1"; exec sleep 5', 'sh', '{pid_file_text}' }}, {{ timeoutMs = 200 }})
               assert(result == nil and kind == 'timeout', tostring(kind))
               assert(message:find('timeoutMs', 1, true), message)"#
        ))
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    std::fs::remove_file(&pid_file).unwrap();
    // SAFETY: signal 0 only checks that the pid names a process this test may signal.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "the timed out child {pid} is still running");
}

/// A captured stream past its ceiling ends the run at once as `outputLimit`, and nothing of the
/// stream is returned.
#[cfg(unix)]
#[test]
fn a_captured_stream_past_its_ceiling_fails_as_output_limit() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(
            r"local result, message, kind = process.run('head', { '-c', '10000000', '/dev/zero' }, { stdout = 'capture', maxStdoutBytes = 1000 })
              assert(result == nil and kind == 'outputLimit', tostring(kind))
              assert(message:find('byte limit', 1, true), message)",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
}

/// Each ceiling is for its own stream: stderr over its ceiling fails the run even when stdout is
/// well under its own, and stdout over its ceiling fails it even when stderr is well under.
#[cfg(unix)]
#[test]
fn stdout_and_stderr_ceilings_are_separate() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local result, _, kind = process.run('sh', { '-c', 'printf ok; head -c 10000000 /dev/zero >&2' },
                  { stdout = 'capture', stderr = 'capture', maxStdoutBytes = 1000, maxStderrBytes = 1000 })
              assert(result == nil and kind == 'outputLimit', tostring(kind))
              result, _, kind = process.run('sh', { '-c', 'head -c 10000000 /dev/zero; printf err >&2' },
                  { stdout = 'capture', stderr = 'capture', maxStdoutBytes = 1000, maxStderrBytes = 1000 })
              assert(result == nil and kind == 'outputLimit', tostring(kind))",
        )
        .unwrap();
}

/// Output under both ceilings, and a run under its timeout, succeeds with exactly what was written.
/// A stream may write exactly its ceiling.
#[cfg(unix)]
#[test]
fn output_within_its_ceilings_and_time_is_returned_whole() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local result = assert(process.run('sh', { '-c', 'printf abc; printf de >&2' },
                  { stdout = 'capture', stderr = 'capture', timeoutMs = 5000, maxStdoutBytes = 3, maxStderrBytes = 2 }))
              assert(result.success and result.stdout == 'abc' and result.stderr == 'de', tostring(result.stdout))
              result = assert(process.run('head', { '-c', '5000', '/dev/zero' }, { stdout = 'capture', maxStdoutBytes = 6000 }))
              assert(result.success and #result.stdout == 5000)",
        )
        .unwrap();
}

/// A grandchild that holds the captured pipe open does not hold the call past its timeout: the
/// whole process group is killed at the deadline.
#[cfg(unix)]
#[test]
fn a_descendant_holding_the_pipe_does_not_stall_a_timed_run() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(
            r"local result, _, kind = process.run('sh', { '-c', 'sleep 30 & echo started; wait' },
                  { stdout = 'capture', timeoutMs = 300 })
              assert(result == nil and kind == 'timeout', tostring(kind))",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
}

/// The new options are validated the way the others are: a bad number, or a ceiling on a stream
/// that is not captured, raises before anything starts.
#[test]
fn bounds_options_are_validated() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local bad = {
                { timeoutMs = 0 }, { timeoutMs = -1 }, { timeoutMs = 1.5 }, { timeoutMs = math.huge },
                { timeoutMs = 0 / 0 }, { maxStdoutBytes = 10 }, { maxStderrBytes = 10, stderr = 'null' },
                { maxStdoutBytes = -1, stdout = 'capture' }, { maxStderrBytes = 2.5, stderr = 'capture' },
              }
              for index = 1, #bad do
                  local ok, err = pcall(process.run, 'sh', { '-c', 'exit 0' }, bad[index])
                  assert(not ok, 'option ' .. index .. ' was accepted')
              end",
        )
        .unwrap();
}

/// A limit no clock can hold is refused while the options are read, before any child starts. Each
/// refused call would create a marker file if it ran; the marker must not exist afterwards.
#[cfg(unix)]
#[test]
fn an_unrepresentable_limit_is_refused_before_any_child_starts() {
    let marker = std::env::temp_dir().join(format!("l3i-process-refused-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r#"local touch = {{ '-c', 'touch "$1"', 'sh', '{}' }}
               local timeouts = {{ 1e300, 2^63, 2^53, 86400001, 1 / 0, 0 / 0, -1, 1.5 }}
               for _, timeout in ipairs(timeouts) do
                   local ok, err = pcall(process.run, 'sh', touch, {{ timeoutMs = timeout }})
                   assert(not ok and err:find('timeoutMs must be', 1, true), tostring(err))
                   ok, err = pcall(process.start, 'sh', touch, {{ timeoutMs = timeout }})
                   assert(not ok and err:find('timeoutMs must be', 1, true), tostring(err))
               end
               for _, ceiling in ipairs({{ 2^53, 1e300 }}) do
                   local ok, err = pcall(process.run, 'sh', touch, {{ stdout = 'capture', maxStdoutBytes = ceiling }})
                   assert(not ok and err:find('maxStdoutBytes must be', 1, true), tostring(err))
                   ok, err = pcall(process.start, 'sh', touch, {{ stdout = 'pipe', maxStdoutBytes = ceiling }})
                   assert(not ok and err:find('maxStdoutBytes must be', 1, true), tostring(err))
               end
               process.sleep(0.2)"#,
            marker.display()
        ))
        .unwrap();
    assert!(!marker.exists(), "a refused child started anyway");
}

/// The longest timeout, a day, is accepted for a run and for a started child.
#[cfg(unix)]
#[test]
fn the_longest_timeout_is_accepted() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local result = assert(process.run('sh', { '-c', 'exit 3' }, { timeoutMs = 86400000 }))
              assert(not result.success and result.code == 3)
              local child = assert(process.start('sh', { '-c', 'exit 0' }, { timeoutMs = 86400000 }))
              assert(assert(child:wait(5000)).success)
              child:close()",
        )
        .unwrap();
}

/// A runtime whose process extension allows `max` live children, with the spawn capability.
#[cfg(unix)]
fn runtime_limiting_children(max: u32) -> Runtime {
    let policy = RuntimePolicy::new().compat_global("@dream/process", "process").capability(SPAWN_CAPABILITY);
    let plan =
        RuntimePlan::builder().policy(policy).extension(ProcessExtension.with_max_children(max)).finalize().unwrap();
    Runtime::from_plan(&plan).unwrap()
}

/// A live child is one not yet reaped. At the limit a start raises, and `run` counts too; a closed
/// child frees its slot at once, so the next start succeeds straight away.
#[cfg(unix)]
#[test]
fn the_live_child_limit_refuses_the_next_start_and_a_closed_child_frees_its_slot() {
    let runtime = runtime_limiting_children(2);
    runtime
        .exec(
            r"local first = assert(process.start('sleep', { '30' }))
              local second = assert(process.start('sleep', { '30' }))
              local ok, err = pcall(process.start, 'sleep', { '30' })
              assert(not ok and err:find('at most 2 child processes', 1, true), tostring(err))
              ok, err = pcall(process.run, 'true', {})
              assert(not ok and err:find('dream.process.run', 1, true), tostring(err))
              first:close()
              local third = assert(process.start('sleep', { '30' }))
              second:close()
              third:close()
              assert(process.run('true', {}).success)",
        )
        .unwrap();
}

/// A failed spawn holds no slot: ten programs that do not exist leave the whole limit for later.
#[cfg(unix)]
#[test]
fn a_failed_spawn_does_not_hold_a_slot() {
    let runtime = runtime_limiting_children(2);
    runtime
        .exec(
            r"for _ = 1, 10 do
                  local child, _, kind = process.start('dream-process-no-such-program')
                  assert(child == nil and kind == 'notFound', tostring(kind))
              end
              local first = assert(process.start('sleep', { '30' }))
              local second = assert(process.start('sleep', { '30' }))
              first:close()
              second:close()",
        )
        .unwrap();
}

/// Start and close cycles far past the limit all succeed, since each close frees its slot.
#[cfg(unix)]
#[test]
fn repeated_start_and_close_cycles_stay_within_the_limit() {
    let runtime = runtime_limiting_children(2);
    runtime
        .exec(
            r"for _ = 1, 200 do
                  local child = assert(process.start('sleep', { '30' }))
                  child:close()
              end",
        )
        .unwrap();
}

/// A child a timeout kills frees its slot without a wait or a close: the timer reaps it.
#[cfg(unix)]
#[test]
fn a_timeout_frees_the_slot_of_a_child_nobody_waits_for() {
    let runtime = runtime_limiting_children(1);
    runtime
        .exec(
            r"local held = assert(process.start('sleep', { '30' }, { timeoutMs = 50 }))
              process.sleep(0.5)
              local next = assert(process.start('sleep', { '30' }))
              next:close()
              held:close()",
        )
        .unwrap();
}

/// A child that exited still holds its slot until its handle is closed. A wait answers its status
/// and does not free the slot: the handle can still stop what the child left behind, so the child
/// is still counted.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "freebsd"))]
#[test]
fn a_child_that_exited_holds_its_slot_until_its_handle_is_closed() {
    let runtime = runtime_limiting_children(1);
    runtime
        .exec(
            r"local done = assert(process.start('true', {}))
              local ok = pcall(process.start, 'sleep', { '30' })
              assert(not ok, 'an unreaped child freed its slot')
              assert(assert(done:wait(5000)).success)
              assert(assert(done:wait(0)).success, 'a second wait lost the status')
              ok = pcall(process.start, 'sleep', { '30' })
              assert(not ok, 'a wait freed the slot of a handle that is still open')
              done:close()
              local next = assert(process.start('sleep', { '30' }))
              next:close()",
        )
        .unwrap();
}

/// Whether process `pid` still exists, to this user.
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Runs `script`, which leaves a background process whose pid it returns, and asserts that process
/// is gone soon afterwards.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "freebsd"))]
fn assert_descendant_stopped(script: &str) {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let pid: f64 = runtime.eval(script).unwrap();
    // A script that left its handle open has it closed by the runtime's shutdown.
    drop(runtime);
    #[allow(clippy::cast_possible_truncation)]
    let pid = pid as i32;
    assert!(pid > 1, "the fixture did not report its background process");
    let started = Instant::now();
    while alive(pid) {
        if started.elapsed() > Duration::from_secs(5) {
            // Do not leave a thirty-second sleep behind a failed assertion.
            let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("the descendant outlived the handle that should have stopped it");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The fixture the descendant tests share: a shell that starts a long sleep holding its stdout,
/// prints that sleep's pid and exits. `after` runs once the shell's exit has been waited for.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "freebsd"))]
fn left_behind(after: &str) -> String {
    format!(
        r"local child = assert(process.start('sh', {{ '-c', 'sleep 30 & echo $!; exit 0' }}, {{ stdout = 'pipe' }}))
          local text, ticks = '', 0
          while not text:find('\n', 1, true) do
              local chunk, _, kind = child:readStdout(64)
              if chunk then text = text .. chunk
              else
                  assert(kind == 'wouldBlock', tostring(kind))
                  process.sleep(0.005)
                  ticks = ticks + 1
                  assert(ticks < 2000, 'the shell never reported its background process')
              end
          end
          assert(assert(child:wait(5000)).success)
          {after}
          return tonumber(text:match('%d+'))"
    )
}

/// `close` after `wait` still stops a process the child started: the child that `wait` answered
/// for is not reaped until the close, so the group it led can still be signalled. Before this, a
/// wait reaped the child and the close that followed could reach nothing, leaving the process and
/// the pipe it held.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "freebsd"))]
#[test]
fn close_after_wait_stops_what_the_child_left_in_its_group() {
    assert_descendant_stopped(&left_behind("child:close()"));
}

/// `kill` after `wait` reaches it too, and so does the shutdown of a runtime whose script waited
/// and never closed its handle.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "freebsd"))]
#[test]
fn kill_after_wait_and_a_runtime_shutdown_stop_what_the_child_left_too() {
    assert_descendant_stopped(&left_behind("assert(child:kill())"));
    assert_descendant_stopped(&left_behind(""));
}

/// A limit of 0 is raised to 1: one child may run at a time, never none.
#[cfg(unix)]
#[test]
fn a_limit_below_one_is_raised_to_one() {
    let runtime = runtime_limiting_children(0);
    runtime
        .exec(
            r"local only = assert(process.start('sleep', { '30' }))
              local ok = pcall(process.start, 'sleep', { '30' })
              assert(not ok)
              only:close()",
        )
        .unwrap();
}

/// Kills the sentinel on drop, so a failing assertion does not leave it running.
#[cfg(unix)]
struct Sentinel(std::process::Child);

#[cfg(unix)]
impl Drop for Sentinel {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A handle that was closed, which is what reaps its child, never signals a group again: not by its
/// timer after the deadline, not by a second close. The sentinel is a process of our own in a group
/// of its own; churning through short processes keeps the pids moving, though pid reuse itself is
/// not forced, so this cannot fail on the reuse it guards against: the unit tests that count the
/// signals sent are what hold that.
#[cfg(unix)]
#[test]
fn a_closed_handle_never_signals_its_group_again() {
    use std::os::unix::process::CommandExt as _;
    let mut sentinel = Sentinel(std::process::Command::new("sleep").arg("600").process_group(0).spawn().unwrap());
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local old = assert(process.start('true', {}, { timeoutMs = 50 }))
              assert(assert(old:wait(5000)).success)
              old:close()
              for _ = 1, 300 do assert(process.run('true', {}).success) end
              assert(not pcall(function() return old:kill() end), 'a closed handle answered kill')
              old:close()
              process.sleep(0.2)",
        )
        .unwrap();
    assert!(sentinel.0.try_wait().unwrap().is_none(), "the sentinel was signalled through a closed handle");
}

/// A bounded run whose leader exits while a process it started holds the captured pipe open ends
/// at its time limit: the group, which still holds that process, is killed while the leader is
/// unreaped, and the run does not wait out the process's own sleep.
#[cfg(unix)]
#[test]
fn a_bounded_run_kills_a_group_member_that_outlives_its_leader() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(
            r"local result, _, kind = process.run('sh', { '-c', 'sleep 30 & exit 0' },
                  { stdout = 'capture', timeoutMs = 300 })
              assert(result == nil and kind == 'timeout', tostring(kind))",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "the run waited {:?}", started.elapsed());
}

/// A started child whose leader exits while a process it started holds its stdout is ended at the
/// timeout: its readers reach end of stream, rather than waiting on a pipe nothing closes.
#[cfg(unix)]
#[test]
fn a_timed_handle_ends_a_group_member_its_leader_left_behind() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local child = assert(process.start('sh', { '-c', 'sleep 30 & echo started; exit 0' },
                  { stdout = 'pipe', timeoutMs = 300 }))
              local text, ticks = '', 0
              while true do
                  local chunk, _, kind = child:readStdout(4096)
                  if chunk then
                      text = text .. chunk
                  elseif kind == 'wouldBlock' then
                      process.sleep(0.01)
                      ticks = ticks + 1
                      assert(ticks < 1000, 'a group member held the pipe past the timeout')
                  else
                      break
                  end
              end
              assert(text == 'started\n', text)
              child:close()",
        )
        .unwrap();
}

/// Luau helpers the handle tests share: drain a stream to its end, failing on any real error, and
/// pause between polls without waiting on the child.
#[cfg(unix)]
const DRAIN: &str = r"
local function drain(child, which)
    local out = ''
    while true do
        local chunk, message, kind = child[which](child, 4096)
        if chunk then out = out .. chunk
        elseif kind == 'wouldBlock' then process.sleep(0.002)
        elseif message then error(message .. ' ' .. tostring(kind))
        else return out end
    end
end
";

#[cfg(unix)]
#[test]
fn a_started_child_round_trips_its_input_and_output() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r"{DRAIN}
            local child = assert(process.start('cat', nil, {{ stdin = 'pipe', stdout = 'pipe' }}))
            local written, message, kind = child:write('hello\0world\n')
            assert(written == 12 and kind == nil, message)
            assert(child:closeStdin() == nil)
            assert(drain(child, 'readStdout') == 'hello\0world\n')
            local status = assert(child:wait(5000))
            assert(status.success and status.code == 0 and status.signal == nil)
            child:close()"
        ))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn reads_answer_wouldblock_instead_of_blocking() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(
            r"local child = assert(process.start('sleep', { '5' }, { stdout = 'pipe' }))
              local value, message, kind = child:readStdout(10)
              assert(value == nil and kind == 'wouldBlock', tostring(kind))
              assert(child:wait() == nil, 'poll of a running child must not block')
              child:close()",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
}

#[cfg(unix)]
#[test]
fn reads_never_return_more_than_asked() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r"{DRAIN}
            local child = assert(process.start('head', {{ '-c', '100000', '/dev/zero' }}, {{ stdout = 'pipe' }}))
            local total = 0
            while true do
                local chunk, message, kind = child:readStdout(1000)
                if chunk then assert(#chunk >= 1 and #chunk <= 1000, #chunk) total = total + #chunk
                elseif kind == 'wouldBlock' then process.sleep(0.002)
                else assert(message == nil, message) break end
            end
            assert(total == 100000, total)
            assert(child:wait(5000).success)
            child:close()"
        ))
        .unwrap();
}

/// A child that writes far more than its pipe holds, and is not read, stays blocked on the pipe:
/// the handle buffers a bounded amount, so the child is still running after a while, and it
/// finishes once everything is read.
#[cfg(unix)]
#[test]
fn an_unread_child_blocks_on_its_pipe_and_finishes_once_read() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r"{DRAIN}
            local child = assert(process.start('head', {{ '-c', '10000000', '/dev/zero' }}, {{ stdout = 'pipe' }}))
            process.sleep(0.3)
            assert(child:wait() == nil, 'an unread child finished: its output is not bounded')
            assert(#drain(child, 'readStdout') == 10000000)
            assert(child:wait(5000).success)
            child:close()"
        ))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn stdin_backpressure_answers_wouldblock_when_the_queue_is_full() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local child = assert(process.start('sleep', { '5' }, { stdin = 'pipe' }))
              local big = string.rep('x', 1024 * 1024)
              local accepted, message, kind = child:write(big)
              assert(accepted == 1024 * 1024 and kind == nil, tostring(message))
              -- The queue holds 1 MiB not yet written; the kernel pipe takes more, and then writes block.
              local total, blocked = accepted, false
              for _ = 1, 64 do
                  local count, message2, kind2 = child:write(big)
                  if count then total = total + count
                  else assert(count == nil and kind2 == 'wouldBlock', tostring(message2)) blocked = true break end
              end
              assert(blocked, 'writes to a child that never reads never blocked')
              assert(total >= 1024 * 1024 and total <= 2 * 1024 * 1024, total)
              child:close()",
        )
        .unwrap();
}

#[cfg(unix)]
#[test]
fn stdout_and_stderr_are_drained_together() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r"{DRAIN}
            local script ='i=0; while [ $i -lt 100 ]; do head -c 10000 /dev/zero; head -c 10000 /dev/zero >&2; i=$((i+1)); done'
            local child = assert(process.start('sh', {{ '-c', script }}, {{ stdout = 'pipe', stderr = 'pipe' }}))
            local out, err = '', ''
            local finished_out, finished_err = false, false
            while not (finished_out and finished_err) do
                if not finished_out then
                    local chunk, message, kind = child:readStdout(8192)
                    if chunk then out = out .. chunk elseif kind == 'wouldBlock' then else finished_out = true end
                end
                if not finished_err then
                    local chunk, message, kind = child:readStderr(8192)
                    if chunk then err = err .. chunk elseif kind == 'wouldBlock' then else finished_err = true end
                end
                process.sleep(0.001)
            end
            assert(#out == 1000000 and #err == 1000000, #out .. ' ' .. #err)
            assert(child:wait(5000).success)
            child:close()"
        ))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn a_timeout_kills_the_child_and_reads_report_it() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(
            r"local child = assert(process.start('sleep', { '10' }, { stdout = 'pipe', timeoutMs = 200 }))
              local status = assert(child:wait(5000))
              assert(not status.success and status.signal == 9, tostring(status.signal))
              local value, message, kind = child:readStdout(10)
              assert(value == nil and kind == 'timeout', tostring(kind))
              child:close()",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
}

#[cfg(unix)]
#[test]
fn a_captured_stream_past_its_ceiling_kills_the_child_and_reads_report_it() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local child = assert(process.start('head', { '-c', '10000000', '/dev/zero' }, { stdout = 'pipe', maxStdoutBytes = 1000 }))
              local kind
              for _ = 1, 2000 do
                  local value, message
                  value, message, kind = child:readStdout(100)
                  if kind ~= 'wouldBlock' then break end
                  process.sleep(0.002)
              end
              assert(kind == 'outputLimit', tostring(kind))
              local status = assert(child:wait(5000))
              assert(status.signal == 9)
              local _, _, again = child:readStdout(100)
              assert(again == 'outputLimit', tostring(again))
              child:close()",
        )
        .unwrap();
}

#[cfg(unix)]
#[test]
fn kill_and_close_are_idempotent_and_methods_after_close_raise() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local child = assert(process.start('sleep', { '10' }))
              assert(child:kill() == true and child:kill() == true)
              child:close()
              child:close()
              local ok, err = pcall(child.kill, child)
              assert(not ok and err:find('closed', 1, true), err)
              ok, err = pcall(child.readStdout, child, 1)
              assert(not ok and err:find('closed', 1, true), err)
              ok, err = pcall(child.wait, child)
              assert(not ok and err:find('closed', 1, true), err)",
        )
        .unwrap();
}

#[cfg(unix)]
#[test]
fn a_started_program_that_does_not_exist_answers_without_a_handle() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r"local child, message, kind = process.start('dream-process-no-such-program')
              assert(child == nil and kind == 'notFound' and message:find('dream.process.start', 1, true), message)
              local ok, err = pcall(process.start, 'sh', { '-c', 'exit 0' }, { stdin = 'maybe' })
              assert(not ok and err:find('stdin must be', 1, true), err)",
        )
        .unwrap();
}

/// A grandchild holding the pipes does not keep `close` waiting after `kill`: the group is killed,
/// and the pipes end with it.
#[cfg(unix)]
#[test]
fn a_descendant_holding_pipes_does_not_hold_close() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let started = Instant::now();
    runtime
        .exec(
            r"local child = assert(process.start('sh', { '-c', 'sleep 30 & echo started; wait' }, { stdout = 'pipe' }))
              local text = ''
              while not text:find('started', 1, true) do
                  local chunk, _, kind = child:readStdout(100)
                  if chunk then text = text .. chunk else assert(kind == 'wouldBlock') process.sleep(0.002) end
              end
              assert(child:kill())
              child:close()",
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "close waited {:?}", started.elapsed());
}

/// Reads the pid a shell wrote to `path`, waiting for it to appear.
#[cfg(unix)]
fn pid_written_to(path: &std::path::Path) -> libc::pid_t {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(pid) = std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse().ok()) {
            return pid;
        }
        assert!(Instant::now() < until, "no pid was written to {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the pid names a process this test may signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Dropping the runtime (and so collecting the handle) kills and reaps a child still running.
#[cfg(unix)]
#[test]
fn dropping_a_handle_kills_its_child() {
    let path = std::env::temp_dir().join(format!("l3i-process-drop-{}", std::process::id()));
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r#"held = assert(process.start('sh', {{ '-c', 'echo $$ > "$1"; exec sleep 30', 'sh', '{}' }}))"#,
            path.display()
        ))
        .unwrap();
    let pid = pid_written_to(&path);
    drop(runtime);
    std::fs::remove_file(&path).unwrap();
    assert!(!process_exists(pid), "the child {pid} outlived its handle");
}

/// Shutdown does what close does, while the script still holds the handle.
#[cfg(unix)]
#[test]
fn shutdown_kills_a_started_child() {
    let path = std::env::temp_dir().join(format!("l3i-process-shutdown-{}", std::process::id()));
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(&format!(
            r#"held = assert(process.start('sh', {{ '-c', 'echo $$ > "$1"; exec sleep 30', 'sh', '{}' }}))"#,
            path.display()
        ))
        .unwrap();
    let pid = pid_written_to(&path);
    runtime.shutdown();
    std::fs::remove_file(&path).unwrap();
    assert!(!process_exists(pid), "the child {pid} outlived shutdown");
}

/// A secret reaches a child through a descriptor it inherits and nowhere else: the variable holds
/// the descriptor's number, the bytes are read once and then the pipe is at end of file, and
/// neither the child's arguments nor its environment contains them.
#[cfg(unix)]
#[test]
fn a_secret_reaches_a_child_through_an_inherited_descriptor_and_nowhere_else() {
    runtime_with(false)
        .exec("assert(not pcall(process.run, 'true', {}, { secret = { env = 'ROOK_SECRET_FD', data = 'x' } }))")
        .unwrap();
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r#"
            local secret = 'tok-' .. string.rep('s3cr3t', 20)
            -- The child reads the descriptor named in the variable, then reads it again.
            local result = assert(process.run('sh', { '-c',
              'printf "%s|" "$ROOK_SECRET_FD"; cat <&"$ROOK_SECRET_FD"; printf "|"; cat <&"$ROOK_SECRET_FD"; printf "|end"' },
              { secret = { env = 'ROOK_SECRET_FD', data = secret }, stdout = 'capture', timeoutMs = 10000 }))
            assert(result.success, tostring(result.code))
            local number, first, second = string.match(result.stdout, '^(%d+)|(.*)|(.*)|end$')
            assert(tonumber(number) >= 3, result.stdout)
            assert(first == secret, 'the child did not read the secret')
            assert(second == '', 'the secret could be read twice')

            -- Bytes that are not text, from a buffer.
            local binary = buffer.fromstring('a\0b\255c')
            local echoed = assert(process.run('sh', { '-c', 'cat <&"$FD"' },
              { secret = { env = 'FD', data = binary }, stdout = 'capture', timeoutMs = 10000 }))
            assert(echoed.stdout == 'a\0b\255c')

            -- What the child can see of itself holds the descriptor's number and not the secret.
            local seen = assert(process.run('sh', { '-c', 'env; echo "$0" "$@"; cat /proc/$$/cmdline 2>/dev/null; cat /proc/$$/environ 2>/dev/null' },
              { secret = { env = 'ROOK_SECRET_FD', data = secret }, stdout = 'capture', timeoutMs = 10000 }))
            assert(string.find(seen.stdout, 'ROOK_SECRET_FD=', 1, true), seen.stdout)
            assert(not string.find(seen.stdout, 's3cr3t', 1, true), 'the secret is in the child\'s arguments or environment')

            -- A helper the child starts inherits the descriptor: this is how a credential helper reads it.
            local helped = assert(process.run('sh', { '-c', 'sh -c \'sh -c "cat <&$ROOK_SECRET_FD"\'' },
              { secret = { env = 'ROOK_SECRET_FD', data = secret }, stdout = 'capture', timeoutMs = 10000 }))
            assert(helped.stdout == secret, 'a grandchild could not read the secret')

            -- A child without a secret has no such descriptor: the one a sibling was given is not left open.
            local sibling = assert(process.run('sh', { '-c', 'cat <&' .. number .. ' 2>/dev/null; echo "rc=$?"' },
              { stdout = 'capture', timeoutMs = 10000 }))
            assert(string.find(sibling.stdout, 'rc=', 1, true) and not string.find(sibling.stdout, 's3cr3t', 1, true), sibling.stdout)
            "#,
        )
        .unwrap();
}

/// The same through a started child, and the refusals: every one is raised before anything starts.
#[cfg(unix)]
#[test]
fn a_started_child_takes_a_secret_and_malformed_ones_start_nothing() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r#"
            local child = assert(process.start('sh', { '-c', 'cat <&"$TOKEN_FD"' },
              { secret = { env = 'TOKEN_FD', data = 'handle-secret' }, stdout = 'pipe', timeoutMs = 10000 }))
            local text = ''
            while true do
              local chunk, message, kind = child:readStdout(4096)
              if chunk then text = text .. chunk
              elseif kind == 'wouldBlock' then process.sleep(0.005)
              elseif message then error(message)
              else break end
            end
            assert(text == 'handle-secret', text)
            assert(assert(child:wait(5000)).success)
            child:close()

            local marker = '/tmp/l3i-secret-test-must-not-exist-' .. tostring(os.clock())
            -- A value that appears in no message this library writes, so that finding it in one
            -- would mean a refusal repeated the secret.
            local Marked = 'ZZ-SECRET-ZZ'
            local refused = {
              { env = 'TOKEN_FD' },
              { data = Marked },
              { env = 'lower', data = Marked },
              { env = '1FD', data = Marked },
              { env = 'A=B', data = Marked },
              { env = '', data = Marked },
              { env = string.rep('A', 65), data = Marked },
              { env = 'TOKEN_FD', data = '' },
              { env = 'TOKEN_FD', data = string.rep(Marked, 400) },
              { env = 'TOKEN_FD', data = Marked, extra = true },
              { env = 'TOKEN_FD', data = 7 },
              -- A number in one of these would break how the child finds and loads programs.
              { env = 'PATH', data = Marked },
              { env = 'LD_PRELOAD', data = Marked },
              { env = 'DYLD_INSERT_LIBRARIES', data = Marked },
              'not a table',
            }
            for index, secret in refused do
              for _, start in { process.run, process.start } do
                local ok, problem = pcall(start, 'sh', { '-c', 'touch ' .. marker }, { secret = secret, timeoutMs = 5000 })
                assert(not ok, 'malformed secret ' .. index .. ' was accepted')
                assert(not string.find(tostring(problem), 'ZZ', 1, true), 'a refusal repeated the secret: ' .. tostring(problem))
                assert(string.find(tostring(problem), 'secret', 1, true), tostring(problem))
              end
            end
            -- The variable the secret names cannot also be set by env, through either call.
            for _, start in { process.run, process.start } do
              local ok, problem = pcall(start, 'true', {}, { env = { TOKEN_FD = '9' }, secret = { env = 'TOKEN_FD', data = Marked } })
              assert(not ok and not string.find(tostring(problem), 'ZZ', 1, true), tostring(problem))
            end
            -- A program that cannot be started is an answer, with a secret as without one.
            local missing, missingMessage = process.start('l3i-no-such-program', {}, { secret = { env = 'TOKEN_FD', data = Marked } })
            assert(missing == nil and not string.find(tostring(missingMessage), 'ZZ', 1, true), tostring(missingMessage))
            -- A cleared environment still carries the variable, and the secret behind it.
            local cleared = assert(process.run('/bin/sh', { '-c', 'cat <&"$TOKEN_FD"' },
              { clearEnv = true, secret = { env = 'TOKEN_FD', data = Marked }, stdout = 'capture', timeoutMs = 10000 }))
            assert(cleared.stdout == Marked, cleared.stdout)
            -- The largest secret fits.
            local largest = string.rep('k', 4096)
            local fits = assert(process.run('sh', { '-c', 'cat <&"$FD" | wc -c' },
              { secret = { env = 'FD', data = largest }, stdout = 'capture', timeoutMs = 10000 }))
            assert(tonumber((string.gsub(fits.stdout, '%s', ''))) == 4096, fits.stdout)
            -- Nothing a refusal named was started.
            local probe = assert(process.run('sh', { '-c', 'test -e ' .. marker .. ' && echo there || echo absent' }, { stdout = 'capture' }))
            assert(probe.stdout == 'absent\n', probe.stdout)
            "#,
        )
        .unwrap();
}

/// A secret costs no descriptor once the child is started: a hundred of them leave none behind.
#[cfg(target_os = "linux")]
#[test]
fn secrets_leave_no_descriptor_in_the_host() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    let open = || std::fs::read_dir("/proc/self/fd").map_or(0, Iterator::count);
    runtime
        .exec("assert(process.run('true', {}, { secret = { env = 'FD', data = 'warm' }, timeoutMs = 5000 }).success)")
        .unwrap();
    let before = open();
    runtime
        .exec(
            "for index = 1, 100 do \
               assert(process.run('true', {}, { secret = { env = 'FD', data = 'secret' .. index }, timeoutMs = 5000 }).success) \
               local missing = process.run('l3i-no-such-program', {}, { secret = { env = 'FD', data = 'x' }, timeoutMs = 5000 }) \
               assert(missing == nil) \
             end",
        )
        .unwrap();
    // Other tests of this binary start children meanwhile, so allow for theirs: a leak would be 100.
    assert!(open() < before + 50, "secrets left descriptors behind: {} against {before}", open());
}

/// The half of the next test that runs with its standard input closed: the pipe the secret is
/// written into would be given descriptor 0, which the child's own input is then put on.
#[cfg(unix)]
#[test]
fn secret_with_closed_stdin_child() {
    if std::env::var_os("L3I_SECRET_CLOSED_STDIN_TEST").is_none() {
        return;
    }
    // SAFETY: closing this process's own standard input and output; nothing else here uses them
    // (the test harness's capture is in-process and the result goes to stderr).
    unsafe {
        libc::close(0);
        libc::close(1);
    }
    let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
    runtime
        .exec(
            r#"
            local result = assert(process.run('sh', { '-c', 'printf "%s|" "$FD"; cat <&"$FD"; printf "|"; cat' },
              { secret = { env = 'FD', data = 'THE-SECRET' }, stdin = 'SCRIPT-STDIN', stdout = 'capture', timeoutMs = 10000 }))
            local number, secret, input = string.match(result.stdout, '^(%d+)|(.*)|(.*)$')
            assert(tonumber(number) >= 3, 'the secret was put on a standard descriptor: ' .. result.stdout)
            assert(secret == 'THE-SECRET', 'the secret was lost: ' .. result.stdout)
            -- Whether the child's own input arrives in a host without standard descriptors is the
            -- standard library's business, with a secret or without one; see the comparison below.
            local plain = assert(process.run('sh', { '-c', 'cat' }, { stdin = 'SCRIPT-STDIN', stdout = 'capture', timeoutMs = 10000 }))
            assert(input == plain.stdout, 'a secret changed what the child read on its own input: ' .. result.stdout)
            -- With the child's output inherited (and closed in the host), the secret is not its output.
            local started = assert(process.start('sh', { '-c', 'cat <&"$FD" >&2' },
              { secret = { env = 'FD', data = 'SECOND-SECRET' }, stderr = 'pipe', timeoutMs = 10000 }))
            local text = ''
            while true do
              local chunk, message, kind = started:readStderr(4096)
              if chunk then text = text .. chunk
              elseif kind == 'wouldBlock' then process.sleep(0.005)
              elseif message then error(message)
              else break end
            end
            started:close()
            assert(text == 'SECOND-SECRET', text)
            "#,
        )
        .unwrap();
}

/// A host that has closed its own standard streams: the secret still reaches the child on a
/// descriptor of its own, and the child's standard streams are still its own.
#[cfg(unix)]
#[test]
fn a_secret_never_lands_on_a_standard_descriptor() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process::secret_with_closed_stdin_child", "--nocapture"])
        .env("L3I_SECRET_CLOSED_STDIN_TEST", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success(), "closed-stdin child: {}", String::from_utf8_lossy(&output.stderr));
}

/// Secrets that are never read, in children started from several threads at once, each of which
/// reads every descriptor it was given: none finds a secret that is not its own.
#[cfg(target_os = "linux")]
#[test]
fn concurrent_children_never_inherit_one_another_s_secret() {
    let workers: Vec<_> = (0..6)
        .map(|worker| {
            std::thread::spawn(move || {
                let runtime = runtime_granting(&[SPAWN_CAPABILITY], Limits::default());
                runtime
                    .exec(&format!(
                        r#"
                        for round = 1, 25 do
                          local mine = 'secret-of-{worker}-' .. round .. '-'
                          -- Every descriptor above the standard three, read to its end. Half the
                          -- children have a secret; the other half must find nothing at all.
                          local options = {{ stdout = 'capture', stderr = 'null', timeoutMs = 20000 }}
                          if round % 2 == 0 then options.secret = {{ env = 'FD', data = mine }} end
                          local result = assert(process.run('sh', {{ '-c',
                            'for fd in /proc/self/fd/*; do n=${{fd##*/}}; [ "$n" -gt 2 ] && cat "/proc/self/fd/$n" 2>/dev/null; done; true' }}, options))
                          local seen = result.stdout
                          local expected = if round % 2 == 0 then mine else ''
                          for found in string.gmatch(seen, 'secret%-of%-%d+%-%d+%-') do
                            assert(found == expected, 'a child read another child\'s secret: ' .. found)
                          end
                          if round % 2 == 0 then assert(string.find(seen, mine, 1, true), 'a child could not read its own secret') end
                        end
                        "#
                    ))
                    .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}

/// On Windows the child inherits a handle and the variable holds its value: read once, then end of
/// file, and neither the environment nor the command line holds the bytes.
#[cfg(windows)]
#[test]
fn a_secret_reaches_a_windows_child_through_an_inherited_handle() {
    let runtime = runtime_granting(&[SPAWN_CAPABILITY, ENVIRONMENT_CAPABILITY], Limits::default());
    runtime
        .exec(
            r#"
            local reader = '$h=[IntPtr][Int64]$env:L3I_SECRET_HANDLE; '
              .. '$s=New-Object System.IO.FileStream((New-Object Microsoft.Win32.SafeHandles.SafeFileHandle($h,$false)),[System.IO.FileAccess]::Read); '
              .. '$r=New-Object System.IO.StreamReader($s); [Console]::Out.Write($r.ReadToEnd()); [Console]::Out.Write("|"); '
              .. '[Console]::Out.Write(($s.ReadByte())); [Console]::Out.Write("|"); '
              .. '[Console]::Out.Write(((Get-ChildItem env: | ForEach-Object { $_.Value }) -join ","))'
            local secret = 'windows-ZZ-SECRET-ZZ'
            local result, message = process.run('powershell', { '-NoProfile', '-NonInteractive', '-Command', reader },
              { secret = { env = 'L3I_SECRET_HANDLE', data = secret }, stdout = 'capture', stderr = 'capture', timeoutMs = 120000 })
            assert(result, message)
            assert(result.success, tostring(result.code) .. ': ' .. tostring(result.stderr))
            local first, again, environment = string.match(result.stdout, '^(.-)|(.-)|(.*)$')
            assert(first == secret, 'the child did not read the secret: ' .. result.stdout)
            assert(again == '-1', 'the handle was not at end of file after the secret: ' .. tostring(again))
            assert(not string.find(environment, 'ZZ-SECRET-ZZ', 1, true), 'the secret is in the child\'s environment')
            -- The variable holds a number, and a child that never reads the handle takes the secret with it.
            local handle = assert(process.run('powershell', { '-NoProfile', '-NonInteractive', '-Command', '[Console]::Out.Write($env:L3I_SECRET_HANDLE)' },
              { secret = { env = 'L3I_SECRET_HANDLE', data = secret }, stdout = 'capture', timeoutMs = 120000 })).stdout
            assert(tonumber(handle), 'the variable does not hold a number: ' .. tostring(handle))
            -- A child WITHOUT a secret, told the same handle value: whatever that value is in it, it
            -- is not the secret's pipe. Reading it fails, or reads something that is not the secret.
            local stranger = 'try { ' .. string.gsub(reader, '%[Console%]::Out%.Write%(%(%(Get%-ChildItem.*$', '') .. ' } catch { [Console]::Out.Write("refused") }'
            local other = assert(process.run('powershell', { '-NoProfile', '-NonInteractive', '-Command', stranger },
              { env = { L3I_SECRET_HANDLE = handle }, stdout = 'capture', stderr = 'null', timeoutMs = 120000 }))
            assert(not string.find(other.stdout, 'ZZ-SECRET-ZZ', 1, true), 'a child without a secret read one: ' .. other.stdout)
            -- Names that differ only in case are one variable here: the collision is refused.
            assert(not pcall(process.run, 'cmd', { '/c', 'exit' }, { env = { l3i_secret_handle = '1' }, secret = { env = 'L3I_SECRET_HANDLE', data = 'x' } }))
            -- Malformed secrets raise here as everywhere.
            assert(not pcall(process.run, 'cmd', { '/c', 'exit' }, { secret = { env = 'lower', data = 'x' } }))
            assert(not pcall(process.start, 'cmd', { '/c', 'exit' }, { secret = { env = 'PATH', data = 'x' } }))
            "#,
        )
        .unwrap();
}
