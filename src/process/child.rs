//! The child a `process.start` handle owns: its stdout and stderr read by background threads into
//! bounded buffers, its stdin written by a background thread from a bounded queue, and a timer
//! that kills it at `timeoutMs`. The script polls with non-blocking calls; only `wait` with a
//! timeout waits, and never past the runtime's execution time limit.
//!
//! This plumbing is meant to move onto the common I/O readiness substrate of RFC #11 later. RFC #11
//! is not implemented here, and nothing above this file depends on threads being how it is done.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::bind::{Call, StackResults};
use crate::convert::BytesView;
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::outcome::{Failure, Outcome};
use crate::runtime::shared::Resource;
use crate::stack::{Scope, ValueView};
use crate::userdata::Userdata;

use super::{Feed, Mode, SpawnOptions, arguments, build_command, kill_group};

/// Bytes a stream's buffer holds before its reader stops reading, so the child blocks on its pipe.
pub const PIPE_BUFFER: usize = 64 * 1024;
/// Bytes `write` accepts before it answers `wouldBlock`: the stdin queue's bound.
pub const STDIN_QUEUE: usize = 1024 * 1024;
/// Largest piece a reader or the writer moves at once.
const CHUNK: usize = 8 * 1024;
/// The longest a wait sleeps before it looks at the child again.
const POLL: Duration = Duration::from_millis(20);

/// A child's captured output stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Default)]
struct Pipe {
    piped: bool,
    bytes: VecDeque<u8>,
    eof: bool,
    error: Option<std::io::Error>,
    /// Bytes read from the child over the handle's life, against `ceiling`.
    total: u64,
    ceiling: Option<u64>,
}

#[derive(Default)]
struct Queue {
    piped: bool,
    bytes: VecDeque<u8>,
    /// Bytes the writer took from `bytes` and has not finished writing.
    writing: usize,
    closing: bool,
    error: Option<std::io::Error>,
}

struct State {
    child: Child,
    status: Option<ExitStatus>,
    stdout: Pipe,
    stderr: Pipe,
    stdin: Queue,
    /// The limit that killed the child, which every later read and write reports.
    killed_by: Option<&'static str>,
    closed: bool,
}

impl State {
    fn pipe(&mut self, which: Stream) -> &mut Pipe {
        match which {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        }
    }

    /// Collects the child's exit status if it has exited, without waiting for it.
    fn reap(&mut self) -> Option<ExitStatus> {
        if self.status.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.status = Some(status);
        }
        self.status
    }

    /// Kills the child's process group, then the child. The group is the child's own (see
    /// `process::isolate`), so this reaches what the child started too, even after the child has
    /// exited and been reaped while one of those still holds a pipe.
    fn kill(&mut self) {
        kill_group(&self.child);
        let _ = self.child.kill();
    }

    /// Kills the child because `reason` limit passed, and discards what it had not yet been read.
    fn stop_for(&mut self, reason: &'static str) {
        if self.killed_by.is_none() {
            self.killed_by = Some(reason);
        }
        self.stdout.bytes.clear();
        self.stderr.bytes.clear();
        self.kill();
    }
}

/// The state shared by a handle and its threads. The mutex guards everything, and the condition
/// variable wakes readers, the writer and waits when a buffer or the child changes.
pub(super) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

type Answer<T> = std::result::Result<T, Failure>;

fn failure(message: impl Into<Cow<'static, str>>, kind: &'static str) -> Failure {
    Failure { message: message.into(), kind }
}

fn io_failure(what: &str, error: &std::io::Error) -> Failure {
    Failure::message(format!("dream.process.Child: {what}: {error}"), error)
}

fn answer<T>(answer: Answer<T>) -> Outcome<T> {
    match answer {
        Ok(value) => Outcome::Done(value),
        Err(failure) => Outcome::Failed(failure),
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep<'a>(&self, guard: MutexGuard<'a, State>, span: Duration) -> MutexGuard<'a, State> {
        let (guard, _) = self.changed.wait_timeout(guard, span).unwrap_or_else(PoisonError::into_inner);
        guard
    }

    fn wait_change<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed.wait(guard).unwrap_or_else(PoisonError::into_inner)
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Reads up to `max` bytes from a captured stream without waiting for the child.
    fn read(&self, which: Stream, max: usize) -> Answer<Option<Vec<u8>>> {
        let mut state = self.lock();
        if let Some(reason) = state.killed_by {
            return Err(failure(format!("dream.process.Child: the child was stopped at its {reason} limit"), reason));
        }
        let pipe = state.pipe(which);
        if !pipe.piped {
            return Err(failure("dream.process.Child: that stream is not piped", "invalidInput"));
        }
        if !pipe.bytes.is_empty() {
            let count = max.min(pipe.bytes.len());
            let bytes: Vec<u8> = pipe.bytes.drain(..count).collect();
            self.changed.notify_all();
            return Ok(Some(bytes));
        }
        if let Some(error) = &pipe.error {
            return Err(io_failure("read", error));
        }
        if pipe.eof {
            return Ok(None);
        }
        Err(failure("dream.process.Child: no output is available yet", "wouldBlock"))
    }

    /// Queues as much of `bytes` as the stdin queue has room for, and answers how much that is.
    fn write(&self, bytes: &[u8]) -> Answer<usize> {
        let mut state = self.lock();
        if let Some(reason) = state.killed_by {
            return Err(failure(format!("dream.process.Child: the child was stopped at its {reason} limit"), reason));
        }
        let queue = &mut state.stdin;
        if !queue.piped {
            return Err(failure("dream.process.Child: stdin is not piped", "invalidInput"));
        }
        if queue.closing {
            return Err(failure("dream.process.Child: stdin is closed", "invalidInput"));
        }
        if let Some(error) = &queue.error {
            return Err(io_failure("write", error));
        }
        let room = STDIN_QUEUE.saturating_sub(queue.bytes.len() + queue.writing);
        if room == 0 && !bytes.is_empty() {
            return Err(failure("dream.process.Child: the stdin queue is full", "wouldBlock"));
        }
        let count = room.min(bytes.len());
        queue.bytes.extend(&bytes[..count]);
        self.changed.notify_all();
        Ok(count)
    }

    /// Lets the writer finish what is queued, then close the child's stdin.
    fn close_stdin(&self) {
        let mut state = self.lock();
        if state.stdin.piped {
            state.stdin.closing = true;
            self.changed.notify_all();
        }
    }

    /// Kills the child's process group and the child, without waiting for it to die.
    fn kill(&self) {
        self.lock().kill();
        self.changed.notify_all();
    }

    /// Waits for the child to exit, for at most `timeout`. Answers its status, or nothing if it is
    /// still running.
    fn wait(&self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if let Some(status) = state.reap() {
                return Some(status);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            state = self.sleep(state, (deadline - now).min(POLL));
        }
    }

    /// Kills and reaps the child, releases its pipes and ends every thread of this handle. A
    /// second call does nothing. The wait after a SIGKILL is brief; the threads blocked in a pipe
    /// end with the pipe, and are not joined.
    fn close(&self) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        state.kill();
        if state.status.is_none() {
            state.status = state.child.wait().ok();
        }
        state.closed = true;
        state.stdout.bytes.clear();
        state.stderr.bytes.clear();
        self.changed.notify_all();
    }
}

/// Reads `pipe` into the buffer of `which` on this thread, stopping when the buffer is full, so
/// the child blocks on its pipe. Past the stream's ceiling the child is killed at once.
fn read_loop(shared: &Shared, which: Stream, mut pipe: impl Read) {
    let mut chunk = [0_u8; CHUNK];
    loop {
        let room = {
            let mut state = shared.lock();
            loop {
                if state.closed {
                    return;
                }
                let room = PIPE_BUFFER.saturating_sub(state.pipe(which).bytes.len());
                if room > 0 {
                    break room.min(CHUNK);
                }
                state = shared.wait_change(state);
            }
        };
        let result = loop {
            match pipe.read(&mut chunk[..room]) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => break result,
            }
        };
        let mut state = shared.lock();
        if state.closed {
            return;
        }
        let finished = match result {
            Ok(0) => {
                state.pipe(which).eof = true;
                true
            }
            Ok(count) => {
                let pipe_state = state.pipe(which);
                pipe_state.total = pipe_state.total.saturating_add(count as u64);
                if pipe_state.ceiling.is_some_and(|ceiling| pipe_state.total > ceiling) {
                    state.stop_for("outputLimit");
                    shared.changed.notify_all();
                    return;
                }
                state.pipe(which).bytes.extend(&chunk[..count]);
                false
            }
            Err(error) => {
                state.pipe(which).error = Some(error);
                true
            }
        };
        shared.changed.notify_all();
        if finished {
            return;
        }
    }
}

/// Writes the stdin queue to the child's stdin, and drops the pipe (ending the child's input) once
/// the queue is closed and empty, or the handle is.
fn write_loop(shared: &Shared, mut pipe: ChildStdin) {
    loop {
        let chunk: Vec<u8> = {
            let mut state = shared.lock();
            loop {
                if state.closed {
                    return;
                }
                if !state.stdin.bytes.is_empty() {
                    break;
                }
                if state.stdin.closing {
                    return;
                }
                state = shared.wait_change(state);
            }
            let take = state.stdin.bytes.len().min(CHUNK);
            let chunk: Vec<u8> = state.stdin.bytes.drain(..take).collect();
            state.stdin.writing = chunk.len();
            chunk
        };
        let result = pipe.write_all(&chunk);
        let mut state = shared.lock();
        state.stdin.writing = 0;
        if let Err(error) = result {
            state.stdin.error = Some(error);
            shared.changed.notify_all();
            return;
        }
        shared.changed.notify_all();
    }
}

/// Kills the child at `at` if it is still running, with its group, and marks the kill a timeout.
/// A child that exited first is not killed, though its group still is, at the same deadline.
fn timer_loop(shared: &Shared, at: Instant) {
    let mut state = shared.lock();
    loop {
        if state.closed {
            return;
        }
        let now = Instant::now();
        if now >= at {
            if state.reap().is_none() {
                state.stop_for("timeout");
            } else {
                state.kill();
            }
            shared.changed.notify_all();
            return;
        }
        state = shared.sleep(state, at - now);
    }
}

/// Starts a thread running `body` on `shared`.
fn spawn_thread(name: &str, shared: &Arc<Shared>, body: impl FnOnce(&Shared) + Send + 'static) -> std::io::Result<()> {
    let shared = Arc::clone(shared);
    std::thread::Builder::new().name(name.into()).spawn(move || body(&shared)).map(drop)
}

/// Starts the threads a child's pipes and limits need. A failure leaves the child to be closed.
fn launch(
    shared: &Arc<Shared>,
    streams: (Option<ChildStdout>, Option<ChildStderr>),
    stdin: Option<ChildStdin>,
    timeout: Option<Duration>,
) -> std::io::Result<()> {
    if let Some(pipe) = streams.0 {
        spawn_thread("l3i-process-stdout", shared, move |shared| read_loop(shared, Stream::Stdout, pipe))?;
    }
    if let Some(pipe) = streams.1 {
        spawn_thread("l3i-process-stderr", shared, move |shared| read_loop(shared, Stream::Stderr, pipe))?;
    }
    if let Some(pipe) = stdin {
        spawn_thread("l3i-process-stdin", shared, move |shared| write_loop(shared, pipe))?;
    }
    if let Some(timeout) = timeout {
        let at = Instant::now() + timeout;
        spawn_thread("l3i-process-timer", shared, move |shared| timer_loop(shared, at))?;
    }
    Ok(())
}

/// `process.start`: the child, started with its pipes and limits, owned by a handle. A program
/// that cannot start answers `nil`, the message and the kind, as `run` does.
pub(super) fn start(
    call: &Call<'_>,
    program: &[u8],
    args: Option<ValueView<'_>>,
    options: Option<ValueView<'_>>,
    inherit_environment: bool,
) -> Result<Outcome<StackResults>> {
    let arguments = arguments(call, args)?;
    let spawn = SpawnOptions::read(call, options, Mode::Start)?;
    let name = String::from_utf8_lossy(program).into_owned();
    let mut command = build_command(program, &arguments, &spawn, inherit_environment, true)?;
    let stdin = if matches!(spawn.stdin, Feed::Pipe) { Stdio::piped() } else { Stdio::null() };
    command.stdin(stdin).stdout(spawn.stdout.stdio()).stderr(spawn.stderr.stdio());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("dream.process.start: {name}: {error}");
            return Ok(Outcome::Failed(Failure::message(message, &error)));
        }
    };
    let SpawnOptions { bounds, .. } = spawn;
    let stdin_pipe = child.stdin.take();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let pipe = |piped: bool, ceiling: Option<u64>| Pipe { piped, ceiling, ..Pipe::default() };
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            child,
            status: None,
            stdout: pipe(stdout_pipe.is_some(), bounds.stdout),
            stderr: pipe(stderr_pipe.is_some(), bounds.stderr),
            stdin: Queue { piped: stdin_pipe.is_some(), ..Queue::default() },
            killed_by: None,
            closed: false,
        }),
        changed: Condvar::new(),
    });
    if let Err(error) = launch(&shared, (stdout_pipe, stderr_pipe), stdin_pipe, bounds.timeout) {
        shared.close();
        let message = format!("dream.process.start: {name}: {error}");
        return Ok(Outcome::Failed(Failure::message(message, &error)));
    }
    let owner = Rc::new(Owner(shared));
    let resource: Rc<dyn Resource> = owner.clone();
    crate::runtime::register_resource(call, Rc::downgrade(&resource));
    crate::userdata::push_owned(call, Process(owner))?;
    Ok(Outcome::Done(StackResults))
}

/// Owns the shared state for the runtime: shutdown closes it while Luau still holds the handle,
/// and dropping the last reference (collection of the handle) closes it too.
struct Owner(Arc<Shared>);

impl Resource for Owner {
    fn close_for_shutdown(&self) {
        self.0.close();
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The process handle a script holds. Destruction only kills and reaps; it never calls Luau.
struct Process(Rc<Owner>);

// SAFETY: contains only Rust-owned state; destruction kills a child and touches no Luau value.
unsafe impl Userdata for Process {
    const NAME: &'static str = "dream.process.Child";
}

impl Process {
    /// The shared state, unless the handle is closed.
    fn open(&self) -> Result<&Shared> {
        if self.0.0.is_closed() {
            return Err(Error::runtime("dream.process.Child: the handle is closed"));
        }
        Ok(&self.0.0)
    }
}

/// `maxBytes` for a read: a finite whole number of at least 1.
fn read_limit(value: f64) -> Result<usize> {
    if !value.is_finite() || value.fract() != 0.0 || value < 1.0 {
        return Err(Error::runtime("dream.process.Child: maxBytes must be a finite whole number of at least 1"));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a whole number at least 1, checked above"
    )]
    Ok(value as usize)
}

/// The status a finished child answers, as `run` answers it.
fn push_status(call: &Call<'_>, status: ExitStatus) -> Result<()> {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut frame = call.frame();
    let table = frame.push_table(0, 3)?;
    table.raw_set_value(&frame, "success", &status.success())?;
    table.raw_set_value(&frame, "code", &status.code().map(f64::from))?;
    table.raw_set_value(&frame, "signal", &signal.map(f64::from))?;
    frame.release();
    Ok(())
}

pub(super) fn describe(d: &mut ExtensionDescriptor) {
    let mut handle = d.userdata::<Process>(Process::NAME);
    handle.tag(TagPolicy::Preferred).doc(
        "A started process. Reads and writes never block; wait is the only call that may, and only with a timeout.",
    );
    handle
        .method("readStdout", |process: &Process, count: f64| -> Result<Outcome<Option<Vec<u8>>>> {
            Ok(answer(process.open()?.read(Stream::Stdout, read_limit(count)?)))
        })
        .signature("(self, maxBytes: number): (string?, string?, dream_process_ErrorKind?)")
        .doc("Reads at most maxBytes (at least 1) of the child's stdout that is buffered now. Nil alone is end of stream; nil, message, wouldBlock means nothing is buffered yet.");
    handle
        .method("readStderr", |process: &Process, count: f64| -> Result<Outcome<Option<Vec<u8>>>> {
            Ok(answer(process.open()?.read(Stream::Stderr, read_limit(count)?)))
        })
        .signature("(self, maxBytes: number): (string?, string?, dream_process_ErrorKind?)")
        .doc("Reads at most maxBytes (at least 1) of the child's stderr that is buffered now. Same answers as readStdout.");
    handle
        .method("write", |process: &Process, data: BytesView<'_>| -> Result<Outcome<f64>> {
            let shared = process.open()?;
            // SAFETY: the bytes are copied into the queue at once, with no call into Lua while the slice lives.
            let bytes = unsafe { data.bytes_unchecked() };
            // At most the queue's 1 MiB, which an u32 holds.
            Ok(answer(shared.write(bytes).map(|count| f64::from(u32::try_from(count).unwrap_or(u32::MAX)))))
        })
        .signature("(self, data: buffer | string): (number?, string?, dream_process_ErrorKind?)")
        .doc("Queues data for the child's stdin and answers how many bytes were accepted. The queue holds 1 MiB; when it is full the answer is nil, message, wouldBlock, and the caller retries after the child has read. Needs stdin = \"pipe\".");
    handle
        .method("closeStdin", |process: &Process| -> Result<()> {
            process.open()?.close_stdin();
            Ok(())
        })
        .signature("(self)")
        .doc("Closes the child's stdin once the queued bytes are written, so it reads end of input. Idempotent.");
    handle
        .method("wait", |process: &Process, call: &Call<'_>, timeout_ms: Option<f64>| -> Result<Outcome<StackResults>> {
            let shared = process.open()?;
            let requested = timeout_ms.unwrap_or(0.0);
            if !(0.0..=60_000.0).contains(&requested) {
                return Err(Error::runtime("dream.process.Child:wait: timeoutMs must be in 0..60000 milliseconds"));
            }
            let mut timeout = Duration::from_secs_f64(requested / 1000.0);
            if let Some(remaining) = crate::runtime::watchdog_remaining(call) {
                timeout = timeout.min(remaining);
            }
            let status = shared.wait(timeout);
            crate::runtime::expire_if_due(call);
            match status {
                Some(status) => {
                    push_status(call, status)?;
                    Ok(Outcome::Done(StackResults))
                }
                None => Ok(Outcome::Failed(failure("dream.process.Child:wait: the child is still running", "wouldBlock"))),
            }
        })
        .signature("(self, timeoutMs: number?): ({ success: boolean, code: number?, signal: number? }?, string?, dream_process_ErrorKind?)")
        .doc("The child's exit status once it has exited, { success, code?, signal? }. With timeoutMs (at most 60000) it waits that long, capped by the execution time limit; without it, or at 0, it only looks. Still running is nil, message, wouldBlock.");
    handle
        .method("kill", |process: &Process| -> Result<bool> {
            process.open()?.kill();
            Ok(true)
        })
        .signature("(self): boolean")
        .doc("Kills the child's process group (Unix) or the child, without waiting; idempotent. It does not reap: wait or close does.");
    handle
        .method("close", |process: &Process| -> Result<()> {
            process.0.0.close();
            Ok(())
        })
        .signature("(self)")
        .doc("Kills the child if it is still running, reaps it and releases its pipes. Idempotent; every other method raises afterwards.");
}
