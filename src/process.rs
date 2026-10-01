//! Child processes for scripts: the `dream.process` extension, module `@dream/process`.
//!
//! One function, `process.run`: start a program with arguments, wait for it, and get its exit
//! code, with its output passed through, captured or discarded. It needs the `process.spawn`
//! capability ([`SPAWN_CAPABILITY`]), which no plan grants unless the host asks for it; without
//! it the function exists, typed, and raises a permission error.
//!
//! ```lua
//! local process = require('@dream/process')
//! local result = process.run('tes3cmd', { 'clean', 'Mod.esp' }, { cwd = 'Data Files' })
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
use crate::stack::{Scope, ValueView};

/// The extension id.
pub const EXTENSION_ID: &str = "dream.process";
/// The module path.
pub const MODULE: &str = "@dream/process";
/// The capability `process.run` needs.
pub const SPAWN_CAPABILITY: &str = "process.spawn";

const RUN_SIGNATURE: &str =
    "(program: string, args: { string }?, options: dream_process_RunOptions?) -> dream_process_Result";
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
) -> Result<StackResults> {
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
    let mut child = command.spawn().map_err(|error| Error::runtime(format!("dream.process.run: {name}: {error}")))?;
    // The input goes in on its own thread, so a child that fills its output pipe before it has
    // read all of its input cannot deadlock against this one.
    let feeder = match (options.stdin, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => Some(std::thread::spawn(move || std::io::Write::write_all(&mut pipe, &input))),
        _ => None,
    };
    let output =
        child.wait_with_output().map_err(|error| Error::runtime(format!("dream.process.run: {name}: {error}")))?;
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
    Ok(StackResults)
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
        d.optional_capability(SPAWN_CAPABILITY);
        d.module(MODULE)
            .doc("Child processes: run a program with arguments, wait for it, and get its exit code.")
            .installed("run")
            .signature(RUN_SIGNATURE)
            .doc("Runs program with args (no shell), waits for it, and returns how it ended; needs the process.spawn capability. stdout and stderr are inherited unless captured or discarded; stdin is empty unless given.");
        Ok(())
    }

    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let granted = cx.has_capability(SPAWN_CAPABILITY)?;
        let module = cx.module(MODULE)?;
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
