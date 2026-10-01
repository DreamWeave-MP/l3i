//! `@dream/process` through Luau: a child's exit code, its captured output and input, its
//! working directory and environment, and the capability it needs.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::process::{ENVIRONMENT_CAPABILITY, ProcessExtension, SPAWN_CAPABILITY};

fn runtime_with(granted: bool) -> Runtime {
    let mut policy = RuntimePolicy::new().compat_global("@dream/process", "process");
    if granted {
        policy = policy.capability(SPAWN_CAPABILITY).capability(ENVIRONMENT_CAPABILITY);
    }
    let plan = RuntimePlan::builder().policy(policy).extension(ProcessExtension).finalize().unwrap();
    Runtime::from_plan(&plan).unwrap()
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
             local ok, err = pcall(process.run, 'dream-process-no-such-program') \
             assert(not ok and err:find('dream.process.run: dream%-process%-no%-such%-program'), err) \
             ok, err = pcall(process.run, 'sh', { 1 }) assert(not ok and err:find('args%[1%]'), err) \
             ok, err = pcall(process.run, 'sh', {}, { stdout = 'file' }) assert(not ok and err:find('stdout'), err)",
        )
        .unwrap();
}

#[test]
fn running_and_the_environment_need_their_capabilities() {
    runtime_with(false)
        .exec(
            "local ok, err = pcall(process.run, 'sh', { '-c', 'exit 0' }) \
             assert(not ok and err:find(\"needs the 'process.spawn' capability\"), err) \
             ok, err = pcall(process.env, 'PATH') \
             assert(not ok and err:find(\"needs the 'process.environment' capability\"), err) \
             process.write('stdout', '') process.write('stderr', buffer.create(0)) \
             assert(type(process.isTerminal('stdout')) == 'boolean') \
             ok, err = pcall(process.write, 'stdin', 'x') assert(not ok and err:find('stream must be'), err)",
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
