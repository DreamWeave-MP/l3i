//! The process a script runs in, and the ones it starts: the `dream.process` extension, module
//! `@dream/process`.
//!
//! - `process.run`: start a program with arguments, wait for it, and get its exit code, with its
//!   output passed through, captured or discarded. It needs the `process.spawn` capability
//!   ([`SPAWN_CAPABILITY`]), which no plan grants unless the host asks for it. The child inherits
//!   the host's environment only under `process.environment`; without it, it gets only the
//!   variables `env` names. The wait never outlasts the runtime's execution time limit: a child
//!   still running at the deadline is killed and reaped. `timeoutMs` bounds the child's run, and
//!   `maxStdoutBytes` and `maxStderrBytes` bound what a captured stream may produce; passing either
//!   kills the child (on Unix, with every process it started) and discards what it wrote.
//! - `process.env(name)`: an environment variable, with the `process.environment` capability
//!   ([`ENVIRONMENT_CAPABILITY`]).
//! - `process.knownFolder(name)`: the user's `home`, or on Windows the `documents` known folder,
//!   with the same capability: where they are says who the user is.
//! - `process.platform()`: the OS the host was built for; no capability.
//! - `process.write(stream, data)`: bytes to the host's standard output or error, unbuffered and
//!   without the newline `print` adds, with the `process.stdio` capability ([`STDIO_CAPABILITY`]):
//!   a sandboxed script's output goes where its host routes `print`, not to the host's streams.
//! - `process.isTerminal(stream)`: whether a stream is a terminal (to decide on colors); no
//!   capability.
//! - `process.sleep(seconds)`: blocks the script's thread, never past the runtime's execution time
//!   limit; no capability.
//!
//! A function whose capability the runtime lacks exists, typed, and raises a permission error.
//! A program the OS can't start, or a stream it can't write, is an answer, not an exception:
//! `nil`, the message and the error's kind (`dream_process_ErrorKind`), as `@dream/fs` answers.
//!
//! ```lua
//! local process = require('@dream/process')
//! local result, message = process.run('tes3cmd', { 'clean', 'Mod.esp' }, { cwd = 'Data Files' })
//! if not result then error(message) end
//! if not result.success then error(`tes3cmd exited with {result.code}`) end
//! ```
//!
//! The program is found the way the OS finds one (`PATH`), and the arguments go to it as they
//! are, with no shell in between: nothing is split, quoted or expanded.

pub mod input;

use std::borrow::Cow;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::bind::{Call, StackResults};
use crate::convert::{BytesView, FromView};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::options::Options;
use crate::outcome::{Failure, Outcome, done};
use crate::stack::{Scope, ValueView};

/// The extension id.
pub const EXTENSION_ID: &str = "dream.process";
/// The module path.
pub const MODULE: &str = "@dream/process";
/// The capability `process.run` needs.
pub const SPAWN_CAPABILITY: &str = "process.spawn";
/// The capability `process.env` needs.
pub const ENVIRONMENT_CAPABILITY: &str = "process.environment";
/// The capability `process.write` and `process.openStdin` need: the host's standard streams.
pub const STDIO_CAPABILITY: &str = "process.stdio";

const RUN_SIGNATURE: &str = "(program: string, args: { string }?, options: dream_process_RunOptions?) -> (dream_process_Result?, string?, dream_process_ErrorKind?)";
const OPTIONS_TYPE: &str = "{ cwd: string?, env: { [string]: string }?, clearEnv: boolean?, stdin: (buffer | string)?, \
    stdout: (\"inherit\" | \"capture\" | \"null\")?, stderr: (\"inherit\" | \"capture\" | \"null\")?, \
    timeoutMs: number?, maxStdoutBytes: number?, maxStderrBytes: number? }";
const RESULT_TYPE: &str = "{ success: boolean, code: number?, signal: number?, stdout: string?, stderr: string? }";
/// `dream_process_ErrorKind`: the OS kinds, and the two limits a bounded process answers with.
const PROCESS_ERROR_KIND_TYPE: &str = "\"notFound\" | \"permissionDenied\" | \"alreadyExists\" | \"isADirectory\" \
    | \"notADirectory\" | \"directoryNotEmpty\" | \"readOnlyFilesystem\" | \"storageFull\" | \"crossesDevices\" \
    | \"invalidInput\" | \"invalidFilename\" | \"unsupported\" | \"wouldBlock\" | \"timeout\" | \"outputLimit\" \
    | \"other\"";

/// Which entry point an options table belongs to: `run` takes stdin bytes and calls captured
/// output `capture`; `start` takes a stdin pipe and calls it `pipe`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Run,
}

impl Mode {
    fn context(self) -> &'static str {
        match self {
            Mode::Run => "dream.process.run",
        }
    }

    fn captured(self) -> &'static str {
        match self {
            Mode::Run => "capture",
        }
    }
}

/// Where a child's output goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Inherit,
    Capture,
    Null,
}

impl Output {
    fn parse(mode: Mode, key: &str, text: &str) -> Result<Output> {
        let captured = mode.captured();
        match text {
            "inherit" => Ok(Output::Inherit),
            "null" => Ok(Output::Null),
            other if other == captured => Ok(Output::Capture),
            other => Err(Error::runtime(format!(
                "{}: {key} must be 'inherit', '{captured}' or 'null', got '{other}'",
                mode.context()
            ))),
        }
    }

    fn stdio(self) -> Stdio {
        match self {
            Output::Inherit => Stdio::inherit(),
            Output::Capture => Stdio::piped(),
            Output::Null => Stdio::null(),
        }
    }
}

/// The limits a child runs under. A ceiling applies to a captured stream's bytes, and is only
/// set where that stream is captured.
#[derive(Default)]
struct Bounds {
    timeout: Option<Duration>,
    stdout: Option<u64>,
    stderr: Option<u64>,
}

impl Bounds {
    fn is_bounded(&self) -> bool {
        self.timeout.is_some() || self.stdout.is_some() || self.stderr.is_some()
    }
}

/// A whole-number option of at least `least`, or nothing when it is absent.
fn whole_number(context: &str, key: &str, value: Option<f64>, least: f64) -> Result<Option<u64>> {
    let Some(value) = value else { return Ok(None) };
    if !value.is_finite() || value.fract() != 0.0 || value < least {
        return Err(Error::runtime(format!(
            "{context}: {key} must be a finite whole number of at least {least}, got {value}"
        )));
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked whole and at least 0 above")]
    Ok(Some(value as u64))
}

struct SpawnOptions {
    cwd: Option<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    clear_env: bool,
    stdin: Option<Vec<u8>>,
    stdout: Output,
    stderr: Output,
    bounds: Bounds,
}

impl SpawnOptions {
    fn read(scope: &impl Scope, options: Option<ValueView<'_>>, mode: Mode) -> Result<SpawnOptions> {
        let mut spawn = SpawnOptions {
            cwd: None,
            env: Vec::new(),
            clear_env: false,
            stdin: None,
            stdout: Output::Inherit,
            stderr: Output::Inherit,
            bounds: Bounds::default(),
        };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(spawn);
        };
        let context = mode.context();
        Options::read(scope, options, context, |o| {
            spawn.cwd = o.optional_bytes("cwd", |bytes| Ok(bytes.to_vec()))?;
            spawn.clear_env = o.or("clearEnv", false)?;
            spawn.stdin = o.with_optional("stdin", |value| {
                let data = BytesView::from_view(value)?;
                // SAFETY: copied out at once, with no call into Lua while the slice lives.
                Ok(unsafe { data.bytes_unchecked() }.to_vec())
            })?;
            if let Some(stdout) = o.optional_str("stdout", |text| Output::parse(mode, "stdout", text))? {
                spawn.stdout = stdout;
            }
            if let Some(stderr) = o.optional_str("stderr", |text| Output::parse(mode, "stderr", text))? {
                spawn.stderr = stderr;
            }
            if let Some(env) = o.optional_table("env", |frame, table| {
                let mut env = Vec::new();
                table.for_each(frame, |_, key, value| {
                    let (Ok(key), Ok(value)) = (key.read::<&[u8]>(), value.read::<&[u8]>()) else {
                        return Err(Error::runtime("env maps names to strings"));
                    };
                    env.push((key.to_vec(), value.to_vec()));
                    Ok(())
                })?;
                Ok(env)
            })? {
                spawn.env = env;
            }
            let timeout = whole_number(context, "timeoutMs", o.optional("timeoutMs")?, 1.0)?;
            spawn.bounds = Bounds {
                timeout: timeout.map(Duration::from_millis),
                stdout: whole_number(context, "maxStdoutBytes", o.optional("maxStdoutBytes")?, 0.0)?,
                stderr: whole_number(context, "maxStderrBytes", o.optional("maxStderrBytes")?, 0.0)?,
            };
            for (key, ceiling, stream) in [
                ("maxStdoutBytes", spawn.bounds.stdout, spawn.stdout),
                ("maxStderrBytes", spawn.bounds.stderr, spawn.stderr),
            ] {
                if ceiling.is_some() && stream != Output::Capture {
                    return Err(Error::runtime(format!(
                        "{context}: {key} needs its stream set to '{}'",
                        mode.captured()
                    )));
                }
            }
            Ok(())
        })?;
        Ok(spawn)
    }
}

/// A Luau string as an OS string: its bytes on Unix, UTF-8 elsewhere.
// Infallible on Unix, where any bytes are a path; elsewhere a path must be UTF-8.
#[cfg_attr(unix, allow(clippy::unnecessary_wraps))]
fn os_string(what: &str, bytes: &[u8]) -> Result<std::ffi::OsString> {
    #[cfg(unix)]
    {
        let _ = what;
        use std::os::unix::ffi::OsStrExt;
        Ok(std::ffi::OsStr::from_bytes(bytes).to_owned())
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(std::ffi::OsString::from)
            .map_err(|_| Error::runtime(format!("dream.process.run: {what} is not UTF-8")))
    }
}

/// The arguments a script passed, as OS strings.
fn arguments(call: &Call<'_>, args: Option<ValueView<'_>>) -> Result<Vec<std::ffi::OsString>> {
    let mut arguments = Vec::new();
    if let Some(args) = args.filter(|view| !view.is_nil()) {
        if !args.is_table() {
            return Err(Error::runtime("dream.process.run: args must be an array of strings"));
        }
        call.with_frame(|frame| {
            let table = frame.at(args.index()).as_table()?;
            table.for_each_array(frame, |_, index, value| {
                let argument = value
                    .read::<&[u8]>()
                    .map_err(|_| Error::runtime(format!("dream.process.run: args[{index}] must be a string")))?;
                arguments.push(os_string("an argument", argument)?);
                Ok(())
            })
        })?;
    }
    Ok(arguments)
}

/// Starts the child as the leader of a process group of its own, so a limit can kill everything
/// it started too. Only a bounded run asks for this: an unbounded one keeps the terminal's own
/// signals, which a group of its own would not receive.
#[cfg(unix)]
fn isolate(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate(_: &mut Command) {}

/// Kills the process group `child` leads, which [`isolate`] made of it. The group may already be
/// gone, which the kill answers with ESRCH and nothing else.
#[cfg(unix)]
fn kill_group(child: &Child) {
    if let Ok(group) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `-group` names the process group the child leads (see `isolate`); kill only
        // sends a signal and touches no memory of this process.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn kill_group(_: &Child) {}

/// Kills the child, and with `group` every process in its group, without reaping it.
fn kill_tree(child: &mut Child, group: bool) {
    if group {
        kill_group(child);
    }
    let _ = child.kill();
}

/// Kills and reaps `child`, so nothing is left running behind the script; it may have exited on
/// its own since it was last asked.
fn stop(child: &mut Child, group: bool) {
    kill_tree(child, group);
    let _ = child.wait();
}

fn build_command(
    program: &[u8],
    arguments: &[std::ffi::OsString],
    spawn: &SpawnOptions,
    inherit_environment: bool,
    group: bool,
) -> Result<Command> {
    let mut command = Command::new(os_string("the program", program)?);
    command.args(arguments);
    if let Some(cwd) = &spawn.cwd {
        command.current_dir(os_string("cwd", cwd)?);
    }
    if spawn.clear_env || !inherit_environment {
        command.env_clear();
    }
    if !spawn.clear_env
        && !inherit_environment
        && let Some(path) = std::env::var_os("PATH")
    {
        command.env("PATH", path);
    }
    for (key, value) in &spawn.env {
        command.env(os_string("an env name", key)?, os_string("an env value", value)?);
    }
    if group {
        isolate(&mut command);
    }
    Ok(command)
}

/// `process.run`. Without the environment capability the child starts with the host's `PATH` and
/// what `env` names, nothing else: inheriting the host's environment would hand a script every
/// variable `process.env` refuses it, and without `PATH` neither the program nor anything it starts
/// is found where the host would find it. `clearEnv` clears `PATH` too.
fn run(
    call: &Call<'_>,
    program: &[u8],
    args: Option<ValueView<'_>>,
    options: Option<ValueView<'_>>,
    inherit_environment: bool,
) -> Result<Outcome<StackResults>> {
    let arguments = arguments(call, args)?;
    let spawn = SpawnOptions::read(call, options, Mode::Run)?;
    let name = String::from_utf8_lossy(program).into_owned();
    let group = spawn.bounds.is_bounded();
    let mut command = build_command(program, &arguments, &spawn, inherit_environment, group)?;
    let stdin = if spawn.stdin.is_some() { Stdio::piped() } else { Stdio::null() };
    command.stdin(stdin).stdout(spawn.stdout.stdio()).stderr(spawn.stderr.stdio());
    let SpawnOptions { stdin, stdout, stderr, bounds, .. } = spawn;
    let failed = |error: std::io::Error| Failure::message(format!("dream.process.run: {name}: {error}"), &error);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(Outcome::Failed(failed(error))),
    };
    // The input goes in on its own thread, so a child that fills its output pipe before it has
    // read all of its input cannot deadlock against this one.
    let feeder = match (stdin, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => Some(std::thread::spawn(move || {
            #[cfg(unix)]
            block_sigpipe();
            std::io::Write::write_all(&mut pipe, &input)
        })),
        _ => None,
    };
    let exceeded = Arc::new(AtomicBool::new(false));
    let capture = Capture {
        stdout: drain(child.stdout.take(), bounds.stdout, Arc::clone(&exceeded)),
        stderr: drain(child.stderr.take(), bounds.stderr, Arc::clone(&exceeded)),
        exceeded,
    };
    let output = done!(match wait(call, &mut child, &bounds, capture, group) {
        Ok(Ended::Exited(output)) => Outcome::Done(output),
        Ok(Ended::TimedOut) => Outcome::Failed(Failure {
            message: Cow::Owned(format!("dream.process.run: {name}: stopped at its timeoutMs limit")),
            kind: "timeout",
        }),
        Ok(Ended::OutputLimit) => Outcome::Failed(Failure {
            message: Cow::Owned(format!("dream.process.run: {name}: captured output passed its byte limit")),
            kind: "outputLimit",
        }),
        Ok(Ended::Expired) => {
            let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "stopped at the execution time limit");
            Outcome::Failed(failed(error))
        }
        Err(error) => Outcome::Failed(failed(error)),
    });
    // The input thread is left to finish on its own: a child that exits without reading all of
    // its input closes the pipe, which is its business, and one it started may hold the pipe
    // open past the time limit, which joining would wait out.
    drop(feeder);
    let status = output.status;
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut frame = call.frame();
    let table = frame.push_table(0, 5)?;
    table.raw_set_value(&frame, "success", &status.success())?;
    table.raw_set_value(&frame, "code", &status.code().map(f64::from))?;
    table.raw_set_value(&frame, "signal", &signal.map(f64::from))?;
    if stdout == Output::Capture {
        table.raw_set_value(&frame, "stdout", output.stdout.as_slice())?;
    }
    if stderr == Output::Capture {
        table.raw_set_value(&frame, "stderr", output.stderr.as_slice())?;
    }
    frame.release();
    Ok(Outcome::Done(StackResults))
}

/// How a child's wait ended.
enum Ended {
    /// The child exited on its own; here is what it wrote.
    Exited(std::process::Output),
    /// `timeoutMs` passed first; the child was killed and reaped.
    TimedOut,
    /// A captured stream passed its ceiling first; the child was killed and reaped.
    OutputLimit,
    /// The runtime's execution time limit passed first; the child was killed and reaped.
    Expired,
}

/// The readers of a child's captured streams, and whether one of them passed its ceiling.
struct Capture {
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    exceeded: Arc<AtomicBool>,
}

/// Reads `pipe` to its end on its own thread. Past `ceiling` bytes the reader stops, keeps nothing
/// more, and raises `exceeded`, so memory stays within the ceiling however much the child writes.
fn drain(
    pipe: Option<impl Read + Send + 'static>,
    ceiling: Option<u64>,
    exceeded: Arc<AtomicBool>,
) -> Option<JoinHandle<Vec<u8>>> {
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 8192];
            let mut total = 0_u64;
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        total = total.saturating_add(count as u64);
                        if ceiling.is_some_and(|ceiling| total > ceiling) {
                            exceeded.store(true, Ordering::SeqCst);
                            break;
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    // A read error ends the capture where it stopped, as wait_with_output would
                    // surface; the exit status still says how the child ended.
                    Err(_) => break,
                }
            }
            bytes
        })
    })
}

/// Waits for `child` and collects what it was asked to capture, as `wait_with_output` does, but
/// never past the limits: the child's own `timeoutMs` and the runtime's execution time limit,
/// whichever comes first. At the deadline the child is killed and reaped; the runtime's limit also
/// marks the call expired, so the watchdog stops the script at its next safepoint. A captured
/// stream passing its ceiling kills the child at once. A capture still open after the child exited
/// can be held by a process it started; the deadline or a ceiling ends that too, and with
/// `group` the kill reaches it.
fn wait(call: &Call<'_>, child: &mut Child, bounds: &Bounds, capture: Capture, group: bool) -> std::io::Result<Ended> {
    let started = Instant::now();
    // The deadline that comes first, and whether the child's own limit is the one it is.
    let runtime = crate::runtime::watchdog_remaining(call).map(|remaining| (started + remaining, false));
    let own = bounds.timeout.map(|timeout| (started + timeout, true));
    let limit = match (runtime, own) {
        (Some(runtime), Some(own)) => Some(if own.0 < runtime.0 { own } else { runtime }),
        (runtime, own) => runtime.or(own),
    };
    let Capture { stdout, stderr, exceeded } = capture;
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if exceeded.load(Ordering::SeqCst) {
            stop(child, group);
            return Ok(Ended::OutputLimit);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                stop(child, group);
                return Err(error);
            }
        }
        let now = Instant::now();
        if let Some((at, by_timeout)) = limit
            && now >= at
        {
            stop(child, group);
            if by_timeout {
                return Ok(Ended::TimedOut);
            }
            crate::runtime::expire_if_due(call);
            return Ok(Ended::Expired);
        }
        let nap = limit.map_or(pause, |(at, _)| pause.min(at - now));
        std::thread::sleep(nap);
        pause = (pause * 2).min(Duration::from_millis(20));
    };
    let readers = [&stdout, &stderr];
    loop {
        if exceeded.load(Ordering::SeqCst) {
            kill_tree(child, group);
            return Ok(Ended::OutputLimit);
        }
        if readers.iter().all(|reader| reader.as_ref().is_none_or(JoinHandle::is_finished)) {
            break;
        }
        let now = Instant::now();
        if let Some((at, by_timeout)) = limit
            && now >= at
        {
            kill_tree(child, group);
            if by_timeout {
                return Ok(Ended::TimedOut);
            }
            crate::runtime::expire_if_due(call);
            return Ok(Ended::Expired);
        }
        let nap = limit.map_or(pause, |(at, _)| pause.min(at - now));
        std::thread::sleep(nap);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
    // A reader can raise the flag between the last check and its own exit.
    if exceeded.load(Ordering::SeqCst) {
        return Ok(Ended::OutputLimit);
    }
    let collect = |reader: Option<JoinHandle<Vec<u8>>>| {
        reader.map(|reader| reader.join().unwrap_or_default()).unwrap_or_default()
    };
    Ok(Ended::Exited(std::process::Output { status, stdout: collect(stdout), stderr: collect(stderr) }))
}

/// Blocks SIGPIPE on the calling thread: a child that exits without reading all of its input
/// then fails the write with EPIPE instead of raising a signal that would kill a host that has
/// not ignored it. A signal raised anyway stays pending on this thread and ends with it.
#[cfg(unix)]
fn block_sigpipe() {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set before sigaddset and pthread_sigmask read it, and
    // the mask changed is this thread's own; the thread only writes the child's input.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut());
    }
}

/// `stdout` or `stderr`.
fn stream(what: &str, name: &str) -> Result<bool> {
    match name {
        "stdout" => Ok(false),
        "stderr" => Ok(true),
        other => {
            Err(Error::runtime(format!("dream.process.{what}: stream must be 'stdout' or 'stderr', got '{other}'")))
        }
    }
}

/// `process.write(stream, data)`: every byte, unbuffered; a reader that went away (a closed
/// pipe) is not an error, so `| head` ends the output quietly.
fn write(name: &str, data: BytesView<'_>) -> Result<Outcome<bool>> {
    use std::io::Write as _;
    // SAFETY: the bytes go straight to the stream, with no call into Lua while the slice lives.
    let bytes = unsafe { data.bytes_unchecked() };
    let result = if stream("write", name)? {
        std::io::stderr().lock().write_all(bytes)
    } else {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes).and_then(|()| out.flush())
    };
    Ok(match result {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            Outcome::Failed(Failure::message(format!("dream.process.write: {name}: {error}"), &error))
        }
        _ => Outcome::Done(true),
    })
}

/// `process.sleep(seconds)`: blocks the script's thread for `seconds`, never past the runtime's
/// execution time limit: a sleep the deadline cuts short marks the call expired, so the watchdog
/// stops the script as soon as it runs again.
fn sleep(call: &Call<'_>, seconds: f64) -> Result<()> {
    if !(seconds.is_finite() && seconds >= 0.0) {
        return Err(Error::runtime(format!(
            "dream.process.sleep: seconds must be a finite number of at least 0, got {seconds}"
        )));
    }
    // Above `Duration::MAX` a sleep is forever in every sense that matters.
    let mut duration = std::time::Duration::try_from_secs_f64(seconds).unwrap_or(std::time::Duration::MAX);
    let remaining = crate::runtime::watchdog_remaining(call);
    let cut = remaining.is_some_and(|remaining| remaining < duration);
    if let Some(remaining) = remaining {
        duration = duration.min(remaining);
    }
    std::thread::sleep(duration);
    if cut {
        crate::runtime::expire_if_due(call);
    }
    Ok(())
}

fn is_terminal(name: &str) -> Result<bool> {
    use std::io::IsTerminal as _;
    Ok(if stream("isTerminal", name)? { std::io::stderr().is_terminal() } else { std::io::stdout().is_terminal() })
}

/// `process.platform()`: Rust's name for the OS the host was built for (`std::env::consts::OS`):
/// `windows`, `macos`, `linux`, `android` and so on. A function rather than a compile constant,
/// so bytecode compiled on one platform never carries another's answer.
fn platform() -> &'static str {
    std::env::consts::OS
}

fn non_empty_env_path(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// The user's home folder as the platform's own tools find it: `HOME` on Unix; on Windows
/// `USERPROFILE`, then `HOMEDRIVE` with `HOMEPATH`, then `HOME`.
fn home_folder() -> Option<std::ffi::OsString> {
    #[cfg(windows)]
    {
        if let Some(path) = non_empty_env_path("USERPROFILE") {
            return Some(path);
        }
        if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            let mut merged = drive;
            merged.push(path);
            if !merged.is_empty() {
                return Some(merged);
            }
        }
    }
    non_empty_env_path("HOME")
}

/// The Documents known folder, which Windows lets a user move (and OneDrive moves for them), so
/// only `SHGetKnownFolderPath` knows where it is. Declared here rather than taken from a bindings
/// crate: it is the one call l3i needs.
#[cfg(windows)]
fn documents_folder() -> Option<std::ffi::OsString> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt as _;

    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: *mut c_void, path: *mut *mut u16) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(memory: *mut c_void);
    }

    // FOLDERID_Documents, {FDD39AD0-238F-46AF-ADB4-6C85480369C7}.
    let documents = Guid {
        data1: 0xFDD3_9AD0,
        data2: 0x238F,
        data3: 0x46AF,
        data4: [0xAD, 0xB4, 0x6C, 0x85, 0x48, 0x03, 0x69, 0xC7],
    };
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `documents` outlives the call, a null token means the current user, and `path` is
    // a valid place for the out pointer.
    let status = unsafe { SHGetKnownFolderPath(&raw const documents, 0, std::ptr::null_mut(), &raw mut path) };
    let folder = (status == 0 && !path.is_null()).then(|| {
        let mut length = 0_usize;
        // SAFETY: on success `path` is a null-terminated UTF-16 string.
        while unsafe { *path.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `path` holds `length` code units before its terminator.
        std::ffi::OsString::from_wide(unsafe { std::slice::from_raw_parts(path, length) })
    });
    // SAFETY: the caller frees what SHGetKnownFolderPath allocated, whether it succeeded or not,
    // and CoTaskMemFree accepts null.
    unsafe { CoTaskMemFree(path.cast()) };
    folder.filter(|folder| !folder.is_empty())
}

/// Windows is the one platform whose Documents folder only the OS can name.
#[cfg(not(windows))]
fn documents_folder() -> Option<std::ffi::OsString> {
    None
}

/// `process.knownFolder(name)`: a folder the OS defines for the user, or nil when it can't name
/// one. What an application makes of it (`.config`, `Library/Preferences`, `My Games`) is the
/// application's to decide.
fn known_folder(name: &str) -> Result<Option<Vec<u8>>> {
    let folder = match name {
        "home" => home_folder(),
        "documents" => documents_folder(),
        other => {
            return Err(Error::runtime(format!(
                "dream.process.knownFolder: name must be 'home' or 'documents', got '{other}'"
            )));
        }
    };
    Ok(folder.map(|folder| folder.as_encoded_bytes().to_vec()))
}

/// The `dream.process` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessExtension;

impl Extension for ProcessExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        input::describe(d);
        d.type_alias("dream_process_RunOptions", OPTIONS_TYPE);
        d.type_alias("dream_process_Result", RESULT_TYPE);
        d.type_alias("dream_process_ErrorKind", PROCESS_ERROR_KIND_TYPE);
        d.optional_capability(SPAWN_CAPABILITY);
        d.optional_capability(ENVIRONMENT_CAPABILITY);
        d.optional_capability(STDIO_CAPABILITY);
        d.module(MODULE)
            .doc("The script's process and the ones it starts: run a program and wait for its exit code, environment variables, and the standard streams.")
            .installed("run")
            .signature(RUN_SIGNATURE)
            .doc("Runs program with args (no shell), waits for it, and returns how it ended; needs the process.spawn capability. stdout and stderr are inherited unless captured or discarded; stdin is empty unless given. timeoutMs (at least 1) stops the child after that many milliseconds, and maxStdoutBytes or maxStderrBytes (at least 0, each for a captured stream) stops it once that stream passes its ceiling: the child is killed and reaped, its output discarded, and the failure is timeout or outputLimit. On Unix a bounded child runs in a process group of its own, which a limit kills whole; elsewhere only the child is killed.")
            .installed("env")
            .signature("(name: string) -> string?")
            .doc("The environment variable name, or nil when it is unset; needs the process.environment capability.")
            .installed("knownFolder")
            .signature("(name: \"home\" | \"documents\") -> string?")
            .doc("A folder the OS defines for the user: home, or documents (the Windows Documents known folder, wherever the user moved it; nil elsewhere). Nil when the OS can't name it; needs the process.environment capability.")
            .function("platform", platform)
            .signature("() -> string")
            .doc("The OS the host was built for, as Rust names it: windows, macos, linux, android, and so on.")
            .installed("openStdin")
            .signature("() -> (dream_process_Stdin?, string?, dream_process_ErrorKind?)")
            .doc("Claims the process stdin stream exclusively; needs process.stdio. Returns wouldBlock if another runtime, worker or handle owns it. Read methods are nonblocking; wait is explicit. Close releases ownership without waiting for the OS reader. Host code must not concurrently read stdin outside this API.")
            .installed("write")
            .signature("(stream: \"stdout\" | \"stderr\", data: buffer | string) -> (boolean?, string?, dream_process_ErrorKind?)")
            .doc("Writes data to the host's standard output or error, without the newline print adds; needs the process.stdio capability.")
            .function("isTerminal", is_terminal)
            .signature("(stream: \"stdout\" | \"stderr\") -> boolean")
            .doc("Whether the stream is a terminal.")
            .function("sleep", sleep)
            .signature("(seconds: number) -> ()")
            .doc("Blocks the script for seconds, never past the runtime's execution time limit.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        install(cx, None)
    }
}

impl ProcessExtension {
    /// This extension with a host's [`input::Policy`] checked at every stdin operation, on top of
    /// the `process.stdio` capability gate.
    #[must_use]
    pub fn with_input_policy(self, policy: std::rc::Rc<dyn input::Policy>) -> impl Extension {
        InputProcessExtension { policy }
    }
}

struct InputProcessExtension {
    policy: std::rc::Rc<dyn input::Policy>,
}

impl Extension for InputProcessExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }
    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        ProcessExtension.describe(d)
    }
    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        install(cx, Some(std::rc::Rc::clone(&self.policy)))
    }
}

fn install(cx: &mut InstallContext<'_>, input_policy: Option<std::rc::Rc<dyn input::Policy>>) -> Result<()> {
    let granted = cx.has_capability(SPAWN_CAPABILITY)?;
    let environment = cx.has_capability(ENVIRONMENT_CAPABILITY)?;
    let stdio = cx.has_capability(STDIO_CAPABILITY)?;
    let module = cx.module(MODULE)?;
    module.gated("env", ENVIRONMENT_CAPABILITY, environment, |name: &[u8]| -> Result<Option<Vec<u8>>> {
        Ok(std::env::var_os(os_string("a variable name", name)?).map(|value| value.as_encoded_bytes().to_vec()))
    })?;
    module.gated("knownFolder", ENVIRONMENT_CAPABILITY, environment, known_folder)?;
    module.gated("write", STDIO_CAPABILITY, stdio, write)?;
    module
        .gated("openStdin", STDIO_CAPABILITY, stdio, move |call: &Call<'_>| input::open(call, input_policy.as_ref()))?;
    module.gated(
        "run",
        SPAWN_CAPABILITY,
        granted,
        move |call: &Call<'_>, program: &[u8], args: Option<ValueView<'_>>, options: Option<ValueView<'_>>| {
            run(call, program, args, options, environment)
        },
    )?;
    Ok(())
}
