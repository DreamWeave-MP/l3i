//! One process-wide reader, with demand-driven bounded buffering and an exclusive consumer.
//! The worker owns no runtime state and is never joined: a terminal read may never finish.

use crate::bind::{Call, StackResults};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::outcome::{Failure, Outcome};
use crate::userdata::Userdata;

use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const CHUNK: usize = 8192;
/// Maximum bytes in one raw read or line, excluding its LF (but including a preceding CR).
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct State {
    owned: bool,
    requested: bool,
    bytes: VecDeque<u8>,
    eof: bool,
    error: Option<std::io::Error>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn start(reader: impl Read + Send + 'static) -> std::io::Result<Arc<Self>> {
        let shared = Arc::new(Self::default());
        let worker = Arc::clone(&shared);
        std::thread::Builder::new().name("l3i-stdin".into()).spawn(move || worker.run(reader))?;
        Ok(shared)
    }

    fn run(&self, mut reader: impl Read) {
        let mut buffer = [0; CHUNK];
        loop {
            let mut state = self.lock();
            while !state.requested {
                state = self.changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            drop(state);
            let result = loop {
                match reader.read(&mut buffer) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    result => break result,
                }
            };
            let mut state = self.lock();
            state.requested = false;
            match result {
                Ok(0) => state.eof = true,
                Ok(count) => state.bytes.extend(&buffer[..count]),
                Err(error) => state.error = Some(error),
            }
            self.changed.notify_all();
            if state.eof || state.error.is_some() {
                return;
            }
        }
    }
}

/// An exclusive consumer of the process's stdin. Reads never block; `wait` is explicit.
/// Closing releases ownership immediately, including while the OS reader is blocked.
/// Bytes already removed by this consumer are not replayed to the next owner.
pub struct Input {
    shared: Arc<Shared>,
    closed: bool,
    line: Vec<u8>,
    line_limit: Option<usize>,
}

impl Input {
    /// Claims stdin, or reports `WouldBlock` if another consumer holds it.
    /// Only one lazy worker is created across all runtimes and threads in this process.
    pub fn open() -> std::io::Result<Self> {
        static SHARED: OnceLock<Mutex<Option<Arc<Shared>>>> = OnceLock::new();
        let mut slot = SHARED.get_or_init(Mutex::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(Shared::start(std::io::stdin())?);
        }
        Self::claim(Arc::clone(slot.as_ref().expect("initialized")))
    }

    fn claim(shared: Arc<Shared>) -> std::io::Result<Self> {
        let mut state = shared.lock();
        if state.owned {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "stdin already has a consumer"));
        }
        state.owned = true;
        drop(state);
        Ok(Self { shared, closed: false, line: Vec::new(), line_limit: None })
    }

    fn validate(&self, limit: usize) -> std::io::Result<()> {
        if self.closed {
            return Err(std::io::Error::other("stdin consumer is closed"));
        }
        if !(1..=MAX_INPUT_BYTES).contains(&limit) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "stdin byte limit must be in 1..=1048576",
            ));
        }
        Ok(())
    }

    fn next(&mut self, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
        let mut state = self.shared.lock();
        if !state.bytes.is_empty() {
            let count = limit.min(state.bytes.len());
            return Ok(Some(state.bytes.drain(..count).collect()));
        }
        if let Some(error) = &state.error {
            return Err(std::io::Error::new(error.kind(), error.to_string()));
        }
        if state.eof {
            return Ok(None);
        }
        if !state.requested {
            state.requested = true;
            self.shared.changed.notify_all();
        }
        Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "stdin input is pending"))
    }

    /// Reads at most `limit` bytes, preserving arbitrary binary data. `None` means EOF.
    /// `WouldBlock` means retry after waiting or on a later frame.
    pub fn read(&mut self, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
        self.validate(limit)?;
        if self.line_limit.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "finish the pending line before a raw read",
            ));
        }
        self.next(limit)
    }

    /// Reads a line as bytes, stripping LF and a preceding CR. An empty line is `Some([])`.
    /// Retry pending lines with the same limit. Invalid UTF-8 is preserved without replacement.
    /// An overlong line consumes its prefix and returns `InvalidInput`; close the consumer or
    /// explicitly recover its remainder with raw reads. No unbounded draining is performed.
    pub fn read_line(&mut self, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
        self.validate(limit)?;
        if self.line_limit.is_some_and(|previous| previous != limit) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "pending line byte limit cannot change"));
        }
        self.line_limit = Some(limit);
        let mut state = self.shared.lock();
        if !state.bytes.is_empty() {
            let newline = state.bytes.iter().position(|byte| *byte == b'\n');
            let count = newline.unwrap_or(state.bytes.len());
            let remaining = limit - self.line.len();
            if count > remaining {
                state.bytes.drain(..=remaining);
                self.line.clear();
                self.line_limit = None;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "stdin line exceeds its byte limit (prefix consumed)",
                ));
            }
            self.line.extend(state.bytes.drain(..count));
            if newline.is_some() {
                state.bytes.pop_front();
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                self.line_limit = None;
                return Ok(Some(std::mem::take(&mut self.line)));
            }
        }
        if let Some(error) = &state.error {
            return Err(std::io::Error::new(error.kind(), error.to_string()));
        }
        if state.eof {
            self.line_limit = None;
            return Ok((!self.line.is_empty()).then(|| std::mem::take(&mut self.line)));
        }
        if !state.requested {
            state.requested = true;
            self.shared.changed.notify_all();
        }
        Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "stdin input is pending"))
    }

    /// Waits for input from a read already requested, for at most `timeout`.
    /// Does not start a read and does not invoke script code.
    pub fn wait(&self, timeout: Duration) -> bool {
        if self.closed {
            return true;
        }
        let state = self.shared.lock();
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, timeout, |s| s.bytes.is_empty() && !s.eof && s.error.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.bytes.is_empty() || state.eof || state.error.is_some()
    }

    /// Releases ownership without joining the OS reader. Idempotent.
    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.shared.lock().owned = false;
            self.line = Vec::new();
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.close();
    }
}

use std::cell::RefCell;
use std::rc::Rc;

/// What a host [`Policy`] is admitting. Reads are never checked: the handle is the authority to
/// read, so passing one to less privileged code delegates that. Nor is closing: cleanup stays
/// available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Claiming stdin, checked before the process stream is touched.
    Open,
    /// An explicit blocking wait, which a frame-driven host can refuse outright.
    Wait,
}

/// A host's own check on stdin, on top of the runtime's `process.stdio` grant, which it cannot
/// bypass. A failure raises in the caller.
pub trait Policy {
    /// Admits `operation` for the code calling now, or says why not.
    ///
    /// # Errors
    ///
    /// Whatever the host wants raised instead of the operation.
    fn check(&self, operation: Operation) -> Result<()>;
}

/// The consumer, shared with the runtime's resource list so [`Runtime::shutdown`] releases the
/// claim even while Luau still references the handle.
///
/// [`Runtime::shutdown`]: crate::Runtime::shutdown
struct Consumer(RefCell<Input>);

impl crate::runtime::shared::Resource for Consumer {
    fn close_for_shutdown(&self) {
        self.0.borrow_mut().close();
    }
}

struct Stdin {
    input: Rc<Consumer>,
    policy: Option<Rc<dyn Policy>>,
}

impl Stdin {
    fn check(&self, operation: Operation) -> Result<()> {
        self.policy.as_ref().map_or(Ok(()), |policy| policy.check(operation))
    }
}

// SAFETY: contains only Rust-owned I/O state; destruction never touches Luau.
unsafe impl Userdata for Stdin {
    const NAME: &'static str = "dream.process.Stdin";
}

fn outcome<T>(result: std::io::Result<T>) -> Outcome<T> {
    match result {
        Ok(value) => Outcome::Done(value),
        Err(error) => Outcome::Failed(Failure::message(format!("dream.process.stdin: {error}"), &error)),
    }
}

pub(super) fn open(call: &Call<'_>, policy: Option<&Rc<dyn Policy>>) -> Result<Outcome<StackResults>> {
    if let Some(policy) = policy {
        policy.check(Operation::Open)?;
    }
    match Input::open() {
        Ok(input) => {
            let input = Rc::new(Consumer(RefCell::new(input)));
            let resource: Rc<dyn crate::runtime::shared::Resource> = input.clone();
            crate::runtime::register_resource(call, Rc::downgrade(&resource));
            crate::userdata::push_owned(call, Stdin { input, policy: policy.map(Rc::clone) })?;
            Ok(Outcome::Done(StackResults))
        }
        Err(error) => Ok(Outcome::Failed(Failure::message(format!("dream.process.openStdin: {error}"), &error))),
    }
}

fn limit(value: f64) -> Result<usize> {
    if value.fract() != 0.0 || !(1.0..=1_048_576.0).contains(&value) {
        return Err(Error::runtime("dream.process.stdin: byte limit must be an integer in 1..=1048576"));
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "bounded integer checked above")]
    Ok(value as usize)
}

pub(super) fn describe(d: &mut ExtensionDescriptor) {
    let mut input = d.userdata::<Stdin>(Stdin::NAME);
    input
        .tag(TagPolicy::Preferred)
        .doc("Exclusive stdin consumer. Read methods never block; close explicitly when finished.");
    input.method("read", |input: &Stdin, count: f64| -> Result<Outcome<Option<Vec<u8>>>> {
        Ok(outcome(input.input.0.borrow_mut().read(limit(count)?)))
    }).signature("(self, maxBytes: number): (string?, string?, dream_process_ErrorKind?)")
      .doc("Reads at most maxBytes (1..1048576) unchanged bytes into a binary Luau string. Nil alone is EOF; nil, message, wouldBlock means retry later. Cannot interrupt a pending line.");
    input.method("readLine", |input: &Stdin, count: Option<f64>| -> Result<Outcome<Option<Vec<u8>>>> {
        Ok(outcome(input.input.0.borrow_mut().read_line(limit(count.unwrap_or(65536.0))?)))
    }).signature("(self, maxBytes: number?): (string?, string?, dream_process_ErrorKind?)")
      .doc("Nonblocking bounded line read (default 65536, maximum 1048576 bytes before LF). Strips LF and a preceding CR; preserves other bytes including invalid UTF-8. Empty string is an empty line, nil alone is EOF. Retry wouldBlock with the same limit. An oversized line returns invalidInput after consuming its prefix; the next readLine returns what is left of it, so call again until a line comes back to skip the rest.");
    input.method("wait", |input: &Stdin, call: &Call<'_>, ms: f64| -> Result<bool> {
        input.check(Operation::Wait)?;
        if !(0.0..=60000.0).contains(&ms) {
            return Err(Error::runtime("dream.process.stdin.wait: timeout must be in 0..60000 milliseconds"));
        }
        let mut timeout = Duration::from_secs_f64(ms / 1000.0);
        if let Some(remaining) = crate::runtime::watchdog_remaining(call) { timeout = timeout.min(remaining); }
        let ready = input.input.0.borrow().wait(timeout);
        crate::runtime::expire_if_due(call);
        Ok(ready)
    }).signature("(self, timeoutMs: number): boolean")
      .doc("Explicitly waits for a requested read, EOF or error, at most timeoutMs and the runtime execution limit. Does not initiate a read. Hosted frame handlers should poll read/readLine instead of waiting.");
    input.method("close", |input: &Stdin| input.input.0.borrow_mut().close())
        .signature("(self)").doc("Releases stdin immediately, even with a pending terminal read; idempotent. Partial line bytes consumed by this owner are discarded. Unconsumed process buffer bytes remain for the next owner.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    fn input(bytes: &[u8]) -> Input {
        Input::claim(Shared::start(Cursor::new(bytes.to_vec())).unwrap()).unwrap()
    }

    fn line(input: &mut Input, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match input.read_line(limit) {
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "reader stalled");
                    input.wait(Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    #[test]
    fn lines_distinguish_empty_eof_crlf_and_binary() {
        let mut input = input(b"hello\r\n\n\xff\0\nlast\r");
        assert_eq!(line(&mut input, 64).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(line(&mut input, 64).unwrap(), Some(vec![]));
        assert_eq!(line(&mut input, 64).unwrap(), Some(vec![255, 0]));
        assert_eq!(line(&mut input, 64).unwrap(), Some(b"last\r".to_vec()));
        assert_eq!(line(&mut input, 64).unwrap(), None);
        assert_eq!(line(&mut input, 64).unwrap(), None);
    }

    #[test]
    fn lines_span_chunks_and_enforce_limits_without_draining_forever() {
        let mut bytes = vec![b'x'; CHUNK * 3];
        bytes.extend_from_slice(b"\nend\n");
        let mut stream = input(&bytes);
        assert_eq!(line(&mut stream, CHUNK * 3).unwrap(), Some(vec![b'x'; CHUNK * 3]));
        assert_eq!(line(&mut stream, 3).unwrap(), Some(b"end".to_vec()));
        let mut stream = input(b"abcd\nnext\n");
        assert_eq!(line(&mut stream, 2).unwrap_err().kind(), ErrorKind::InvalidInput);
        assert_eq!(line(&mut stream, 2).unwrap(), Some(b"d".to_vec()));
        assert_eq!(line(&mut stream, 4).unwrap(), Some(b"next".to_vec()));
        assert_eq!(stream.read(0).unwrap_err().kind(), ErrorKind::InvalidInput);
        assert_eq!(stream.read(MAX_INPUT_BYTES + 1).unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    struct Gated(std::sync::mpsc::Receiver<Vec<u8>>);
    impl Read for Gated {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let value = self.0.recv_timeout(Duration::from_secs(2)).map_err(std::io::Error::other)?;
            bytes[..value.len()].copy_from_slice(&value);
            Ok(value.len())
        }
    }

    #[test]
    fn a_blocked_reader_survives_owner_teardown_without_duplicate_reads() {
        let (send, receive) = std::sync::mpsc::channel();
        let shared = Shared::start(Gated(receive)).unwrap();
        let mut first = Input::claim(Arc::clone(&shared)).unwrap();
        assert!(matches!(Input::claim(Arc::clone(&shared)), Err(e) if e.kind() == ErrorKind::WouldBlock));
        assert_eq!(first.read_line(10).unwrap_err().kind(), ErrorKind::WouldBlock);
        assert_eq!(first.read(1).unwrap_err().kind(), ErrorKind::InvalidInput);
        assert_eq!(first.read_line(20).unwrap_err().kind(), ErrorKind::InvalidInput);
        assert!(!first.wait(Duration::ZERO));
        first.close();
        first.close();
        assert!(first.read(1).is_err());
        let mut second = Input::claim(Arc::clone(&shared)).unwrap();
        assert_eq!(second.read(3).unwrap_err().kind(), ErrorKind::WouldBlock);
        send.send(b"abc\n".to_vec()).unwrap();
        assert!(second.wait(Duration::from_secs(2)));
        assert_eq!(second.read(3).unwrap(), Some(b"abc".to_vec()));
        assert_eq!(line(&mut second, 5).unwrap(), Some(vec![]));
        // Demand has not restarted a reader just because ownership changed.
        assert!(!shared.lock().requested);
        assert_eq!(second.read(1).unwrap_err().kind(), ErrorKind::WouldBlock);
        send.send(vec![]).unwrap();
        assert!(second.wait(Duration::from_secs(2)));
        assert_eq!(second.read(1).unwrap(), None);
    }

    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(ErrorKind::PermissionDenied, "test error"))
        }
    }

    #[test]
    fn io_errors_are_distinct_from_eof_and_remain_observable() {
        let mut stream = Input::claim(Shared::start(Broken).unwrap()).unwrap();
        assert_eq!(stream.read(10).unwrap_err().kind(), ErrorKind::WouldBlock);
        assert!(stream.wait(Duration::from_secs(2)));
        for _ in 0..2 {
            assert_eq!(stream.read(10).unwrap_err().kind(), ErrorKind::PermissionDenied);
        }
    }
}
