//! What a secret given to a real Git leaves behind when Git's credential helper never answers:
//! nothing. One test, in a test binary of its own, because it counts this process's descriptors
//! exactly and looks for one particular descendant, and nothing else may be starting children
//! meanwhile.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::process::{ProcessExtension, SPAWN_CAPABILITY};

/// The sleep the helper runs, so that it can be found and told from any other.
const HELPER: &str = "47.1173";

/// How many helpers are running. A `pgrep` that cannot be run or that reports an error is a
/// failure of the test, never "none".
fn helpers() -> usize {
    // Anchored to the helper's whole command line, so it matches nothing that only mentions it.
    let pattern = format!("^sleep {}$", HELPER.replace('.', "\\."));
    let output = std::process::Command::new("pgrep")
        .args(["-f", &pattern])
        .output()
        .expect("pgrep is needed to look for the helper");
    // 0: some matched. 1: none matched. Anything else is pgrep failing.
    match output.status.code() {
        Some(0) => String::from_utf8_lossy(&output.stdout).lines().count(),
        Some(1) => 0,
        other => panic!("pgrep failed with {other:?}: {}", String::from_utf8_lossy(&output.stderr)),
    }
}

/// Waits up to five seconds for `wanted` helpers, and answers how many there are then.
fn settled(wanted: usize) -> usize {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let now = helpers();
        if now == wanted || Instant::now() >= until {
            return now;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, Iterator::count)
}

/// Ends the helpers when the test does, however it ends.
struct Helpers;

impl Drop for Helpers {
    fn drop(&mut self) {
        let pattern = format!("^sleep {}$", HELPER.replace('.', "\\."));
        let _ = std::process::Command::new("pkill").args(["-9", "-f", &pattern]).status();
    }
}

const FIXTURE: &str = r"
Token = 'ghs_SYNTHETIC0TOKEN0FOR0TESTS0ONLY0ZZ9'
Environment = { GIT_CONFIG_NOSYSTEM = '1', GIT_CONFIG_GLOBAL = '/dev/null', GIT_TERMINAL_PROMPT = '0', HOME = '/nonexistent' }
Asked = 'protocol=https\nhost=example.invalid\n\n'
-- A credential helper that never reads the descriptor and never answers. Git starts it.
Arguments = { '-c', 'credential.helper=', '-c', 'credential.helper=!f() { sleep 47.1173; }; f', 'credential', 'fill' }
";

#[test]
fn a_git_whose_helper_hangs_is_stopped_and_leaves_no_process_and_no_descriptor() {
    let git = std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !git {
        eprintln!("skipped: no git");
        return;
    }
    assert_eq!(helpers(), 0, "a helper of an earlier run is still there");
    let _helpers = Helpers;
    let policy = RuntimePolicy::new().compat_global("@dream/process", "process").capability(SPAWN_CAPABILITY);
    let plan = RuntimePlan::builder().policy(policy).extension(ProcessExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.exec(FIXTURE).unwrap();
    // Whatever a first child with a secret makes this process keep for good is in the baseline.
    runtime
        .exec(
            "assert(process.run('true', {}, { secret = { env = 'FD', data = 'warm' }, timeoutMs = 5000 }).success) \
             local warm = assert(process.start('true', {}, { secret = { env = 'FD', data = 'warm' }, stdout = 'pipe' })) \
             warm:wait(5000) warm:close()",
        )
        .unwrap();
    // A closed handle's threads are cancelled, not joined: the baseline is taken once it is still.
    let mut baseline = descriptors();
    let mut unchanged = 0;
    while unchanged < 10 {
        std::thread::sleep(Duration::from_millis(10));
        let now = descriptors();
        unchanged = if now == baseline { unchanged + 1 } else { 0 };
        baseline = now;
    }

    // A started Git, its helper hanging. The helper is SEEN running before anything is stopped.
    runtime
        .exec(
            "held = assert(process.start('git', Arguments, { stdin = 'pipe', stdout = 'pipe', stderr = 'null', \
               env = Environment, secret = { env = 'L3I_GIT_TOKEN_FD', data = Token } })) \
             assert(held:write(Asked) == #Asked) held:closeStdin()",
        )
        .unwrap();
    assert_eq!(settled(1), 1, "Git did not start its credential helper");
    runtime.exec("held:close() held = nil").unwrap();
    assert_eq!(settled(0), 0, "closing the handle left the helper Git started running");

    // A bounded run. It blocks this thread, so another one watches for the helper meanwhile.
    let seen = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (seen, done) = (Arc::clone(&seen), Arc::clone(&done));
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                seen.fetch_max(helpers(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let started = Instant::now();
    let outcome = runtime.exec(
        "local result, message, kind = process.run('git', Arguments, { stdin = Asked, stdout = 'capture', \
           stderr = 'capture', env = Environment, timeoutMs = 1500, secret = { env = 'L3I_GIT_TOKEN_FD', data = Token } }) \
         assert(result == nil and kind == 'timeout', tostring(kind)) \
         assert(not string.find(tostring(message), 'SYNTHETIC', 1, true), 'the failure repeats the token')",
    );
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    watcher.join().unwrap();
    outcome.unwrap();
    // Wall time: the limit, and not the helper's forty-seven seconds, ended the call.
    assert!(elapsed >= Duration::from_millis(1400), "the call ended before its limit: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(15), "the limit did not end the call: {elapsed:?}");
    assert_eq!(seen.load(Ordering::Relaxed), 1, "the helper was never seen running during the bounded run");
    assert_eq!(settled(0), 0, "the limit left the helper Git started running");

    // Exactly the descriptors there were: nothing of either secret's pipe, or of either call.
    let until = Instant::now() + Duration::from_secs(2);
    while descriptors() != baseline && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(descriptors(), baseline, "descriptors were left behind");
}
