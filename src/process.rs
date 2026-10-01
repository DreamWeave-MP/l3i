//! The process a script runs in, and the ones it starts: the `dream.process` extension, module
//! `@dream/process`.
//!
//! - `process.run`: start a program with arguments, wait for it, and get its exit code, with its
//!   output passed through, captured or discarded. It needs the `process.spawn` capability
//!   ([`SPAWN_CAPABILITY`]), which no plan grants unless the host asks for it.
//! - `process.env(name)`: an environment variable, with the `process.environment` capability
//!   ([`ENVIRONMENT_CAPABILITY`]).
//! - `process.write(stream, data)` and `process.isTerminal(stream)`: bytes to the host's standard
//!   output or error, unbuffered and without the newline `print` adds, and whether a stream is a
//!   terminal (to decide on colors). They need no capability: `print` already reaches stdout.
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

use crate::bind::{ArgView, Call, StackResults};
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

fn run(
    call: &Call<'_>,
    program: &[u8],
    args: Option<ValueView<'_>>,
    options: Option<ValueView<'_>>,
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
    if options.clear_env {
        command.env_clear();
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
        (Some(input), Some(mut pipe)) => Some(std::thread::spawn(move || std::io::Write::write_all(&mut pipe, &input))),
        _ => None,
    };
    let output = done!(match child.wait_with_output() {
        Ok(output) => Outcome::Done(output),
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
        d.module(MODULE)
            .doc("The script's process and the ones it starts: run a program and wait for its exit code, environment variables, and the standard streams.")
            .installed("run")
            .signature(RUN_SIGNATURE)
            .doc("Runs program with args (no shell), waits for it, and returns how it ended; needs the process.spawn capability. stdout and stderr are inherited unless captured or discarded; stdin is empty unless given.")
            .installed("env")
            .signature("(name: string) -> string?")
            .doc("The environment variable name, or nil when it is unset; needs the process.environment capability.")
            .function("write", write)
            .signature("(stream: \"stdout\" | \"stderr\", data: buffer | string) -> (boolean?, string?, dream_process_ErrorKind?)")
            .doc("Writes data to the host's standard output or error, without the newline print adds.")
            .function("isTerminal", is_terminal)
            .signature("(stream: \"stdout\" | \"stderr\") -> boolean")
            .doc("Whether the stream is a terminal.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let granted = cx.has_capability(SPAWN_CAPABILITY)?;
        let environment = cx.has_capability(ENVIRONMENT_CAPABILITY)?;
        let module = cx.module(MODULE)?;
        if environment {
            module.function("env", |name: &[u8]| -> Result<Option<Vec<u8>>> {
                Ok(std::env::var_os(os_string("a variable name", name)?).map(|value| value.as_encoded_bytes().to_vec()))
            })?;
        } else {
            module.function("env", |_: ArgView<'_>| -> Result<()> {
                Err(Error::permission(format!(
                    "dream.process.env: needs the '{ENVIRONMENT_CAPABILITY}' capability, which this runtime does not grant"
                )))
            })?;
        }
        if granted {
            module.function("run", run)?;
        } else {
            module.function("run", |_: ArgView<'_>| -> Result<()> {
                Err(Error::permission(format!(
                    "dream.process.run: needs the '{SPAWN_CAPABILITY}' capability, which this runtime does not grant"
                )))
            })?;
        }
        Ok(())
    }
}
