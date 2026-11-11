//! What `@dream/process` leaves behind when a process a child started leaves the child's process
//! group and keeps a pipe: nothing. One test, in a test binary of its own, because it counts this
//! process's threads and descriptors and nothing else may be starting any meanwhile.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::process::{ProcessExtension, SPAWN_CAPABILITY};

/// The sleep every stray process of this test runs, so that all of them can be found and ended.
const STRAY: &str = "31.4159";

fn count(directory: &str) -> usize {
    std::fs::read_dir(directory).map_or(0, Iterator::count)
}

/// This process's threads and open descriptors.
fn footprint() -> (usize, usize) {
    (count("/proc/self/task"), count("/proc/self/fd"))
}

/// Waits for the footprint to come back to `baseline`, and answers what it is when it has, or when
/// two seconds have passed.
fn settled(baseline: (usize, usize)) -> (usize, usize) {
    let started = Instant::now();
    loop {
        let now = footprint();
        if now == baseline || started.elapsed() > Duration::from_secs(2) {
            return now;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Ends the strays when the test does, however it ends.
struct Strays;

impl Drop for Strays {
    fn drop(&mut self) {
        // Anchored to the whole command line, so that it ends the strays and nothing that merely
        // mentions them, such as the shell that ran this test.
        let pattern = format!("^sleep {}$", STRAY.replace('.', "\\."));
        let _ = std::process::Command::new("pkill").args(["-9", "-f", &pattern]).status();
    }
}

#[test]
fn a_descendant_that_left_the_group_holding_a_pipe_costs_no_thread_and_no_descriptor() {
    if std::process::Command::new("setsid").arg("true").status().map_or(true, |status| !status.success()) {
        eprintln!("skipped: no setsid to make a process leave its group");
        return;
    }
    let _strays = Strays;
    let policy = RuntimePolicy::new().compat_global("@dream/process", "process").capability(SPAWN_CAPABILITY);
    let plan = RuntimePlan::builder().policy(policy).extension(ProcessExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    // Whatever the first child makes this process allocate for good is in the baseline.
    runtime.exec("assert(process.run('true', {}, { stdout = 'capture', timeoutMs = 5000 }).success)").unwrap();
    runtime
        .exec("local child = assert(process.start('true', {}, { stdout = 'pipe' })); child:wait(5000); child:close()")
        .unwrap();
    // A closed handle's threads are cancelled, not joined: the baseline is taken once they are gone.
    let mut baseline = footprint();
    let mut unchanged = 0;
    while unchanged < 10 {
        std::thread::sleep(Duration::from_millis(10));
        let now = footprint();
        unchanged = if now == baseline { unchanged + 1 } else { 0 };
        baseline = now;
    }

    // A bounded run whose child exits at once, leaving a stray in a session of its own on the
    // captured stdout. The run stops waiting at its limit, and takes its reader with it.
    runtime
        .exec(&format!(
            r"for _ = 1, 12 do
                  local result, _, kind = process.run('sh', {{ '-c', 'setsid sleep {STRAY} & exit 0' }},
                      {{ stdout = 'capture', stderr = 'capture', timeoutMs = 100 }})
                  assert(result == nil and kind == 'timeout', tostring(kind))
              end"
        ))
        .unwrap();
    assert_eq!(settled(baseline), baseline, "bounded runs left threads or descriptors behind (threads, descriptors)");

    // Input the stray holds and never reads: the child exits, nothing is captured, so the run
    // answers at once, with its feeder still blocked on a full pipe. The feeder goes with the run.
    runtime
        .exec(&format!(
            r"for _ = 1, 6 do
                  local result = assert(process.run('sh', {{ '-c', 'setsid sleep {STRAY} <&0 & exit 0' }},
                      {{ stdin = string.rep('x', 4 * 1024 * 1024), timeoutMs = 5000 }}))
                  assert(result.success)
              end"
        ))
        .unwrap();
    assert_eq!(settled(baseline), baseline, "runs with unread input left threads or descriptors behind");

    // Started children: both captures and the stdin queue held by the stray, then closed.
    runtime
        .exec(&format!(
            r"for _ = 1, 12 do
                  local child = assert(process.start('sh', {{ '-c', 'setsid sleep {STRAY} <&0 & exit 0' }},
                      {{ stdin = 'pipe', stdout = 'pipe', stderr = 'pipe' }}))
                  assert(assert(child:wait(5000)).success)
                  assert(child:write(string.rep('x', 1024 * 1024)))
                  process.sleep(0.02)
                  child:close()
              end"
        ))
        .unwrap();
    assert_eq!(settled(baseline), baseline, "closed handles left threads or descriptors behind");

    // And a handle whose child a limit stopped, which nobody closed until the runtime went.
    runtime
        .exec(&format!(
            r"kept = {{}}
              for index = 1, 6 do
                  kept[index] = assert(process.start('sh', {{ '-c', 'setsid sleep {STRAY} & sleep 30' }},
                      {{ stdout = 'pipe', timeoutMs = 50 }}))
              end
              process.sleep(0.3)"
        ))
        .unwrap();
    // The limit ended each handle's threads; what is left open is the handle itself.
    assert_eq!(settled((baseline.0, usize::MAX)).0, baseline.0, "handles stopped at their limit kept their threads");
    drop(runtime);
    let after = settled((1, baseline.1));
    assert!(
        after.1 <= baseline.1,
        "the runtime's shutdown left descriptors behind: {} against {}",
        after.1,
        baseline.1
    );
}
