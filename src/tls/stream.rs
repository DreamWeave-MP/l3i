//! `dream.tls.Stream`: a rustls session over a TCP socket it owns exclusively, driven one
//! bounded step at a time by the script's calls.
//!
//! Every call moves bytes as far as it can without waiting and stops: pending ciphertext goes
//! to the socket first (a handshake reply, an alert, a key update), then the call does its own
//! work. `read_tls` is only called while rustls wants to read, so its 16 KiB plaintext buffer
//! never overflows, and plaintext reaches the script's buffer straight from rustls's.
//!
//! Readiness for the poller is [`Flags`]: the socket's cached OS readiness combined with what
//! only the session knows (decrypted plaintext waiting, ciphertext waiting for a writable
//! socket, room for more plaintext, a handshake deadline). A stream watched for reading is
//! woken when a pending write of the session's own can make progress, so a handshake or key
//! update that must write first never stalls a reader.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;

use crate::bind::Call;
use crate::convert::{BufferView, BytesView, Exact};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::outcome::{Failure, Outcome};
use crate::stack::ValueView;
use crate::tcp::poller::{Synthetic, Target, Watchable, mask};
use crate::tcp::socket::{CLOSED, Io, READABLE, StreamCount, WRITABLE};
use crate::userdata::{Owned, Userdata};

use super::config::{ClientConfig, ServerConfig};

/// Socket reads and writes one handshake call makes at most.
const HANDSHAKE_STEPS: usize = 32;
/// Socket reads one `readInto` makes at most before answering would-block.
const READ_STEPS: usize = 64;

/// The default cap on ciphertext waiting for the socket, in bytes.
pub const DEFAULT_BUFFER_LIMIT: usize = 64 * 1024;
/// The bounds of `bufferLimit`.
pub const MIN_BUFFER_LIMIT: usize = 4 * 1024;
pub const MAX_BUFFER_LIMIT: usize = 16 * 1024 * 1024;
/// The default and largest handshake deadline, in milliseconds.
pub const DEFAULT_HANDSHAKE_TIMEOUT_MS: u32 = 10_000;
pub const MAX_HANDSHAKE_TIMEOUT_MS: u32 = 300_000;

/// Where a session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsState {
    /// The handshake is not complete; no application data moves.
    Handshaking,
    /// Authenticated: application data moves.
    Open,
    /// A fatal error ended the session and shut its socket down both ways; every call reports
    /// the error, and `close` releases the socket.
    Failed,
    /// Closed locally.
    Closed,
}

impl TlsState {
    /// The name scripts see.
    pub fn name(self) -> &'static str {
        match self {
            TlsState::Handshaking => "handshaking",
            TlsState::Open => "open",
            TlsState::Failed => "failed",
            TlsState::Closed => "closed",
        }
    }
}

/// What the poller reads of a session, refreshed after every call.
pub(crate) struct Flags {
    state: Cell<TlsState>,
    /// Ciphertext is waiting for the socket.
    pending_out: Cell<bool>,
    /// Decrypted plaintext is waiting for `readInto`.
    plaintext: Cell<bool>,
    /// `write` would accept plaintext now.
    room: Cell<bool>,
    /// The peer sent close_notify.
    peer_closed: Cell<bool>,
    deadline: Instant,
}

impl Synthetic for Flags {
    fn reportable(&self, os: u8, interest: u8, now: Instant) -> u8 {
        let flush = self.pending_out.get() && os & WRITABLE != 0;
        let bits = match self.state.get() {
            // The next call reports what happened.
            TlsState::Failed | TlsState::Closed => READABLE | WRITABLE,
            TlsState::Handshaking => {
                if now >= self.deadline || os & (READABLE | CLOSED) != 0 || flush {
                    READABLE | WRITABLE
                } else {
                    0
                }
            }
            TlsState::Open => {
                let ended = os & CLOSED != 0 || self.peer_closed.get();
                let mut bits = 0;
                if self.plaintext.get() || os & READABLE != 0 || ended || flush {
                    bits |= READABLE;
                }
                if ended {
                    bits |= CLOSED;
                }
                if self.room.get() || flush {
                    bits |= WRITABLE;
                }
                bits
            }
        };
        bits & mask(interest)
    }

    fn deadline(&self) -> Option<Instant> {
        (self.state.get() == TlsState::Handshaking).then_some(self.deadline)
    }
}

/// The socket as rustls's reader and writer.
struct Socket<'a>(&'a mio::net::TcpStream);

impl Read for Socket<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl Write for Socket<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }
    /// A whole flight in one send: records written one by one would wait on Nagle's
    /// algorithm and the peer's delayed acknowledgement between them.
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        (&*self.0).write_vectored(bufs)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What one socket read brought.
enum Inbound {
    Data,
    Eof,
    Blocked,
}

fn would_block(message: &'static str) -> Failure {
    Failure { message: Cow::Borrowed(message), kind: "wouldBlock" }
}

/// The kind a script sees for a rustls error.
pub(crate) fn tls_kind(error: &rustls::Error) -> &'static str {
    use rustls::{CertificateError as C, Error as E};
    match error {
        E::InvalidCertificate(C::NotValidForName | C::NotValidForNameContext { .. }) => "certificateNameMismatch",
        E::InvalidCertificate(C::Expired | C::ExpiredContext { .. }) => "certificateExpired",
        E::InvalidCertificate(C::NotValidYet | C::NotValidYetContext { .. }) => "certificateNotYetValid",
        E::InvalidCertificate(C::UnknownIssuer) => "certificateUntrusted",
        E::InvalidCertificate(C::Revoked) => "certificateRevoked",
        E::InvalidCertificate(_) => "certificateInvalid",
        E::AlertReceived(_) => "tlsAlert",
        E::NoApplicationProtocol => "noApplicationProtocol",
        E::PeerIncompatible(_) => "tlsIncompatible",
        _ => "tlsProtocol",
    }
}

/// `dream.tls.Stream`.
pub struct TlsStream {
    io: Rc<Io>,
    conn: RefCell<rustls::Connection>,
    flags: Rc<Flags>,
    limit: usize,
    failure: RefCell<Option<(String, &'static str)>>,
    /// close_notify was queued.
    write_closed: Cell<bool>,
    /// The TCP write half was shut after close_notify left.
    fin_sent: Cell<bool>,
    peer: SocketAddr,
    local: Option<SocketAddr>,
    /// The identity a client verifies (as given), or the name a client asked a server for.
    server_name: Option<String>,
    counted: RefCell<Option<Rc<StreamCount>>>,
}

// SAFETY: plain Rust state with no Lua references; dropping it closes a socket and touches no
// Lua API.
unsafe impl Userdata for TlsStream {
    const NAME: &'static str = "dream.tls.Stream";
}

impl TlsStream {
    fn new(
        taken: crate::tcp::socket::Taken,
        mut conn: rustls::Connection,
        limit: usize,
        handshake_timeout: Duration,
        server_name: Option<String>,
    ) -> TlsStream {
        conn.set_buffer_limit(Some(limit));
        TlsStream {
            io: taken.io,
            conn: RefCell::new(conn),
            flags: Rc::new(Flags {
                state: Cell::new(TlsState::Handshaking),
                pending_out: Cell::new(false),
                plaintext: Cell::new(false),
                room: Cell::new(false),
                peer_closed: Cell::new(false),
                deadline: Instant::now() + handshake_timeout,
            }),
            limit,
            failure: RefCell::new(None),
            write_closed: Cell::new(false),
            fin_sent: Cell::new(false),
            peer: taken.peer,
            local: taken.local,
            server_name,
            counted: RefCell::new(taken.counted),
        }
    }

    /// Where the session is.
    pub fn state(&self) -> TlsState {
        self.flags.state.get()
    }

    fn release(&self) {
        if let Some(count) = self.counted.borrow_mut().take() {
            count.release();
        }
    }

    /// Closes the socket and drops its registration at once, without close_notify; the
    /// listener's count is released. Idempotent.
    pub fn close(&self) {
        self.io.close();
        if self.flags.state.get() != TlsState::Failed {
            self.flags.state.set(TlsState::Closed);
        }
        self.release();
    }

    /// Ends the session for `failure`: the alert rustls queued goes out if the socket takes it
    /// now, then the socket is shut down both ways, so nothing can continue in plaintext.
    fn fatal(&self, conn: &mut rustls::Connection, message: String, kind: &'static str) -> Failure {
        let _ = self.pump_out(conn);
        // Shut, not closed: the peer sees the end and nothing more moves either way, while the
        // descriptor and its watch stay until `close`, so a script's token stays valid.
        let _ = self.io.with_stream(|socket| socket.shutdown(Shutdown::Both));
        self.flags.state.set(TlsState::Failed);
        *self.failure.borrow_mut() = Some((message.clone(), kind));
        Failure { message: Cow::Owned(message), kind }
    }

    fn fatal_tls(&self, conn: &mut rustls::Connection, what: &str, error: &rustls::Error) -> Failure {
        self.fatal(conn, format!("dream.tls.{what}: {}: {error}", self.peer), tls_kind(error))
    }

    fn fatal_io(&self, conn: &mut rustls::Connection, what: &str, error: &io::Error) -> Failure {
        self.fatal(conn, format!("dream.tls.{what}: {}: {error}", self.peer), crate::outcome::network_kind_of(error))
    }

    /// The failure a failed session reports again, or a script error once closed.
    fn gate(&self, what: &str) -> Result<Option<Failure>> {
        match self.flags.state.get() {
            TlsState::Closed => Err(Error::runtime(format!("dream.tls.{what}: the stream is closed"))),
            TlsState::Failed => {
                let failure = self.failure.borrow();
                let (message, kind) = failure.as_ref().expect("a failed session keeps its failure");
                Ok(Some(Failure { message: Cow::Owned(message.clone()), kind }))
            }
            TlsState::Handshaking | TlsState::Open => Ok(None),
        }
    }

    /// Writes pending ciphertext until rustls has none or the socket is full. True when
    /// nothing is left; whether any byte moved is the second value.
    fn pump_out(&self, conn: &mut rustls::Connection) -> io::Result<(bool, bool)> {
        let mut moved = false;
        while conn.wants_write() {
            let wrote = self
                .io
                .with_stream(|socket| conn.write_tls(&mut Socket(socket)))
                .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::NotConnected, "the socket is closed")));
            match wrote {
                Ok(0) => return Ok((false, moved)),
                Ok(_) => moved = true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.io.clear(WRITABLE);
                    return Ok((false, moved));
                }
                Err(error) => return Err(error),
            }
        }
        Ok((true, moved))
    }

    /// One socket read into rustls.
    fn pump_in(&self, conn: &mut rustls::Connection) -> io::Result<Inbound> {
        loop {
            let read = self
                .io
                .with_stream(|socket| conn.read_tls(&mut Socket(socket)))
                .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::NotConnected, "the socket is closed")));
            return match read {
                Ok(0) => Ok(Inbound::Eof),
                Ok(_) => Ok(Inbound::Data),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.io.clear(READABLE);
                    Ok(Inbound::Blocked)
                }
                Err(error) => Err(error),
            };
        }
    }

    /// Updates the flags the poller reads; the session's own error ends it.
    fn refresh(&self, conn: &mut rustls::Connection, what: &str) -> std::result::Result<(), Failure> {
        let io = match conn.process_new_packets() {
            Ok(io) => io,
            Err(error) => return Err(self.fatal_tls(conn, what, &error)),
        };
        let flags = &self.flags;
        flags.plaintext.set(io.plaintext_bytes_to_read() > 0);
        flags.pending_out.set(io.tls_bytes_to_write() > 0);
        flags.room.set(io.tls_bytes_to_write() < self.limit && !self.write_closed.get());
        flags.peer_closed.set(io.peer_has_closed());
        if flags.state.get() == TlsState::Handshaking && !conn.is_handshaking() {
            flags.state.set(TlsState::Open);
        }
        Ok(())
    }

    /// Moves the handshake as far as the socket allows; true once it is complete.
    fn drive(&self, what: &str) -> std::result::Result<bool, Failure> {
        let mut conn = self.conn.borrow_mut();
        let conn = &mut *conn;
        if Instant::now() >= self.flags.deadline {
            return Err(self.fatal(
                conn,
                format!("dream.tls.{what}: {}: the handshake did not finish in time", self.peer),
                "timedOut",
            ));
        }
        for _ in 0..HANDSHAKE_STEPS {
            let (_, mut progressed) = self.pump_out(conn).map_err(|error| self.fatal_io(conn, what, &error))?;
            if !conn.is_handshaking() {
                break;
            }
            if conn.wants_read() {
                match self.pump_in(conn).map_err(|error| self.fatal_io(conn, what, &error))? {
                    Inbound::Data => {
                        if let Err(error) = conn.process_new_packets() {
                            return Err(self.fatal_tls(conn, what, &error));
                        }
                        progressed = true;
                    }
                    Inbound::Eof => {
                        // An alert the peer sent before closing has been processed already.
                        return Err(self.fatal(
                            conn,
                            format!(
                                "dream.tls.{what}: {}: the peer closed the connection during the handshake",
                                self.peer
                            ),
                            "connectionAborted",
                        ));
                    }
                    Inbound::Blocked => {}
                }
            }
            if !conn.is_handshaking() || !progressed {
                break;
            }
        }
        // The last flight (a client's Finished) leaves now if the socket takes it; what does
        // not leaves with the next call.
        self.pump_out(conn).map_err(|error| self.fatal_io(conn, what, &error))?;
        self.refresh(conn, what)?;
        Ok(!conn.is_handshaking())
    }

    fn handshake(&self) -> Result<Outcome<bool>> {
        if let Some(failure) = self.gate("Stream.handshake")? {
            return Ok(Outcome::Failed(failure));
        }
        if self.flags.state.get() == TlsState::Open {
            return Ok(Outcome::Done(true));
        }
        Ok(match self.drive("Stream.handshake") {
            Ok(done) => Outcome::Done(done),
            Err(failure) => Outcome::Failed(failure),
        })
    }

    /// The gate every data call passes: the stored failure, or the handshake still running.
    fn ready(&self, what: &str, pending: &'static str) -> Result<Option<Failure>> {
        if let Some(failure) = self.gate(what)? {
            return Ok(Some(failure));
        }
        if self.flags.state.get() == TlsState::Handshaking {
            match self.drive(what) {
                Ok(true) => {}
                Ok(false) => return Ok(Some(would_block(pending))),
                Err(failure) => return Ok(Some(failure)),
            }
        }
        Ok(None)
    }

    fn read_into(
        &self,
        mut buffer: BufferView<'_>,
        offset: Option<Exact<i64>>,
        length: Option<Exact<i64>>,
    ) -> Result<Outcome<f64>> {
        const WHAT: &str = "Stream.readInto";
        let (offset, length) =
            crate::tcp::socket::window("dream.tls.Stream.readInto", "buffer", buffer.len(), offset, length)?;
        if length == 0 {
            return Err(Error::runtime(
                "dream.tls.Stream.readInto: no room to read into; a zero-byte read would look like end of stream",
            ));
        }
        if let Some(failure) = self.ready(WHAT, "dream.tls.Stream.readInto: the handshake is in progress")? {
            return Ok(Outcome::Failed(failure));
        }
        // SAFETY: the window is inside the buffer and this call holds the only view of it;
        // rustls copies plaintext into the slice and nothing keeps its address after the call.
        let dst = unsafe { &mut buffer.bytes_mut_unchecked()[offset..offset + length] };
        let mut conn = self.conn.borrow_mut();
        let conn = &mut *conn;
        if let Err(error) = self.pump_out(conn) {
            return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error)));
        }
        for _ in 0..READ_STEPS {
            match conn.reader().read(dst) {
                Ok(count) => {
                    // 0 only after the peer's close_notify: a clean end of stream.
                    if let Err(failure) = self.refresh(conn, WHAT) {
                        return Ok(Outcome::Failed(failure));
                    }
                    return Ok(Outcome::Done(count as f64));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(Outcome::Failed(self.fatal(
                        conn,
                        format!(
                            "dream.tls.{WHAT}: {}: the connection ended without close_notify; what was received may be truncated",
                            self.peer
                        ),
                        "truncated",
                    )));
                }
                Err(error) => return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error))),
            }
            if !conn.wants_read() {
                break;
            }
            match self.pump_in(conn) {
                Ok(Inbound::Data | Inbound::Eof) => {
                    if let Err(error) = conn.process_new_packets() {
                        return Ok(Outcome::Failed(self.fatal_tls(conn, WHAT, &error)));
                    }
                    // A key update or an alert to answer goes out at once.
                    if let Err(error) = self.pump_out(conn) {
                        return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error)));
                    }
                }
                Ok(Inbound::Blocked) => break,
                Err(error) => return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error))),
            }
        }
        if let Err(failure) = self.refresh(conn, WHAT) {
            return Ok(Outcome::Failed(failure));
        }
        Ok(Outcome::Failed(would_block("dream.tls.Stream.readInto: no plaintext is ready")))
    }

    fn write(
        &self,
        data: BytesView<'_>,
        offset: Option<Exact<i64>>,
        length: Option<Exact<i64>>,
    ) -> Result<Outcome<f64>> {
        const WHAT: &str = "Stream.write";
        let (offset, length) =
            crate::tcp::socket::window("dream.tls.Stream.write", "data", data.len(), offset, length)?;
        if let Some(failure) = self.ready(WHAT, "dream.tls.Stream.write: the handshake is in progress")? {
            return Ok(Outcome::Failed(failure));
        }
        if self.write_closed.get() {
            return Ok(Outcome::Failed(Failure {
                message: Cow::Borrowed("dream.tls.Stream.write: the write side was shut down with close_notify"),
                kind: "brokenPipe",
            }));
        }
        if length == 0 {
            return Ok(Outcome::Done(0.0));
        }
        // SAFETY: a string's bytes are immutable; a buffer's are only read, by rustls's copy into
        // its own record, through this call's one view.
        let src = unsafe { &data.bytes_unchecked()[offset..offset + length] };
        let mut conn = self.conn.borrow_mut();
        let conn = &mut *conn;
        if let Err(error) = self.pump_out(conn) {
            return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error)));
        }
        let accepted = match conn.writer().write(src) {
            Ok(count) => count,
            Err(error) => return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error))),
        };
        if let Err(error) = self.pump_out(conn) {
            return Ok(Outcome::Failed(self.fatal_io(conn, WHAT, &error)));
        }
        if let Err(failure) = self.refresh(conn, WHAT) {
            return Ok(Outcome::Failed(failure));
        }
        if accepted == 0 {
            return Ok(Outcome::Failed(would_block(
                "dream.tls.Stream.write: the TLS send buffer is full; flush when the stream is writable",
            )));
        }
        Ok(Outcome::Done(accepted as f64))
    }

    /// Writes pending ciphertext; after close_notify has left, shuts the TCP write half.
    fn flush_now(&self, what: &str) -> std::result::Result<bool, Failure> {
        let mut conn = self.conn.borrow_mut();
        let conn = &mut *conn;
        let (drained, _) = self.pump_out(conn).map_err(|error| self.fatal_io(conn, what, &error))?;
        if drained && self.write_closed.get() && !self.fin_sent.get() {
            // The peer may already have closed; that is not this side's error.
            let _ = self.io.with_stream(|socket| socket.shutdown(Shutdown::Write));
            self.fin_sent.set(true);
        }
        self.refresh(conn, what)?;
        Ok(drained)
    }

    fn flush(&self) -> Result<Outcome<bool>> {
        const WHAT: &str = "Stream.flush";
        if let Some(failure) = self.gate(WHAT)? {
            return Ok(Outcome::Failed(failure));
        }
        Ok(match self.flush_now(WHAT) {
            Ok(drained) => Outcome::Done(drained),
            Err(failure) => Outcome::Failed(failure),
        })
    }

    fn shutdown_write(&self) -> Result<Outcome<bool>> {
        const WHAT: &str = "Stream.shutdownWrite";
        if let Some(failure) = self.ready(WHAT, "dream.tls.Stream.shutdownWrite: the handshake is in progress")? {
            return Ok(Outcome::Failed(failure));
        }
        if !self.write_closed.get() {
            self.conn.borrow_mut().send_close_notify();
            self.write_closed.set(true);
        }
        Ok(match self.flush_now(WHAT) {
            Ok(drained) => Outcome::Done(drained),
            Err(failure) => Outcome::Failed(failure),
        })
    }

    pub(crate) fn target(&self) -> Result<Target> {
        match self.flags.state.get() {
            TlsState::Closed | TlsState::Failed => {
                Err(Error::runtime("dream.tcp.Poller.watch: the TLS stream is closed or failed"))
            }
            TlsState::Handshaking | TlsState::Open => Ok(Target {
                source: Rc::clone(&self.io) as Rc<dyn Watchable>,
                ready: Rc::clone(&self.io.ready),
                read_only: false,
                synthetic: Some(Rc::clone(&self.flags) as Rc<dyn crate::tcp::poller::Synthetic>),
            }),
        }
    }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        // The handed-over TCP handle may still hold the Io; the socket is this stream's alone
        // and closes with it.
        self.close();
    }
}

/// The poller's target for a TLS stream, or `None` when `handle` is not one.
pub(crate) fn watch_target(handle: ValueView<'_>) -> Option<Result<Target>> {
    crate::userdata::receiver::<TlsStream>(handle).map(TlsStream::target)
}

/// Options both constructors read.
struct Common {
    limit: usize,
    timeout: Duration,
}

fn common(o: &mut Options<'_, '_>, what: &str) -> Result<Common> {
    let limit = match o.optional::<Exact<i64>>("bufferLimit")? {
        Some(limit) => usize::try_from(limit.0)
            .ok()
            .filter(|limit| (MIN_BUFFER_LIMIT..=MAX_BUFFER_LIMIT).contains(limit))
            .ok_or_else(|| {
                Error::runtime(format!(
                    "{what}: bufferLimit must be in [{MIN_BUFFER_LIMIT}, {MAX_BUFFER_LIMIT}], got {}",
                    limit.0
                ))
            })?,
        None => DEFAULT_BUFFER_LIMIT,
    };
    let timeout = match o.optional::<Exact<i64>>("handshakeTimeoutMs")? {
        Some(ms) => {
            u32::try_from(ms.0).ok().filter(|ms| (1..=MAX_HANDSHAKE_TIMEOUT_MS).contains(ms)).ok_or_else(|| {
                Error::runtime(format!(
                    "{what}: handshakeTimeoutMs must be in [1, {MAX_HANDSHAKE_TIMEOUT_MS}], got {}",
                    ms.0
                ))
            })?
        }
        None => DEFAULT_HANDSHAKE_TIMEOUT_MS,
    };
    Ok(Common { limit, timeout: Duration::from_millis(u64::from(timeout)) })
}

fn tcp_stream<'v>(what: &str, view: ValueView<'v>) -> Result<&'v crate::tcp::Stream> {
    crate::userdata::receiver::<crate::tcp::Stream>(view)
        .ok_or_else(|| view.field_type_error(what, "a connected dream.tcp.Stream"))
}

/// The configuration `tls.client` uses when given none, built once per runtime.
pub(crate) type DefaultClient = RefCell<Option<Arc<rustls::ClientConfig>>>;

/// `tls.client(stream, { serverName, config?, alpn?, bufferLimit?, handshakeTimeoutMs? })`.
pub(crate) fn client(
    call: &Call<'_>,
    defaults: &DefaultClient,
    stream: ValueView<'_>,
    options: ValueView<'_>,
) -> Result<Outcome<Owned<TlsStream>>> {
    const WHAT: &str = "dream.tls.client";
    let tcp = tcp_stream(WHAT, stream)?;
    let mut name = None;
    let mut config = None;
    let mut alpn = None;
    let mut common_options = None;
    Options::read(call, options, WHAT, |o| {
        name = Some(o.required::<String>("serverName")?);
        config = o.with_optional("config", |view| {
            crate::userdata::receiver::<ClientConfig>(view)
                .map(|config| Arc::clone(&config.0))
                .ok_or_else(|| view.field_type_error(&format!("{WHAT}: config"), "a dream.tls.ClientConfig"))
        })?;
        alpn = o.with_optional_in("alpn", |frame, view| super::config::alpn(frame, &format!("{WHAT}: alpn"), view))?;
        common_options = Some(common(o, WHAT)?);
        Ok(())
    })?;
    let name = name.expect("required");
    let common_options = common_options.expect("read");
    let host = crate::hostname::normalize(&name)
        .map_err(|message| Error::runtime(format!("{WHAT}: serverName: {message}")))?;
    let server_name: ServerName<'static> = match host.literal {
        Some(ip) => ServerName::IpAddress(ip.into()),
        None => ServerName::try_from(host.bare().to_owned())
            .map_err(|error| Error::runtime(format!("{WHAT}: serverName '{name}': {error}")))?,
    };
    let mut config = match config {
        Some(config) => config,
        None => {
            let mut cached = defaults.borrow_mut();
            match cached.as_ref() {
                Some(config) => Arc::clone(config),
                None => match super::config::platform_default() {
                    Ok(config) => Arc::clone(cached.insert(config)),
                    Err(failure) => return Ok(Outcome::Failed(failure)),
                },
            }
        }
    };
    if let Some(alpn) = alpn {
        let mut changed = (*config).clone();
        changed.alpn_protocols = alpn;
        config = Arc::new(changed);
    }
    let conn = match rustls::ClientConnection::new(config, server_name) {
        Ok(conn) => conn,
        Err(error) => {
            return Ok(Outcome::Failed(Failure {
                message: Cow::Owned(format!("{WHAT}: {error}")),
                kind: tls_kind(&error),
            }));
        }
    };
    // Last, so a refusal above leaves the TCP stream usable.
    let taken = tcp.take(WHAT)?;
    Ok(Outcome::Done(Owned(TlsStream::new(
        taken,
        conn.into(),
        common_options.limit,
        common_options.timeout,
        Some(name),
    ))))
}

/// `tls.server(stream, config, { bufferLimit?, handshakeTimeoutMs? }?)`.
pub(crate) fn server(
    call: &Call<'_>,
    stream: ValueView<'_>,
    config: ValueView<'_>,
    options: Option<ValueView<'_>>,
) -> Result<Outcome<Owned<TlsStream>>> {
    const WHAT: &str = "dream.tls.server";
    let tcp = tcp_stream(WHAT, stream)?;
    let config = crate::userdata::receiver::<ServerConfig>(config)
        .map(|config| Arc::clone(&config.0))
        .ok_or_else(|| config.field_type_error(WHAT, "a dream.tls.ServerConfig"))?;
    let mut common_options =
        Common { limit: DEFAULT_BUFFER_LIMIT, timeout: Duration::from_millis(u64::from(DEFAULT_HANDSHAKE_TIMEOUT_MS)) };
    if let Some(options) = options.filter(|view| !view.is_nil()) {
        Options::read(call, options, WHAT, |o| {
            common_options = common(o, WHAT)?;
            Ok(())
        })?;
    }
    let conn = match rustls::ServerConnection::new(config) {
        Ok(conn) => conn,
        Err(error) => {
            return Ok(Outcome::Failed(Failure {
                message: Cow::Owned(format!("{WHAT}: {error}")),
                kind: tls_kind(&error),
            }));
        }
    };
    let taken = tcp.take(WHAT)?;
    Ok(Outcome::Done(Owned(TlsStream::new(taken, conn.into(), common_options.limit, common_options.timeout, None))))
}

fn version_name(version: rustls::ProtocolVersion) -> Option<&'static str> {
    match version {
        rustls::ProtocolVersion::TLSv1_3 => Some("1.3"),
        rustls::ProtocolVersion::TLSv1_2 => Some("1.2"),
        _ => None,
    }
}

pub(crate) fn describe(d: &mut ExtensionDescriptor) {
    let mut stream = d.userdata::<TlsStream>(TlsStream::NAME);
    stream.tag(TagPolicy::Preferred).doc(
        "A verified TLS session over a TCP socket it owns exclusively, driven by the script's calls; never waits.",
    );
    stream
        .method("handshake", |s: &TlsStream| s.handshake())
        .signature("(self): (boolean?, string?, dream_tls_ErrorKind?)")
        .doc("Moves the handshake as far as the socket allows: true once authenticated, false while it needs the socket again, or nil, a message and the kind when it failed (the socket is then shut both ways; close() releases it).");
    stream
        .method("readInto", |s: &TlsStream, buffer: BufferView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
            s.read_into(buffer, offset, length)
        })
        .signature("(self, target: buffer, offset: number?, length: number?): (number?, string?, dream_tls_ErrorKind?)")
        .doc("Decrypted bytes into target at offset (at most length, at least 1). Returns the count; 0 only after the peer's close_notify; 'wouldBlock' when nothing is ready; 'truncated' when the connection ended without close_notify.");
    stream
        .method("write", |s: &TlsStream, data: BytesView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
            s.write(data, offset, length)
        })
        .signature("(self, data: buffer | string, offset: number?, length: number?): (number?, string?, dream_tls_ErrorKind?)")
        .doc("Encrypts as much of the range as the bounded send buffer takes and returns that plaintext count (not bytes on the wire); ciphertext goes to the socket at once as far as it takes it. 'wouldBlock' when the buffer is full.");
    stream
        .method("flush", |s: &TlsStream| s.flush())
        .signature("(self): (boolean?, string?, dream_tls_ErrorKind?)")
        .doc("Writes pending ciphertext: true once none is left in the session (handed to the OS, not necessarily received), false when the socket is full.");
    stream
        .method("shutdownWrite", |s: &TlsStream| s.shutdown_write())
        .signature("(self): (boolean?, string?, dream_tls_ErrorKind?)")
        .doc("Queues close_notify and flushes; once it has left, the TCP write half is shut. Reading stays open. true when flushed.");
    stream
        .method("close", |s: &TlsStream| s.close())
        .signature("(self)")
        .doc("Closes the socket and drops its registration at once, without close_notify. Closing twice does nothing.");
    stream.getter("state", |s: &TlsStream| s.state().name()).signature("dream_tls_State");
    stream.getter("handshaking", |s: &TlsStream| s.state() == TlsState::Handshaking).signature("boolean");
    stream.getter("closed", |s: &TlsStream| s.state() == TlsState::Closed).signature("boolean");
    stream
        .getter("wantsRead", |s: &TlsStream| s.conn.borrow().wants_read())
        .signature("boolean")
        .doc("Whether the session needs bytes from the peer to make progress.");
    stream
        .getter("wantsWrite", |s: &TlsStream| s.conn.borrow().wants_write())
        .signature("boolean")
        .doc("Whether ciphertext is waiting for the socket.");
    stream
        .getter("alpn", |s: &TlsStream| {
            s.conn.borrow().alpn_protocol().map(|p| String::from_utf8_lossy(p).into_owned())
        })
        .signature("string?");
    stream
        .getter("protocolVersion", |s: &TlsStream| s.conn.borrow().protocol_version().and_then(version_name))
        .signature("string?");
    stream
        .getter("cipherSuite", |s: &TlsStream| {
            s.conn.borrow().negotiated_cipher_suite().and_then(|suite| suite.suite().as_str())
        })
        .signature("string?");
    stream
        .getter("serverName", |s: &TlsStream| match &*s.conn.borrow() {
            rustls::Connection::Client(_) => s.server_name.clone(),
            rustls::Connection::Server(server) => server.server_name().map(str::to_owned),
        })
        .signature("string?")
        .doc("A client's: the identity it verifies. A server's: the name the client asked for (SNI), if any.");
    stream.getter("peerAddress", |s: &TlsStream| s.peer.to_string()).signature("string");
    stream.getter("localAddress", |s: &TlsStream| s.local.map(|a| a.to_string())).signature("string?");
    stream.metamethod("__tostring", |s: &TlsStream| format!("dream.tls.Stream({}, {})", s.peer, s.state().name()));
}
