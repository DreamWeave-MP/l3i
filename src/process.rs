//! The process a script runs in, and the ones it starts: the `dream.process` extension, module
//! `@dream/process`.
//!
//! - `process.run`: start a program with arguments, wait for it, and get its exit code, with its
//!   output passed through, captured or discarded. It needs the `process.spawn` capability
//!   ([`SPAWN_CAPABILITY`]), which no plan grants unless the host asks for it. The child inherits
//!   the host's environment only under `process.environment`; without it, it gets only the
//!   variables `env` names. The wait never outlasts the runtime's execution time limit: a child
//!   still running at the deadline is killed and reaped.
//! - `process.env(name)`: an environment variable, with the `process.environment` capability
//!   ([`ENVIRONMENT_CAPABILITY`]).
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

use std::process::{Command, Stdio};

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
/// The capability `process.write` needs: the host's standard streams.
pub const STDIO_CAPABILITY: &str = "process.stdio";

const RUN_SIGNATURE: &str = "(program: string, args: { string }?, options: dream_process_RunOptions?) -> (dream_process_Result?, string?, dream_process_ErrorKind?)";
const OPTIONS_TYPE: &str = "{ cwd: string?, env: { [string]: string }?, clearEnv: boolean?, stdin: (buffer | string)?, \
    stdout: (\"inherit\" | \"capture\" | \"null\")?, stderr: (\"inherit\" | \"capture\" | \"null\")? }";
const RESULT_TYPE: &str = "{ success: boolean, code: number?, signal: number?, stdout: string?, stderr: string? }";

/// Where a child's output goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Inherit,
    Capture,
    Null,
}

impl Output {
    fn parse(key: &str, text: &str) -> Result<Output> {
        match text {
            "inherit" => Ok(Output::Inherit),
            "capture" => Ok(Output::Capture),
            "null" => Ok(Output::Null),
            other => Err(Error::runtime(format!(
                "dream.process.run: {key} must be 'inherit', 'capture' or 'null', got '{other}'"
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

struct RunOptions {
    cwd: Option<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    clear_env: bool,
    stdin: Option<Vec<u8>>,
    stdout: Output,
    stderr: Output,
}

impl RunOptions {
    fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<RunOptions> {
        let mut run = RunOptions {
            cwd: None,
            env: Vec::new(),
            clear_env: false,
            stdin: None,
            stdout: Output::Inherit,
            stderr: Output::Inherit,
        };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(run);
        };
        Options::read(scope, options, "dream.process.run", |o| {
            run.cwd = o.optional_bytes("cwd", |bytes| Ok(bytes.to_vec()))?;
            run.clear_env = o.or("clearEnv", false)?;
            run.stdin = o.with_optional("stdin", |value| {
                let data = BytesView::from_view(value)?;
                // SAFETY: copied out at once, with no call into Lua while the slice lives.
                Ok(unsafe { data.bytes_unchecked() }.to_vec())
            })?;
            if let Some(stdout) = o.optional_str("stdout", |text| Output::parse("stdout", text))? {
                run.stdout = stdout;
            }
            if let Some(stderr) = o.optional_str("stderr", |text| Output::parse("stderr", text))? {
                run.stderr = stderr;
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
                run.env = env;
            }
            Ok(())
        })?;
        Ok(run)
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
    let options = RunOptions::read(call, options)?;
    let name = String::from_utf8_lossy(program).into_owned();
    let mut command = Command::new(os_string("the program", program)?);
    command.args(&arguments);
    if let Some(cwd) = &options.cwd {
        command.current_dir(os_string("cwd", cwd)?);
    }
    if options.clear_env || !inherit_environment {
        command.env_clear();
    }
    if !options.clear_env
        && !inherit_environment
        && let Some(path) = std::env::var_os("PATH")
    {
        command.env("PATH", path);
    }
    for (key, value) in &options.env {
        command.env(os_string("an env name", key)?, os_string("an env value", value)?);
    }
    command
        .stdin(if options.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(options.stdout.stdio())
        .stderr(options.stderr.stdio());
    let failed = |error: std::io::Error| Failure::message(format!("dream.process.run: {name}: {error}"), &error);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(Outcome::Failed(failed(error))),
    };
    // The input goes in on its own thread, so a child that fills its output pipe before it has
    // read all of its input cannot deadlock against this one.
    let feeder = match (options.stdin, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => Some(std::thread::spawn(move || {
            #[cfg(unix)]
            block_sigpipe();
            std::io::Write::write_all(&mut pipe, &input)
        })),
        _ => None,
    };
    let output = done!(match wait(call, &mut child) {
        Ok(Some(output)) => Outcome::Done(output),
        Ok(None) => {
            let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "stopped at the execution time limit");
            Outcome::Failed(failed(error))
        }
        Err(error) => Outcome::Failed(failed(error)),
    });
    if let Some(feeder) = feeder {
        // A child that exits without reading all of its input closes the pipe; that is its
        // business, not an error of the run.
        let _ = feeder.join();
    }
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
    if options.stdout == Output::Capture {
        table.raw_set_value(&frame, "stdout", output.stdout.as_slice())?;
    }
    if options.stderr == Output::Capture {
        table.raw_set_value(&frame, "stderr", output.stderr.as_slice())?;
    }
    frame.release();
    Ok(Outcome::Done(StackResults))
}

/// Waits for `child` and collects what it was asked to capture, as `wait_with_output` does, but
/// never past the runtime's execution time limit: a child still running at the deadline is
/// killed and reaped, the call is marked expired so the watchdog stops the script at its next
/// safepoint, and the answer is `None`.
fn wait(call: &Call<'_>, child: &mut std::process::Child) -> std::io::Result<Option<std::process::Output>> {
    fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> Option<std::thread::JoinHandle<Vec<u8>>> {
        pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                // A read error ends the capture where it stopped, as wait_with_output would
                // surface; the exit status still says how the child ended.
                let _ = pipe.read_to_end(&mut bytes);
                bytes
            })
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let collect = |reader: Option<std::thread::JoinHandle<Vec<u8>>>| {
        reader.map(|reader| reader.join().unwrap_or_default()).unwrap_or_default()
    };
    let status = match crate::runtime::watchdog_remaining(call) {
        None => child.wait()?,
        Some(remaining) => {
            let deadline = std::time::Instant::now() + remaining;
            let mut pause = std::time::Duration::from_millis(1);
            loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    // The child may have exited between the check and the kill; either way it
                    // is reaped here, so nothing is left running behind the script.
                    let _ = child.kill();
                    let _ = child.wait();
                    collect(stdout);
                    collect(stderr);
                    crate::runtime::expire_if_due(call);
                    return Ok(None);
                }
                std::thread::sleep(pause.min(deadline - now));
                pause = (pause * 2).min(std::time::Duration::from_millis(20));
            }
        }
    };
    Ok(Some(std::process::Output { status, stdout: collect(stdout), stderr: collect(stderr) }))
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

/// The `dream.process` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessExtension;

impl Extension for ProcessExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_process_RunOptions", OPTIONS_TYPE);
        d.type_alias("dream_process_Result", RESULT_TYPE);
        d.type_alias("dream_process_ErrorKind", crate::outcome::ERROR_KIND_TYPE);
        d.optional_capability(SPAWN_CAPABILITY);
        d.optional_capability(ENVIRONMENT_CAPABILITY);
        d.optional_capability(STDIO_CAPABILITY);
        d.module(MODULE)
            .doc("The script's process and the ones it starts: run a program and wait for its exit code, environment variables, and the standard streams.")
            .installed("run")
            .signature(RUN_SIGNATURE)
            .doc("Runs program with args (no shell), waits for it, and returns how it ended; needs the process.spawn capability. stdout and stderr are inherited unless captured or discarded; stdin is empty unless given.")
            .installed("env")
            .signature("(name: string) -> string?")
            .doc("The environment variable name, or nil when it is unset; needs the process.environment capability.")
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
        let granted = cx.has_capability(SPAWN_CAPABILITY)?;
        let environment = cx.has_capability(ENVIRONMENT_CAPABILITY)?;
        let stdio = cx.has_capability(STDIO_CAPABILITY)?;
        let module = cx.module(MODULE)?;
        module.gated("env", ENVIRONMENT_CAPABILITY, environment, |name: &[u8]| -> Result<Option<Vec<u8>>> {
            Ok(std::env::var_os(os_string("a variable name", name)?).map(|value| value.as_encoded_bytes().to_vec()))
        })?;
        module.gated("write", STDIO_CAPABILITY, stdio, write)?;
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
}
