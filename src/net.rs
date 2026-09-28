//! The dream-net bridge: extension `dream.net`, module `@dream/net`, types `dream.net.Server`,
//! `dream.net.Client`, `dream.net.Schema`.
//!
//! Networking is runtime infrastructure here, not a feature. dream-net stays pure Rust and
//! knows peers, event ids, channels, and bytes; this module owns the Luau-facing shape:
//!
//! - peer, event, channel, and client ids are Luau integers (exact, compared with `==`),
//!   never strings or doubles; sizes and counters are plain numbers, because Luau integers
//!   have no `<`/`<=` operators (the `integer` library orders them) and counters get
//!   thresholded;
//! - payloads are Luau buffers (or strings on send); `pollInto` copies one received payload
//!   into a caller-owned buffer and `sendEvent` copies out of one before returning, so no Lua
//!   memory is ever retained by the transport;
//! - the transport clock is host-controlled: `update()` reads a clock the host installed, so a
//!   script cannot spoof time;
//! - the server private key never reaches Luau: servers are created by the host in Rust and
//!   handed to scripts as [`Server`] handles; clients may be created from Luau only when the
//!   runtime policy grants the `network.transport` capability;
//! - everything is polled during the host's network phase; nothing calls into Luau from inside
//!   dream-net.
//!
//! `pollInto` is deliberately a tuple interface (`kind, peer, a, b, c`) so a frame of events
//! allocates nothing; an engine event layer above turns it into something pleasant.

use std::cell::RefCell;
use std::ffi::c_int;
use std::rc::Rc;
use std::time::Instant;

use dream_net::{
    ChannelConfig, ChannelId, Client, ClientConfig, ClientEvent, ClientStatus, Delivery, EventTypeId, OverflowPolicy,
    PeerId, Schema, SchemaBuilder, SendError, ServerEvent, TransportConfig,
};

use crate::bind::{Call, Return};
use crate::convert::{Bits64, Exact, BufferView, BytesView, Integer, Push};
use crate::direct::field::{DirectField, FieldValue};
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext, TagPolicy};
use crate::options::Options;
use crate::stack::{Scope, ValueView};
use crate::userdata::{Owned, Userdata};
use crate::value::Table;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.net";
/// The module path.
pub const MODULE: &str = "@dream/net";
/// The capability that lets scripts create transport objects (`net.client`).
pub const TRANSPORT_CAPABILITY: &str = "network.transport";

/// A monotonic clock the transport reads on `update()`; seconds as f64.
pub type Clock = Rc<dyn Fn() -> f64>;

/// A clock counting seconds since it was created.
pub fn monotonic_clock() -> Clock {
    let start = Instant::now();
    Rc::new(move || start.elapsed().as_secs_f64())
}

// ---------------------------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------------------------

/// `PeerId` as the Luau integer scripts see (the u64 bit pattern).
#[inline]
fn peer_to_int(peer: PeerId) -> i64 {
    peer.0 as i64
}

/// Peer ids are opaque 64-bit patterns (`Bits64`): all bits cross, none are numbers.
#[inline]
fn peer_from_int(value: Bits64) -> PeerId {
    PeerId(value.0)
}

fn event_from_int(value: Exact<i64>) -> Result<EventTypeId> {
    u32::try_from(value.0).map(EventTypeId).map_err(|_| Error::runtime(format!("event id {} is out of range", value.0)))
}

fn send_error(error: SendError) -> Error {
    Error::runtime(format!("dream.net: {error}"))
}

fn config_error(error: impl std::fmt::Display) -> Error {
    Error::runtime(format!("dream.net: {error}"))
}

// ---------------------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------------------

/// A frozen wire schema (`dream.net.Schema`).
pub struct NetSchema(pub Schema);

// SAFETY: plain Rust data (an `Arc`), no Lua references, no Lua API in `Drop`.
unsafe impl Userdata for NetSchema {
    const NAME: &'static str = "dream.net.Schema";
}

/// `net.schema{ version = 1, channels = { {name, delivery, capacity?, overflow?} }, events = {
/// {name, channel, maxPayload, codecVersion?} } }`.
fn build_schema(call: &Call<'_>, options: ValueView<'_>) -> Result<Owned<NetSchema>> {
    let schema = Options::read(call, options, "net.schema", |o| {
        let version = o.required::<Exact<i64>>("version")?.0;
        let version = u32::try_from(version).map_err(|_| Error::runtime("net.schema.version: must fit in 32 bits"))?;
        let mut builder = SchemaBuilder::new(version);
        if let Some(max) = o.optional::<Exact<i64>>("maxMessagesPerPacket")? {
            let max =
                u32::try_from(max.0).map_err(|_| Error::runtime("net.schema.maxMessagesPerPacket: out of range"))?;
            builder = builder.max_messages_per_packet(max);
        }
        let channels = Table::from_value(o.required::<crate::value::Value>("channels")?)
            .map_err(|_| Error::runtime("net.schema.channels: expected a table"))?;
        let events = Table::from_value(o.required::<crate::value::Value>("events")?)
            .map_err(|_| Error::runtime("net.schema.events: expected a table"))?;
        let mut channel_ids: Vec<(String, ChannelId)> = Vec::new();
        o.frame().with_frame(|frame| {
            let channels = channels.push_to(frame)?;
            let count = channels.raw_len();
            for index in 1..=count {
                let entry = channels.raw_get_index(frame, index as i64)?;
                let context = format!("net.schema.channels[{index}]");
                let config = Options::read(frame, entry, &context, |c| {
                    let name: String = c.required("name")?;
                    let delivery: String = c.required("delivery")?;
                    let mut config = match delivery.as_str() {
                        "reliableOrdered" => ChannelConfig::reliable_ordered(name.clone()),
                        "unreliableUnordered" => ChannelConfig::unreliable_unordered(name.clone()),
                        other => {
                            return Err(Error::runtime(format!(
                                "{context}.delivery: expected 'reliableOrdered' or 'unreliableUnordered', got '{other}'"
                            )));
                        }
                    };
                    if let Some(capacity) = c.optional::<Exact<i64>>("capacity")? {
                        let capacity = u16::try_from(capacity.0)
                            .map_err(|_| Error::runtime(format!("{context}.capacity: out of range")))?;
                        config = config.with_capacity(capacity);
                    }
                    if let Some(overflow) = c.optional::<String>("overflow")? {
                        config = config.with_overflow(match overflow.as_str() {
                            "fail" => OverflowPolicy::Fail,
                            "dropOldest" => OverflowPolicy::DropOldest,
                            "dropNewest" => OverflowPolicy::DropNewest,
                            other => {
                                return Err(Error::runtime(format!(
                                    "{context}.overflow: expected 'fail', 'dropOldest', or 'dropNewest', got '{other}'"
                                )));
                            }
                        });
                    }
                    if let Some(budget) = c.optional::<Exact<i64>>("packetBudget")? {
                        let budget = u32::try_from(budget.0)
                            .map_err(|_| Error::runtime(format!("{context}.packetBudget: out of range")))?;
                        config = config.with_packet_budget(budget);
                    }
                    if let Some(interval) = c.optional::<f64>("resendInterval")? {
                        config = config.with_resend_interval(interval);
                    }
                    Ok((name, config))
                })?;
                let id = builder.channel(config.1).map_err(config_error)?;
                channel_ids.push((config.0, id));
            }
            let events = events.push_to(frame)?;
            let count = events.raw_len();
            for index in 1..=count {
                let entry = events.raw_get_index(frame, index as i64)?;
                let context = format!("net.schema.events[{index}]");
                Options::read(frame, entry, &context, |e| {
                    let name: String = e.required("name")?;
                    let channel: String = e.required("channel")?;
                    let max_payload = e.required::<Exact<i64>>("maxPayload")?.0;
                    let max_payload = u32::try_from(max_payload)
                        .map_err(|_| Error::runtime(format!("{context}.maxPayload: out of range")))?;
                    let codec = e.optional::<Exact<i64>>("codecVersion")?.map(|codec| codec.0);
                    let Some((_, id)) = channel_ids.iter().find(|(n, _)| *n == channel) else {
                        return Err(Error::runtime(format!("{context}.channel: unknown channel '{channel}'")));
                    };
                    match codec {
                        Some(codec) => {
                            let codec = u32::try_from(codec)
                                .map_err(|_| Error::runtime(format!("{context}.codecVersion: out of range")))?;
                            builder.event_with_codec(name, *id, max_payload, codec).map_err(config_error)
                        }
                        None => builder.event(name, *id, max_payload).map_err(config_error),
                    }
                })?;
            }
            Ok(())
        })?;
        builder.build().map_err(config_error)
    })?;
    Ok(Owned(NetSchema(schema)))
}

// ---------------------------------------------------------------------------------------------
// Poll results
// ---------------------------------------------------------------------------------------------

/// What `pollInto` returns: `nil` when the queue is empty, else `kind, peer, a, b, c`.
///
/// Ids (peer, event, channel, client) are integers; `size` is a plain number, like every count.
///
/// | kind           | peer | a          | b         | c    |
/// |----------------|------|------------|-----------|------|
/// | `message`      | peer | eventId    | channel   | size |
/// | `connected`    | peer | clientId   |           |      |
/// | `disconnected` | peer | reason     |           |      |
/// | `rejected`     | peer | clientId   | reason    |      |
/// | `connectFailed`| 0    | reason     |           |      |
enum Polled {
    Empty,
    Message { peer: i64, event: i64, channel: i64, size: f64 },
    Connected { peer: i64, client_id: i64 },
    Disconnected { peer: i64, reason: &'static str },
    Rejected { peer: i64, client_id: i64, reason: &'static str },
    ConnectFailed { reason: &'static str },
}

impl Return for Polled {
    fn push_results(self, call: &Call<'_>) -> Result<c_int> {
        match self {
            Polled::Empty => Ok(0),
            Polled::Message { peer, event, channel, size } => {
                "message".push_into(call)?;
                Integer(peer).push_into(call)?;
                Integer(event).push_into(call)?;
                Integer(channel).push_into(call)?;
                size.push_into(call)?;
                Ok(5)
            }
            Polled::Connected { peer, client_id } => {
                "connected".push_into(call)?;
                Integer(peer).push_into(call)?;
                Integer(client_id).push_into(call)?;
                Ok(3)
            }
            Polled::Disconnected { peer, reason } => {
                "disconnected".push_into(call)?;
                Integer(peer).push_into(call)?;
                reason.push_into(call)?;
                Ok(3)
            }
            Polled::Rejected { peer, client_id, reason } => {
                "rejected".push_into(call)?;
                Integer(peer).push_into(call)?;
                Integer(client_id).push_into(call)?;
                reason.push_into(call)?;
                Ok(4)
            }
            Polled::ConnectFailed { reason } => {
                "connectFailed".push_into(call)?;
                Integer(0).push_into(call)?;
                reason.push_into(call)?;
                Ok(3)
            }
        }
    }
}

/// The bytes of a send: a buffer or string with an optional `offset, length` range.
fn payload_range<R>(
    bytes: BytesView<'_>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
    body: impl FnOnce(&[u8]) -> R,
) -> Result<R> {
    let total = bytes.len();
    let offset =
        usize::try_from(offset.map_or(0, |o| o.0)).map_err(|_| Error::runtime("dream.net: negative payload offset"))?;
    let length = match length {
        Some(length) => usize::try_from(length.0).map_err(|_| Error::runtime("dream.net: negative payload length"))?,
        None => {
            total.checked_sub(offset).ok_or_else(|| Error::runtime("dream.net: payload offset exceeds the buffer"))?
        }
    };
    if offset.checked_add(length).is_none_or(|end| end > total) {
        return Err(Error::runtime(format!(
            "dream.net: payload range {offset}..{} exceeds the {total}-byte buffer",
            offset.saturating_add(length)
        )));
    }
    // SAFETY: `body` is this module's own code handing the range to dream-net, which holds no
    // Lua handle and no other view; nothing writes the buffer while the slice lives.
    let all = unsafe { bytes.bytes_unchecked() };
    Ok(body(&all[offset..offset + length]))
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

/// A dream-net server as scripts see it (`dream.net.Server`). Created by the host in Rust,
/// which keeps the private key; pushed with [`Server::push`] or returned as `Owned<Server>`.
pub struct Server {
    inner: RefCell<dream_net::Server>,
    clock: Clock,
}

// SAFETY: `dream_net::Server` is plain Rust state with no Lua references; dropping it stops
// the transport without touching the Lua API.
unsafe impl Userdata for Server {
    const NAME: &'static str = "dream.net.Server";
}

impl Server {
    /// Wraps a server the host created, with the clock its `update()` reads.
    pub fn new(server: dream_net::Server, clock: Clock) -> Server {
        Server { inner: RefCell::new(server), clock }
    }

    /// Pushes a server handle onto `scope` (the `dream.net` extension must be installed).
    pub fn push<'s>(scope: &'s impl Scope, server: dream_net::Server, clock: Clock) -> Result<ValueView<'s>> {
        crate::userdata::push_owned(scope, Server::new(server, clock))
    }

    /// Runs `body` with the wrapped server borrowed mutably, for host code.
    pub fn with(&self, body: impl FnOnce(&mut dream_net::Server)) {
        body(&mut self.inner.borrow_mut());
    }

    fn poll_into(&self, mut buffer: BufferView<'_>) -> Result<Polled> {
        let mut server = self.inner.borrow_mut();
        // SAFETY: dream-net fills the slice and returns; it holds no Lua handle and no other
        // view of the buffer exists in this call, so the slice is the only access while it lives.
        let polled = server.poll_into(unsafe { buffer.bytes_mut_unchecked() });
        match polled {
            Ok(None) => Ok(Polled::Empty),
            Ok(Some(ServerEvent::Message { peer, event, channel, payload })) => Ok(Polled::Message {
                peer: peer_to_int(peer),
                event: i64::from(event.0),
                channel: i64::from(channel.0),
                size: payload as f64,
            }),
            Ok(Some(ServerEvent::Connected { peer, client_id })) => {
                Ok(Polled::Connected { peer: peer_to_int(peer), client_id: client_id as i64 })
            }
            Ok(Some(ServerEvent::Disconnected { peer, reason })) => {
                Ok(Polled::Disconnected { peer: peer_to_int(peer), reason: reason.name() })
            }
            Ok(Some(ServerEvent::Rejected { peer, client_id, failure })) => Ok(Polled::Rejected {
                peer: peer_to_int(peer),
                client_id: client_id as i64,
                reason: failure.reason().name(),
            }),
            Err(too_small) => Err(Error::runtime(format!(
                "dream.net: pollInto buffer of {} bytes is too small for a {}-byte payload",
                buffer.len(),
                too_small.needed
            ))),
        }
    }

    fn stat(&self, peer: Bits64, pick: impl Fn(&dream_net::ConnectionStats) -> f32) -> Option<f64> {
        self.inner.borrow().stats(peer_from_int(peer)).map(|stats| f64::from(pick(&stats)))
    }
}

fn counters_table(scope: &impl Scope, counters: &dream_net::Counters) -> Result<Table> {
    let table = Table::new(scope, 0, 16)?;
    let fields: [(&str, u64); 15] = [
        ("packetsSent", counters.packets_sent),
        ("datagramsSent", counters.datagrams_sent),
        ("bytesSent", counters.bytes_sent),
        ("datagramsReceived", counters.datagrams_received),
        ("bytesReceived", counters.bytes_received),
        ("packetsReceived", counters.packets_received),
        ("eventsQueued", counters.events_queued),
        ("eventsSent", counters.events_sent),
        ("eventsResent", counters.events_resent),
        ("eventsReceived", counters.events_received),
        ("eventsDroppedOnSend", counters.events_dropped_on_send),
        ("eventsDroppedOnReceive", counters.events_dropped_on_receive),
        ("duplicateEvents", counters.duplicate_events),
        ("malformedPackets", counters.malformed_packets),
        ("packetsRefused", counters.packets_refused),
    ];
    for (name, value) in fields {
        // Counters are plain numbers: Luau integers compare only for equality, and a counter is
        // something scripts threshold.
        table.set(scope, name, &(value as f64))?;
    }
    Ok(table)
}

fn memory_table(scope: &impl Scope, usage: &dream_net::MemoryUsage) -> Result<Table> {
    let table = Table::new(scope, 0, 5)?;
    table.set(scope, "connection", &(usage.connection as f64))?;
    table.set(scope, "reliable", &(usage.reliable as f64))?;
    table.set(scope, "send", &(usage.send as f64))?;
    table.set(scope, "receive", &(usage.receive as f64))?;
    table.set(scope, "total", &(usage.total() as f64))?;
    Ok(table)
}

// ---------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------

/// A dream-net client as scripts see it (`dream.net.Client`).
pub struct NetClient {
    inner: RefCell<Client>,
    clock: Clock,
}

// SAFETY: as `Server`.
unsafe impl Userdata for NetClient {
    const NAME: &'static str = "dream.net.Client";
}

impl NetClient {
    pub fn new(client: Client, clock: Clock) -> NetClient {
        NetClient { inner: RefCell::new(client), clock }
    }

    pub fn push<'s>(scope: &'s impl Scope, client: Client, clock: Clock) -> Result<ValueView<'s>> {
        crate::userdata::push_owned(scope, NetClient::new(client, clock))
    }

    pub fn with(&self, body: impl FnOnce(&mut Client)) {
        body(&mut self.inner.borrow_mut());
    }

    fn poll_into(&self, mut buffer: BufferView<'_>) -> Result<Polled> {
        let mut client = self.inner.borrow_mut();
        // SAFETY: as the server's `poll_into`.
        let polled = client.poll_into(unsafe { buffer.bytes_mut_unchecked() });
        match polled {
            Ok(None) => Ok(Polled::Empty),
            Ok(Some(ClientEvent::Message { event, channel, payload })) => Ok(Polled::Message {
                peer: 0,
                event: i64::from(event.0),
                channel: i64::from(channel.0),
                size: payload as f64,
            }),
            Ok(Some(ClientEvent::Connected)) => Ok(Polled::Connected { peer: 0, client_id: 0 }),
            Ok(Some(ClientEvent::Disconnected { reason })) => {
                Ok(Polled::Disconnected { peer: 0, reason: reason.name() })
            }
            Ok(Some(ClientEvent::ConnectFailed { reason, .. })) => Ok(Polled::ConnectFailed { reason: reason.name() }),
            Err(too_small) => Err(Error::runtime(format!(
                "dream.net: pollInto buffer of {} bytes is too small for a {}-byte payload",
                buffer.len(),
                too_small.needed
            ))),
        }
    }

    fn stat(&self, pick: impl Fn(&dream_net::ConnectionStats) -> f32) -> FieldValue {
        match self.inner.borrow().stats() {
            Some(stats) => FieldValue::Number(f64::from(pick(&stats))),
            None => FieldValue::Nil,
        }
    }

    fn status_name(&self) -> &'static str {
        match self.inner.borrow().status() {
            ClientStatus::Disconnected => "disconnected",
            ClientStatus::Connecting => "connecting",
            ClientStatus::Handshaking => "handshaking",
            ClientStatus::Connected => "connected",
        }
    }
}

macro_rules! client_stat_field {
    ($name:ident, $field:ident) => {
        struct $name;
        impl DirectField<NetClient> for $name {
            fn get(client: &NetClient) -> FieldValue {
                client.stat(|stats| stats.$field)
            }
        }
    };
}

client_stat_field!(RttField, rtt);
client_stat_field!(JitterField, jitter);
client_stat_field!(PacketLossField, packet_loss);
client_stat_field!(SentKbpsField, sent_kbps);
client_stat_field!(ReceivedKbpsField, received_kbps);
client_stat_field!(AckedKbpsField, acked_kbps);

struct ConnectedField;
impl DirectField<NetClient> for ConnectedField {
    fn get(client: &NetClient) -> FieldValue {
        FieldValue::Boolean(client.inner.borrow().status() == ClientStatus::Connected)
    }
}

// ---------------------------------------------------------------------------------------------
// The extension
// ---------------------------------------------------------------------------------------------

/// The `dream.net` extension: in every runtime plan, added by the planner (the id is reserved).
pub struct NetExtension {
    clock: Clock,
}

impl NetExtension {
    /// With a monotonic clock started now.
    pub fn new() -> Self {
        NetExtension { clock: monotonic_clock() }
    }

    /// With the host's clock (seconds, monotonic).
    pub fn with_clock(clock: Clock) -> Self {
        NetExtension { clock }
    }
}

impl Default for NetExtension {
    fn default() -> Self {
        Self::new()
    }
}

/// The bridge every `RuntimePlan` carries; the planner adds it itself.
pub(crate) fn extension() -> NetExtension {
    NetExtension::new()
}

impl Extension for NetExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        describe_schema(d);
        describe_server(d);
        describe_client(d);
        d.module(MODULE)
            .doc("dream-net transport: schemas, clients, and the host's server handles.")
            .function("schema", build_schema).signature("(options: { version: number, maxMessagesPerPacket: number?, channels: { { [string]: any } }, events: { { [string]: any } } }) -> dream_net_Schema")
            .constant(
                "CONNECT_TOKEN_BYTES",
                crate::source::CompileConstant::Number(dream_net::CONNECT_TOKEN_BYTES as f64),
            )
            .constant(
                "MAX_EVENT_PAYLOAD",
                crate::source::CompileConstant::Number(f64::from(dream_net::schema::MAX_EVENT_PAYLOAD)),
            )
            .constant("MAX_CHANNELS", crate::source::CompileConstant::Number(dream_net::schema::MAX_CHANNELS as f64));
        d.module(MODULE)
            .installed("client")
            .signature("(options: { schema: dream_net_Schema, bind: string? }) -> dream_net_Client")
            .doc("A transport client; needs the network.transport capability at call time.");
        d.optional_capability(TRANSPORT_CAPABILITY);
        d.memory_category("dream.net");
        Ok(())
    }

    /// `net.client{}` depends on the runtime's capabilities, so its declared member binds here.
    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let transport_allowed = cx.has_capability(TRANSPORT_CAPABILITY);
        let clock = Rc::clone(&self.clock);
        cx.module(MODULE)?.function("client", move |call: &Call, options: ValueView| -> Result<Owned<NetClient>> {
            if !transport_allowed {
                return Err(Error::permission(format!(
                    "net.client requires the '{TRANSPORT_CAPABILITY}' capability, which this runtime does not grant"
                )));
            }
            let (bind, schema) = Options::read(call, options, "net.client", |o| {
                let bind: Option<String> = o.optional("bind")?;
                let schema: crate::value::Value = o.required("schema")?;
                let schema = o.frame().with_frame(|frame| {
                    let view = schema.push_to(frame)?;
                    crate::userdata::check_receiver::<NetSchema>(view)
                        .map(|s| s.0.clone())
                        .map_err(|_| Error::runtime("net.client.schema: expected a dream.net.Schema"))
                })?;
                Ok((bind.unwrap_or_else(|| "0.0.0.0:0".to_owned()), schema))
            })?;
            let bind_address = bind.parse().map_err(|e| Error::runtime(format!("net.client.bind: {e}")))?;
            let config = ClientConfig { bind_address, transport: TransportConfig::default() };
            let client = Client::new(config, schema, clock()).map_err(config_error)?;
            Ok(Owned(NetClient::new(client, Rc::clone(&clock))))
        })?;
        Ok(())
    }
}

fn describe_schema(d: &mut ExtensionDescriptor) {
    let mut schema = d.userdata::<NetSchema>("dream.net.Schema");
    schema.tag(TagPolicy::Never).doc("A frozen wire schema: channels, events, fingerprint.");
    schema.getter("version", |s: &NetSchema| i64::from(s.0.schema_version())).signature("number");
    schema.getter("fingerprint", |s: &NetSchema| s.0.fingerprint().to_string()).signature("string");
    schema.getter("eventCount", |s: &NetSchema| s.0.events().len() as i64).signature("number");
    schema.getter("channelCount", |s: &NetSchema| s.0.channels().len() as i64).signature("number");
    schema
        .method("eventId", |s: &NetSchema, name: &str| s.0.event_id(name).map(|id| Integer(i64::from(id.0))))
        .signature("(self, name: string): integer?");
    schema
        .method("channelId", |s: &NetSchema, name: &str| s.0.channel_id(name).map(|id| Integer(i64::from(id.0))))
        .signature("(self, name: string): integer?");
    schema
        .method("eventName", |s: &NetSchema, id: Exact<i64>| -> Option<String> {
            u32::try_from(id.0).ok().and_then(|id| s.0.event(EventTypeId(id))).map(|e| e.name.clone())
        })
        .signature("(self, id: integer): string?");
    schema
        .method("channelName", |s: &NetSchema, id: Exact<i64>| -> Option<String> {
            u8::try_from(id.0).ok().and_then(|id| s.0.channel(ChannelId(id))).map(|c| c.name().to_owned())
        })
        .signature("(self, id: integer): string?");
    schema
        .method("maxPayload", |s: &NetSchema, id: Exact<i64>| -> Option<Integer> {
            u32::try_from(id.0).ok().and_then(|id| s.0.event(EventTypeId(id))).map(|e| Integer(i64::from(e.max_payload)))
        })
        .signature("(self, eventId: integer): integer?");
    schema
        .method("fingerprintHalves", |s: &NetSchema| {
            let (hi, lo) = s.0.fingerprint().halves();
            (Integer(hi as i64), Integer(lo as i64))
        })
        .signature("(self): (number, number)");
    schema.metamethod("__tostring", |s: &NetSchema| {
        format!("dream.net.Schema(v{}, {})", s.0.schema_version(), s.0.fingerprint())
    });
}

// One declaration per member reads best as one list, however long.
#[allow(clippy::too_many_lines)]
fn describe_server(d: &mut ExtensionDescriptor) {
    let mut server = d.userdata::<Server>("dream.net.Server");
    server
        .tag(TagPolicy::Preferred)
        .doc("The host's transport server; created in Rust, the private key stays there.");
    server
        .method("update", |server: &Server| {
            let now = (server.clock)();
            server.inner.borrow_mut().update(now);
        })
        .signature("(self)");
    server
        .method("pollInto", |server: &Server, buffer: BufferView| server.poll_into(buffer))
        .signature("(self, buffer: buffer): (string?, integer, ...any)");
    server
        .method(
            "sendEvent",
            |server: &Server, peer: Bits64, event: Exact<i64>, payload: BytesView, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
                let event = event_from_int(event)?;
                payload_range(payload, offset, length, |bytes| {
                    server.inner.borrow_mut().send(peer_from_int(peer), event, bytes)
                })?
                .map_err(send_error)
            },
        )
        .signature("(self, peer: integer, eventId: integer, payload: buffer | string, offset: number?, length: number?)");
    server
        .method(
            "broadcast",
            |server: &Server, event: Exact<i64>, payload: BytesView, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
                let event = event_from_int(event)?;
                payload_range(payload, offset, length, |bytes| server.inner.borrow_mut().broadcast(event, bytes))?
                    .map(|refused| Integer(refused as i64))
                    .map_err(send_error)
            },
        )
        .signature("(self, eventId: integer, payload: buffer | string, offset: number?, length: number?): integer");
    server
        .method(
            "broadcastExcept",
            |server: &Server, peer: Bits64, event: Exact<i64>, payload: BytesView, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
                let event = event_from_int(event)?;
                payload_range(payload, offset, length, |bytes| {
                    server.inner.borrow_mut().broadcast_except(Some(peer_from_int(peer)), event, bytes)
                })?
                .map(|refused| Integer(refused as i64))
                .map_err(send_error)
            },
        )
        .signature(
            "(self, peer: integer, eventId: integer, payload: buffer | string, offset: number?, length: number?): integer",
        );
    server.method("flush", |server: &Server| server.inner.borrow_mut().flush()).signature("(self)");
    server
        .method("disconnect", |server: &Server, peer: Bits64| server.inner.borrow_mut().disconnect(peer_from_int(peer)))
        .signature("(self, peer: integer)");
    server.method("disconnectAll", |server: &Server| server.inner.borrow_mut().disconnect_all()).signature("(self)");
    server
        .method("peers", |server: &Server, call: &Call| {
            let peers: Vec<PeerId> = server.inner.borrow().peers().collect();
            let table = Table::new(call, peers.len(), 0)?;
            call.with_frame(|frame| {
                let view = table.push_to(frame)?;
                for (index, peer) in peers.iter().enumerate() {
                    Integer(peer_to_int(*peer)).push_into(frame)?;
                    view.raw_set_index(frame, (index + 1) as i64)?;
                }
                Ok(())
            })?;
            Ok::<Table, Error>(table)
        })
        .signature("(self): { integer }");
    server
        .method("clientId", |server: &Server, peer: Bits64| {
            server.inner.borrow().client_id(peer_from_int(peer)).map(|id| Integer(id as i64))
        })
        .signature("(self, peer: integer): integer?");
    server
        .method("clientAddress", |server: &Server, peer: Bits64| {
            server.inner.borrow().client_address(peer_from_int(peer)).map(|a| a.to_string())
        })
        .signature("(self, peer: integer): string?");
    server.method("peerRtt", |server: &Server, peer: Bits64| server.stat(peer, |s| s.rtt)).signature("(self, peer: integer): number?");
    server
        .method("peerJitter", |server: &Server, peer: Bits64| server.stat(peer, |s| s.jitter))
        .signature("(self, peer: integer): number?");
    server
        .method("peerPacketLoss", |server: &Server, peer: Bits64| server.stat(peer, |s| s.packet_loss))
        .signature("(self, peer: integer): number?");
    server
        .method("peerSentKbps", |server: &Server, peer: Bits64| server.stat(peer, |s| s.sent_kbps))
        .signature("(self, peer: integer): number?");
    server
        .method("peerReceivedKbps", |server: &Server, peer: Bits64| server.stat(peer, |s| s.received_kbps))
        .signature("(self, peer: integer): number?");
    server
        .method("peerAckedKbps", |server: &Server, peer: Bits64| server.stat(peer, |s| s.acked_kbps))
        .signature("(self, peer: integer): number?");
    server
        .method("counters", |server: &Server, call: &Call, peer: Bits64| -> Result<Option<Table>> {
            match server.inner.borrow().counters(peer_from_int(peer)) {
                Some(counters) => counters_table(call, &counters).map(Some),
                None => Ok(None),
            }
        })
        .signature("(self, peer: integer): { [string]: number }?");
    server
        .method("memoryUsage", |server: &Server, call: &Call| memory_table(call, &server.inner.borrow().memory_usage()))
        .signature("(self): { [string]: number }");
    server.getter("numConnected", |server: &Server| server.inner.borrow().num_connected() as i64).signature("number");
    server.getter("maxClients", |server: &Server| server.inner.borrow().max_clients() as i64).signature("number");
    server.getter("address", |server: &Server| server.inner.borrow().address().to_string()).signature("string");
    server.metamethod("__tostring", |server: &Server| {
        let inner = server.inner.borrow();
        format!("dream.net.Server({}, {} connected)", inner.address(), inner.num_connected())
    });
}

fn describe_client(d: &mut ExtensionDescriptor) {
    let mut client = d.userdata::<NetClient>("dream.net.Client");
    client
        .tag(TagPolicy::Preferred)
        .doc("A transport client; `net.client{}` needs the network.transport capability.");
    client
        .method("connect", |client: &NetClient, token: BytesView| {
            let mut bytes = [0u8; dream_net::CONNECT_TOKEN_BYTES];
            if token.len() != bytes.len() {
                return Err(Error::runtime(format!("dream.net: a connect token is {} bytes", bytes.len())));
            }
            token.read(0, &mut bytes)?;
            let token = bytes;
            client.inner.borrow_mut().connect(&token).map_err(config_error)
        })
        .signature("(self, token: buffer | string)");
    client.method("disconnect", |client: &NetClient| client.inner.borrow_mut().disconnect()).signature("(self)");
    client
        .method("update", |client: &NetClient| {
            let now = (client.clock)();
            client.inner.borrow_mut().update(now);
        })
        .signature("(self)");
    client
        .method("pollInto", |client: &NetClient, buffer: BufferView| client.poll_into(buffer))
        .signature("(self, buffer: buffer): (string?, integer, ...any)");
    client
        .method(
            "sendEvent",
            |client: &NetClient, event: Exact<i64>, payload: BytesView, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
                let event = event_from_int(event)?;
                payload_range(payload, offset, length, |bytes| client.inner.borrow_mut().send(event, bytes))?
                    .map_err(send_error)
            },
        )
        .signature("(self, eventId: integer, payload: buffer | string, offset: number?, length: number?)");
    client.method("flush", |client: &NetClient| client.inner.borrow_mut().flush()).signature("(self)");
    client
        .method("counters", |client: &NetClient, call: &Call| -> Result<Option<Table>> {
            match client.inner.borrow().counters() {
                Some(counters) => counters_table(call, &counters).map(Some),
                None => Ok(None),
            }
        })
        .signature("(self): { [string]: number }?");
    client
        .method("memoryUsage", |client: &NetClient, call: &Call| memory_table(call, &client.inner.borrow().memory_usage()))
        .signature("(self): { [string]: number }");
    client.getter("status", |client: &NetClient| client.status_name()).signature("string");
    client.getter("port", |client: &NetClient| i64::from(client.inner.borrow().port())).signature("number");
    client
        .getter("serverAddress", |client: &NetClient| client.inner.borrow().server_address().map(|a| a.to_string()))
        .signature("string?");
    client.field::<ConnectedField>("connected").signature("boolean");
    client.field::<RttField>("rtt").signature("number?");
    client.field::<JitterField>("jitter").signature("number?");
    client.field::<PacketLossField>("packetLoss").signature("number?");
    client.field::<SentKbpsField>("sentKbps").signature("number?");
    client.field::<ReceivedKbpsField>("receivedKbps").signature("number?");
    client.field::<AckedKbpsField>("ackedKbps").signature("number?");
    client.metamethod("__tostring", |client: &NetClient| format!("dream.net.Client({})", client.status_name()));
}

/// `Delivery` names as scripts spell them.
pub fn delivery_name(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::ReliableOrdered => "reliableOrdered",
        Delivery::UnreliableUnordered => "unreliableUnordered",
    }
}
