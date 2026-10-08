//! `dream.tcp.Listener` and `dream.tcp.Stream`: nonblocking sockets owned by their userdata.
//!
//! Each handle keeps its socket in an [`Io`] behind an `Rc` only the userdata holds. A poller
//! watching the handle keeps a `Weak` to the `Io` and shares its readiness cell, so a watched
//! handle that is collected still closes its socket, and its registration goes with it.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::rc::{Rc, Weak};

use crate::bind::{Call, StackResults};
use crate::convert::{BufferView, BytesView, Exact};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::outcome::{Failure, Outcome};
use crate::stack::{Scope, ValueView};
use crate::userdata::Userdata;

use super::ListenOptions;
use super::poller::Core;

/// Readiness bits: the handle may read (or accept, or has an error to report).
pub(super) const READABLE: u8 = 1;
/// The handle may write (or a connect has finished, either way).
pub(super) const WRITABLE: u8 = 2;
/// The peer will send nothing more: end of stream, a hang-up or a socket error. Never cleared.
pub(super) const CLOSED: u8 = 4;

/// A socket a poller can register.
pub(super) enum Socket {
    Listener(mio::net::TcpListener),
    Stream(mio::net::TcpStream),
}

impl Socket {
    /// The readiness the OS is asked to report: everything the socket can have. What a script
    /// is told is filtered by its interest at wait time, so changing interest costs no syscall.
    fn os_interest(&self) -> mio::Interest {
        match self {
            Socket::Listener(_) => mio::Interest::READABLE,
            Socket::Stream(_) => mio::Interest::READABLE | mio::Interest::WRITABLE,
        }
    }

    pub(super) fn register(&mut self, registry: &mio::Registry, token: mio::Token) -> io::Result<()> {
        let interest = self.os_interest();
        match self {
            Socket::Listener(listener) => registry.register(listener, token, interest),
            Socket::Stream(stream) => registry.register(stream, token, interest),
        }
    }

    pub(super) fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        match self {
            Socket::Listener(listener) => registry.deregister(listener),
            Socket::Stream(stream) => registry.deregister(stream),
        }
    }
}

/// Which poller slot watches a handle.
pub(super) struct Watch {
    pub(super) poller: Weak<Core>,
    pub(super) slot: u32,
}

/// A handle's socket, its readiness and its registration.
pub(super) struct Io {
    socket: RefCell<Option<Socket>>,
    /// Set by the poller from OS events; READABLE and WRITABLE are cleared here when an
    /// operation reports would-block, which is what makes edge notifications level-triggered.
    pub(super) ready: Rc<Cell<u8>>,
    pub(super) watch: RefCell<Option<Watch>>,
}

impl Io {
    fn new(socket: Socket) -> Rc<Io> {
        Rc::new(Io { socket: RefCell::new(Some(socket)), ready: Rc::new(Cell::new(0)), watch: RefCell::new(None) })
    }

    pub(super) fn is_closed(&self) -> bool {
        self.socket.borrow().is_none()
    }

    /// Runs `body` on the open socket; `None` once closed.
    pub(super) fn with_socket<R>(&self, body: impl FnOnce(&mut Socket) -> R) -> Option<R> {
        self.socket.borrow_mut().as_mut().map(body)
    }

    fn clear(&self, bits: u8) {
        self.ready.set(self.ready.get() & !bits);
    }

    /// Releases the registration and the socket. Idempotent.
    fn close(&self) {
        let watch = self.watch.borrow_mut().take();
        let socket = self.socket.borrow_mut().take();
        if let (Some(watch), Some(mut socket)) = (watch, socket)
            && let Some(core) = watch.poller.upgrade()
        {
            core.release(watch.slot, &mut socket);
        }
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        self.close();
    }
}

/// A would-block answer: a constant message, so the hot miss allocates nothing.
fn would_block(message: &'static str) -> Failure {
    Failure { message: Cow::Borrowed(message), kind: "wouldBlock" }
}

/// An OS refusal of `what` on the socket at `address`.
fn refused(what: &str, address: Option<SocketAddr>, error: &io::Error) -> Failure {
    let message = match address {
        Some(address) => format!("dream.tcp.{what}: {address}: {error}"),
        None => format!("dream.tcp.{what}: {error}"),
    };
    Failure { message: Cow::Owned(message), kind: crate::outcome::network_kind_of(error) }
}

/// The window `(offset, length)` of `len` bytes, defaults `0` and the space left, each argument
/// checked the way `@dream/fs` checks a buffer window.
fn window(
    what: &str,
    noun: &str,
    len: usize,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<(usize, usize)> {
    let non_negative = |name: &str, value: Exact<i64>| {
        usize::try_from(value.0)
            .map_err(|_| Error::runtime(format!("dream.tcp.{what}: {name} {} is negative", value.0)))
    };
    let offset = offset.map(|offset| non_negative("offset", offset)).transpose()?.unwrap_or(0);
    if offset > len {
        return Err(Error::runtime(format!(
            "dream.tcp.{what}: offset {offset} past the end of the {noun} (size {len})"
        )));
    }
    let space = len - offset;
    let length = length.map(|length| non_negative("length", length)).transpose()?.unwrap_or(space);
    if length > space {
        return Err(Error::runtime(format!(
            "dream.tcp.{what}: length {length} does not fit the {noun} (space {space} after offset {offset})"
        )));
    }
    Ok((offset, length))
}

// ---------------------------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------------------------

/// The accepted streams of one listener that are still open, against its `maxStreams`.
struct StreamCount {
    live: Cell<u32>,
    max: u32,
    /// The listener's readiness: freeing a stream at the limit marks it readable again, since
    /// the connections that waited in the backlog raised no new notification.
    listener_ready: Rc<Cell<u8>>,
}

/// `dream.tcp.Listener`: a nonblocking listening socket.
pub struct Listener {
    io: Rc<Io>,
    local: SocketAddr,
    no_delay: bool,
    streams: Rc<StreamCount>,
}

// SAFETY: plain Rust state with no Lua references; dropping it closes a socket and touches no
// Lua API.
unsafe impl Userdata for Listener {
    const NAME: &'static str = "dream.tcp.Listener";
}

impl Listener {
    pub(super) fn bind(address: SocketAddr, options: &ListenOptions) -> io::Result<Listener> {
        use socket2::{Domain, Protocol, Socket as RawSocket, Type};
        let socket = RawSocket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
        // Windows' SO_REUSEADDR lets another socket take a port in use; there a closed
        // listener's port can be bound again without it, so the option only applies elsewhere.
        #[cfg(not(windows))]
        socket.set_reuse_address(options.reuse_address)?;
        #[cfg(windows)]
        let _ = options.reuse_address;
        socket.set_nonblocking(true)?;
        socket.bind(&address.into())?;
        // The backlog is at most 65535 (checked when read), so it fits a c_int.
        socket.listen(options.backlog.try_into().unwrap_or(i32::MAX))?;
        let listener: std::net::TcpListener = socket.into();
        let local = listener.local_addr()?;
        Ok(Listener::new(mio::net::TcpListener::from_std(listener), local, options.no_delay, options.max_streams))
    }

    fn new(listener: mio::net::TcpListener, local: SocketAddr, no_delay: bool, max_streams: u32) -> Listener {
        let io = Io::new(Socket::Listener(listener));
        let streams =
            Rc::new(StreamCount { live: Cell::new(0), max: max_streams, listener_ready: Rc::clone(&io.ready) });
        Listener { io, local, no_delay, streams }
    }

    /// Wraps a listener the host bound, for scripts that hold no listen capability: it is made
    /// nonblocking, and at most `max_streams` of the streams it accepts may be open at once.
    pub fn from_std(listener: std::net::TcpListener, max_streams: u32) -> io::Result<Listener> {
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        Ok(Listener::new(mio::net::TcpListener::from_std(listener), local, false, max_streams.max(1)))
    }

    /// Pushes a listener onto `scope` (the `dream.tcp` extension must be installed).
    pub fn push<'s>(scope: &'s impl Scope, listener: Listener) -> Result<ValueView<'s>> {
        crate::userdata::push_owned(scope, listener)
    }

    /// The address it is bound to.
    pub fn local_address(&self) -> SocketAddr {
        self.local
    }

    /// Whether it was closed.
    pub fn is_closed(&self) -> bool {
        self.io.is_closed()
    }

    pub(super) fn io(&self) -> &Rc<Io> {
        &self.io
    }

    /// Releases the socket and its registration; the accepted streams stay open.
    pub fn close(&self) {
        self.io.close();
    }

    fn accept(&self, call: &Call<'_>) -> Result<Outcome<StackResults>> {
        let streams = &self.streams;
        if streams.live.get() >= streams.max {
            self.io.clear(READABLE);
            return Ok(Outcome::Failed(Failure {
                message: Cow::Owned(format!(
                    "dream.tcp.Listener.accept: {} accepted streams are open, the listener's maxStreams",
                    streams.max
                )),
                kind: "limitReached",
            }));
        }
        let accepted = self.io.with_socket(|socket| {
            let Socket::Listener(listener) = socket else { unreachable!("a listener's socket is a listener") };
            loop {
                match listener.accept() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => return result,
                }
            }
        });
        let (stream, peer) = match accepted {
            None => return Err(closed("Listener.accept", "listener")),
            Some(Ok(accepted)) => accepted,
            Some(Err(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear(READABLE);
                return Ok(Outcome::Failed(would_block("dream.tcp.Listener.accept: no connection is waiting")));
            }
            Some(Err(error)) => return Ok(Outcome::Failed(refused("Listener.accept", Some(self.local), &error))),
        };
        if let Err(error) = configure_accepted(&stream, self.no_delay) {
            return Ok(Outcome::Failed(refused("Listener.accept", Some(peer), &error)));
        }
        streams.live.set(streams.live.get() + 1);
        let local = stream.local_addr().ok();
        let stream = Stream::new(stream, StreamState::Connected, local, peer, Some(Rc::clone(streams)));
        crate::userdata::push_owned(call, stream)?;
        call.push(peer.to_string().as_str())?;
        Ok(Outcome::Done(StackResults))
    }
}

/// Options an accepted stream gets: Nagle off when the listener asked, and on Apple platforms
/// no SIGPIPE (Linux sends with MSG_NOSIGNAL; an accepted socket there does not inherit it).
fn configure_accepted(stream: &mio::net::TcpStream, no_delay: bool) -> io::Result<()> {
    if no_delay {
        stream.set_nodelay(true)?;
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    socket2::SockRef::from(stream).set_nosigpipe(true)?;
    Ok(())
}

fn closed(what: &str, noun: &str) -> Error {
    Error::runtime(format!("dream.tcp.{what}: the {noun} is closed"))
}

pub(super) fn describe_listener(d: &mut ExtensionDescriptor) {
    let mut listener = d.userdata::<Listener>(Listener::NAME);
    listener.tag(TagPolicy::Preferred).doc("A nonblocking TCP listener. Accepted streams carry its authority.");
    listener
        .method("accept", |l: &Listener, call: &Call<'_>| l.accept(call))
        .signature("(self): (dream_tcp_Stream?, string?, dream_tcp_ErrorKind?)")
        .doc("The next waiting connection and its peer address, or nil, a message and 'wouldBlock' when none is waiting ('limitReached' at maxStreams). Never waits.");
    listener
        .method("close", |l: &Listener| l.close())
        .signature("(self)")
        .doc("Closes the socket and drops its registration. Accepted streams stay open; closing twice does nothing.");
    listener.getter("localAddress", |l: &Listener| l.local.to_string()).signature("string");
    listener.getter("closed", |l: &Listener| l.is_closed()).signature("boolean");
    listener.getter("streams", |l: &Listener| f64::from(l.streams.live.get())).signature("number");
    listener.metamethod("__tostring", |l: &Listener| {
        format!("dream.tcp.Listener({}{})", l.local, if l.is_closed() { ", closed" } else { "" })
    });
}

// ---------------------------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------------------------

/// Where a stream is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamState {
    /// A connect is in progress.
    Connecting,
    /// Bytes can flow.
    Connected,
    /// The connect failed; every transfer reports that failure.
    Failed,
    /// Closed locally; every method but `close` raises.
    Closed,
}

impl StreamState {
    /// The name scripts see.
    pub fn name(self) -> &'static str {
        match self {
            StreamState::Connecting => "connecting",
            StreamState::Connected => "connected",
            StreamState::Failed => "failed",
            StreamState::Closed => "closed",
        }
    }
}

/// What a transfer may do after the connect state is settled.
enum Gate {
    Open,
    Pending,
    Failed(Failure),
}

/// `dream.tcp.Stream`: a nonblocking TCP byte stream.
pub struct Stream {
    io: Rc<Io>,
    state: Cell<StreamState>,
    /// A failed connect, reported again by every later transfer.
    failure: RefCell<Option<(String, &'static str)>>,
    local: Cell<Option<SocketAddr>>,
    peer: SocketAddr,
    /// The accepting listener's count, released once when the stream closes.
    counted: RefCell<Option<Rc<StreamCount>>>,
}

// SAFETY: as `Listener`.
unsafe impl Userdata for Stream {
    const NAME: &'static str = "dream.tcp.Stream";
}

impl Stream {
    fn new(
        stream: mio::net::TcpStream,
        state: StreamState,
        local: Option<SocketAddr>,
        peer: SocketAddr,
        counted: Option<Rc<StreamCount>>,
    ) -> Stream {
        Stream {
            io: Io::new(Socket::Stream(stream)),
            state: Cell::new(state),
            failure: RefCell::new(None),
            local: Cell::new(local),
            peer,
            counted: RefCell::new(counted),
        }
    }

    /// Starts a nonblocking connect; the stream is `Connecting` unless the OS finished at once.
    pub(super) fn connect(address: SocketAddr, no_delay: bool) -> io::Result<Stream> {
        let stream = mio::net::TcpStream::connect(address)?;
        if no_delay {
            stream.set_nodelay(true)?;
        }
        let local = stream.local_addr().ok();
        Ok(Stream::new(stream, StreamState::Connecting, local, address, None))
    }

    /// Wraps a connected stream the host made, for scripts that hold no connect capability.
    pub fn from_std(stream: std::net::TcpStream) -> io::Result<Stream> {
        stream.set_nonblocking(true)?;
        let peer = stream.peer_addr()?;
        let local = stream.local_addr().ok();
        let stream = mio::net::TcpStream::from_std(stream);
        configure_accepted(&stream, false)?;
        Ok(Stream::new(stream, StreamState::Connected, local, peer, None))
    }

    /// Pushes a stream onto `scope` (the `dream.tcp` extension must be installed).
    pub fn push<'s>(scope: &'s impl Scope, stream: Stream) -> Result<ValueView<'s>> {
        crate::userdata::push_owned(scope, stream)
    }

    /// Where the stream is in its life.
    pub fn state(&self) -> StreamState {
        self.state.get()
    }

    /// The remote address.
    pub fn peer_address(&self) -> SocketAddr {
        self.peer
    }

    /// The local address, once the OS assigned one.
    pub fn local_address(&self) -> Option<SocketAddr> {
        self.local.get()
    }

    pub(super) fn io(&self) -> &Rc<Io> {
        &self.io
    }

    /// Releases the socket, its registration and its place in the listener's count.
    pub fn close(&self) {
        self.io.close();
        self.state.set(StreamState::Closed);
        self.uncount();
    }

    fn uncount(&self) {
        if let Some(count) = self.counted.borrow_mut().take() {
            let live = count.live.get();
            if live == count.max {
                count.listener_ready.set(count.listener_ready.get() | READABLE);
            }
            count.live.set(live.saturating_sub(1));
        }
    }

    fn with_stream<R>(&self, what: &str, body: impl FnOnce(&mio::net::TcpStream) -> R) -> Result<R> {
        self.io
            .with_socket(|socket| {
                let Socket::Stream(stream) = socket else { unreachable!("a stream's socket is a stream") };
                body(stream)
            })
            .ok_or_else(|| closed(what, "stream"))
    }

    fn fail(&self, failure: Failure) -> Failure {
        self.state.set(StreamState::Failed);
        *self.failure.borrow_mut() = Some((failure.message.to_string(), failure.kind));
        failure
    }

    fn stored_failure(&self) -> Failure {
        let failure = self.failure.borrow();
        let (message, kind) = failure.as_ref().expect("a failed stream keeps its failure");
        Failure { message: Cow::Owned(message.clone()), kind }
    }

    /// Settles a pending connect by the socket's own error, never by writability alone: the
    /// pending error first, then whether the socket has a peer.
    fn settle(&self, what: &str) -> Result<Gate> {
        match self.state.get() {
            StreamState::Closed => return Err(closed(what, "stream")),
            StreamState::Connected => return Ok(Gate::Open),
            StreamState::Failed => return Ok(Gate::Failed(self.stored_failure())),
            StreamState::Connecting => {}
        }
        let settled = self.with_stream(what, |stream| match stream.take_error() {
            Ok(Some(error)) | Err(error) => Err(error),
            Ok(None) => match stream.peer_addr() {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(false),
                Err(error) => Err(error),
            },
        })?;
        Ok(match settled {
            Ok(true) => {
                self.state.set(StreamState::Connected);
                if let Ok(local) = self.with_stream(what, mio::net::TcpStream::local_addr)? {
                    self.local.set(Some(local));
                }
                Gate::Open
            }
            Ok(false) => {
                self.io.clear(WRITABLE);
                Gate::Pending
            }
            Err(error) => Gate::Failed(self.fail(refused("Stream.connect", Some(self.peer), &error))),
        })
    }

    fn finish_connect(&self) -> Result<Outcome<bool>> {
        Ok(match self.settle("Stream.finishConnect")? {
            Gate::Open => Outcome::Done(true),
            Gate::Pending => Outcome::Done(false),
            Gate::Failed(failure) => Outcome::Failed(failure),
        })
    }

    fn read_into(
        &self,
        mut buffer: BufferView<'_>,
        offset: Option<Exact<i64>>,
        length: Option<Exact<i64>>,
    ) -> Result<Outcome<f64>> {
        const WHAT: &str = "Stream.readInto";
        let (offset, length) = window(WHAT, "buffer", buffer.len(), offset, length)?;
        if length == 0 {
            return Err(Error::runtime(
                "dream.tcp.Stream.readInto: no room to read into; a zero-byte read would look like end of stream",
            ));
        }
        match self.settle(WHAT)? {
            Gate::Open => {}
            Gate::Pending => return Ok(Outcome::Failed(would_block("dream.tcp.Stream.readInto: still connecting"))),
            Gate::Failed(failure) => return Ok(Outcome::Failed(failure)),
        }
        // SAFETY: the window is inside the buffer and this call holds the only view of it; the
        // OS writes the slice during `recv` and nothing keeps its address after this call.
        let dst = unsafe { &mut buffer.bytes_mut_unchecked()[offset..offset + length] };
        let read = self.with_stream(WHAT, |mut stream| {
            loop {
                match stream.read(dst) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => return result,
                }
            }
        })?;
        Ok(match read {
            Ok(count) => Outcome::Done(count as f64),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear(READABLE);
                Outcome::Failed(would_block("dream.tcp.Stream.readInto: no bytes are ready"))
            }
            Err(error) => Outcome::Failed(refused(WHAT, Some(self.peer), &error)),
        })
    }

    fn write(
        &self,
        data: BytesView<'_>,
        offset: Option<Exact<i64>>,
        length: Option<Exact<i64>>,
    ) -> Result<Outcome<f64>> {
        const WHAT: &str = "Stream.write";
        let (offset, length) = window(WHAT, "data", data.len(), offset, length)?;
        match self.settle(WHAT)? {
            Gate::Open => {}
            Gate::Pending => return Ok(Outcome::Failed(would_block("dream.tcp.Stream.write: still connecting"))),
            Gate::Failed(failure) => return Ok(Outcome::Failed(failure)),
        }
        if length == 0 {
            return Ok(Outcome::Done(0.0));
        }
        // SAFETY: a string's bytes are immutable; a buffer's are only read, by `send`, through
        // this call's one view, and nothing keeps their address after this call.
        let src = unsafe { &data.bytes_unchecked()[offset..offset + length] };
        let written = self.with_stream(WHAT, |mut stream| {
            loop {
                match stream.write(src) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => return result,
                }
            }
        })?;
        Ok(match written {
            Ok(count) => Outcome::Done(count as f64),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear(WRITABLE);
                Outcome::Failed(would_block("dream.tcp.Stream.write: the send buffer is full"))
            }
            Err(error) => Outcome::Failed(refused(WHAT, Some(self.peer), &error)),
        })
    }

    fn shutdown(&self, how: &str) -> Result<Outcome<bool>> {
        const WHAT: &str = "Stream.shutdown";
        let how = match how {
            "read" => Shutdown::Read,
            "write" => Shutdown::Write,
            "both" => Shutdown::Both,
            other => {
                return Err(Error::runtime(format!(
                    "dream.tcp.Stream.shutdown: expected 'read', 'write' or 'both', got '{other}'"
                )));
            }
        };
        match self.settle(WHAT)? {
            Gate::Open => {}
            Gate::Pending => return Ok(Outcome::Failed(would_block("dream.tcp.Stream.shutdown: still connecting"))),
            Gate::Failed(failure) => return Ok(Outcome::Failed(failure)),
        }
        Ok(match self.with_stream(WHAT, |stream| stream.shutdown(how))? {
            Ok(()) => Outcome::Done(true),
            Err(error) => Outcome::Failed(refused(WHAT, Some(self.peer), &error)),
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.uncount();
    }
}

pub(super) fn describe_stream(d: &mut ExtensionDescriptor) {
    let mut stream = d.userdata::<Stream>(Stream::NAME);
    stream.tag(TagPolicy::Preferred).doc("A nonblocking TCP byte stream: no framing, no queue, no hidden waits.");
    stream
        .method("readInto", |s: &Stream, buffer: BufferView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
            s.read_into(buffer, offset, length)
        })
        .signature("(self, target: buffer, offset: number?, length: number?): (number?, string?, dream_tcp_ErrorKind?)")
        .doc("Receives at most length bytes (default the space after offset, at least 1) into target at offset. Returns the count, 0 at end of stream, or nil, a message and 'wouldBlock' when nothing is ready.");
    stream
        .method("write", |s: &Stream, data: BytesView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
            s.write(data, offset, length)
        })
        .signature("(self, data: buffer | string, offset: number?, length: number?): (number?, string?, dream_tcp_ErrorKind?)")
        .doc("Hands the OS the range once and returns how many bytes it took, possibly fewer than given; nil, a message and 'wouldBlock' when it took none. An empty range returns 0.");
    stream
        .method("finishConnect", |s: &Stream| s.finish_connect())
        .signature("(self): (boolean?, string?, dream_tcp_ErrorKind?)")
        .doc("true once connected, false while the connect is pending, or nil, a message and the kind when it failed (checked through the socket's error, not writability).");
    stream
        .method("shutdown", |s: &Stream, how: &str| s.shutdown(how))
        .signature("(self, how: dream_tcp_Shutdown): (boolean?, string?, dream_tcp_ErrorKind?)")
        .doc(
            "Half-closes the stream: 'write' sends end of stream and leaves reading open; 'read' and 'both' as named.",
        );
    stream
        .method("close", |s: &Stream| s.close())
        .signature("(self)")
        .doc("Closes the socket and drops its registration at once. Closing twice does nothing.");
    stream.getter("localAddress", |s: &Stream| s.local.get().map(|a| a.to_string())).signature("string?");
    stream.getter("peerAddress", |s: &Stream| s.peer.to_string()).signature("string");
    stream.getter("state", |s: &Stream| s.state.get().name()).signature("dream_tcp_StreamState");
    stream.getter("closed", |s: &Stream| s.state.get() == StreamState::Closed).signature("boolean");
    stream.metamethod("__tostring", |s: &Stream| format!("dream.tcp.Stream({}, {})", s.peer, s.state.get().name()));
}
