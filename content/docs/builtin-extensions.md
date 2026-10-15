+++
title = "Built-in extensions"
description = "The extensions l3i ships: the dream.udp bridge every plan carries, packed rotations in dream.quat, colors and clip rectangles in dream.raster, the dream.soft_render device, bytes for parsing in dream.bytes, the bulk-data plane in dream.data, textual identity as numbers in dream.intern, locales, plural rules and number formatting in dream.intl, Luau's own parser in dream.luau, the host filesystem in dream.fs, child processes in dream.process, raw TCP streams with a readiness poller in dream.tcp, host name resolution in dream.dns, and verified TLS sessions in dream.tls."
weight = 90

[extra]
kind = "guide"
+++

Fourteen extensions come with the crate. `dream.udp` is in every plan; the others are added with
`RuntimePlan::builder().extension(..)`. Each is an ordinary `Extension` built on the
[primitives](@/docs/primitives.md), with a Luau signature on every member, so a strict script
that requires its module type checks against the plan's definitions.

| Extension | Module | Rust | Feature |
|---|---|---|---|
| `dream.udp` | `@dream/udp` | `l3i::udp` | always |
| `dream.quat` | `@dream/quat` | `l3i::quat::QuatExtension` | always (`quat.math()` needs `jit`) |
| `dream.raster` | `@dream/raster` | `l3i::raster::RasterExtension` | always |
| `dream.soft_render` | `@dream/soft-render` | `l3i::soft_render::SoftRenderExtension` | `soft-render` |
| `dream.bytes` | `@dream/bytes` | `l3i::bytes::BytesExtension` | `bytes` |
| `dream.data` | `@dream/data` | `l3i::data::DataExtension` | `data` (native loops need `jit`) |
| `dream.intern` | `@dream/intern` | `l3i::intern::InternExtension` | `intern` |
| `dream.intl` | `@dream/intl` | `l3i::intl::IntlExtension` | `intl` |
| `dream.luau` | `@dream/luau` | `l3i::syntax::SyntaxExtension` | `syntax` |
| `dream.fs` | `@dream/fs` | `l3i::fs::FsExtension` | `fs` |
| `dream.process` | `@dream/process` | `l3i::process::ProcessExtension` | `process` |
| `dream.tcp` | `@dream/tcp` | `l3i::tcp::TcpExtension` | `tcp` |
| `dream.dns` | `@dream/dns` | `l3i::dns::DnsExtension` | `dns` |
| `dream.tls` | `@dream/tls` | `l3i::tls::TlsExtension` | `tls` (with `tcp`) |

The samples below reach the modules as compat globals (`RuntimePolicy::new().compat_global("@dream/quat", "quat")`),
which is what the tests do; `require("@dream/quat")` is the canonical path.

## dream.udp

The game transport is runtime infrastructure, not a feature: l3i depends on dream-net and owns
the Luau bridge. (Raw TCP is a separate, optional extension, [dream.tcp](#dream-tcp).) The planner adds `UdpExtension` to every plan and reserves the id `dream.udp`, so no
runtime lacks the network and the policy's capabilities decide what scripts may do with it.
dream-net stays pure Rust and knows peers, event ids, channels and bytes; this module owns the
Luau-facing shape.

- Peer, event, channel and client ids are Luau integers, exact and compared with `==`; a peer id is a `Bits64` pattern, all 64 bits. Sizes and counters are plain numbers, because scripts threshold them.
- Payloads are Luau buffers, or strings on send. `pollInto` copies one received payload into a caller-owned buffer and `sendEvent` copies out of one before returning, so the transport retains no Lua memory.
- The transport clock is the plan's: `update()` reads a monotonic clock started with the bridge, or the clock the host gave `RuntimePlanBuilder::network_clock`, so a script cannot spoof time and a simulation can drive it.
- The server private key never reaches Luau. A host may still hand scripts a `udp::Server` handle, or Luau may create one with `udp.server{}`; both `udp.server{}` and `udp.client{}` require the `network.transport` capability (`udp::TRANSPORT_CAPABILITY`). A Luau-created server keeps its generated key in native userdata and mints client tokens with `server:connectToken(clientId, expiresInSeconds?, timeoutSeconds?)`.
- Nothing calls into Luau from inside dream-net; the host drives one network phase per frame.

### The module

| Member | Type |
|---|---|
| `schema(options)` | `(options: { version: number, maxMessagesPerPacket: number?, channels: { { [string]: any } }, events: { { [string]: any } } }) -> dream_udp_Schema` |
| `client(options)` | `(options: { schema: dream_udp_Schema, bind: string? }) -> dream_udp_Client`, bound at install because it depends on the capability |
| `server(options)` | `(options: { schema: dream_udp_Schema, address: string, protocolId: integer, maxClients: number? }) -> dream_udp_Server`, bound at install because it depends on the capability |
| `CONNECT_TOKEN_BYTES`, `MAX_EVENT_PAYLOAD`, `MAX_CHANNELS` | Folded numbers: 2048, dream-net's payload limit, 64 |

`udp.schema{}` is a strict option table. A channel is `{ name, delivery, capacity?, overflow?,
packetBudget?, resendInterval? }` with `delivery` one of `'reliableOrdered'` and
`'unreliableUnordered'` and `overflow` one of `'fail'`, `'dropOldest'` and `'dropNewest'`; an
event is `{ name, channel, maxPayload, codecVersion? }` naming its channel. Errors carry the path
(`udp.schema.channels[1].delivery`, `unknown channel 'missing'`).

```luau
local udp = require("@dream/udp")
local schema = udp.schema{
    version = 1,
    channels = {
        { name = "reliable", delivery = "reliableOrdered" },
        { name = "state", delivery = "unreliableUnordered", capacity = 256, overflow = "dropOldest" },
    },
    events = {
        { name = "Ping", channel = "reliable", maxPayload = 64 },
        { name = "Move", channel = "state", maxPayload = 12, codecVersion = 2 },
    },
}
assert(schema.eventCount == 2 and schema.channelCount == 2)
assert(schema:eventName(schema:eventId("Ping")) == "Ping")
assert(schema:maxPayload(schema:eventId("Move")) == 12i)
```

### Schema, client, server

Creating the server and minting client tokens are ordinary Luau operations. The only authority
required is the host-granted `network.transport` capability; token bytes are safe to distribute to
clients, while the signing key remains private to the server userdata:

```luau
local server = udp.server{
    schema = schema,
    address = "127.0.0.1:0", -- the bound and advertised address
    protocolId = 0x12345678,
    maxClients = 4,
}
local token = server:connectToken(1) -- buffer; expires after 300 seconds by default
```

`dream_udp_Schema` is untagged: getters `version`, `fingerprint` (32 hex digits), `eventCount`,
`channelCount`; methods `eventId(name)`, `channelId(name)` (`integer?`), `eventName(id)`,
`channelName(id)` (`string?`), `maxPayload(eventId)` (`integer?`), `fingerprintHalves()`.

`dream_udp_Client` and `dream_udp_Server` are tagged when tags allow. The hot calls are the same
on both:

| Call | Does |
|---|---|
| `update()` | Reads the clock; receive, decode, deliver, acknowledge, handshake, timers |
| `pollInto(buffer)` | The next record as `kind, peer, a, b, c`, or nothing when the queue is empty; one payload copy into `buffer`, no allocation. A buffer too small for the payload is an error naming the size needed, and the record stays queued |
| `sendEvent(peer, eventId, payload, offset?, length?)` (server), `sendEvent(eventId, payload, offset?, length?)` (client) | Queues a range of a buffer or string, copied before the call returns |
| `flush()` | Packs queued events into packets and sends them |

| `kind` | `peer` | `a` | `b` | `c` |
|---|---|---|---|---|
| `"message"` | peer | event id | channel id | size |
| `"connected"` | peer | client id | | |
| `"disconnected"` | peer | reason | | |
| `"rejected"` | peer | client id | reason | |
| `"connectFailed"` | `0` | reason | | |

A client polls with `peer` 0 and `client id` 0. The client also has `connect(token)`,
`disconnect()`, `counters()`, `memoryUsage()`, the getters `status` (`disconnected`,
`connecting`, `handshaking`, `connected`), `port`, `serverAddress`, and the direct fields
`connected`, `rtt`, `jitter`, `packetLoss`, `sentKbps`, `receivedKbps`, `ackedKbps` (`number?`,
nil before statistics exist). The server adds `broadcast`, `broadcastExcept(peer, ..)` (both
return the number of peers refused), `disconnect(peer)`, `disconnectAll()`, `peers()`,
`clientId(peer)`, `clientAddress(peer)`, `peerRtt(peer)` and the other per-peer statistics,
`counters(peer)`, `memoryUsage()`, and the getters `numConnected`, `maxClients`, `address`.

```luau
local buf = buffer.create(64)
local function step()
    server:update() client:update()
    while true do
        local kind, peer, a, b, c = server:pollInto(buf)
        if not kind then break end
        if kind == "message" and a == schema:eventId("Ping") then
            server:sendEvent(peer, schema:eventId("Pong"), "pong!")
        end
    end
    while true do
        local kind, _, a, b, c = client:pollInto(buf)
        if not kind then break end
        if kind == "connected" then
            local out = buffer.create(16)
            buffer.writestring(out, 0, "ping")
            client:sendEvent(schema:eventId("Ping"), out, 0, 4)
        elseif kind == "message" and a == schema:eventId("Pong") then
            assert(buffer.readstring(buf, 0, c) == "pong!")
        end
    end
    server:flush() client:flush()
end
```

Misuse is a script error prefixed `dream.udp:`, never a panic: an unknown event id, a payload
range past its buffer (`payload range 0..40 exceeds the 7-byte buffer`), a peer that is not
connected. `benches/udp.rs` measures the bridge over localhost UDP.

## dream.quat

`QuatExtension` provides rotations as packed Luau integers. A unit quaternion is compressed
smallest-three into the 56-bit payload of `quat::Quaternion` (kind 1): two bits name the largest
component, the other three are 18-bit lanes over `[-1/√2, 1/√2]`, and the omitted one is rebuilt
from the unit norm. The grid has an exact zero, so the identity and axis-aligned rotations round
trip exactly; a random rotation comes back within 1.6e-5 rad (mean 5.7e-6). Scripts see one
integer: no allocation, no GC object, and a type error rather than garbage when an integer of
another kind is passed.

`quat::AnimationKey` (kind 2) is the rotation plus four opaque flag bits: a pose key, an
animation frame, a network snapshot, in one integer. l3i keeps the nibble opaque; the system that
produces the keys defines the bits.

| Member | Type |
|---|---|
| `IDENTITY` | `integer`, a compiler-folded constant |
| `axisAngle(axis, angle)` | `(axis: vector, angle: number) -> integer` |
| `fromXYZW(x, y, z, w)` | `(x: number, y: number, z: number, w: number) -> integer`, normalised |
| `toXYZW(q)` | `(q: integer) -> (number, number, number, number)` |
| `mul(a, b)` | `(a: integer, b: integer) -> integer`, the Hamilton product: apply `b`, then `a` |
| `inverse(q)` | `(q: integer) -> integer` |
| `slerp(a, b, t)` | `(a: integer, b: integer, t: number) -> integer`, the shorter arc, `t` clamped to `0..=1` |
| `rotate(q, v)` | `(q: integer, v: vector) -> vector` |
| `angleTo(a, b)` | `(a: integer, b: integer) -> number`, radians |
| `key(q, flags)` | `(q: integer, flags: number) -> integer`, an `AnimationKey` from the low four bits of an exact integer |
| `keyRotation(k)` | `(k: integer) -> integer` |
| `keyFlags(k)` | `(k: integer) -> number` |
| `math()` | `() -> dream_quat_Math` (`jit` feature) |

```luau
local quat = require("@dream/quat")
local a = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2)
local c = quat.mul(a, a)
local v = quat.rotate(c, vector.create(1, 0, 0))
assert(math.abs(v.x + 1) < 1e-4 and math.abs(v.y) < 1e-4)
assert(quat.angleTo(quat.mul(a, quat.inverse(a)), quat.IDENTITY) < 1e-5)
local x, y, z, w = quat.toXYZW(quat.IDENTITY)
assert(x == 0 and y == 0 and z == 0 and w == 1)
local key = quat.key(a, 5)
assert(quat.keyFlags(key) == 5 and quat.angleTo(quat.keyRotation(key), a) < 1e-5)
assert(not pcall(quat.mul, a, key)) -- expected a packed Quaternion
```

Malformed input is refused: `axisAngle` with a zero or non-finite axis or a non-finite angle,
`fromXYZW` with non-finite components or all zeros (a scaled identity normalises), `slerp` with
a non-finite weight (finite weights outside `0..1` clamp), `key` with a fractional flag value
(`Exact` refuses it; higher bits are dropped). A quaternion where a key is expected, or the
reverse, fails naming `Quaternion` or `AnimationKey`; a plain integer fails too.

The interpolation uses polynomial trigonometry, `acos` to 2e-8 and sine to 6e-8 on their
domains, so the bound path and the native lowering compute the same formulas; the angular error
against exact slerp stays below 1e-7 rad, three orders of magnitude under the packed form's own
quantisation.

The packed form is storage and transport, never the live accumulator: re-encoding after every
blend step random-walks (1.7e-3 rad over 100k slerp steps), while packing an f64 state each step
stays within one quantisation step. Keep long-lived rotation state as `quat::Quat` on the host
(`Quat::from_axis_angle`, `slerp`, `rotate`, `angle_to`, `Quaternion::pack(q)`,
`AnimationKey::pack(q, flags)`) and pack what scripts, saves and the wire see.

### quat.math()

With the `jit` feature, `quat.math()` returns a payload-free tagged receiver, `dream.quat.Math`,
whose `rotate(q, v)`, `mul(a, b)`, `slerp(a, b, t)`, `fromXYZW(x, y, z, w)`, `key(q, flags)`,
`keyRotation(k)` and `keyFlags(k)` are ordinary bound methods on the interpreter path and lower to IR through
`quat::lowering::Lowering` when the compiler knows the receiver's type:

```luau
--!native
local quat = require("@dream/quat")
local Q: dream_quat_Math = quat.math()
local a = quat.axisAngle(vector.create(0, 0, 1), 0.3)
local b = quat.axisAngle(vector.create(1, 0, 0), 0.7)
local m = Q:mul(a, b)
local r = Q:rotate(m, vector.create(1, 2, 3))
local f = Q:fromXYZW(0, 0, 0.5, 0.5)
local k = Q:key(a, 3)
local back = Q:keyRotation(k)
```

No C call: the integer is unpacked with shifts and masks, the arithmetic runs on doubles, and the
result is stored as a vector, a number, or a fresh packed integer. The lowering checks the
receiver's tag, the operands' integer tags and packed kinds, a non-finite weight, a fractional
flag value, and `fromXYZW` components that are zero, not finite, or so small their squared norm
is below 1e-290 (the binder rescales those); a mismatch exits to the interpreter, whose bound method raises the same error. Only
single-result, fixed-arity call sites lower: `return Q:mul(a, b)` and `Q:keyRotation(Q:key(q, 3))`
run through the bound method, so bind the inner result to a local first. The type declares
`TagPolicy::Required` and `CompilerTypePolicy::Required`, so a plan that cannot give it a tag and
a compiler slot fails instead of leaving the path interpreted.

Measured per call inside native code: `rotate` 21 ns and `mul` 45 ns, against 99 ns and 150 ns
through the binder and 46 ns and 111 ns for an f32 quaternion userdata (the latter allocating);
`slerp` 86 ns against 167 ns and 214 ns; `key` plus `keyRotation` together 5 ns against 220 ns
through the binder. `fromXYZW` normalizes and packs in 93 instructions and 70 cycles, against 459
and 196 through the binder and 616 and 196 for a four-field table
([Performance](@/docs/performance.md#instructions-and-cycles)): which component is largest, and
its sign, do not change with the norm, so one factor folds the sign, the norm, and the grid, and
its quotient and square root run side by side.

## dream.raster

`RasterExtension` provides raster scalars as packed integers.

`raster::Color` (kind 3) is an RGBA8 color. Its 32-bit form, red in bits 0 to 7 through alpha
in bits 24 to 31, is exactly the four bytes `[r, g, b, a]` a vertex color field or a texture
pixel holds, so one value serves script code, vertex buffers and pixel buffers with no
conversion. Every `u32` is a color; the only runtime check is the kind nibble. Channel meaning is
the consumer's: the renderer below reads colors as premultiplied.

`raster::ClipRect` (kind 4) is an integer pixel rectangle, `minX, minY, maxX, maxY` in four
14-bit fields, so a coordinate is at most 16383 (`ClipRect::MAX_COORD`). That is a limit of the
packed form, not of a renderer, which takes `u32` coordinates: a surface past 16K on an axis needs
a clip type of its own. Construction and unpacking both require `min <= max` on each axis;
`ClipRect::ALL` has every field at the maximum, which a clamping renderer treats as no clip.

`raster::Color16` is the wide form for formats that require 16 bits per channel: red in bits 0
to 15 through alpha in bits 48 to 63, the little-endian `u64` being an RGBA16 pixel. It fills the
whole Luau integer and so has no kind nibble: any integer is accepted as a `Color16`, and nothing
at runtime distinguishes it from an RGBA8 color or an id. That is the deliberate price of exact
interchange; `widen` (`x * 257`) and `narrow` (`round(x / 257)`) convert exactly.

| Member | Type |
|---|---|
| `TRANSPARENT`, `BLACK`, `WHITE`, `CLIP_ALL`, `TRANSPARENT16`, `BLACK16`, `WHITE16` | Folded `integer` constants |
| `CLIP_MAX_COORD` | `number`, 16383 |
| `rgba8(r, g, b, a)`, `rgb8(r, g, b)` | `-> integer`; strict: each channel an exact integer in `0..=255` |
| `packed(color)` | `-> number`, the `u32` for `buffer.writeu32` |
| `channels(color)` | `-> (number, number, number, number)` |
| `withAlpha(color, a)`, `lerp(a, b, t)`, `mul(a, b)`, `add(a, b)`, `scale(color, factor)`, `premultiply(color)` | `-> integer` |
| `rgba16`, `rgb16`, `channels16`, `withAlpha16`, `lerp16`, `mul16`, `add16`, `scale16`, `premultiply16` | The same on `Color16` |
| `widen(color)`, `narrow(color)` | `-> integer` between the two widths |
| `clip(minX, minY, maxX, maxY)` | `-> integer`; exact integers in `0..=16383`, `min <= max` |
| `clipBounds(clip)` | `-> (number, number, number, number)` |
| `math()` | `() -> dream_raster_Math` |

`mul` modulates (`a * b / 255`, as a tint multiplies a texel), `add` saturates, `scale`
multiplies the color channels and leaves alpha, `premultiply` computes `(c * a + 127) / 255` in
integer arithmetic, the rounding renderers use, and `lerp` clamps `t` to `0..=1`.

```luau
local raster = require("@dream/raster")
local c = raster.rgba8(0x11, 0x22, 0x33, 0x44)
local r, g, b, a = raster.channels(c)
assert(raster.lerp(raster.BLACK, raster.WHITE, 0.5) == raster.rgba8(128, 128, 128, 255))
assert(raster.premultiply(raster.rgba8(255, 128, 1, 128)) == raster.rgba8(128, 64, 1, 128))
local buf = buffer.create(4)
buffer.writeu32(buf, 0, raster.packed(c))
assert(buffer.readu8(buf, 0) == 0x11 and buffer.readu8(buf, 3) == 0x44) -- r, g, b, a
assert(raster.narrow(raster.widen(c)) == c)
local clip = raster.clip(1, 2, 640, 480)
local x0, y0, x1, y1 = raster.clipBounds(clip)
assert(not pcall(raster.channels, clip)) -- a clip is not a color
assert(not pcall(raster.rgba8, 256, 0, 0, 0)) -- outside 0..=255
```

### raster.math()

Color arithmetic for GUI and shader-style scripts goes through `raster.math()`, a tagged
receiver (`dream.raster.Math`, `TagPolicy::Required`, `CompilerTypePolicy::Required`) whose
methods `rgba8`, `rgb8`, `red`, `green`, `blue`, `alpha`, `channels`, `withAlpha`, `lerp`, `mul`,
`add`, `scale`, `premultiply`, each with a `16` form, plus `widen` and `narrow`, lower to native
code under `jit` when the script annotates it:

```luau
--!native
local raster = require("@dream/raster")
local M: dream_raster_Math = raster.math()
local tint = M:rgba8(80, 160, 255, 192)
local shaded = M:mul(tint, M:lerp(raster.BLACK, raster.WHITE, 0.3))
local wide = M:lerp16(M:widen(shaded), raster.WHITE16, 0.5)
local back = M:narrow(wide)
```

Shader semantics throughout: inputs clamp to their range, results round to nearest, and a NaN
input yields channel 0; `M:rgba8(-4, 255.4, 254.5, 1e9)` is `raster.rgba8(0, 255, 255, 255)`.
The module's `rgba8` and `rgb8` are the strict constructors; the receiver's clamp, so both paths
of a lowered call agree by construction. The lowering, `raster::lowering::ColorMath`, unpacks
with shifts and masks, computes on doubles, clamps, rounds and packs into one integer store; the
8-bit form is tag- and kind-checked, the 16-bit form tag-checked only.

Measured per call (`benches/raster.rs`): `rgba8` 71 ns through the module against 2.6 ns lowered,
`lerp` 72 ns against 15 ns, `mul` 61 ns against 17 ns, `premultiply` 52 ns against 16 ns, `lerp16`
69 ns against 14 ns, `narrow` 48 ns against 3 ns.

## dream.soft_render

With the `soft-render` feature, `SoftRenderExtension` binds
[dream-soft-render](https://github.com/DreamWeave-MP/dream-soft-render) as a small software
rendering device. It requires `dream.raster` in the same plan: colors are `raster` integers the
renderer reads as premultiplied (`soft.premultiply` converts straight alpha), clip rectangles are
`ClipRect` integers. Luau describes raster work in native-shaped values and Rust does it: nothing
is per-pixel and nothing is a table.

dream-net (runtime infrastructure) and dream-soft-render (an experimental rendering primitive
whose lowering lives next to the raster kinds) are the two explicit l3i integrations; every other
DreamWeave crate owns its l3i extension and depends upward on l3i, never the other way round. l3i
does not depend on the renderer unless the feature is on.

| Member | Type |
|---|---|
| `renderer()` | `() -> dream_soft_render_Renderer` |
| `vertices()` | `() -> dream_soft_render_Vertices` |
| `premultiply(color)` | `(color: integer) -> integer` |
| `MAX_SURFACE_PIXELS`, `MAX_TEXTURE_BYTES`, `VERTEX_BYTES` | Folded numbers; `VERTEX_BYTES` is 20 |

| Type | Members |
|---|---|
| `Renderer` | `beginFrame(width, height): Frame`, `createTexture(width, height, pixels: buffer \| string): Texture`, `readInto(buffer, offset?): number` (the byte count, `width * height * 4`), fields `width`, `height` |
| `Frame` | `clear(color)`, `rect(min: vector, max: vector, color, clip)`, `image(min, max, uvMin, uvMax, texture, tint, clip)`, `mesh(vertices: buffer, indices: buffer, texture: Texture?, clip)`, `finish()`, fields `width`, `height` |
| `Texture` | `update(x, y, width, height, pixels)`, `free()`, fields `width`, `height` |
| `Vertices` | `write(buffer, offset: number, pos: vector, uv: vector, color: integer): number`, the next offset |

Draws rasterize immediately in call order, as the crate does; a `Frame` is a token that goes
stale on `finish()` or the next `beginFrame`. A mesh is a buffer of 20-byte vertices (`x, y, u, v`
as f32 and `[r, g, b, a]`) and a buffer of `u32` indices, borrowed for one call and never kept,
validated for stride and alignment here and for indices and finiteness by the crate. A `Texture`
frees its storage on `free()` or when collected; Luau has no `collectgarbage`, so a host that
wants textures back at frame boundaries drives the collector there (`runtime.gc(GcControl::Collect)`).

```luau
local raster = require("@dream/raster")
local soft = require("@dream/soft-render")
local renderer = soft.renderer()
local checker = buffer.create(16)
for i, v in { 255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255 } do
    buffer.writeu8(checker, i - 1, v)
end
local texture = renderer:createTexture(2, 2, checker)
local frame = renderer:beginFrame(64, 48)
frame:clear(raster.rgb8(17, 20, 28))
frame:rect(vector.create(4, 4), vector.create(28, 16), soft.premultiply(raster.rgba8(80, 160, 255, 192)), raster.CLIP_ALL)
frame:image(vector.create(32, 4), vector.create(48, 20), vector.zero, vector.one, texture, raster.WHITE, raster.CLIP_ALL)
local V = soft.vertices()
local vertices = buffer.create(3 * soft.VERTEX_BYTES)
local off = 0
off = V:write(vertices, off, vector.create(8, 40), vector.zero, raster.rgb8(220, 40, 40))
off = V:write(vertices, off, vector.create(24, 24), vector.zero, raster.rgb8(220, 40, 40))
off = V:write(vertices, off, vector.create(40, 40), vector.zero, raster.rgb8(220, 40, 40))
local indices = buffer.create(3 * 4)
for i = 0, 2 do buffer.writeu32(indices, i * 4, i) end
frame:mesh(vertices, indices, nil, raster.CLIP_ALL)
frame:finish()
local out = buffer.create(64 * 48 * 4)
assert(renderer:readInto(out) == 64 * 48 * 4)
```

A scene drawn from Luau is byte-identical to the same scene drawn from Rust:
`tests/soft_render.rs` draws both and compares every pixel, and two runtimes with different tags
draw the same bytes. Malformed input is an error in the renderer's own words, never a quietly
clipped draw: a vertex buffer whose length is not a multiple of 20, an index past the vertices,
a NaN corner, a freed texture, a frame used after `finish()` or after the next `beginFrame`, a
pixel budget exceeded.

### soft.vertices()

`Vertices:write(buffer, offset, pos, uv, color)` packs one vertex with a single bounds check and
returns the next offset; the offset truncates toward zero like the buffer library's, and anything
outside the buffer is `buffer access out of bounds`. Under `jit`,
`soft_render::lowering::VertexWriter` lowers a call on an annotated receiver
(`local V: dream_soft_render_Vertices = soft.vertices()`) to one bounds check, four f32 stores
from the two vectors, and two 16-bit stores for the color; a wrong tag, a color of another kind,
or an offset outside the buffer exits to the interpreter, whose bound method reports the error.
The bytes written are identical on all three paths: five `buffer.write*` calls, the bound writer,
and the lowered writer.

Measured (`benches/soft_render.rs`, 640x480): a frame of 48 panels costs 363 µs from Luau against
353 µs native, 3200 glyph quads 5.1 ms against 4.8 ms, the 256-triangle fan 4.7 ms either way; an
offscreen `frame:rect` call is 213 ns against 102 ns native, of which the bound call itself (the
namecall plus its four arguments) is 84 ns; writing the fan's 258 vertices takes 506 ns per vertex
with five `buffer.write*` calls, 330 ns through the bound writer, and 70 ns through the lowered
writer (cos, sin, and `vector.create` included).

## dream.bytes

Feature `bytes`; module `@dream/bytes`, `l3i::bytes::BytesExtension`. The extension for scripts
that parse foreign file formats, which is most of what a preserved game engine does. Luau's own
`buffer` library already reads and writes every little-endian width, 64-bit integers (`readinteger`)
and bit fields (`readbits`), and its code generator lowers all of them to native loads and stores,
so a parser written against `buffer` under `--!native` already runs at native speed. This module
adds what a script cannot do fast, or at all, on top of that. Every "some bytes" input is a
`buffer | string`, every bytes output is a new `buffer` of exactly its length, offsets are
zero-based like `buffer`'s, and nothing allocates on a read.

```luau
local bytes = require("@dream/bytes")
local header = file:readRange(0, 64)
assert(bytes.equals(bytes.slice(header, 0, 4), "FORM"), "IFF")
local size = bytes.readu32be(header, 4)
local name, next = bytes.readCString(header, 8, 32)
local count, after = bytes.readVarint(header, next)
```

### Searching, comparing, record strings

| Function | Returns |
|---|---|
| `find(haystack, needle, start?)`, `rfind(haystack, needle, endOffset?)` | The offset of the needle, or nil; SIMD searches through `memchr` |
| `count(haystack, needle)` | Non-overlapping occurrences |
| `equals(a, b)`, `startsWith(haystack, prefix, offset?)` | Booleans |
| `compare(a, aOffset, b, bOffset, length)` | -1, 0 or 1 over `length` bytes of each |
| `slice(source, offset, length)` | A new buffer |
| `translate(text, from, to, { collapse?, trimStart?, trimEnd? }?)` | `text` with each byte found in `from` replaced by the byte at the same position in `to`, like `tr`, then runs of `collapse` made one and `trimStart` and `trimEnd` removed from the ends; `text` itself, uncopied, when nothing changes |
| `toHex(data)`, `fromHex(text)` | Lower-case hex and back; whitespace between digits is ignored |
| `toBase64(source, offset?, length?)`, `fromBase64(text)` | Standard padded base64 (RFC 4648) of a range, the whole source by default, and back; `fromBase64` refuses anything else, naming the byte |
| `readCString(source, offset, fieldLength?)` | The text up to the first NUL and the offset after the terminator, or after the fixed-width field when `fieldLength` is given |
| `writeCString(target, offset, text, fieldLength?)` | Writes the text and a NUL, NUL-padded to the field; returns the offset after it |
| `readVarint`, `readSignedVarint(source, offset)` | LEB128: the value as an integer and the offset after it |
| `writeVarint`, `writeSignedVarint(target, offset, value)` | The offset after the encoding |

Every bounds failure names the call, the width and the offset: `bytes.readu32be: 4 bytes at
offset 30 past the end (length 32)`.

### Framing chunks

`frame(source, layout, offset?, finish?)` walks a run of tag-length-payload chunks, the shape
RIFF, IFF, PNG, GLB and Bethesda's records share, in one native call, and returns where each
chunk is: a buffer of `u32` start and end pairs, the count, and, when the walk stopped early,
why and where. The layout is data: `header` (bytes before the payload), `lengthAt` and
`lengthSize` (1, 2, 4 or 8; 4 by default) for the length field, `bigEndian`, and an `inner`
layout that frames each chunk's payload in turn and checks that its chunks tile it exactly,
without recording them, which is the walk a parser makes before it trusts any offset in a file.

```luau
local layout = { header = 16, lengthAt = 4, inner = { header = 8, lengthAt = 4 } }
local spans, count, problem, at = bytes.frame(file, layout)
if problem then error(`{problem} at offset {at}`) end   -- truncated, overrun, innerTruncated, innerOverrun
for i = 0, count - 1 do
    local start, finish = buffer.readu32(spans, i * 8), buffer.readu32(spans, i * 8 + 4)
end
```

The chunks before a problem are still in the result, so a parser reports it in its own words.

### Regular expressions (`bytes-regex`)

`regex(pattern, { caseInsensitive?, unicode?, multiLine?, dotAll? }?)` compiles a pattern once
(the regex crate's syntax, matched over bytes: Unicode-aware by default, `unicode = false` or
`(?-u)` for data in another encoding; no look-around, linear time). The `dream.bytes.Regex` it
returns has `isMatch(source, offset?, length?)`, `find(source, offset?, length?)` (start and end,
absolute), `pattern()`, and `matchSpans(source, spans, count?)`: one call tests every
`(start, end)` `u32` pair in `spans` against its own range of `source`, as if that range were the
whole input, and answers with a buffer of one byte per span, 1 where it matched. A thousand small
fields laid out in one buffer cost one call instead of a thousand, and no string is made for any
of them; `frame`'s spans are already that layout.

```luau
local teleports = bytes.regex([[\b(position|positioncell)\b]], { caseInsensitive = true })
local flags = teleports:matchSpans(texts, spans, count)
for i = 0, count - 1 do
    if buffer.readu8(flags, i) == 1 then print(`record {i} teleports`) end
end
```

### The widths and orders buffer lacks

`readu16be`, `readi16be`, `readu24`, `readi24`, `readu24be`, `readi24be`, `readu32be`,
`readi32be`, `readf32be`, `readf64be`, `readi64be` (an integer), `readf16` and `readf16be` (IEEE
half floats), each `(source, offset)`, and the matching `write*(target, offset, value)`. A value
written through a `u` or `i` form is truncated to the width exactly as `buffer.writeu16`
truncates, so both paths of a lowered call agree by construction.

The same methods live on `bytes.math()`, the `dream_bytes_Math` receiver. Under `jit`, when the
script annotates it (`local B: dream_bytes_Math = bytes.math()`), the integer forms lower to
native code: the receiver's tag check, Luau's own buffer bounds check, one load or store, and a
byte swap, which is what `buffer.readu32` compiles to plus the swap. A bad offset exits to the
interpreter, whose bound method raises the error. The float forms stay on the binder path, since
the IR has no bit cast between integers and floats; a script that needs a big-endian float in
native code reads the integer form and moves it through a scratch buffer with `buffer.writeu32`
and `buffer.readf32`, both of which Luau lowers.

```luau
--!native
local B: dream_bytes_Math = bytes.math()
for i = 0, count - 1 do
    local id = B:readu16be(table, i * 6)      -- native: load, swap, no C call
    local offset = B:readu24be(table, i * 6 + 2)
end
```

### Codecs (`bytes-codecs`)

| Function | Notes |
|---|---|
| `inflate(source, { format?, maxSize? }?)` | DEFLATE: `zlib` (default), `raw` or `gzip`; the gzip trailer's CRC and size are checked |
| `deflate(source, { format?, level? }?)` | Levels 0 to 10, default 6 |
| `lz4Decompress(source, decompressedSize, { maxSize? }?)`, `lz4Compress(source)` | LZ4 blocks; the block format carries no size, so the caller supplies it, and `maxSize` caps what the caller may ask for |
| `lz4FrameDecompress(source, { maxSize? }?)`, `lz4FrameCompress(source)` | LZ4 frames |
| `zstdDecompress(source, { maxSize? }?)`, `zstdCompress(source, { level? }?)` | Zstandard; the encoder is ruzstd's, whose only implemented level is 1 (roughly zstd's own level 1), so any other `level` is an error. Frames carry a content checksum |
| `lzmaDecompress(source, { format?, maxSize? }?)` | `.lzma` (default) or `xz`, decoding only |

Every decoder takes `maxSize`, the most it will produce (default 1 GiB), and enforces it while
producing, never only afterwards: DEFLATE and zstd through their libraries' limits, LZMA and XZ
through a sink that refuses the byte that would exceed the cap, LZ4 blocks by refusing a
`decompressedSize` past it before anything is allocated. That matters because the decoder's
heap is Rust's, outside Luau's memory limit. Exceeding the cap is an error, never a truncated
result. All of it is pure Rust: `miniz_oxide`, `lz4_flex`, `ruzstd`, `lzma-rs`.

### Digests (`bytes-digests`)

`crc32(data, seed?)`, `adler32`, `fnv1a32`, `xxh32(data, seed?)` return numbers; `fnv1a64`,
`xxh64(data, seed?)`, `xxh3` return integers carrying the 64 bits; `md5`, `sha1`, `sha256` and
`blake3` return lower-case hex; `digest(data, algorithm)` returns the raw bytes of any of them
(checksums big-endian). For data that arrives in pieces, `hasher(algorithm)` returns a
`dream_bytes_Hasher`: `update(data)` any number of times, `finish()` for the hex, `finishBytes()`
for the bytes, `value()` for the integer of a checksum, `reset()` to start over. `finish` leaves
the state intact, so a running digest can be read at any point.

### Text (`bytes-text`)

`decode(source, encoding, { strict? }?)` turns bytes in any WHATWG-labelled encoding
(`windows-1252`, `latin1`, `shift_jis`, `euc-kr`, `gbk`, `koi8-r`, `macintosh`, `utf-16le`,
`utf-16be`, and the rest of `encoding_rs`'s table, aliases included) into a string; malformed
input becomes U+FFFD unless `strict`. `encode(text, encoding)` goes the other way and refuses a
character the encoding lacks; the UTF-16 labels produce real UTF-16, not the UTF-8 the WHATWG
spec substitutes. `encodingName(label)` gives the canonical name, `isUtf8(data)` the check.

### What it costs

Measured by `benches/bytes.rs` (`cargo bench --features bytes,bytes-codecs,bytes-digests,bytes-text,jit -- bytes_`)
on an i7-10870H. Per call, from a Luau loop:

| Read | Interpreted | Native |
|---|---:|---:|
| `buffer.readu32` + `bit32.byteswap`, Luau's own | 116 ns | 4.3 ns |
| `bytes.readu32be`, the module | 65 ns | 94 ns |
| `B:readu32be`, the receiver | 68 ns | 5.9 ns |
| `B:readi64be` | 66 ns | 5.1 ns |
| `B:readu24be` | | 6.5 ns |
| `B:writeu32be` | | 5.3 ns |
| `bytes.readf16` | 69 ns | |
| `bytes.readCString`, 16-byte field | 157 ns | |
| `bytes.readVarint` | 64 ns | |

A lowered receiver call is within two nanoseconds of Luau's own lowered builtins, and a module
call, at 65 ns, is faster than the interpreted builtin pair it replaces. Over one megabyte, one
call each: `find` with an absent needle 23 GiB/s, `equals` 28 GiB/s, `count` of a byte
4.6 GiB/s, `slice` 3.1 GiB/s, `crc32` 23 GiB/s, `xxh3` 17 GiB/s, `blake3` 3.7 GiB/s, `sha256`
238 MiB/s (no SHA extensions on that CPU), `inflate` of a compressible megabyte 1.6 GiB/s of
output, `deflate` at level 1 2.8 GiB/s, LZ4 block decompression 1.1 GiB/s and compression
5.9 GiB/s, `decode` of Windows-1252 1.5 GiB/s, `isUtf8` 21 GiB/s. The boundary is a fixed
few tens of nanoseconds; the rest is the library's own speed.

`frame` and `matchSpans` over one megabyte of 16-byte-header records holding 8-byte-header
fields (26000 records, 78000 fields; a verified build under a load average of 4.5):

| Work | One call | The loop it replaces |
|---|---:|---:|
| `frame`, records and their fields checked | 0.98 ms | 9.8 ms, a Luau walk (interpreted) |
| `matchSpans`, one text field per record | 1.02 ms | 7.7 ms, `isMatch` per record |

About 9 ns per chunk framed and 39 ns per span matched, the regex's own time included.

`translate` is for when the normalized text itself is needed; to compare or key by it, an intern
pool's rules policy (below) does the same without making the string. Folding a 36-byte path
with `translate(path, "ABC…Z\\", "abc…z/")` costs 141 ns a call against 470 ns for
`string.gsub(string.lower(path), "\\", "/")`; a path already folded costs 108 ns against 230 ns
for `string.lower` plus a `string.find` for the separator.

## dream.data

Feature `data`; module `@dream/data`, `l3i::data::DataExtension`. The bulk-data plane: dense
typed storage, selection, movement, ordering and reduction over Luau buffers, executed natively
with no per-element callback and no per-element host crossing. Luau is the control plane, policy
and domain semantics; a script says which bulk operation it needs and l3i owns the element loop.
It is also where the dialect's [recognized pipelines](@/docs/dialect.md#recognized-data-pipelines)
go when a reduction over a buffer slice is simple enough to be one call.

```luau
local data = require("@dream/data")
local total = data.sum(samples, "f32", 0, n)
local hot = data.compare(samples, "f32", 0, n, "gt", threshold)
local count = hot:count()
local order = buffer.create(n * 4)
data.argsort(samples, "f32", 0, n, order)
data.gather(samples, "f32", 0, n, order, sorted, 0)
```

### Spans

The operand everywhere is a **span**, `(buffer, kind, offset, count)`: `count` elements of `kind`
starting `offset` bytes into a buffer. Kinds are the `buffer` library's own little-endian
representations, `u8 i8 u16 i16 u32 i32 f32 f64`. A kind may carry a stride and a field offset:
`"f32@16"` is the first f32 of every 16-byte record and `"f32@16+12"` the field twelve bytes in,
which is how packed records live in buffers: a simulated body, an entity slot, a token record.
Element-wise operations take the layout for every operand, so a record field updates in place;
`gather` reads a possibly strided field into a contiguous column and `scatter` writes one back.

A span is validated once, before any element is read: offset and count non-negative, and
`offset + (count - 1) × stride + size ≤ length`, or it raises before the operation starts. The
offset of an empty span must still be in range. A span is a descriptor, never an allocation.

Element values cross as Luau numbers with exactly the `buffer` library's conversions: integer
kinds read exactly and write by truncating toward zero then wrapping to the width (NaN writes
zero; values beyond ±2^63 saturate before the wrap), `f32` rounds on write, `f64` is exact.
Reductions accumulate in `f64` in element order, so `sum` over a span equals the handwritten
`buffer.read*` loop bit for bit, `f32` included, and integer sums stay exact below 2^53.

Two more shapes appear. An **index vector** is a `u32` buffer of zero-based element positions:
the input of `gather` and `scatter`, the output of `argsort` and `partition`. A **selection**
(`dream.data.Selection`) is a bitset over the positions of a span of one length, produced by
`compare`, combined with boolean algebra, counted, or turned into an index vector. Every
operation that produces output takes the storage from the caller, `into` for selections and
`out` for buffers, so steady state allocates nothing.

### The operations

| Operation | Reads | Writes or returns | Empty input | NaN | Aliasing | Raises before any write |
|---|---|---|---|---|---|---|
| `sum` | span | number, `f64` accumulation in element order | `0` | propagates as Luau `+` | n/a | bounds |
| `min`, `max` | span | number or nil | nil | a leading NaN stays; later NaNs never replace (Luau `<` and `>`) | n/a | bounds |
| `argmin`, `argmax` | span | zero-based position or nil | nil | as `min` and `max` | n/a | bounds |
| `count` | span | how many satisfy `cmp` | `0` | every comparison false except `ne` (IEEE) | n/a | bounds, comparison name |
| `any`, `all` | span | boolean; stops at the first deciding element | `false`, `true` | as `count` | n/a | bounds, comparison name |
| `compare`, `K:select<Cmp>` | span | selection of the positions satisfying `cmp`, into `into` (resized) or new | empty selection | as `count` | `into` may be any selection | bounds |
| `Selection:intersect/union/xor/difference` | two selections of one length | selection, into `into` or new | | | `into` may be either operand | lengths differ |
| `Selection:complement` | selection | selection | | | `into` may be the source | |
| `Selection:count/len/get/any/all/clear` | selection | number, boolean, nothing | `all` of nothing is true | | | |
| `Selection:indices` | selection | u32 index vector and count, into `out` from `offset` or a new exact buffer | empty buffer, 0 | | | `out` too small |
| `gather` | source span, index vector | `destination[i] = source[idx[i]]`; count written | 0 | copied bit-exact | sequential, read then write per element, in index order | bounds, any index ≥ source count |
| `scatter` | source span, index vector | `destination[idx[i]] = source[i]`; count written | 0 | copied bit-exact | sequential; a repeated index keeps the last write | bounds, any index ≥ destination count |
| `fill` | | every element of the span | | writes zero for integer kinds | | bounds |
| `add` | two spans | `out[i] = l[i] + r[i]` | | Luau `+` | `out` may be either input | bounds, all three spans |
| `scale` | span | `out[i] = s[i] × factor` | | Luau `*` | `out` may be the source | bounds |
| `clamp` | span | `out[i] = clamp(s[i], low, high)` | | passes through, as `math.clamp` | `out` may be the source | bounds, `low > high` or NaN bounds |
| `argsort` | key span | u32 permutation into `out` from `outOffset`; count | 0 | NaN keys last, in original order | keys are copied before sorting, so `out` may overlap them | bounds |
| `partition` | key span | u32 index vector, satisfying positions first then the rest, each in original order; the accepted count | 0 | as `count` | as `argsort` | bounds, comparison name |

Comparisons are named: `"eq"`, `"ne"`, `"lt"`, `"le"`, `"gt"`, `"ge"`. Aliasing is defined,
never assumed: element-wise operations read each element and write its result before the next,
so an output overlapping an input sees the sequential result, and in-place forms are the ordinary
case. Out-of-range indices raise before anything is written.

`argsort` is stable: equal keys keep their position order. Without scratch it sorts a copy of
the keys paired with positions, so the buffer is read once and `out` may overlap the keys. With a
caller scratch of `count × 4` bytes it is a bottom-up merge of the index vector between `out` and
the scratch, reading keys from the span as it compares, so nothing is allocated in steady state.
That is the shape a spatial-index builder otherwise hand-rolls in Luau. Keys, `out` and scratch
must then not overlap, and the call refuses them if they do. The scratch form reads keys on every
compare and runs about 25 percent slower than the allocating form at 65,536 elements; use it where
allocation, not latency, is the cost.

### Typed receivers

Every module function is a bound Rust function invoked once per bulk operation: one host
crossing amortized over the span, element loops in Rust over raw pointers after the single bounds
check. That is the portable path and the correctness oracle for everything else.

The reductions also exist as methods of **typed receivers**, one class per element kind,
`dream_data_Kind_u8` through `dream_data_Kind_f64`, from `data.u8(stride?)` through
`data.f64(stride?)`, or `data.kind("f32@16")` when the layout is only known at run time. Every
receiver has the same thirty-nine members: `sum`, `min` and `max` over the whole span, and each
of `sum`, `min`, `max`, `count`, `any` and `all` with a comparison suffix (`sumGt`, `minLe`,
`countEq`, `anyNe`, `allGe`, ...) over the elements that compare so against a threshold argument,
plus `selectEq` through `selectGe`, which is `compare` written into a caller selection that is
resized to `count` and returned. Bound, both spellings run one shared element loop.

```luau
--!native
local data = require("@dream/data")
local F: dream_data_Kind_f32 = data.f32(16)      -- the f32 field of 16-byte records
local heaviest = F:max(bodies, 12, count)         -- offset 12: the field twelve bytes in
local moving = F:countGt(bodies, 0, count, 0.01)
```

Under `jit`, a call on a receiver the script annotated with its class in a `--!native` chunk is
lowered by `l3i::data::lowering` into Luau IR: the receiver tag, buffer tag, whole non-negative
bounds and the span's fit are checked first, anything else jumps to the binder whose errors are
the contract, and the loop reads elements with the same `BUFFER_READ*` the `buffer` library
lowers to and accumulates in double in element order. The element kind is the receiver's *type*,
so a site compiles exactly one loop pair, a contiguous one with constant steps and eight elements
unrolled per cursor round trip, and a strided one stepping by the value's stride. A single
untyped receiver dispatching on a kind word was tried first and cost sixteen loops per site,
enough to push a function with a hundred sites past Luau's 32K-block limit, which silently leaves
the whole function interpreted. Loop state lives in the receiver's scratch words, since Luau IR
has no loop-carried values and the VM's registers are memory on the Lua stack anyway.

Three things the lowering needs from you: the receiver annotated as a local, a local buffer, and
exactly one result. `return K:sum(..)` in tail position compiles with a multi-value result count,
which the hook declines; assign the result first. The dialect's bridge always does.

No worker threads, no SIMD framework, no second JIT. One semantic implementation, a bound path,
an optional native lowering: the same pattern as `bytes.math()` and the intern pool.

### Reaching it from the dialect

The extension publishes its module table under the global `__l3i_data` as well as `@dream/data`.
A recognized pipeline's lowering snapshots that global in the chunk prelude and calls it once with
the span the ordinary prologue already validated and normalized; when the global is absent, the
runtime did not install the extension and the same code takes its scalar loop. Recognition is
narrow and semantic: the lowered call must compute exactly what the scalar loop computes, in the
same representation and with the same errors, or it is not emitted. An arbitrary filter or
projection has unknown effects and is never moved into the data plane; the fused scalar loop is
its correct and final form. Nothing requires recognition to reach the fast path: the explicit API
is first-class.

The alias is an ordinary global, read once when the chunk starts, like the other globals the
prelude snapshots. Whoever can assign a chunk's globals can redirect its recognized pipelines, and
with them the buffers they read, exactly as replacing `buffer` would. Scripts that do not trust
each other must not share a writable global environment: under `Runtime::sandbox_globals` the
globals are read-only, and a script that assigns `__l3i_data` only shadows it in its own
environment. The outputs `argsort` and a selection allocate are host memory the VM's memory limit
does not see.

### What it costs

`cargo bench --features data -- data_` (interpreter) and `--features data,jit` (every chunk
compiled natively, so the loop baselines are CodeGen loops), on an i7-10870H: retired
instructions per call (minimum of five rounds of 64 calls) and Criterion median time. `u8` sums
and `> 127` counts over `n` bytes; the recognized dialect form includes the slice prologue; the
scalar fallback is the same source run without the extension. Globals are sandboxed in both, so
the interpreter takes the `buffer.readu8` fastcall and Luau's own loops are about three times
faster than they would be unsandboxed. Those are the fair baselines.

Interpreter:

| n | dialect, recognized | dialect, scalar fallback | explicit `data.sum` / `data.count` | handwritten `readu8` loop |
|---:|---:|---:|---:|---:|
| sum 16 | 3,907 (431 ns) | 7,395 (778 ns) | 1,945 (238 ns) | 4,540 (586 ns) |
| sum 256 | 7,027 (733 ns) | 77,235 (6.26 µs) | 5,065 (510 ns) | 56,860 (4.45 µs) |
| sum 4,096 | 56,947 (5.10 µs) | 1,194,675 (89.8 µs) | 54,985 (5.94 µs) | 893,980 (62.0 µs) |
| sum 65,536 | 855,746 (89.2 µs) | 19,073,717 (1.40 ms) | 853,705 (82.5 µs) | 14,287,901 (1.00 ms) |
| count 16 | 3,918 (647 ns) | 7,625 (804 ns) | 2,156 (216 ns) | 5,120 (692 ns) |
| count 256 | 7,278 (787 ns) | 82,959 (7.91 µs) | 6,716 (656 ns) | 67,204 (5.56 µs) |
| count 4,096 | 61,038 (5.39 µs) | 1,287,792 (108 µs) | 79,676 (7.12 µs) | 1,060,282 (84.9 µs) |
| count 65,536 | 921,198 (84.7 µs) | 20,565,998 (1.75 ms) | 1,247,036 (113 µs) | 16,949,988 (1.33 ms) |

There is no small-n crossover: at 16 elements the recognized pipeline is already ahead of the
handwritten loop, and the explicit call at well under half its instructions. The dialect form
carries about 2,000 instructions of prologue over the explicit call, independent of n.

| n | explicit `data.sum` f32 | handwritten `readf32` loop | `data.gather` f32 | handwritten indexed copy | stable `data.argsort` f32 | `table.sort` index table with a comparator (unstable) |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 6,273 (648 ns) | 223,837 (15.7 µs) | 14,070 (1.20 µs) | 599,977 (49.5 µs) | 63,129 (5.15 µs) | 4,247,685 (369 µs) |
| 4,096 | | | | | 1,251,732 (106 µs) | 108,144,296 (9.22 ms) |
| 65,536 | 1,050,753 (116 µs) | 56,952,163 (4.17 ms) | 2,886,390 (238 µs) | 153,094,071 (11.8 ms) | 16,006,037 (1.88 ms) | 2,307,862,971 (199 ms) |

Native code (`jit`): the recognized sum runs the receiver's unrolled IR loop, and the handwritten
and fallback loops are Luau CodeGen loops.

| n | dialect, recognized (IR loop) | dialect, scalar fallback (CodeGen loop) | explicit `data.sum` (bound) | handwritten `readu8` (CodeGen loop) |
|---:|---:|---:|---:|---:|
| sum 16 | 1,367 (135 ns) | 1,719 (151 ns) | 1,635 (136 ns) | 1,395 (116 ns) |
| sum 256 | 2,777 (393 ns) | 9,639 (879 ns) | 4,275 (361 ns) | 9,075 (831 ns) |
| sum 4,096 | 25,337 (4.74 µs) | 136,359 (12.7 µs) | 46,515 (4.10 µs) | 131,955 (12.1 µs) |
| sum 65,536 | 386,297 (75.3 µs) | 2,163,879 (184 µs) | 722,355 (63.3 µs) | 2,098,035 (197 µs) |
| count 16 | 1,147 (93 ns) ¹ | 1,749 (143 ns) | 1,919 (178 ns) | 1,393 (112 ns) |
| count 256 | 4,396 (391 ns) ¹ | 10,259 (922 ns) | 6,479 (560 ns) | 9,183 (916 ns) |
| count 4,096 | 56,354 (5.91 µs) ¹ | 146,384 (14.6 µs) | 79,439 (6.87 µs) | 133,788 (13.7 µs) |
| count 65,536 | 887,721 (90.3 µs) ¹ | 2,324,444 (234 µs) | 1,246,799 (101 µs) | 2,127,528 (223 µs) |
| min 256 | 4,090 (378 ns) ¹ | | 5,823 (437 ns) | 10,350 (969 ns) |
| min 65,536 | 787,450 (69.6 µs) ¹ | | 1,115,583 (74.4 µs) | 2,425,710 (219 µs) |

¹ The receiver's IR loop called directly (`K:countGt`, `K:min` on an annotated local), which is
what the bridge emits; the rows above it carry the slice prologue on top.

| n | explicit `data.sum` f32 | `readf32` CodeGen loop | `data.gather` f32 | indexed copy, CodeGen | stable `data.argsort` | `table.sort` comparator, CodeGen (unstable) |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 5,514 (432 ns) | 9,843 (849 ns) | 12,913 (982 ns) | 13,184 (1.28 µs) | 63,111 (4.80 µs) | 1,138,409 (99.6 µs) |
| 4,096 | | | | | 1,275,925 (100 µs) | 28,714,511 (2.20 ms) |
| 65,536 | 1,049,994 (83.6 µs) | 2,294,643 (193 µs) | 2,885,233 (201 µs) | 3,146,624 (339 µs) | 16,327,107 (1.75 ms) | 612,840,657 (52.4 ms) |

What the machine shape says:

- Luau's own CodeGen loop costs about 32 instructions per byte: `FORNLOOP`, the `readu8`
  fastcall with its checks, the add through a VM register. The bound Rust loop costs about 18
  including its once-per-call argument parsing, and wins in time from 256 elements up.
- The first IR loop kept its cursor and accumulator in the receiver's scratch, so every element
  paid three memory round trips: fewer instructions than the bound call but slower at 65,536,
  latency bound on store-to-load forwarding. Unrolling the pure-add `sum` body to eight elements
  per block, one cursor and one accumulator round trip per eight, cut it to 6 instructions per
  byte: faster than the bound call at every size and within noise of the handwritten native loop
  at 16 elements, with no call overhead left to amortize.
- `count`, `min` and `max` branch per element, which ends an IR block, so they cannot fold into
  one block. They unroll as a chain of one block per element with the cursor advanced once per
  eight and the accumulator touched only on a hit. That halves their time too, and called
  directly they beat the handwritten native loop even at 16 elements.
- Strided layouts made the bound loops faster, not slower: the stride-aware span let the
  compiler vectorize the u8-to-double sum. The native loops dispatch once on contiguous versus
  strided and keep their constant-step unrolled form for contiguous spans; a run-time stride in
  the unrolled body cost 40 percent. Net: the IR sum wins below about 4,096 elements and the
  bound sum above, by 19 percent at 65,536, so the bridge switches the unfiltered sum to the
  bound call past that size and keeps the receiver for everything else. A size-based switch for
  more cases is the one tuning deliberately left until a workload asks for it.
- Writing comparisons into a selection from native code first measured 222 µs at 65,536 bytes:
  a read-modify-write of the bitset word per hit through two pointer loads. Reshaped as one
  64-element pass per word, each hit ORing a compile-time bit into a scratch word written once,
  it runs in 89.6 µs including the `count()` afterwards, faster than the native count alone and
  than bound `compare` then `count`. Composing selections natively is the fast path, not a
  concession, and `compare` into a reused selection then `count()` costs within 5 percent of the
  direct count at every size.

Interpreter and native agree on the ordering of everything else: the bound operations beat the
best obvious Luau by 2× (gather, native) to 300× (stable argsort against a comparator sort,
native) and never lose, down to 16 elements.

### What real workloads asked for

Hot loops in downstream code were read for the shapes this plane must serve, and each shipped:
strided layouts (16-byte simulation bodies, 8-byte entity slots, 12-byte token records read by
field), `argsort` through caller scratch (a spatial-index builder otherwise hand-rolls that
stable merge), buffer sinks (a spatial query writes a filtered index list into a caller buffer
with a capacity), and thresholds held in locals (every comparison in those loops is against a
local). Nothing on the original list remains deferred.

## dream.intern

Feature `intern`; module `@dream/intern`, `l3i::intern::InternExtension`. Textual identity as
numbers: a pool turns a byte sequence, a string or a span of a `buffer`, into a small whole
number under one equivalence policy. Equivalent inputs get the same number and different ones
different numbers, so from then on identity is `==` on numbers and a table read. It is for the
names a content format repeats (record ids, asset paths, script and resource names), where the
same few hundred thousand identities occur millions of times. It knows nothing about any format;
what an identity means is the caller's business.

```luau
local intern = require("@dream/intern")
local ids = intern.new("ascii-nocase")
local id = ids:intern(record, offset, length)   -- a buffer span: no Luau string is made
assert(id == ids:intern("Caius Cosades") and id == ids:intern("CAIUS COSADES"))
local npcs = {}
npcs[id] = npc                                  -- a dense key: the table's array part
print(ids:resolve(id))                          -- the first spelling the pool saw
```

| Function | Returns |
|---|---|
| `intern.new(policy?)` | An empty pool: `"exact"` (the default), `"ascii-nocase"`, or a rules table (below) |
| `pool:intern(source, offset?, length?)` | The identity of `length` bytes of a string or buffer from `offset` (the whole of it by default), added on first sight |
| `pool:interner()` | The same operation as a plain function bound to the pool |
| `pool:find(source, offset?, length?)` | The identity the bytes already have, or nil; never adds one |
| `pool:resolve(token)` | The first spelling the pool saw for `token`, as a string; under rules, the normal form |
| `pool:count()`, `pool:memory()`, `pool:policy()` | Identities held (also the last token handed out), native bytes held, the policy |

### Policies

`exact` compares bytes. `ascii-nocase` makes `A`..`Z` equal to `a`..`z` and compares every other
byte exactly, UTF-8 included: SQLite's `NOCASE`. The folding happens inside the hash and the
comparison, eight bytes at a time; no folded copy of the input is made, in Luau or in Rust.

A rules table normalizes keys before they compare, in this order: ASCII letters lowered when
`nocase`, up to two bytes replaced by others (`replace = { ["\\"] = "/" }`; a replaced byte isn't
also lowered, and NUL can't be replaced), runs of the `collapse` byte made one, and the
`trimStart` and `trimEnd` bytes removed from the ends. Case-insensitive paths with either
separator are one rules table:

```luau
local paths = intern.new({ nocase = true, replace = { ["\\"] = "/" }, collapse = "/", trimStart = "/" })
local id = paths:intern("\\Meshes\\X//Rock.NIF")
assert(id == paths:intern("meshes/x/rock.nif") and paths:resolve(id) == "meshes/x/rock.nif")
```

The pool keeps each identity's normal form, and that is what `resolve` returns: rules that drop
bytes make a first spelling a poor answer to "which key is this", and a caller that wants the key
wants it normal. Natively, the byte map runs eight bytes at a time inside the hash and the
compare, and no normalized copy is made; a span that needs a run collapsed or a byte trimmed takes
the binder, which normalizes it once into the pool's reused scratch. Unicode case folding, and any
rule a byte map and these switches can't say, stay the domain's, applied before interning.

### Tokens

A token is the identity's 1-based position in its pool, `1, 2, 3, ...` in first-seen order, as a
Luau number. A number and not an `integer`, because Luau keeps dense number keys in a table's
array part and always hashes an `integer` key: reading a table by token costs about a fifth of
the instructions of reading it by an `integer` or a string, and a third of an `integer` key's L1
misses (below). Dense tokens also fit a `u32` column, `buffer.writeu32(column, i * 4, id)`.

Tokens are pool-relative: two pools hand out the same numbers, and a token means something only
to the pool that made it. A pool only grows (nothing is removed or reused), so a token never
changes meaning while its pool lives and needs no generation or pool bits. Drop the pool and its
memory goes at once, in one native free per array; tokens kept after that are plain numbers. For
content that loads and unloads, a pool per load is the unit.

### Storage

The first spelling of each identity is copied once into the pool's arena and kept as it was
seen: under `ascii-nocase`, `cAiUs CoSaDeS` interned first is what `resolve` returns for every
spelling after it. A duplicate copies nothing and allocates nothing, native or VM. The index is
open addressing with each slot holding its hash: 8 bytes per slot at three quarters load at
most, 8 bytes per identity, and the text. None of it is VM memory, so the collector never
traverses a pool.

### The hot path

Under `jit`, in `--!native` code with the pool annotated, `pool:intern` and `pool:find` lower to
native code. A lookup that finds its identity never leaves it: the receiver's tag check, the
argument tag and range checks, the policy's hash eight bytes at a time, bit-identical to the
Rust one, the probe of the slot array and a word compare against the arena, then the token in
the result register. Anything else (an insert, a wrong type, an offset that is not a whole
number, a span past the end) runs the bound method in place, through the binder, and native
code carries on after it; so does `find` on an identity the pool lacks, natively, returning nil.

```luau
--!native
local ids: dream_intern_Pool = intern.new("ascii-nocase")
for i = 0, count - 1 do
    local id = ids:intern(text, buffer.readu32(spans, i * 8), buffer.readu32(spans, i * 8 + 4))
end
```

The annotation is what lowers it: Luau's compiler learns a userdata type only from one. Lengths
under 8 bytes, 8 to 16 and 17 to 32 each get a straight-line hash and compare; longer spans
loop. `pool:interner()` returns the same operation as a plain function bound to the pool, which
skips the method lookup; it is a call, not a namecall, so no hook sees it and it never lowers.
Use it only in interpreted code. The bound function keeps its pool alive.

### What it costs

Counted by `benches/intern.rs` (`cargo bench --features intern,jit -- intern`) on an
i7-10870H in retired instructions and cache misses from the CPU's own counters, not time, every
chunk native. The workload is parse-shaped: 350K record-id-like identities, 5M occurrences
drawn Zipf-distributed, a quarter of them in random case, read from a text buffer through an
index of spans. Per occurrence, the span reads included, on the pass that sees only duplicates:

| Case-insensitive identity | Instructions | L1D misses | VM allocated | VM held | Native held |
|---|---:|---:|---:|---:|---:|
| `string.lower(buffer.readstring(..))` then a string-keyed table | 1378 | 12.4 | 93 MiB | 36 MiB | 0 |
| the function from `pool:interner()` | 616 | 3.5 | 0 | 0 | 17.5 MiB |
| `pool:intern(text, offset, length)`, lowered | 315 | 3.1 | 0 | 0 | 17.5 MiB |

Exact identity on the same text: 244 instructions lowered against 706 for `buffer.readstring`
and a string-keyed table. The pool allocates nothing in the VM and leaves the collector nothing
to walk: a full collection with the string table live costs 40M instructions and freeing it
59M more; a pool costs neither. 350K identities take 15 to 17.5 MiB native, about 50 bytes each
with the spelling. The first pass, where inserts take the binder path, costs 368 instructions
per occurrence against 1467 for `string.lower`.

Per call, cache-resident, a 16-byte duplicate:

| Call | Instructions |
|---|---:|
| `pool:intern(string)`, lowered | 163 |
| `pool:intern(buffer, 0, 16)`, lowered | 201 |
| `pool:find(string)`, absent, lowered | 108 |
| the function from `pool:interner()`, buffer span | 532 |
| `Interner::intern` from Rust, `ascii-nocase` | 184 |
| `map[string.lower(buffer.readstring(..))]` | 1311 |

A 31-byte path through the rules pool above, duplicate: 522 instructions lowered, whether the map
folds it or it is already normal; 2178 when a run of `/` sends it to the binder; the Luau it replaces,
`map[string.gsub(string.lower(path), "\\", "/")]`, is 5008. From Rust, `Interner::intern` on the
same key is 898 and 551.

The lowered call costs about what the Rust lookup does: the C-call protocol, 277 instructions
for even a hand-written `lua_CFunction`, is gone. Reading a table keyed by tokens is 25
instructions and a quarter to one L1 miss, against 140 and 2.5 to 4 for `integer` keys and 122
and 3 to 5.5 for strings. `resolve` is about 900 instructions, most of it making the string.

## dream.intl

Feature `intl`; module `@dream/intl`, `l3i::intl::IntlExtension`. Three internationalization
primitives, each usable without the others: BCP 47 locale identity, CLDR plural rules, and
CLDR decimal formatting, all from [ICU4X](https://github.com/unicode-org/icu4x) 2.3 and its
compiled CLDR data. The data is in the binary: no file, no provider, and no capability is
needed. The module has no current locale, no fallback order, no message catalog and no
message syntax; which locale to use, which translation a category picks and where a formatted
number goes are the application's.

```luau
local intl = require("@dream/intl")

local polish = intl.locale("pl_pl")                  -- pl-PL
local cardinal = intl.pluralRules(polish, "cardinal")
print(cardinal:category(1), cardinal:category(2), cardinal:category(5))   -- one few many
print(cardinal:category("1.00"))                     -- other: two visible fraction digits

local ordinal = intl.pluralRules("en", "ordinal")
print(ordinal:category(21))                          -- one, as in 21st

local french = intl.decimalFormatter("fr", { minFractionDigits = 2, maxFractionDigits = 2 })
print(french:format("1234567.895"))                  -- 1 234 567,90, narrow no-break spaces
```

| Function | Returns |
|---|---|
| `intl.locale(tag)` | A `dream_intl_Locale`: the tag parsed and spelled canonically |
| `intl.canonicalize(tag)` | The canonical spelling as a string, without making a locale |
| `intl.pluralRules(locale, type?)` | A `dream_intl_PluralRules`: `"cardinal"` (the default) or `"ordinal"` |
| `intl.decimalFormatter(locale, options?)` | A `dream_intl_DecimalFormatter` under `{ grouping?, minFractionDigits?, maxFractionDigits? }` |
| `locale:tag()`, `tostring(locale)` | The canonical tag |
| `locale:baseName()` | The tag without its extensions (`ar-EG` of `ar-EG-u-nu-latn`) |
| `locale:language()`, `locale:script()`, `locale:region()`, `locale:variants()` | The subtags; script and region may be nil, variants is a list |
| `rules:category(value)` | `"zero"`, `"one"`, `"two"`, `"few"`, `"many"` or `"other"` |
| `rules:categories()` | The categories these rules can select, in CLDR order, ending with `"other"` |
| `rules:locale()`, `rules:type()` | The rules' canonical tag and kind |
| `formatter:format(value)` | The number as text for the locale |
| `formatter:locale()`, `formatter:resolvedOptions()` | The canonical tag; the options with defaults filled in |

A locale argument is a `dream_intl_Locale` or a tag string. Values are a Luau number, a Luau
`integer`, or a decimal string. Handles are built once and reused; build them where the locale is
chosen, not per value.

### Locales

Parsing is ICU4X's BCP 47 parser. A tag is a language, an optional script and region, variants,
and Unicode, transformed, private-use and other extensions. Parsing spells it canonically: the
language lowercase, the script title case, the region uppercase, variants lowercase and sorted,
extension keys and values lowercase and sorted, so `DE-de-u-NU-latn-CA-gregory` is
`de-DE-u-ca-gregory-nu-latn`. Underscores separate subtags as hyphens do (`zh_hant_tw` is
`zh-Hant-TW`): ICU4C and POSIX spell locales that way. Two locales are `==` when their canonical
tags are, and the tag is the identity to key a cache with. `intl.canonicalize` makes no heap
allocation for a tag of up to 64 bytes without extensions.

Canonicalization is syntactic. CLDR's alias replacement (`iw` to `he`, `sh` to `sr-Latn`) and
likely subtags are not applied, so tags that differ only there are different locales. An empty
string, a malformed subtag (`english`, `en-US-`, `en--US`), whitespace, or a legacy grandfathered
tag (`i-klingon`) is an error naming the call and ICU4X's reason:
`intl.locale: 'english' is not a BCP 47 language tag: the given language subtag is invalid`.

### Plural rules

`rules:category(value)` evaluates the locale's CLDR rules. A decimal string keeps what it shows,
and CLDR's rules can see it: English `1` is `one` but `"1.0"` and `"1.00"` are `other`; French
`1000000` is `many` but `"1000000.0"` is `other`; Latvian `"0.1"` is `one` and `"0.10"` is
`other`. A Luau number carries no visible fraction digits, so it reads as the shortest decimal
that round-trips to the same double: `1.0` is `1`, `0.1` is `0.1`. Pass a string when the
digits a reader will see matter. Negative values select as their magnitude does. A decimal
string is an optional sign, digits with at most one decimal point, and an optional exponent
(`-12.50`, `.5`, `1e6`; the exponent is multiplied out, so it is not CLDR's compact-notation
operand). NaN, the infinities, a malformed string and a string of more than 32767 digits either
side of the point are errors; there is no silent `other`.

A locale CLDR has no rules for takes its nearest parent's, down to the root rules, where every
number is `other`; that fallback is ICU4X's. Unicode extensions do not change plural rules.

### Decimal formatting

`formatter:format(value)` writes the locale's digits, decimal separator, grouping separator and
group sizes, and signs: `1,234,567.891` in English, `1.234.567,891` in German,
`1 234 567,891` with U+202F narrow no-break spaces in French, `١٢٣` in `ar-EG`,
`12,34,567` in Hindi. A Unicode numbering system extension picks the digits: `ar-EG-u-nu-latn`
writes `123`, `en-u-nu-arab` writes `١,٢٣٤`.

| Option | Values | Default |
|---|---|---|
| `grouping` | `"auto"` as the locale groups (Polish writes `1234` but `12 345`); `"never"`; `"min2"`, only with two digits before the first separator (`1234`, `12,345`) | `"auto"` |
| `minFractionDigits` | 0 to 100: fraction digits always written, zeros padding a shorter number | 0 |
| `maxFractionDigits` | 0 to 100: more are rounded half to even, and trailing zeros past the minimum are dropped | the larger of 3 and the minimum |

The defaults are ECMA-402's and ICU's decimal style. Rounding is half to even:
`"0.125"` is `0.12` and `"0.135"` is `0.14` at two digits, `2.5` is `2` and `3.5` is `4` at
none. A decimal string formats exactly, leading zeros and an explicit `+` dropped. A Luau
number formats as its shortest round-trip decimal, not its binary value: `1234567.895` rounds to
`…,90` at two digits, as ICU rounds it. Negative zero keeps its sign (`-0`, and `-0.0004` at
three digits is `-0`), ECMA-402's default sign display. An unknown option, a grouping name not
in the table, a fraction digit count that is not a whole number from 0 to 100, or a minimum above
the maximum is an error naming the option; so are the numbers plural rules refuse.

### Not here

The first pass is the three primitives. Percent, currency, compact and scientific notation,
significant digits, other rounding modes and sign displays, measurement units, dates and times,
collation, and CLDR alias canonicalization are not in it. A percentage is not a decimal with
`%` appended: CLDR places, spaces and sometimes reorders the sign per locale, so it waits for
real percent formatting rather than an approximation.

### What it costs

Counted by `benches/intl.rs` (`cargo bench --features intl -- intl`) on an i7-10870H:
retired instructions from the CPU's counters, best of five rounds, with nanoseconds beside
them. Luau rows are interpreted and have the loop subtracted.

| Warm call | Rust instr | Rust ns | Luau instr | Luau ns |
|---|---:|---:|---:|---:|
| a bound method that does nothing (`rules:type()`) | | | 695 | 45 |
| `category(5)`, Polish cardinal | 561 | 36 | 1253 | 92 |
| `category(23)`, English ordinal | 372 | 25 | 1057 | 81 |
| `category(1.5)`, Polish cardinal | 1884 | 132 | 2570 | 195 |
| `category("1.00")`, Polish cardinal | 990 | 63 | 1970 | 157 |
| `format(1234567)`, English | 1053 | 66 | 1839 | 139 |
| `format(1234567.891)`, English | 2322 | 188 | 3118 | 250 |
| `format("1234567.895")`, French, two digits | 1828 | 138 | 2981 | 245 |
| `intl.canonicalize("pt_br")` | 1037 | 80 | 1652 | 131 |

The boundary is the binder's own method call, about 700 instructions and 45 ns whatever the
method does, and nothing on top of it: the rest of each Luau row is the Rust row. A warm
`category` allocates nothing, native or VM. A warm `format` writes into a buffer its handle
keeps and allocates nothing native, except the one 16-byte digit array ICU4X's decimal takes
for more than eight significant digits, and in the VM only the Luau string a new result is; a
result Luau already interned costs no VM bytes. A double that is not a whole number goes through
ryu's shortest round-trip conversion, built in its small-table mode by ICU4X's decimal crate:
57 of the 132 ns of `category(1.5)`; whole numbers skip it.

Construction, from Luau: `intl.locale` 2470 instructions (213 ns), `intl.pluralRules` 4368
(452 ns), `intl.decimalFormatter("en")` 7645 (702 ns) and with an options table 10776
(1.1 µs, the table read included). From Rust, without the userdata, `PluralRules::new` is about
1370 instructions (140 ns) and `DecimalFormatter::new` 2229 to 4636 (219 to 395 ns) depending on
the locale's data. A live handle holds 224 VM bytes for a locale and 368 for rules or a formatter,
plus its canonical tag on the native heap; the collector sees one userdata each.

The feature adds ICU4X's locale, plural and decimal crates with their compiled CLDR data,
`fixed_decimal` and `ryu`: 29 crates, all Rust, about 5 seconds of a clean release build on
16 threads. A stripped thin-LTO binary that registers the extension grows by 288 KiB; one built
with the feature that never registers it does not grow.

## dream.luau

Feature `syntax`; module `@dream/luau`, `l3i::syntax::SyntaxExtension`. Luau's own parser for
scripts: `Luau::Parser`, the parser the compiler and the type checker use, under the runtime's
frozen fast flags, its tree built as Luau tables in native code. It is for tools written in Luau
that read Luau: linters, style checkers, formatters, documentation generators. Nothing in it
knows a rule; it hands a script the tree, comments and errors Luau's own tools see.

```luau
local luau = require("@dream/luau")
local result = luau.parse(source, { tokens = true })
for _, stat in result.root.body do
    print(stat.kind, stat.line, stat.column, stat.endLine, stat.endColumn)
end
for _, comment in result.comments do
    print(comment.kind, comment.line)
end
```

| Member | What it is |
|---|---|
| `luau.parse(source, options?)` | Parses a string or buffer. `options.dialect` is `"l3i"` (the default: comprehensions, slices and record patterns come back as source nodes) or `"luau"` (strict stock syntax, where they are errors); `options.declarations` allows definition-file syntax (`declare`, `declare extern type`); `options.tokens` adds the token stream. An unknown dialect or option raises |
| `luau.tokenKinds` | The token kinds in the token stream, by name (`name = 1` ... `error = 14`); read-only |

### The result

| Field | What it holds |
|---|---|
| `root` | The chunk, a `StatBlock` |
| `errors` | Every parse error: `kind = "Error"`, a span and `message` |
| `comments` | Every comment: `kind` `"line"`, `"block"` or `"broken"` (unterminated), and a span |
| `hotComments` | `--!strict`, `--!native` and the rest: `header` (before any code) and `content` |
| `lineStarts` | `lineStarts[n]` is the 1-based source index of line `n`'s first byte |
| `tokens` | With `{ tokens = true }`: a buffer of 12-byte records, a token each, comments included |

A syntax error is data. The parser recovers, the tree holds `ExprError`, `StatError` and
`TypeError` nodes where it did, and `errors` lists them; `parse` raises only for a bad argument
or when the VM cannot allocate.

### The tree

Every node is a table with `kind`, the class name without `Ast` (`StatLocal`, `ExprCall`,
`TypeReference`, 66 kinds in all), and an exact span: `line`, `column`, `endLine`, `endColumn`,
1-based, the end column inclusive, columns counted in bytes. A node's text is

```luau
string.sub(source, result.lineStarts[node.line] + node.column - 1,
    result.lineStarts[node.endLine] + node.endColumn - 1)
```

The other fields are Luau's own member names (`thenbody`, `elsebody`, `func`, `args`, `vars`,
`values`), so Luau's `Ast.h` is the reference. A few additions say what the tree folds away:
`ExprConstantString.quoteStyle` is `"single"`, `"double"`, `"backtick"`, `"long"` or
`"unquoted"` (a record key or an attribute name); every statement has `hasSemicolon`; `StatIf`
has `thenLocation` and `elseLocation`; `ExprCall` has `argLocation`.

A local is one `Local` table (`name`, `isConst`, `functionDepth`, `loopDepth`, `annotation`,
`shadow`) shared by its declaration and every `ExprLocal` that reads it, so `use['local'] ==
declaration` resolves scope with no scope tracking. A global read is an `ExprGlobal` with its
`name`.

The definitions declare every node as a `dream_luau_*` table type with a singleton `kind`, and
`dream_luau_Expr`, `dream_luau_Stat`, `dream_luau_Type`, `dream_luau_TypePack` and
`dream_luau_Node` as unions over them, so a strict walker refines a node by testing its `kind`:

```luau
--!strict
local function callee(expr: dream_luau_Expr): string?
    if expr.kind == "ExprCall" and expr.func.kind == "ExprGlobal" then
        return expr.func.name
    end
    return nil
end
```

### Tokens and trivia

Between two tokens there is only whitespace and comments. `comments` lists every comment, and
the token stream is Luau's own lexer over the whole source: each record is three `u32`s, the
kind, then the 1-based indices of the token's first and last bytes, so `string.sub(source,
first, last)` is its text. What precedes any node, a blank line, a comment, a semicolon, is then
exact:

```luau
local tokens = result.tokens :: buffer
for at = 0, buffer.len(tokens) - 12, 12 do
    local kind = buffer.readu32(tokens, at)
    local first, last = buffer.readu32(tokens, at + 4), buffer.readu32(tokens, at + 8)
    if kind == luau.tokenKinds.comment then
        print(string.sub(source, first, last))
    end
end
```

Interpolated strings lex as `interpolatedBegin`, `interpolatedMid` and `interpolatedEnd` around
their expressions, or `interpolatedSimple` with none; `longString` is `[[...]]`. Under the `l3i`
dialect adjacent `=` and `>` merge into one `symbol` token for `=>`. Tokens, comments, hot
comments and `lineStarts` always describe the original bytes, interpolation regions included.

### The dialect's nodes

Under the default dialect, [the l3i dialect](@/docs/dialect.md) appears in the tree as what you
wrote, never as what it lowers to: no generated function, local or read is anywhere in the tree,
completions or diagnostics. Spans are original, like every other node's, and a node's `complete`
flag means it has no structural hole and no parser error inside its span. It says nothing about
whether it type checks.

```luau
local result = luau.parse('return [for e in entities if e.active => e.id]')
local comprehension = result.root.body[1].list[1]
assert(comprehension.kind == 'ExprComprehension')
```

| Node | Fields |
|---|---|
| `ExprComprehension` | ordered `clauses`, `projection`, `openLocation`, optional `arrowLocation` and `closeLocation`, and `hasArrow`, `hasClose`, `complete` |
| `ComprehensionGenerator` | `bindings` (each a shared `Local` or a `PatternRecord`; `binding` is the first), `source`, `keywordLocation`, optional `inLocation`, `hasIn`. A generator's source is outside its own bindings' scope |
| `ComprehensionFilter` | `condition`, `keywordLocation` |
| `ExprRange`, `ExprEnumerate`, `ExprZip` | the intrinsic generator sources, with their original `args`; a binding named `range` is never resolved |
| `ExprSlice` | `source`, `first`, `last`, and `elementKind` (the literal's content) for a typed slice, in generator and standalone positions alike |
| `ExprReduction` | `op` (`"sum"`, `"min"`, `"max"`, `"any"`, `"all"`) and `expr`, the `ExprComprehension`; the span includes the reducer name |
| `ExprSink` | `destination`, optional `elementKind` and `offset`, and `expr`; the span starts at `into`, and the destination and offset keep their own nodes |
| `ExprUnary` with a comprehension operand | `#[for ...]`; count fusion is a lowering decision, not a tree shape |
| `StatLocalPattern` | `pattern`, optional `annotation`, `value` and `equalsSignLocation`; a declaration still missing its `=` has no `value` |
| `PatternRecord` | `fields`, `openLocation`, optional `closeLocation`, `hasClose`, `complete`, and in parameter position the parameter's `annotation`; appears in `ExprFunction.args` and `ComprehensionGenerator.bindings` in the slot's position |
| `PatternField` | `key`, `keyLocation`, optional `colonLocation`, `shorthand`, and `target`, a `PatternIdentifier { name, local }` or a nested `PatternRecord`, absent while missing |

The definitions type these too: `dream_luau_StatLocalPattern` is a member of `dream_luau_Stat`,
and a slot that may hold a pattern is a `dream_luau_Binding`, `dream_luau_Local |
dream_luau_PatternRecord`, which is what `ExprFunction.args` and a generator's `bindings` hold. A
strict walker refines one by its `kind`, `"Local"` or `"PatternRecord"`, before reading a `name`.

A zero-width insertion point has `endColumn = column - 1` on its line. Missing expressions are
`ExprError` with `isMissing = true` and no children; a malformed ordinary expression keeps stock
recovery nodes or one error covering the whole expression, never a valid prefix that hides the
bad suffix. `messageIndex` is zero-based into `errors`, `-1` when no message matches. Every prefix
of a construct, `[for x in`, `local {position: {x`, `function f({x,`, parses to the same records,
errors and tree on every run.

### What it costs

Every table is made at its final size and every key string is pushed once per call, so building
a node hashes no string. Counted by `benches/syntax.rs` (`cargo bench --features syntax,analysis
-- syntax`) in retired instructions per source line, on a typed module of ordinary script
code repeated to 4701 lines, the minimum of five rounds, the tree collected after each:

| | Instructions per line |
|---|---:|
| `Luau::Parser` alone, from Rust | 2518 |
| the parser and Luau's `toJson` of the tree | 27861 |
| `luau.parse` | 17205 |
| `luau.parse` with `tokens` | 18867 |
| `luau.parse` and a Luau walk over every statement and expression | 20173 |

The tables cost less than Luau's JSON encoding alone, which is 18 times the source's size (2.2
MB for 119 KB) and would still have to be decoded in Luau before anything could walk it. Most of
`luau.parse`'s cost is the tables: making them, filling their fields and collecting them again.

## dream.fs

Feature `fs`; module `@dream/fs`, `l3i::fs::FsExtension`. The host filesystem, for tools that
work on files on disk: whole and positional reads, readers over a memory map, writers with
positions and truncation, metadata with and without following links, listings and a fast
recursive walk, directories made and removed, renames, copies, hard and symbolic links,
canonical paths and file identity.

```luau
local fs = require("@dream/fs")
local reader, message, kind = fs.open("Data Files/Morrowind.bsa")
if not reader then
    return if kind == "notFound" then nil else error(message)
end
local header = reader:readAt(0, 12)
local walk = assert(fs.walk("Data Files", { followLinks = true, include = "files", sort = true }))
for index, path in walk.paths do
    print(path, walk.kinds[index])
end
```

Paths are bytes: every path argument is a string read as its bytes, and every path the module
returns is what the OS gave, so a file name that isn't UTF-8 round-trips on Unix. Windows paths
are Unicode, so there a path must be UTF-8.

### What the disk refuses is an answer

An operation the OS refuses returns `nil`, the message `dream.fs.<function>: <path>: <the OS's
message>` and the error's kind (`dream_fs_ErrorKind`: `notFound`, `permissionDenied`,
`alreadyExists`, `isADirectory`, `notADirectory`, `directoryNotEmpty`, `readOnlyFilesystem`,
`storageFull`, `crossesDevices`, `invalidInput`, `invalidFilename`, `unsupported`, `other`), the way
`io.open` reports one. A script branches on a missing file instead of catching it, and `assert`
turns the answer into an error when that is what it wants. An operation with nothing to return
returns `true`. `stat`, `lstat` and `readLink` return a lone `nil` for a path where nothing is,
and `exists` returns `false`. Calling a function wrongly is the script's mistake and raises: a
value of the wrong type, an unknown option, a negative offset, a position past the end, a closed
handle.

| Function | Returns |
|---|---|
| `readFile(path)`, `readFileString(path)` | The whole file as a buffer or a string. A FIFO, socket or device is refused (`invalidInput`, `not a regular file`) rather than read: it would block the thread or never end |
| `readAt(path, offset, length)` | `length` bytes from `offset` as a buffer, fewer at the end |
| `open(path)` | A `dream_fs_Reader` over a memory map of the file; refuses what `readFile` refuses |
| `stat(path)`, `lstat(path)` | `{ kind, size, isFile, isDir, isSymlink, readonly, modified, modifiedSeconds, modifiedNanoseconds }`; `lstat` describes a link instead of following it |
| `exists(path)` | Whether something is there, following links |
| `list(path)` | The names in a directory, sorted by their bytes |
| `walk(root, options?)` | Everything under `root`, depth first, as parallel arrays (below) |
| `canonicalize(path)`, `absolute(path)` | The path with every link resolved, or made absolute without touching the disk |
| `readLink(path)`, `sameFile(a, b)`, `cwd()` | A link's target as written, whether two paths are one file, the working directory |
| `writeFile(path, data, { offset?, append?, create? }?)` | The count written; truncates unless `offset` or `append` |
| `openWrite(path, { append?, truncate?, create? }?)` | A buffered `dream_fs_Writer` |
| `mkdir(path, { recursive? }?)`, `remove(path, { recursive? }?)` | `true`; `remove` never follows a link |
| `rename(from, to)`, `copy(from, to)`, `hardLink(source, link)`, `symlink(target, link, { directory? }?)` | `true`, or the byte count for `copy` |

`walk` takes `{ followLinks?, include?, metadata?, sort?, maxDepth?, skipErrors? }` and returns
`{ paths, kinds, sizes?, modifiedSeconds?, modifiedNanoseconds?, errors }`: one table per column,
so a walk of a hundred thousand files makes a handful of tables. `paths` are relative to the
root and separated by `/` on every platform, Windows included, so `root .. '/' .. path` names the
entry. With `skipErrors`, what can't be
read below the root goes into `errors` as `{ path, message }`; without it, the first one is the
walk's answer.

A reader's `read`, `readInto`, `readAt` and `readAtInto` copy from the map; its position moves
with `read`, `readInto`, `seek` and `skip`. A writer's `write`, `writeAt`, `seek`, `truncate`,
`flush` and `close` answer the same way as the module's functions. A writer buffers: what a
script never flushes or closes reaches the file when the writer is collected, when the runtime
drops, or at `Runtime::shutdown()`, which a host that exits without dropping the runtime calls
first.

### Capabilities

Reading needs `filesystem.read` (`l3i::fs::READ_CAPABILITY`) and changing the disk needs
`filesystem.write` (`l3i::fs::WRITE_CAPABILITY`). A plan that grants neither still has the module
and its types, and each function raises a permission error naming what it lacks.

## dream.process

Feature `process`; module `@dream/process`, `l3i::process::ProcessExtension`. The script's own
process and the ones it starts.

```luau
local process = require("@dream/process")
local result, message = process.run("tes3cmd", { "clean", "Mod.esp" }, { cwd = "Data Files", stdout = "capture" })
if not result then error(message) end
if not result.success then error(`tes3cmd exited with {result.code}`) end
```

| Function | Returns |
|---|---|
| `run(program, args?, { cwd?, env?, clearEnv?, stdin?, stdout?, stderr? }?)` | `{ success, code?, signal?, stdout?, stderr? }`; `stdout` and `stderr` are `"inherit"` (the default), `"capture"` or `"null"` |
| `env(name)` | An environment variable, or nil |
| `write(stream, data)` | `true`: data to `"stdout"` or `"stderr"`, unbuffered, without `print`'s newline. A closed pipe is not an error in a host that ignores `SIGPIPE`, which every Rust binary does on startup; a host that has restored the default disposition dies on it, as `| head` has always killed writers |
| `isTerminal(stream)` | Whether the stream is a terminal |

The program is found the way the OS finds one, and its arguments reach it as they are, with no
shell in between. A program the OS can't start returns `nil`, the message and the kind, as
`@dream/fs` does. `run` needs `process.spawn` (`l3i::process::SPAWN_CAPABILITY`), `env` needs
`process.environment` (`ENVIRONMENT_CAPABILITY`), and `write` needs `process.stdio`
(`STDIO_CAPABILITY`), so a sandboxed script's output goes where its host routes `print`;
`isTerminal` needs nothing.

A child inherits the host's environment only when the runtime grants `process.environment`;
without it the child starts with exactly the variables `env` names, since inheriting would hand
the script every variable `env` refuses it. `run` waits no longer than the runtime's execution
time limit: a child still running at the deadline is killed and reaped, and the watchdog stops
the script at its next instruction.

## dream.tcp

Feature `tcp`; module `@dream/tcp`, `l3i::tcp::TcpExtension`. Raw TCP byte streams and a
readiness poller over numeric addresses: a transport primitive, not a protocol. There is no
HTTP, TLS, WebSocket, framing, text encoding or name resolution here; protocols are written in
Luau on top. It is separate from `dream.udp` and shares nothing with it, capabilities included.
The script or its host drives every phase: nothing calls into Luau, there is no thread, event
loop or scheduler behind the module, and only `poller:wait` ever waits, for a bounded time.

```luau
local tcp = require("@dream/tcp")
local listener = assert(tcp.listen("127.0.0.1:0"))
local poller = tcp.poller()
assert(poller:watch(listener, 0, "read"))
local scratch = buffer.create(65536)
local streams, nextToken = {}, 1

while running do
    poller:wait(250)
    while true do
        local token, readable, writable, closed = poller:next()
        if token == nil then break end
        if token == 0 then
            -- Drain the listener: accept until it says 'wouldBlock'.
            while true do
                local stream = listener:accept()
                if not stream then break end
                streams[nextToken] = stream
                assert(poller:watch(stream, nextToken, "read"))
                nextToken += 1
            end
        elseif readable then
            local stream = streams[token]
            while true do
                local count, message, kind = stream:readInto(scratch)
                if count == nil then
                    if kind ~= "wouldBlock" then poller:unwatch(token) stream:close() end
                    break
                end
                if count == 0 then poller:unwatch(token) stream:close() break end -- end of stream
                handle(stream, scratch, count)
            end
        end
    end
end
```

### The module

| Member | Type |
|---|---|
| `listen(address, options?)` | `(address: string, options: dream_tcp_ListenOptions?) -> (dream_tcp_Listener?, string?, dream_tcp_ErrorKind?)`: a nonblocking listener; port `0` picks one, and `localAddress` says which |
| `connect(address, options?)` | `(address: string, options: dream_tcp_ConnectOptions?) -> (dream_tcp_Stream?, string?, dream_tcp_ErrorKind?)`: starts a nonblocking connect; the stream is `"connecting"` until `finishConnect()` says otherwise |
| `poller(options?)` | `(options: dream_tcp_PollerOptions?) -> dream_tcp_Poller` |
| `MAX_WAIT_MS` | Folded number: 60000, the longest wait any poller allows |

| Options of | Key | Values | Default |
|---|---|---|---|
| `listen` | `backlog` | 1 to 65535: connections the OS queues before `accept` | 128 |
| `listen` | `reuseAddress` | `SO_REUSEADDR`, to bind a port a closed listener left in `TIME_WAIT`; ignored on Windows, where it would let another socket take a port in use, and on macOS and the BSDs without `network.tcp.public`, where it would let a loopback listener bind over a listening wildcard socket on the same port | `true` |
| `listen` | `noDelay` | Nagle's algorithm off on every accepted stream. Pass `false` to get Nagle back, for a protocol that really does want small writes coalesced at the cost of latency | `true` |
| `listen` | `maxStreams` | 1 to 65536: accepted streams that may be open at once | 1024 |
| `connect` | `noDelay` | Nagle's algorithm off; `false` turns it back on | `true` |
| `poller` | `maxEvents` | 1 to 65536: the most events one `wait` queues. The event buffer is not allocated at that size up front: it starts at 256 entries and doubles whenever a wait fills it, up to this cap | 256 |
| `poller` | `maxWatches` | 1 to 65536: handles one poller watches | 1024 |
| `poller` | `maxWaitMs` | 0 to 60000: the longest `wait` this poller allows | 1000 |

An unknown option, or a value outside its range, is an error naming it. A runtime holds at
most 64 open pollers (the host's `TcpExtension::max_pollers`). A closed poller does not count,
and neither does one the script dropped: at the maximum `poller()` runs a full collection
first, and is an error only if that frees nothing.

### Streams are bytes

A stream carries bytes and nothing else: no message boundaries, no newline handling, no text
encoding, NUL bytes included. Nothing is buffered behind it in either direction.

| Member | Does |
|---|---|
| `readInto(target, offset?, length?)` | Receives straight into `target` at `offset` (default 0), at most `length` bytes (default the space after `offset`, and at least 1). Returns the count; **`0` is end of stream**, the peer closed its write half, and stays `0`; **`nil, message, "wouldBlock"`** is nothing ready yet. The two are never the same answer |
| `write(data, offset?, length?)` | Hands the OS the range of a buffer or string once and returns how many bytes it took, **possibly fewer than given**; the caller keeps the rest and writes it when the stream is writable. `nil, message, "wouldBlock"` when it took none; an empty range returns `0` without a system call |
| `finishConnect()` | `true` once connected, `false` while the connect is pending, or `nil`, the message and the kind (`"connectionRefused"`, `"timedOut"`, ...) when it failed. It reads the socket's pending error and then its peer, never writability alone |
| `shutdown(how)` | `"write"` sends end of stream and leaves reading open (a half-close, not `close`); `"read"` and `"both"` as named |
| `close()` | Closes the socket and drops its registration at once; closing twice does nothing |
| `localAddress`, `peerAddress` | `"127.0.0.1:50211"`, `"[::1]:50211"`. `localAddress` is asked of the OS the first time it is read and remembered, so it is nil while a connect is still unassigned, and nil on a closed stream that was never asked before closing |
| `state` | `"connecting"`, `"connected"`, `"failed"`, `"closed"`, or `"consumed"` once a TLS session has taken the stream |
| `closed` | Whether `close()` was called |

`readInto` and `write` settle a pending connect first: while it is pending they answer
`"wouldBlock"`, and once it failed every transfer, `finishConnect` included, answers that
failure again. Neither ever waits, and neither keeps the buffer's address after it returns. A
zero-byte read is an error rather than a `0` that would read as end of stream.

A listener has `accept()`, which returns the next waiting stream and its peer's address, or
`nil`, the message and `"wouldBlock"` when none is waiting, or `"limitReached"` at
`maxStreams` without accepting; `close()`; and the getters `localAddress`, `closed` and
`streams`, the accepted streams still open. Accepted streams stay open when their listener
closes. When the OS refuses an accept for want of descriptors or memory, the listener is
reported ready again with the next connection rather than on every `wait`, so a process out of
descriptors does not spin.

### Readiness

A poller watches handles under tokens the caller chooses, whole numbers from 0 to 2^53 unique
within the poller. Tokens are not descriptors and mean nothing to the OS.

| Member | Does |
|---|---|
| `watch(handle, token, interest)` | Watches an open `dream_tcp_Watchable` (a listener or stream, and with their extensions a TLS stream or a DNS request) for `"read"`, `"write"` or `"readwrite"` (a listener or a request only for `"read"`). A handle is watched by one poller at a time. Returns `true`, or the OS's refusal as `nil, message, kind` |
| `modify(token, interest)` | Changes what a watch reports, without a system call |
| `unwatch(token)` | Stops watching; the token may be used again at once |
| `wait(timeoutMs)` | Waits at most `timeoutMs`, `0` to check without waiting, and queues up to `maxEvents` ready watches; returns how many. Never longer than `maxWaitMs`, and never past the runtime's execution time limit: inside a watched call the wait ends at the deadline and the watchdog raises as soon as Luau runs again |
| `next()` | The next queued event as `token, readable, writable, closed`, or `nil` when the queue is drained. Deadline-based readiness (a DNS request's timeout, a TLS handshake deadline) is judged by the clock reading the `wait` took, once, not per event: a deadline that passes between a `wait` and `next` is reported by the next `wait` |
| `close()` | Releases every watch, the OS poller and its event buffer; the handles stay open. Closing twice does nothing |
| `watching`, `closed` | Getters: watches held, and whether `close()` was called |

Readiness is **level-triggered**. The OS reports edges (epoll and kqueue edge-triggered, IOCP
through AFD, all by mio); the poller remembers the last readiness each handle was told and a
handle forgets it only when an operation answers `"wouldBlock"`. So a watch is reported by every
`wait` until the script has drained it, and a `wait` with anything still pending does not
block. Readiness is a hint: an operation after it may still answer `"wouldBlock"`.

- Drain: `accept` and `readInto` until `"wouldBlock"`; after a partial read, the next `wait`
  reports the stream again.
- Watch `"write"` only while bytes are waiting. A stream with room in its send buffer is always
  writable, so a permanent `"readwrite"` watch makes every `wait` return at once. Write until
  done or `"wouldBlock"`, then `modify` back to `"read"`.
- `closed` is true when the peer will send nothing more: end of stream, a hang-up or a socket
  error. It is reported with read interest, alongside `readable`, and stays set; the next
  `readInto` returns `0` or the error. A half-closed stream may still be written.
- After end of stream a read watch stays readable, as epoll's level mode does: unwatch, close,
  or `modify` to `"write"`.
- An event queued by `wait` is checked again by `next()`: a watch removed, replaced under the
  same token, or drained since, is never reported, and an OS event raised for an old watch never
  reaches a new one (each watch's OS token carries a generation).
- When more than `maxEvents` watches are ready, the next `wait` starts its scan after the last
  one reported, so every ready watch is reached within a few waits.
- Events for interest the script does not hold wake the OS wait without being reported; the
  wait goes on for the rest of its time.

### Authority

| Capability | Grants |
|---|---|
| `network.tcp.connect` (`CONNECT_CAPABILITY`) | `tcp.connect`, to any address |
| `network.tcp.listen` (`LISTEN_CAPABILITY`) | `tcp.listen` on loopback addresses (`127.0.0.0/8`, `::1`) |
| `network.tcp.public` (`PUBLIC_CAPABILITY`) | With `network.tcp.listen`, binding wildcard (`0.0.0.0`, `[::]`) and non-loopback addresses; nothing on its own |

All three are denied unless the policy grants them, and none is implied by another or by
`dream.udp`'s `network.transport`. A function whose capability is missing exists, typed, and
raises a permission error. A stream accepted from a listener carries the listener's authority:
a script that may only listen cannot connect anywhere. A host can hand a script handles it made
itself with `Listener::from_std` and `Stream::from_std` and push them with `Listener::push` or
`Stream::push`; the script then needs no capability, and has no other way to make a handle.

`network.tcp.connect` is a coarse grant. It reaches every destination the host can reach,
including services that listen only on loopback (databases, debuggers, admin ports, other
local tools) and the local network. There is no endpoint allowlist; a scoped policy (hosts,
ports, address ranges) is follow-up work, and this module is not a network sandbox.

### Addresses and names

Addresses are numeric: `127.0.0.1:80`, `[::1]:8080`. A name such as
`localhost:80` is an error, not a lookup: resolving a name blocks for as long as the resolver
takes, so it does not belong behind a function that never waits. Names are
[`@dream/dns`](#dream-dns)'s, which resolves them on worker threads and returns addresses in
exactly this form. Whether `[::]` also accepts IPv4 is the OS default.

### Errors

A call the OS refuses returns `nil`, the message and a `dream_tcp_ErrorKind`:
`"wouldBlock"`, `"connectionRefused"`, `"connectionReset"`, `"connectionAborted"`,
`"notConnected"`, `"brokenPipe"`, `"addressInUse"`, `"addressNotAvailable"`, `"timedOut"`,
`"hostUnreachable"`, `"networkUnreachable"`, `"networkDown"`, `"permissionDenied"`,
`"limitReached"` (a bound the script set, never the OS), `"invalidInput"`, `"unsupported"` or
`"other"`. Messages name the call and the address: `dream.tcp.connect: 127.0.0.1:9: Connection
refused (os error 111)`; the would-block messages are constants and allocate nothing. A
malformed address, a bad option, an offset or length outside the buffer, an unknown interest,
a token in use, a closed handle or poller, or a missing capability is an error raised in the
script, never a panic.

### Lifetimes

Sockets and pollers are native state owned by their userdata. A poller refers to the handles it
watches weakly and a handle to its poller weakly, so neither keeps the other or a socket alive,
and no Luau value is retained. Collecting a stream or listener closes its socket and drops its
watch; collecting or closing a poller releases every watch, and the handles may be watched by
another poller. Handles belong to the runtime that made them and are not `Send`. Sockets are
close-on-exec, so `process.run` children do not inherit them, and writes never raise `SIGPIPE`
(`MSG_NOSIGNAL` on Linux, `SO_NOSIGPIPE` on Apple platforms).

### An HTTP server in Luau

`examples/tcp_http_server.luau` is a small HTTP/1.1 server written in Luau over this module,
and `cargo run --example tcp_http_server --features tcp -- 127.0.0.1:8080` serves it to
`curl -i http://127.0.0.1:8080/`. It is a demonstration, not a production server: one request
per connection, a fixed `Content-Length` response, `Connection: close`, no request bodies, no
keep-alive and no timeouts. What it does do properly is the transport part, and
`tests/tcp.rs` holds it to that with standard clients that split their requests across writes
and read a 4 MiB response late.

### Not here

TLS and name resolution (they are [`@dream/tls`](#dream-tls) and [`@dream/dns`](#dream-dns)),
UDP sockets (`dream.udp` is the game transport), endpoint allowlists, keepalive and buffer-size
options, connect and idle timeouts (the caller's loop owns time),
cancelling a wait from another thread, and sharing handles between runtimes. The module has
Windows and macOS code paths, and mio and socket2 support both, but it has been built and
tested on Linux only so far.

### What it costs

Measured by `benches/tcp.rs` (`cargo bench --features tcp -- tcp`) on an i7-10870H,
Linux 7.2, over loopback in one thread; times are the best of five rounds, instructions are
retired user-space instructions. Luau drives `@dream/tcp` from an interpreted loop; the Rust
rows do the same with nonblocking `std::net` sockets.

| Transfer size | Luau MiB/s | `std::net` MiB/s |
|---:|---:|---:|
| 1 KiB | 150 | 179 |
| 4 KiB | 571 | 692 |
| 16 KiB | 1971 | 2323 |
| 64 KiB | 4090 | 4692 |

| Operation | instr | ns |
|---|---:|---:|
| `stream.state`, the floor of a bound member | 760 | 50 |
| `readInto` with nothing ready (`"wouldBlock"`) | 1679 | 328 |
| `write` 16 B + `readInto` 16 B | 1811 | 5423 |
| the same two calls through `std::net` | 169 | 4853 |
| connect, accept, settle and close both ends, from Luau | 21142 | 47337 |
| the same through blocking `std::net` | 334 | 33448 |
| `wait(0)`, one ready among 1024 / 8192 idle watches (µs) | | 0.4 / 0.4 |
| `wait(0)` and draining `next()`, all of 1 / 32 / 256 ready | 2230 / 25945 / 197305 | 289 / 1840 / 13136 |

Warm reads and writes allocate nothing native, and a transfer's throughput is within 13 to 18 %
of `std::net` at every size: the system calls dominate, and the binder's call (about 760
instructions, 50 ns) is the rest. Dispatch costs about 52 ns an event at 256 ready watches,
the bound `next()` call. A `wait` scans only the watches with something to report, so idle
sockets cost it nothing; listeners and TLS and DNS handles are always looked at, since their
readiness can come from a deadline rather than the OS. One ready stream among 8192 idle ones
costs the same 0.4 µs as among 1024 (it was 5 and 42 µs when every watch was walked). The
connection row is dominated by the helper pollers the bench makes per pair (22 native
allocations, mostly theirs) and by loopback's own handshake.

Waiting does not spin: 256 idle watches over four `wait(250)` used 0.9 ms of CPU in a second,
and a writer blocked on a full send buffer, writing until `"wouldBlock"` and waiting for
writability ten times, used 1.3 ms in 0.9 s. Retention, live after a full collection: a stream
is about 210 VM bytes and 90 native bytes, a listener 96 and 120, a watch 79 native bytes and
no VM bytes; kernel socket buffers are the OS's and not counted.

The bottleneck is the system calls themselves, one per `readInto` or `write`, by design: there
is no hidden queue to batch them. The feature adds one crate, mio (socket2 is already in the tree
through dream-net).

Sockets are native memory the VM's memory limit does not see. Streams a script connects have no
per-runtime cap beyond the `network.tcp.connect` grant, and accepted streams are capped per
listener by `maxStreams` only; pollers are capped per runtime (`TcpExtension::max_pollers`, 64)
and a poller's event buffer grows only as waits fill it. A host that must bound a tenant's
sockets does it at the capability, or gives the tenant its own runtime.

## dream.dns

Feature `dns`; module `@dream/dns`, `l3i::dns::DnsExtension`. Host name resolution through the
operating system's resolver, on a bounded pool of worker threads, without ever blocking the
script. DNS resolves; it does not connect and it does not establish trust.

```luau
local dns = require("@dream/dns")
local request = assert(dns.resolve("example.com", 443, { timeoutMs = 5000 }))
request:wait(250)                                -- or watch it with a @dream/tcp poller
local addresses, message, kind = request:take()  -- once; nil, message, 'wouldBlock' while pending
-- { "[2606:4700:10::ac42:93f3]:443", "104.18.27.120:443", ... }
```

| Member | Does |
|---|---|
| `resolve(host, port, options?)` | Returns a request at once, or `nil`, a message and `"limitReached"` when the runtime's open requests or the resolver's queue are full. `host` and `port` are separate: nothing parses `host:port`. Options: `timeoutMs` (1 to 60000, default 5000), `maxAddresses` (1 to 64, default 16) |
| `request:take()` | Once: the addresses, or the failure (`"notFound"`, `"temporaryFailure"`, `"timedOut"`, `"cancelled"`, `"other"`). `nil, message, "wouldBlock"` while pending; a second `take` raises |
| `request:wait(timeoutMs)` | Waits at most `timeoutMs` (0 to 60000, never past the request's deadline or the runtime's execution time limit) on a condition variable, for a feature set without `@dream/tcp`; `true` once finished. The lookup runs on a worker, never here |
| `request:cancel()`, `request:close()` | Give up on a pending lookup: a queued one leaves the queue, a running one's result is thrown away. `close` also drops the result and any watch; it is idempotent |
| `status` | `"pending"`, `"ready"`, `"failed"`, `"cancelled"`, `"timedOut"`, `"consumed"` or `"closed"` |
| `host`, `asciiHost`, `port`, `closed` | The name as given, the name resolved (A-labels), the port |
| `MAX_TIMEOUT_MS`, `MAX_ADDRESSES` | 60000 and 64 |

With `@dream/tcp` in the plan a request is a `dream_tcp_Watchable`: `poller:watch(request,
token, "read")` reports it readable once it finishes or its deadline passes, and never again
once taken. Completion wakes a waiting poller through its `mio::Waker` (taken under the
request's lock, so a completion before or during the watch is never lost), and the wait is
capped at the earliest watched deadline, so a timeout is reported on time with no thread or
timer behind it.

### Names

A name maps to ASCII the way URLs map host names: UTS #46 nontransitional processing with
WHATWG's forbidden host code points refused (`_` is allowed, as real networks use it). It is
then checked before anything is queued: at most 253 characters, labels of 1 to 63 letters,
digits, `-` or `_`, one optional trailing dot, no NUL; `Bücher.example` resolves as
`xn--bcher-kva.example` while `request.host` keeps what the caller wrote. An IPv4 or IPv6
literal, bracketed or not, completes at once without a lookup. A bad name, port or option is
an error raised in the script; a lookup that fails is an answer.

### Results

Addresses come in the order the OS returned them (RFC 6724 and the host's own policy), with
both families kept and exact duplicates removed, cut to `maxAddresses`, each with the port:
IPv6 in brackets, ready for `tcp.connect`. An empty answer is `notFound`. Choosing among them
is the caller's: `examples/https_client.luau` interleaves the families and races staggered
connection attempts.

### The resolver

`Resolver::new(ResolverConfig { workers, queue }, lookup)` runs a `Lookup` on at most
`workers` threads (1 to 64, default 4), started as lookups need them, with at most `queue`
lookups waiting (1 to 4096, default 64); the queue counts lookups no worker has started yet,
so a burst can fill it before idle workers wake. A full queue is `limitReached`, never another
thread. `Resolver::system()` is the process's shared default over `SystemLookup`, the OS's
`getaddrinfo` (through libc on Unix, so a missing name and a temporary failure stay distinct;
std's lookup on Windows). Runtimes may share one resolver; `DnsExtension::max_requests` bounds
each runtime's open requests (default 64). A host can install any `Lookup` instead, which is
how the tests resolve `test.local` deterministically.

Workers own plain Rust data: the name, the port and the request's `Arc` state. No Luau value,
runtime `Rc` or callback crosses to them. A request's terminal phase (done, cancelled, timed
out) is entered exactly once under its lock, and the deadline holds at both ends: a lookup
that returns after it is a timeout even if the script never looked in time, and one that
returns at the deadline or before it stays ready however late the script looks. A running
`getaddrinfo` cannot be interrupted, so a request that times out or is cancelled reads that
way at once while its worker stays busy until the OS returns, and the pool's size bounds how
many such calls run. Nothing is cached: the OS may cache, as it always does.

Because those calls keep their workers, a shared resolver is shared capacity.
`DnsExtension::max_lookups` (default 8, 1 to 4096) caps how many of a runtime's lookups may
occupy the pool at once, queued or running, including those it cancelled or that timed out,
so one runtime cannot hold every worker of a shared resolver; a lookup's place is given back
when its worker is done with it, and literals never count. This is a per-runtime share, not
fair queueing; a host that serves tenants of different trust gives each tier its own
`Resolver`, which isolates them completely. No worker is ever killed, since no OS resolver API
allows it safely.

### Authority

`network.dns.resolve` (`RESOLVE_CAPABILITY`), denied unless granted, and granting nothing
else: no TCP or UDP capability implies it, and it implies no right to connect. Lookups go
wherever the OS sends them (the configured resolvers, hosts file, VPN), so a script that may
resolve can make the machine ask its resolvers about names of its choosing. This is not
DNS-over-HTTPS and no public resolver is ever substituted.

## dream.tls

Feature `tls` (which turns on `tcp`); module `@dream/tls`, `l3i::tls::TlsExtension`, which
requires `dream.tcp` in the plan. Verified TLS 1.3 and 1.2 sessions over `@dream/tcp`
streams, client and server, driven by nonblocking calls; rustls 0.23 with the ring provider.
TLS authenticates and encrypts; HTTP and every other protocol stay in Luau.

```luau
local tls = require("@dream/tls")
-- raw: a dream.tcp.Stream whose finishConnect() returned true, not watched by any poller
local secure = assert(tls.client(raw, { serverName = "example.com", alpn = { "http/1.1" } }))
-- raw is now "consumed". Watch secure, and on readiness:
local done, message, kind = secure:handshake()   -- true, false (call again), or nil
local accepted = secure:write("GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
secure:flush()
local count = secure:readInto(buffer.create(65536))  -- 0 only after close_notify
```

| Member | Does |
|---|---|
| `clientConfig(options?)` | Client trust and protocols, built once and shared: `roots` (PEM or DER strings or buffers, or an array of them), `platform` (default `true`), `alpn`, `versions` (`{ "1.2", "1.3" }` by default). `nil, message, kind` for unusable roots (`"invalidCertificate"`) or an unusable platform store (`"trustStoreUnavailable"`) |
| `serverConfig(options)` | `certChain` (PEM or DER), `privateKey` (PKCS #8, PKCS #1 or SEC1, PEM or DER), `alpn`, `versions`. The key is checked against the leaf certificate (`"keyMismatch"`); malformed input is `"invalidCertificate"` or `"invalidKey"` |
| `client(stream, options)` | Takes a connected, unwatched TCP stream for a client session: `serverName` (required), `config` (default: the platform store, built once per runtime), `alpn`, `bufferLimit`, `handshakeTimeoutMs` |
| `server(stream, config, options?)` | Takes an accepted, unwatched TCP stream for a server session under `config`: `bufferLimit`, `handshakeTimeoutMs` |

| Stream member | Does |
|---|---|
| `handshake()` | Moves the handshake as far as the socket allows: `true` once authenticated, `false` when it needs the socket again, `nil, message, kind` when it failed |
| `readInto(target, offset?, length?)` | Decrypted bytes into the buffer; `0` only after the peer's close_notify; `nil, message, "wouldBlock"` when nothing is ready; `"truncated"` when the connection ended without close_notify |
| `write(data, offset?, length?)` | Encrypts as much as the bounded send buffer takes and returns that plaintext count, not bytes on the wire; the ciphertext goes to the socket at once as far as it takes it, and a socket that has just refused bytes is not tried again within the same call: the record leaves with the next `write` or `flush`, which is when a script waiting for writability calls anyway. `"wouldBlock"` when the buffer is full |
| `flush()` | Pushes pending ciphertext: `true` when none is left in the session (handed to the OS, not necessarily received), `false` when the socket is full |
| `shutdownWrite()` | Queues close_notify and flushes; once it has left, the TCP write half is shut. Reading stays open |
| `close()` | Closes the socket at once without close_notify, drops its watch and its place in the listener's count; idempotent |
| `state`, `handshaking`, `closed` | `"handshaking"`, `"open"`, `"failed"` or `"closed"`; `close()` always leaves it `"closed"` |
| `failure`, `failureKind` | Why the session failed, if it did, kept after `close` |
| `wantsRead`, `wantsWrite` | Whether the session needs the peer's bytes, or has ciphertext waiting |
| `alpn`, `protocolVersion`, `cipherSuite`, `serverName` | The negotiated protocol, `"1.3"` or `"1.2"`, the suite; a client's identity, or the name a client asked a server for (SNI) |
| `peerAddress`, `localAddress` | As the TCP stream's |

A call that has to finish the handshake first does it, and answers `"wouldBlock"` while it is
still running. Every call does a bounded amount of socket work (32 steps for a handshake, 64
socket reads for a read) and stops; nothing waits.

### Verification

A client verifies the certificate chain, its validity period, and the server name it was
given: a DNS name (sent as SNI) or an IP address (matched against IP SANs). The name is the
caller's, independent of the address the socket connected to, so a name resolved to an IPv6
or an IPv4 address is verified the same way; it passes through the same mapping as
`@dream/dns`, so a Unicode name is checked as its A-label.

The default trust is the platform's, through rustls-platform-verifier: the system CA bundle
(found by rustls-native-certs, loaded once per configuration) and webpki on Linux and BSD,
Security.framework on Apple platforms, CryptoAPI on Windows. The platforms differ: Apple and
Windows apply the OS's own policy, including whatever revocation checking the OS does, while
Linux checks no revocation. An unusable store (no CA bundle in a minimal container) fails closed with
`"trustStoreUnavailable"`; no bundled root set is ever substituted. `roots` adds trust
anchors to the platform's, or, with `platform = false`, is the whole trust, for internal PKIs
and tests. Loading the platform store reads files on the calling thread, once per
configuration: about 3.7 ms on the reference machine.

There is no option, anywhere, to skip or weaken verification, accept a name mismatch, accept
an unverified self-signed certificate, log keys or extract session secrets; 0-RTT is off on
both sides. A failure is never a downgrade: the session fails, sends its alert if the socket
takes it, and shuts the socket both ways. The failure persists for every later call, the poller
reports the stream until it is closed, and `close` releases it: the stream is then `closed`
like any other, every call but `close` raises, and `failure` still says what went wrong.

| Kind | Meaning |
|---|---|
| `certificateNameMismatch` | The certificate is not for the server name |
| `certificateExpired`, `certificateNotYetValid` | Outside its validity period |
| `certificateUntrusted` | No chain to a trusted root (an unknown CA, a missing intermediate) |
| `certificateRevoked`, `certificateInvalid` | Revoked where the platform checks; any other certificate problem |
| `tlsAlert` | The peer refused the handshake (the server side of a client's rejection) |
| `noApplicationProtocol`, `tlsIncompatible`, `tlsProtocol` | No common ALPN protocol; no common version or suite; a malformed or unexpected message |
| `truncated` | The connection ended without close_notify: what arrived may be cut short |
| `timedOut` | The handshake missed its deadline (default 10 s, at most 300 s) |
| `invalidCertificate`, `invalidKey`, `keyMismatch`, `trustStoreUnavailable` | Configuration problems |

The socket's own kinds (`connectionReset`, `brokenPipe`, ...) pass through as in `@dream/tcp`.

### Ownership

`tls.client` and `tls.server` take the TCP stream whole: its socket, its addresses and its
place in the listener's `maxStreams` count, released exactly once when the session closes. The
TCP handle becomes `"consumed"`: reading, writing, shutting down, finishing a connect, watching
it or wrapping it again all raise, and closing or collecting it leaves the session's socket
alone. A stream still connecting, failed, closed, already consumed or watched by a poller is
refused (unwatch it first, so no registration or queued event refers to the old handle), and
a refused call leaves it usable.

### Readiness

A session is a `dream_tcp_Watchable`: one poller, one registration, the same tokens. What the
poller reports combines the socket's readiness with what only the session knows:

- readable: decrypted plaintext is waiting, the socket has bytes, the peer ended, or
  ciphertext of the session's own is waiting and the socket can take it (a key update or an
  alert must go out before the read can finish, so a reader is never stalled behind a write);
- writable: the send buffer has room, or pending ciphertext can go out;
- while handshaking: either, when the socket can move the handshake, or at the handshake
  deadline, which caps the poller's wait like a DNS deadline does.

Watch for writing only while there is something to write, as with TCP: an open session with
room in its send buffer is always writable.

### Bounds

`bufferLimit` caps the ciphertext waiting for the socket (default 64 KiB, 4 KiB to 16 MiB);
`write` takes no more plaintext than fits. rustls holds at most 16 KiB of decrypted plaintext
per session and reads no more from the socket until it is taken. Certificate inputs are at
most 1 MiB each; a chain at most 16 certificates; a client configuration at most 1024 added
roots; ALPN at most 16 protocols. Server sessions resume through rustls's bounded in-memory
cache (256 sessions per configuration); clients keep rustls's default resumption cache.

Configurations, sessions and their buffers live in native memory, which the VM's memory limit
does not see: a session may hold up to `bufferLimit` of ciphertext (16 MiB at most, 64 KiB by
default) plus rustls's own buffers, and nothing caps sessions per runtime beyond the TCP streams
they take over. A runtime keeps at most 32 configurations alive (the host's
`TlsExtension::max_configs`): past that, `clientConfig` and `serverConfig` first run a full
collection, since a configuration has no `close` and one the script dropped waits for the
collector, and then answer `limitReached`. The platform's store alone is loaded once per
runtime, for `tls.client` without a configuration and for every `clientConfig` without extra
roots; an unusable store is not read again, and a store that becomes usable later is not noticed
until a new runtime. `clientConfig{ roots = ... }` with the platform store caches the verifier
per distinct set of roots, 8 sets of at most 256 KiB each per runtime; a larger set, or a ninth,
is rebuilt on every call.

### Keys

Certificates and keys are bytes the caller holds: this module never opens a file and has no
capability of its own (a session's network authority is its TCP stream's). Key bytes are read
into a buffer zeroed when it drops, but that is the only copy zeroed: rustls takes a key only
as an owned copy, which its parser drops without zeroing, the parsed key lives in the
configuration as long as it does, and the Luau string or buffer the bytes came from is the
script's. Treat the process's memory as holding the key. No message ever contains key material.

### Not here

Client certificates (mutual TLS) are deferred: servers do not request them and clients do not
offer them. Also not here: OCSP stapling, certificate pinning, session ticket configuration,
key logging (deliberately), QUIC, and HTTP. Only the ring provider is built. The platform
verifier's behaviour has been exercised on Linux only.

### An HTTPS client and server in Luau

`examples/https_client.luau` resolves a name, races connection attempts across the returned
addresses (families interleaved, a new attempt every 250 ms or as soon as one fails, the
first to connect wins and the rest are closed, one deadline for everything), verifies the
server as the name it asked for, and makes one HTTP/1.1 request.
`examples/https_server.luau` serves HTTPS with bounded keep-alive and connections.
`tests/https.rs` runs them against each other through a resolver that maps `test.local` to an
unreachable address, an IPv6 loopback nobody listens on and the real IPv4 server, and serves
independent rustls clients; `cargo run --example https_get --features dns,tls -- example.com /`
fetches from a real server with the OS resolver and platform store. The client's deadlines use
`os.clock()`, which Luau implements as a monotonic wall clock on Windows, Apple platforms,
Linux, FreeBSD and Emscripten (C's CPU-time `clock()` only elsewhere), so time spent waiting
counts; `tests/https.rs` holds a stalled resolver, a connect whose SYNs are dropped and a
silent TLS peer to a 300 ms deadline in wall time. They are demonstrations:
no redirects, chunked bodies, request bodies or connection reuse.

### What it costs

Measured by `benches/dns_tls.rs` (`cargo bench --features dns,tls -- dns_tls`) on an
i7-10870H, Linux 7.2, loopback, one thread, best of five rounds; Luau drives the modules.

| DNS | instr | µs | native allocs |
|---|---:|---:|---:|
| a literal, resolved and taken (no worker) | 8693 | 0.8 | 9 |
| an instant lookup through the pool, `wait` + `take` | 12746 | 7.7 | 11 |
| the same, woken through a poller | 16337 | 7.9 | 14 |
| `localhost` through `getaddrinfo`, warm (cold, with the thread start: 1.0 ms) | 11056 | 44.9 | 12 |

The pool's round trip, a queue, a thread hand-off and a condition variable or waker, costs
about 7 µs. 65 requests blocked on a stalled resolver, watched by one poller through four
`wait(250)`, used 0.39 ms of CPU in a second.

| TLS 1.3 against TCP | TCP | TLS |
|---|---:|---:|
| connect, accept and close a pair (µs) | 42 | 368 (325 for both handshakes, ECDSA P-256, 198 allocations) |
| a 16-byte round trip (µs) | 6.9 | 10.3 |
| a read with nothing ready (µs) | 0.4 | 0.4 |
| throughput at 16 / 64 KiB writes (MiB/s) | 1937 / 3940 | 731 / 1113 |
| held per connected pair, VM / native bytes | 416 / 176 | 2848 / 13302 open, 2848 / 266 closed |

`tls.clientConfig` over explicit roots costs 4.3 µs, `tls.serverConfig` 18.8 µs (parsing and
matching the key), and the platform store 3.7 ms. The throughput gap is cryptography: AES-GCM
on both ends of a single thread, plus rustls's copies and its output buffer per record (about
two allocations per write, 192 per MiB at 64 KiB writes; a warm read makes none). It is not
system calls: a read-side staging buffer that cut the `recv` count was measured at no gain and
rejected. A session's userdata holds the rustls state inline, so the
VM's memory accounting sees it (the VM bytes above); its buffers are native and `close` drops
them at once, so closed handles a script keeps cost almost nothing native.

`dns` adds `idna` and ICU4X's normalizer (28 crates, Unicode-3.0 data, shared with `intl`);
`tls` adds rustls, ring, rustls-webpki, rustls-platform-verifier with rustls-native-certs and
openssl-probe (pure Rust, no OpenSSL), zeroize and the same `idna` (69 crates for this target
with `tls`, against 27 with no features). ring compiles C and assembly through cc with clang.
A stripped release build of `examples/https_get.rs` is 5.7 MiB against 3.5 MiB for the TCP-only
`tcp_http_server` and 2.3 MiB for an example with no features (different programs, so only
indicative).
