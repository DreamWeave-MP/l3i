+++
title = "Built-in extensions"
description = "The extensions l3i ships: the dream.udp bridge every plan carries, packed rotations in dream.quat, colors and clip rectangles in dream.raster, the dream.soft_render device, bytes for parsing in dream.bytes, the bulk-data plane in dream.data, textual identity as numbers in dream.intern, locales, plural rules and number formatting in dream.intl, Luau's own parser in dream.luau, the host filesystem in dream.fs, child processes in dream.process, raw TCP streams with a readiness poller in dream.tcp, host name resolution in dream.dns, verified TLS sessions in dream.tls, and MP3, Vorbis and Opus decoders in dream.audio."
weight = 90

[extra]
kind = "guide"
+++

Fifteen extensions come with the crate. `dream.udp` is in every plan; the others are added with
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
| `dream.audio` | `@dream/audio` | `l3i::audio::AudioExtension` | `audio` |

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
| `rgba8(r, g, b, a)`, `rgb8(r, g, b)` | `-> integer`; each channel clamped to `0..=255` and rounded to nearest, NaN as 0 |
| `red(color)`, `green`, `blue`, `alpha` | `-> number`, one channel |
| `packed(color)` | `-> number`, the `u32` for `buffer.writeu32` |
| `channels(color)` | `-> (number, number, number, number)` |
| `withAlpha(color, a)`, `lerp(a, b, t)`, `mul(a, b)`, `add(a, b)`, `scale(color, factor)`, `premultiply(color)` | `-> integer` |
| `rgba16`, `rgb16`, `red16`, `green16`, `blue16`, `alpha16`, `channels16`, `withAlpha16`, `lerp16`, `mul16`, `add16`, `scale16`, `premultiply16` | The same on `Color16` |
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
assert(raster.rgba8(300, 0, 0, 255) == raster.rgba8(255, 0, 0, 255)) -- channels clamp
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
The module's functions and the receiver's methods expand from one list in `raster.rs`, so a name
means the same arguments and the same arithmetic on both. The lowering,
`raster::lowering::ColorMath`, unpacks with shifts and masks, computes on doubles, clamps, rounds
and packs into one integer store; the 8-bit form is tag- and kind-checked, the 16-bit form
tag-checked only.

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
frees its storage on `free()` or when collected; Luau's VM has no `collectgarbage`, so a host that
wants textures back at frame boundaries drives the collector there (`runtime.gc(GcControl::Collect)`)
or lets scripts do it with [`RuntimePolicy::collectgarbage`](@/docs/runtime-options.md#the-collector).

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
| `translate(text, rules?)` | `text` normalized by the rules table `intern.new` takes (`dream_bytes_RulesTable`), or by `rules(table)` compiled from one: `nocase` lowers `A`..`Z`, `replace` maps single bytes to single bytes (`{ ["\\"] = "/" }`; a replaced byte is not also lowered), then runs of `collapse` are made one and `trimStart` and `trimEnd` removed from the ends; `text` itself, uncopied, when nothing changes |
| `rules(table)` | A `dream.bytes.Rules`: the table read once, so `translate` in a loop doesn't read it on every call |
| `trim(text, characters?)`, `trimStart`, `trimEnd` | `text` without whitespace at both ends, the start or the end: Unicode `White_Space` when `text` is UTF-8, ASCII whitespace when it isn't, or, given `characters`, any of those bytes; `text` itself, uncopied, when nothing goes |
| `shortest(value, width?)` | The fewest decimal digits that read back as `value` at `width` (`"f64"` by default, or `"f32"`): the digits as an integer string led by the sign, and the power of ten that scales them (`0.1` is `"1", -1`; zero is `"0", 0`); `nil` for a value that isn't finite at `width`: a NaN, an infinity, or a double past single precision's range at `"f32"`. The digits are Rust's own (`{:e}`): where two equally short spellings both read back as `value`, Ryū (and so serde_json) can choose the other one in the last digit. |
| `toF32(value)` | `value` rounded to the nearest single-precision float, as a 32-bit field would hold it |
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
`(?-u)` for data in another encoding; no look-around, linear time). The options record is named
`dream_bytes_RegexOptions`. `regex` raises for a pattern it can't compile, so a pattern a user
wrote goes through `pcall`, and an options literal passed through `pcall` gets no expected type.
Annotate the options with the name instead:

```luau
local options: dream_bytes_RegexOptions = { caseInsensitive = true }
local compiled, regex = pcall(bytes.regex, pattern, options)
```

The `dream.bytes.Regex` it
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

The rest work on text. `match(source, offset?, length?)` is the first match's text,
`captures(source, offset?, length?)` its groups (`[0]` the whole match, `[n]` group `n`, a named
group under its name; a group that took no part is absent), and `matches(source, offset?,
length?)` an iterator over every match answering its text, start and end, 0-based with the end
exclusive, as `find` answers. `replace(source, replacement)` replaces the first match and
`replaceAll(source, replacement, limit?)` every one (at most `limit`); both return the text and how
many matches they replaced. A string replacement expands `$1` and `${name}` to groups (`$$` is a
dollar); a function gets the groups table and returns the replacement, or `nil` or `false` to keep
the match. `split(source, limit?)` is the pieces between matches, and `bytes.regexEscape(text)`
escapes every character a pattern treats specially. A range given by `offset` and `length` is
matched as if it were the whole input, so `^` anchors at `offset`. `tostring(regex)` is its pattern.

```luau
local setting = bytes.regex([[(?<key>\w+)\s*=\s*(?<value>.*)]])
local groups = setting:captures('fallback = Data Files')
print(groups.key, groups.value) --> fallback  Data Files
print((setting:replaceAll('a=1', '${value}=${key}'))) --> 1=a
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

The decoders' options records are named, for a caller that reaches a decoder through `pcall`, as
the `regex` options are: `dream_bytes_DecompressOptions` is `inflate`'s and `lzmaDecompress`'s
`{ format?, maxSize? }`, and `dream_bytes_DecompressLimit` is the LZ4 and zstd decoders'
`{ maxSize? }`.

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

### Randomness, HMAC and signatures (`bytes-crypto`)

`random(count)` returns `count` bytes (at most 65536 a call) from the operating system's CSPRNG,
for keys, nonces and WebSocket masks; `math.random` is none of those. `hmac(data, key, algorithm)`
returns the raw HMAC of `data` under `key` with `sha1`, `sha256`, `sha384` or `sha512`, and
`hmacVerify(data, key, algorithm, tag)` says whether `tag` is that HMAC, comparing in constant
time: check a signature with it, never with `==` on two tags.

`rsaSign(data, privateKey, algorithm)` returns the raw PKCS#1 v1.5 RSA signature of `data`: with
`sha256`, `sha384` or `sha512`, what JSON Web Tokens call RS256, RS384 and RS512. The key is an RSA
private key of at least 2048 bits, PKCS#8 (`BEGIN PRIVATE KEY`) or PKCS#1 (`BEGIN RSA PRIVATE
KEY`), as PEM text or DER bytes, which covers the keys services hand out for signing tokens.
PKCS#1 v1.5 is deterministic, so a signature is the same bytes OpenSSL makes from the same key.
An encrypted key, an EC key, or anything else is refused with a message that never quotes the
key, and the decoded key bytes are zeroed once the signature is made.

```lua
-- header and claims are the token's two base64url parts, privateKeyPem the key as issued.
local signature = bytes.rsaSign(header .. '.' .. claims, privateKeyPem, 'sha256')
```

`ecdsaSign(data, privateKey, algorithm)` returns the raw ECDSA signature of `data`, `r` then `s`,
each fixed-width and big-endian, which is the form JSON Web Tokens put in a token: `sha256` signs
under a P-256 key (ES256, 64 bytes) and `sha384` under a P-384 key (ES384, 96 bytes). The key is
PKCS#8 (`BEGIN PRIVATE KEY`) or SEC1 (`BEGIN EC PRIVATE KEY`, the form OpenSSL writes and most
API consoles hand out), as PEM text or DER bytes, and must carry its public point, as both forms
do when a tool generates them. ECDSA is randomized: every call makes a different signature, and
every one of them verifies, so compare signatures by verifying them, never by their bytes. A key
on the other curve, an RSA key, or anything else is refused the way `rsaSign` refuses one.

```lua
local signature = bytes.ecdsaSign(header .. '.' .. claims, ecPrivateKeyPem, 'sha256')
```

All of it is ring, the provider `tls` already builds. There are no ciphers, no key generation
and no key storage here, on purpose: a key arrives as bytes the caller already holds, and leaves
with the call.

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
pool's rules policy (below) does the same without making the string. Folding a path is one scan,
and a path already folded costs that scan and no copy; in a loop, pass rules compiled once with
`bytes.rules` rather than a table, which `translate` would read on every call.

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
local hot = data.select(samples, "f32", 0, n, "gt", threshold)
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
`select`, combined with boolean algebra, counted, or turned into an index vector. Every
operation that produces output takes the storage from the caller, `into` for selections and
`out` for buffers, so steady state allocates nothing. The exceptions are temporary working memory:
`argsort` without scratch, and `gather`, `scatter` or `partition` when their output overlaps the
indices or keys they read.

### The operations

| Operation | Reads | Writes or returns | Empty input | NaN | Aliasing | Raises before any write |
|---|---|---|---|---|---|---|
| `sum` | span | number, `f64` accumulation in element order | `0` | propagates as Luau `+` | n/a | bounds |
| `min`, `max` | span | number or nil | nil | a leading NaN stays; later NaNs never replace (Luau `<` and `>`) | n/a | bounds |
| `argmin`, `argmax` | span | zero-based position or nil | nil | as `min` and `max` | n/a | bounds |
| `countWhere`, `K:count<Cmp>` | span | how many satisfy `cmp` | `0` | every comparison false except `ne` (IEEE) | n/a | bounds, comparison name |
| `any`, `all` | span | boolean; stops early once decided | `false`, `true` | as `countWhere` | n/a | bounds, comparison name |
| `select`, `K:select<Cmp>` | span | selection of the positions satisfying `cmp`, into `into` (resized) or new | empty selection | as `countWhere` | `into` may be any selection | bounds |
| `Selection:intersect/union/xor/difference` | two selections of one length | selection, into `into` or new | | | `into` may be either operand | lengths differ |
| `Selection:complement` | selection | selection | | | `into` may be the source | |
| `Selection:count/len/get/any/all/clear` | selection | number, boolean, nothing | `all` of nothing is true | | | |
| `Selection:indices` | selection | u32 index vector and count, into `out` from `offset` or a new exact buffer | empty buffer, 0 | | | `out` too small |
| `gather` | source span, index vector | `destination[i] = source[idx[i]]`; count written | 0 | copied, quieted if `f32` | sequential, read then write per element, in index order; an output over the indices does not change them | bounds, any index ≥ source count |
| `scatter` | source span, index vector | `destination[idx[i]] = source[i]`; count written | 0 | copied, quieted if `f32` | sequential; a repeated index keeps the last write; an output over the indices does not change them | bounds, any index ≥ destination count |
| `fill` | | every element of the span | | writes zero for integer kinds | | bounds |
| `add` | two spans | `out[i] = l[i] + r[i]` | | Luau `+`; of two different NaNs, either survives, quieted | `out` may be either input | bounds, all three spans |
| `scale` | span | `out[i] = s[i] × factor` | | Luau `*`; a NaN element with a NaN factor gives a quiet NaN | `out` may be the source | bounds |
| `clamp` | span | `out[i] = clamp(s[i], low, high)` | | passes through, as `math.clamp` | `out` may be the source | bounds, `low > high` or NaN bounds |
| `argsort` | key span | u32 permutation into `out` from `outOffset`; count | 0 | NaN keys last, in original order | keys are copied before sorting, so `out` may overlap them | bounds |
| `partition` | key span | u32 index vector, satisfying positions first then the rest, each in original order; the accepted count | 0 | as `countWhere` | keys are copied first if `out` overlaps them, so `out` may overlap them | bounds, comparison name |

Comparisons are named: `"eq"`, `"ne"`, `"lt"`, `"le"`, `"gt"`, `"ge"`. Aliasing is defined,
never assumed: element-wise operations read each element and write its result before the next,
so an output overlapping an input sees the sequential result, and in-place forms are the ordinary
case. Out-of-range indices raise before anything is written.

`argsort` is stable: equal keys keep their position order. Without scratch it sorts a copy of
the keys paired with positions, so the buffer is read once and `out` may overlap the keys. With a
caller scratch of `count × 4` bytes the index vector moves between `out` and the scratch, by a radix
sort from 512 keys and a merge below that, reading keys from the span as it goes, so nothing is
allocated in steady state. That is the shape a spatial-index builder otherwise hand-rolls in Luau.
Keys, `out` and scratch must then not overlap, and the call refuses them if they do. Without scratch
a sort of 512 or more keys takes 16 bytes of temporary memory per key, and is usually the faster
form because each pass reads in sequence: about a quarter less time at 65,536 `f32` keys, and about
even at 1,048,576, where the page faults on that memory catch up. Scratch is what keeps a repeated
sort from allocating.

### Typed receivers

Every module function is a bound Rust function invoked once per bulk operation: one host
crossing amortized over the span, element loops in Rust over raw pointers after the single bounds
check. That is the portable path and the correctness oracle for everything else.

The loops are ordinary Rust with the element kind constant, picked once per call after the spans
are checked, with no intrinsics and no JIT. On a contiguous span LLVM vectorizes what it can, and
each loop is compiled for AVX2 behind a run-time check on x86-64 and for the baseline target
everywhere. A strided span keeps a run-time stride and stays scalar, except that `f32` reads at 8,
12 and 16 bytes, the common record sizes, have the stride constant too: AVX2 can read those in
vectors. Writes do not get that, since AVX2 has no scatter store. Results match plain loops that
dispatch on the kind per element, bit for bit, overlaps included.

Which NaN comes out of arithmetic on two different NaNs is **unspecified**: `add`, `scale` and
`sum` give a quiet copy of one of them, but which one depends on how the loop was compiled, and
can differ between builds, targets and element kinds. One NaN with a number always gives that NaN,
quieted. Comparisons are not affected, since every NaN compares alike.

The reductions also exist as methods of **typed receivers**, one class per element kind,
`dream_data_Kind_u8` through `dream_data_Kind_f64`, from `data.u8(stride?)` through
`data.f64(stride?)`, or `data.kind("f32@16")` when the layout is only known at run time. Every
receiver has the same thirty-nine members: `sum`, `min` and `max` over the whole span, and each
of `sum`, `min`, `max`, `count`, `any` and `all` with a comparison suffix (`sumGt`, `minLe`,
`countEq`, `anyNe`, `allGe`, ...) over the elements that compare so against a threshold argument,
plus `selectEq` through `selectGe`, which is `select` written into a caller selection that is
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

IR loops are cheap to enter and cannot vectorize, so a long span leaves them for the bound method's
loops, which cost more to reach and much less per element. On a contiguous span, `count`, `any`,
`all`, `select` and an unfiltered `min` or `max` hand over at 128 elements, a filtered `min` or
`max` and a filtered floating-point `sum` at 512, and an unfiltered `sum` at 1,024. A filtered
integer `sum` stays in IR. A strided span hands over at 64 elements, or 256 when filtered. Those
are measured crossovers (`cargo bench -- data_receivers`). A contiguous `any` or `all` first tests
its first eight elements in IR, so a span they decide never pays for the call. The dialect's
recognized pipelines go through the same sites, so they get the same hand-over, and the answer is
the same on both sides of it.

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
environment. The temporary memory `argsort` uses, a selection's bitset, and the copies `gather`,
`scatter` and `partition` make when an output overlaps are host memory the VM's memory limit does
not see. Each copy holds only the elements read, however wide their stride.

### What it costs

`cargo bench --features data -- data_` (interpreter) and `--features data,jit` (every chunk
compiled natively, so the loop baselines are CodeGen loops), on an i7-10870H: retired
instructions per call (minimum of five rounds of 64 calls) and Criterion median time. `u8` sums
and `> 127` counts over `n` bytes; the recognized dialect form includes the slice prologue; the
scalar fallback is the same source run without the extension. Globals are sandboxed in both, so
the interpreter takes the `buffer.readu8` fastcall and Luau's own loops are about three times
faster than they would be unsandboxed. Those are the fair baselines.

Interpreter:

| n | dialect, recognized | dialect, scalar fallback | explicit `data.sum` / `data.countWhere` | handwritten `readu8` loop |
|---:|---:|---:|---:|---:|
| sum 16 | 2,490 (263 ns) | 4,276 (388 ns) | 1,546 (146 ns) | 2,886 (210 ns) |
| sum 256 | 3,630 (474 ns) | 43,636 (3.37 µs) | 2,686 (374 ns) | 32,886 (2.05 µs) |
| sum 4,096 | 21,870 (4.16 µs) | 673,396 (50.5 µs) | 20,926 (4.02 µs) | 512,886 (31.5 µs) |
| sum 65,536 | 313,720 (64.5 µs) | 10,749,557 (824 µs) | 312,766 (63.0 µs) | 8,192,887 (532 µs) |
| count 16 | 2,582 (268 ns) | 4,440 (569 ns) | 1,656 (151 ns) | 3,210 (366 ns) |
| count 256 | 3,572 (441 ns) | 47,324 (6.29 µs) | 2,646 (407 ns) | 38,686 (4.13 µs) |
| count 4,096 | 19,412 (3.24 µs) | 733,202 (98.9 µs) | 18,486 (3.12 µs) | 606,148 (67.2 µs) |
| count 65,536 | 272,852 (45.1 µs) | 11,707,707 (1.51 ms) | 271,926 (49.1 µs) | 9,685,806 (1.36 ms) |

There is no small-n crossover when interpreted: at 16 elements the recognized pipeline is already
ahead of the handwritten loop, and the explicit call at about two thirds of its instructions. The
dialect form carries about 800 instructions of prologue over the explicit call, independent of n.

| n | explicit `data.sum` f32 | handwritten `readf32` loop | `data.gather` f32 | handwritten indexed copy | stable `data.argsort` f32 | `table.sort` index table with a comparator (unstable) |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 2,436 (365 ns) | 40,310 (2.52 µs) | 6,206 (476 ns) | 86,184 (6.63 µs) | 29,800 (2.16 µs) | 1,236,623 (109 µs) |
| 4,096 |  |  |  |  | 461,684 (45.5 µs) | 31,285,064 (3.06 ms) |
| 65,536 | 247,236 (61.7 µs) | 10,093,431 (616 µs) | 1,148,606 (93.0 µs) | 21,824,426 (1.57 ms) | 7,281,545 (788 µs) | 666,317,445 (55.3 ms) |

Native code (`jit`): a recognized pipeline runs the receiver's unrolled IR loop until its span is
long enough to hand over to the bound loops, and the handwritten and fallback loops are Luau CodeGen
loops.

| n | dialect, recognized | dialect, scalar fallback (CodeGen loop) | explicit `data.sum` (bound) | handwritten `readu8` (CodeGen loop) |
|---:|---:|---:|---:|---:|
| sum 16 | 1,236 (113 ns) | 1,578 (139 ns) | 1,505 (138 ns) | 1,318 (109 ns) |
| sum 256 | 2,646 (385 ns) | 9,498 (882 ns) | 2,645 (388 ns) | 8,998 (823 ns) |
| sum 4,096 | 21,250 (4.30 µs) | 136,218 (12.9 µs) | 20,885 (4.18 µs) | 131,878 (12.3 µs) |
| sum 65,536 | 312,989 (66.3 µs) | 2,163,738 (206 µs) | 312,725 (66.6 µs) | 2,097,958 (195 µs) |
| count 16 | 1,080 (88 ns) ¹ | 1,608 (141 ns) | 1,594 (141 ns) | 1,316 (117 ns) |
| count 256 | 2,696 (332 ns) ¹ | 10,118 (1.01 µs) | 2,584 (303 ns) | 9,106 (910 ns) |
| count 4,096 | 18,536 (2.90 µs) ¹ | 146,243 (15.1 µs) | 18,424 (2.93 µs) | 133,711 (14.2 µs) |
| count 65,536 | 271,976 (44.4 µs) ¹ | 2,324,304 (232 µs) | 271,864 (44.4 µs) | 2,127,451 (217 µs) |
| min 256 | 2,109 (211 ns) ¹ |  | 2,029 (205 ns) | 10,273 (908 ns) |
| min 65,536 | 129,609 (17.2 µs) ¹ |  | 129,529 (16.8 µs) | 2,425,633 (219 µs) |

¹ The receiver called directly (`K:countGt`, `K:min` on an annotated local), which is what the
bridge emits; the rows above it carry the slice prologue on top. Its IR loop runs at 16 elements;
from 256 it has handed over, which is why it tracks the bound column.

| n | explicit `data.sum` f32 | `readf32` CodeGen loop | `data.gather` f32 | indexed copy, CodeGen | stable `data.argsort` | `table.sort` comparator, CodeGen (unstable) |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 2,395 (380 ns) | 9,766 (894 ns) | 6,121 (442 ns) | 13,107 (1.35 µs) | 29,672 (2.20 µs) | 818,140 (81.7 µs) |
| 4,096 |  |  |  |  | 461,626 (40.8 µs) | 20,712,116 (1.95 ms) |
| 65,536 | 247,195 (66.0 µs) | 2,294,566 (206 µs) | 1,148,521 (74.6 µs) | 3,146,547 (343 µs) | 7,281,487 (728 µs) | 441,798,844 (41.2 ms) |

What the machine shape says:

- Luau's own CodeGen loop costs about 32 instructions per byte: `FORNLOOP`, the `readu8`
  fastcall with its checks, the add through a VM register. The bound Rust loop costs about 5
  including its once-per-call argument parsing, and wins in time from 256 elements up.
- A loop that dispatches on the element kind per element cannot vectorize, so contiguous spans
  run loops with the kind constant, compiled again for AVX2 behind a run-time check. Three
  separate `f32` passes of `relu(a + b + 2)` at 65,536 elements took 415 to 465 µs through the old
  loops and take about 32 µs, against 25 to 31 µs for the same passes with no binder at all, and
  in cache the gain reaches 22 times. Without AVX2 they take about 42 µs. `benches/data-results.md`
  has every operation, kind and size, with the raw rows and how to reproduce them.
- The first IR loop kept its cursor and accumulator in the receiver's scratch, so every element
  paid three memory round trips: fewer instructions than the bound call but slower at 65,536,
  latency bound on store-to-load forwarding. Unrolling the pure-add `sum` body to eight elements
  per block, one cursor and one accumulator round trip per eight, cut it to 6 instructions per
  byte: within noise of the handwritten native loop at 16 elements, with no call overhead left to
  amortize.
- `count`, `min` and `max` branch per element, which ends an IR block, so they cannot fold into
  one block. They unroll as a chain of one block per element with the cursor advanced once per
  eight and the accumulator touched only on a hit. That halves their time too, and called
  directly they beat the handwritten native loop even at 16 elements.
- Strided layouts made the bound loops faster, not slower: the stride-aware span let the
  compiler vectorize the u8-to-double sum. The native loops dispatch once on contiguous versus
  strided and keep their constant-step unrolled form for contiguous spans; a run-time stride in
  the unrolled body cost 40 percent. Lowered calls hand long contiguous spans to the bound loops,
  at the lengths above, so a receiver takes the cheaper entry on short spans and the vector loops
  on long ones.
- Writing comparisons into a selection from native code first measured 222 µs at 65,536 bytes:
  a read-modify-write of the bitset word per hit through two pointer loads. Reshaped as one
  64-element pass per word, each hit ORing a compile-time bit into a scratch word written once,
  it ran in 89.6 µs including the `count()` afterwards. That is still the shape on short spans.
  A long span now takes the bound loop: about 45 µs including the `count()`, against about 40 µs
  for the direct count.

Interpreter and native agree on the ordering of everything else: from 256 elements up the bound
operations beat the best obvious Luau by 2× (`f32` sum, native) to about 55× (stable argsort against
a comparator sort, native). At 16 elements a native handwritten loop can still win, because a bound
call's fixed cost has not been paid off: 109 ns against 138 ns for a `u8` sum.

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
positions and truncation, stable-storage barriers, ranged reads authenticated against a trusted
commitment (`fs-authenticated`), metadata with and without following links,
listings and a fast recursive walk, directories made and removed, renames, copies, hard and
symbolic links, canonical paths, file identity, and the operating system's file locks.

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
`storageFull`, `crossesDevices`, `invalidInput`, `invalidFilename`, `unsupported`, `wouldBlock`,
`other`), the way
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
| `openPositional(path)` | A `dream_fs_PositionalReader`: a retained, unmapped handle for a file that may change (below); refuses what `readFile` refuses |
| `stat(path)`, `lstat(path)` | `{ kind, size, isFile, isDir, isSymlink, readonly, mode?, modified, modifiedSeconds, modifiedNanoseconds }`; `lstat` describes a link instead of following it |
| `exists(path)` | Whether something is there, following links |
| `isFile(path)`, `isDir(path)` | Whether a regular file or a directory is there, following links; `false` when nothing is, or it can't be read |
| `list(path)` | The names in a directory, sorted by their bytes |
| `walk(root, options?)` | Everything under `root`, depth first, as parallel arrays (below) |
| `canonicalize(path)`, `absolute(path)` | The path with every link resolved, or made absolute without touching the disk |
| `readLink(path)`, `sameFile(a, b)`, `cwd()` | A link's target as written, whether two paths are one file, the working directory |
| `writeFile(path, data, { offset?, append?, create?, exclusive?, atomic?, createParents?, mode? }?)` | The count written; truncates unless `offset` or `append`. `exclusive` refuses a file that exists (`alreadyExists`); `atomic` writes a temporary beside the file and renames it over, so a reader sees the old file or the new one, keeping the old one's permissions unless `mode` gives others; `createParents` makes missing parent directories |
| `openWrite(path, { append?, truncate?, create?, exclusive?, writeThrough?, mode? }?)` | A buffered `dream_fs_Writer` |
| `createDurable(path, data, { mode? }?)` | The count written: a new file whose contents and name have been synchronized (below) |
| `syncDirectory(path)`, `replace(source, destination)` | `true`: a directory's entries synchronized, or `source` atomically renamed over `destination` (below) |
| `mkdir(path, { recursive?, mode? }?)`, `remove(path, { recursive? }?)` | `true`; `remove` never follows a link |
| `chmod(path, mode)` | `true`: the Unix permission bits of what `path` names, set (below) |
| `rename(from, to)`, `copy(from, to)`, `hardLink(source, link)`, `symlink(target, link, { directory? }?)` | `true`, or the byte count for `copy` |
| `lock(path, { shared?, createParents?, mode? }?)` | A `dream_fs_Lock`: the operating system's lock on the file, made if it isn't there; a lock held elsewhere is `wouldBlock` (below) |

`walk` takes `{ followLinks?, include?, metadata?, sort?, maxDepth?, skipErrors? }` and returns
`{ paths, kinds, sizes?, modifiedSeconds?, modifiedNanoseconds?, errors }`: one table per column,
so a walk of a hundred thousand files makes a handful of tables. `paths` are relative to the
root and separated by `/` on every platform, Windows included, so `root .. '/' .. path` names the
entry. With `skipErrors`, what can't be
read below the root goes into `errors` as `{ path, message }`; without it, the first one is the
walk's answer.

A reader's `read`, `readInto`, `readAt` and `readAtInto` copy from the map; its position moves
with `read`, `readInto`, `seek` and `skip`. A writer's `write`, `writeAt`, `seek`, `truncate`,
`flush`, `syncData`, `syncAll` and `close` answer the same way as the module's functions. A writer buffers: what a
script never flushes or closes reaches the file when the writer is collected, when the runtime
drops, or at `Runtime::shutdown()`, which a host that exits without dropping the runtime calls
first.

### Permission bits

`mode` is the Unix permission bits of what a call creates: a number from 0 to 4095, the nine
permission bits with set-user-ID, set-group-ID and sticky above them. Luau has no octal literal,
so write one with `tonumber`:

```luau
local Private = tonumber('600', 8)
assert(fs.mkdir(root, { recursive = true, mode = tonumber('700', 8) }))
assert(fs.writeFile(fs.join(root, 'token'), secret, { atomic = true, mode = Private }))
```

The bits are exact, and they are never exceeded on the way there. The file is created with them,
so the process umask can only take some away, and those are put back through the handle just
opened, before anything is written. A file for its owner alone is at no moment readable by anyone
else. `writeFile`, `openWrite`, `createDurable`, `mkdir` and `lock` all take it.

A mode is for what the call creates:

- A file that is already there keeps its own bits, as it does under `open(2)`. `chmod(path, mode)`
  changes those, following symbolic links, and `stat(path).mode` reads them.
- An atomic `writeFile` always makes the file anew, so with a `mode` the new file has it; without
  one it keeps the old file's permissions. Its temporary is created with the bits the file will
  keep.
- `mkdir` with `recursive` gives the mode to every directory it creates, and leaves one that
  exists as it is. A directory keeps the set-group-ID bit it takes from its parent.
- `createParents` makes parents with the default bits. Make them with `mkdir` first when they
  matter.
- With a mode, a file is not created through a dangling symbolic link: the call answers
  `alreadyExists`.
- `mode` with `create = false` raises: there is nothing it could apply to.

Only Unix has these bits. On Windows a `mode` is checked and then changes nothing, `stat` reports
no `mode`, and `chmod` answers `unsupported`.

### Locks

`lock(path)` takes the operating system's own lock on a file (`flock` on Unix, `LockFileEx` on
Windows), exclusive unless `shared = true` asks for one that other shared locks may hold too. It
never waits: a lock held elsewhere, in this process or another, is answered at once with `nil`,
`dream.fs.lock: <path>: held by another lock` and `wouldBlock`, and the script decides whether to
try again later. `createParents` makes the file's missing folders first.

The lock is held for as long as the file it was taken through is open, so it ends however its
holder does: `lock:unlock()` (true if it was held, harmless again), the handle collected, the
runtime dropped or shut down, or the process gone, when the system releases it. It never times
out, and it never outlives the process that took it, which is what a file that names a single
running owner needs: a second instance asks for the lock and is told no until the first is gone,
whether the first exited, crashed or was killed. `lock:held()` and `lock:shared()` say what the
handle holds.

On Unix the lock is advisory, as `flock` is: it excludes other locks on the file, not reads or
writes of it, so it guards what everyone who touches the file agrees to lock first. A lock on a
network file system is only as good as that file system's locking.

```luau
local owner, message, kind = fs.lock("state/owner.lock", { createParents = true })
if not owner then
    return if kind == "wouldBlock" then "already running" else error(message)
end
```

### Stable storage

`flush()` and `close()` hand bytes to the operating system; neither says they survive a power
cut. `writer:syncData()` flushes the writer and synchronizes the file's data, and
`writer:syncAll()` synchronizes its metadata too. `openWrite`'s `writeThrough = true` asks the OS
to write through its cache (`O_SYNC` on Unix, `FILE_FLAG_WRITE_THROUGH` on Windows), but the writer
still buffers, so a commit still needs one of the two.

Any I/O failure leaves a writer's outcome uncertain and poisons it. Later writes, seeks,
truncations, flushes and syncs return the original failure, buffered bytes are discarded without
another write, and every `close()` reports that failure too. Close it and recover before opening
a new writer.

A file's name is a separate thing to persist:

- `createDurable(path, data)` exclusively creates a file with `data` and returns the count once
  the file and its name are synchronized, as far as the platform can say (the table and Windows
  notes below). On Unix a failure can leave a file at `path`, so treat the outcome as uncertain.
  Don't rename or replace the parent directory while it runs.
- `replace(source, destination)` atomically renames across one filesystem. On Unix that is
  visibility, not persistence, until the directory is synchronized.
- `syncDirectory(path)` synchronizes a directory's entries, not the files in it.
- `writeFile(path, data, { atomic = true })` synchronizes its temporary but not the directory, and
  `rename` synchronizes nothing. Neither alone is a durable checkpoint.

For a checkpoint, create an exclusive sibling, write the complete image, `syncAll` and close it,
`replace` the destination, then `syncDirectory` the parent. Every step must succeed before you
acknowledge it. A failure after the replacement can leave either complete version, so it doesn't
prove the replacement failed. Each checkpoint replaces the last, which is also what keeps the
space a store uses bounded. Sync both parents if a rename moves between directories.

A consumer that wants no renames after creation can keep one file: hold an exclusive lock and an
append writer, write a complete transaction, and sync before acknowledging. Append a complete
checkpoint, sync, append its commit marker, and sync again. The file only grows, and the consumer
owns its framing, checksums, ordering and recovery. Keep lock files across closes, so competing
processes don't lock different files.

| Platform | Files | Names | Qualification |
|---|---|---|---|
| Linux and other Unix | `sync_data`, `sync_all` | Directory `fsync` after `rename` | The filesystem and device must honor barriers; network and unusual filesystems may differ |
| macOS | `F_FULLFSYNC` for both methods | `F_FULLFSYNC` on the directory after `rename` | The filesystem and device must honor it |
| Windows | `FlushFileBuffers` for both methods | `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`, then `FlushFileBuffers` on the directory | Local NTFS or ReFS only; anything else is `unsupported` before anything changes |

Windows has no documented way to synchronize a directory entry, so **success there is not the
promise a Unix directory `fsync` makes**. `createDurable` writes and flushes a hidden temporary
beside `path`, then moves it into place with write-through. Microsoft says that move doesn't return
until it is on the disk, but its explicit flush guarantee describes a move done as copy and delete,
not a rename on one volume. The directory flush that follows isn't among the operations documented
for directory handles, so `createDurable` carries on if Windows refuses it outright, while
`syncDirectory` reports the refusal.

On Windows, write recovery that stays correct if the last name change is lost: keep the previous
generation until the new one is confirmed on a later open, and validate whichever you find.

Three more things differ from Unix there. A failure before the move leaves nothing at `path`, though a
process killed mid-way can leave the temporary behind. `replace` can be refused while another
handle holds the destination open, so close readers first. And a path over 260 characters can be
refused unless the process is long-path aware.

CI runs the synchronization, namespace and process-kill tests on Linux, macOS and Windows, and on
Windows also without administrator rights, through relative paths and through a junction. ReFS and
volume mount points aren't exercised. **Nothing here has been tested against a power cut**: killing a process doesn't exercise it, and
no test proves a filesystem or device honors a barrier.

### Files that change

A mapped `Reader` must not be used while anything truncates its file: reading a page that is gone
can terminate the process. `readAt` and `openPositional` read through the OS instead, so a file
that shrinks during a read answers with a failure rather than a short buffer.

`openPositional(path)` keeps one unmapped handle open. Its `readAt(offset, length)` doesn't reopen
the path, `size()` asks the open file for its current size, and `close()` releases it (later calls
raise, and closing twice does nothing). An offset past the current end raises like any other read,
so check `size()` first when the file can shrink.

Appending uses the OS's append flag, and writes retry interruptions and short writes. A record that
takes more than one OS write can still interleave with another writer's, so serialize writers
with a lock and keep one open. `tell()` is the writer's own accounting, not the file's end when
other handles append.

### Authenticated reads

Feature `fs-authenticated`, which adds `sha2`; the Rust API is `l3i::fs::authenticated`. `readAt`
returns whatever is on the disk. These return exactly the bytes of the object you already trust, or
nothing, without reading or hashing the rest of it.

```luau
local fs = require("@dream/fs")

-- Publishing: once, by whoever makes the object.
local manifest, commitment = assert(fs.authManifest("assets.pak"))
assert(fs.writeFile("assets.pak.manifest", manifest))
-- Store `commitment` somewhere the file's storage cannot write: a signed record, your program's
-- own constants, a catalog you already authenticated. This is the step that makes it all mean something.

-- Reading: the manifest can come from anywhere, the commitment cannot.
local reader, message, kind = fs.openAuthenticated(
    "assets.pak",
    assert(fs.readAt("assets.pak.manifest", 0, 4 * 1024 * 1024)),
    commitment
)
assert(reader, message)
local bytes = assert(reader:readAt(1048576, 4096))

-- Damage is an answer, like anything else the disk does.
assert(fs.writeFile("assets.pak", "x", { offset = 0 }))
local nothing, reason, failure = reader:readAt(0, 16)
assert(nothing == nil and failure == "integrityFailed", reason)
```

| Call | What it does |
|---|---|
| `authManifest(path, { chunkSize? })` | Reads the whole file; returns the manifest as a buffer and the commitment as 64 hex digits |
| `openAuthenticated(path, manifest, commitment, { heldBytes? })` | Checks the manifest against the commitment, then opens the file, which must be the committed length; `heldBytes` bounds the verified chunks it keeps |
| `reader:readAt(position, length)` | Exactly `length` verified bytes as a new buffer |
| `reader:readAtInto(target, position, length, bufferOffset?)` | The same into `target`; returns the count |
| `reader:size()`, `chunkSize()`, `commitment()` | What was committed to, not what the file says now; these work after `close` |
| `reader:close()` | Releases the handle; later reads raise, and closing twice does nothing |

**What a successful read proves.** The bytes are the ones at that position of the object whose
manifest hashes to your commitment. The manifest commits to the object's length, the chunk size and
every chunk's digest in order, and each digest also covers its chunk's index and length, so nothing
can be moved, cut, extended or swapped.

**What it does not prove.** The commitment is the whole foundation: compute one from the bytes you
are about to check and you have proven nothing, which is why `authManifest` can't open a reader. It
says nothing about freshness: an old revision stays valid against its old commitment, replaying it
is possible, and whether a commitment is current is your question. It says nothing about the chunks
a read didn't touch, which have not been looked at; damage there surfaces when something asks for
them. And it has nothing to do with durability: use the [stable storage](#stable-storage) calls for
that. Signing and key management are not here either. Authenticated reads do not encrypt the object.

**Failures.** Verification failing answers `nil`, the message and a kind, never partial data:
`integrityFailed` when bytes, the manifest or the file's length don't match, `invalidManifest` when
bytes match their commitment but aren't a manifest, `unsupported` for a version or algorithm this
build doesn't know, and the usual `dream_fs_ErrorKind` when the OS refuses. Mistakes raise: a
commitment that isn't 64 hex digits, a bad `chunkSize`, a closed reader, or a range outside the
object. Unlike `readAt`, an authenticated read never comes back short, so asking past the end raises.
An empty read anywhere up to the end succeeds and touches nothing. If `readAtInto` begins a read
and verification or I/O fails, its target window is zeroed; it holds neither unverified bytes nor a
verified prefix. Argument errors, including an invalid window or range and a closed reader, raise
before the read begins and leave the target unchanged. A successful read fills the window with
verified bytes.

**Files that change underneath.** Truncation, replacement and writes in place are all safe, in the
sense that a read fails rather than returns the wrong bytes. A reader never maps its file, because
a truncated map can kill the process and a mapped page can change after it was hashed: each chunk is
copied into memory nothing else can reach, hashed there, and only then shown. A write that lands
between two chunks of one read fails that read. On Unix a file renamed over the path doesn't affect
an open reader, whose handle keeps the old revision. Windows may refuse that rename while the reader
holds the file. A reader keeps chunks it verified for partial reads, so a repeat of a held chunk
returns the committed bytes without noticing damage done since; those bytes are still correct, and
the damage surfaces once the chunk is evicted. What was never verified is never kept.

**Memory.** The returned buffer is yours and nothing else refers to it. There is no zero-copy view
into the file, because a view of the file could change after verification. Whole chunks are read
straight into the result, so a large read is copied once and holds nothing; a partial chunk goes
through memory the reader owns. The manifest is copied once, after it has matched.

**Native memory limit.** Authenticated operations share a 128 MiB reservation budget per runtime,
separate from Luau's heap limit. Opening a reader reserves its potential cache capacity, manifest
copy and bookkeeping before allocation; opening raises if the budget is exhausted. The reservation
is conservative: it can reject a reader before its cache is actually populated. Closing releases the
cache reservation; the manifest and its reservation remain until the reader is collected because
its metadata methods still work after close. Manifest construction reserves the remaining budget and
checks its output and scratch size before allocation. The budget bounds accounted payload and
bookkeeping, not allocator overhead, process RSS, ordinary filesystem operations, or host Rust
allocations. Each runtime has its own budget. Rust readers cap `hold_bytes` at 64 MiB, with at least
one chunk, and hosts must still limit reader counts, manifests and output buffers themselves.

**Held chunks.** A reader keeps up to 256 KiB of the chunks it verified for partial reads, least
recently used going first, and takes that memory only as it reads. `heldBytes` changes the budget, up
to 64 MiB; it always holds at least one chunk, and at most 1024. Reads that cluster, or that turn
between a table and its records, gain most: 65 µs down to 17 and 33 µs per read at 16 KiB chunks, on a
CPU without SHA extensions. Uniform random reads gain nothing and lose nothing. A binary search or a
skewed load only starts gaining at about 1 MiB, so ask for that if you know you have one.

**Bounds.** Chunk sizes are powers of two from 512 bytes to 16 MiB, at most 16,777,216 chunks to an
object, which is 256 GiB at the default 16 KiB. A manifest is hashed before any part of it is
believed, and one over 512 MiB is refused without hashing. Read a manifest from untrusted storage
through `readAt` with a limit you choose (as above), since `readFile` allocates whatever the file's
size says. A manifest takes 32 bytes per chunk plus 32. Verifying holds a second copy of the manifest
once it has matched, so the largest one peaks at about 1 GiB with the caller's. A script's `readAt`
allocates under the runtime's memory limit, but a reader's copy of its manifest and its held chunks
live outside the Luau heap, where that limit does not see them. From Rust, `read_at` allocates whatever length the caller
asked for inside the object, so a host serving untrusted requests should bound that length or use
`read_into` with windows of its own; on a system that overcommits, memory running out while the
buffer fills is not an error anyone can catch.

**Choosing a chunk size.** On a CPU without SHA extensions, reading 64 bytes of a 64 MiB object took
about 290 ms by hashing all of it, and about 65 µs here at the default chunk size with a reader already
open, or 625 µs opening one first. The cost of a read is one chunk's hash, so smaller chunks read
faster and larger ones open faster. 16 KiB is the default for that reason; choose 64 KiB if you open
often and read little, 4 KiB if you hold a reader and read tiny records. A read of most of the object
costs about what verifying the object does, and an object of about one chunk gains nothing. The
measurements, with their caveats, are in `benches/fs-authenticated-results.md`.

**The format.** Little-endian, fixed widths, one encoding per object and chunk size, so the commitment
is the same on every platform. A commitment is the plain SHA-256 of the manifest's bytes, so
`sha256sum` agrees with it.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic, `L3IAUTH\n` |
| 8 | 2 | Format version, 1 |
| 10 | 1 | Algorithm, 1 for SHA-256 |
| 11 | 1 | Digest length, 32 |
| 12 | 4 | Chunk size |
| 16 | 8 | Object length |
| 24 | 8 | Chunk count |
| 32 | 32 each | Chunk digests in order: SHA-256 of `l3i.fs.authenticated.chunk.v1\0`, the index and the length as `u64`s, then the chunk |

A flat list of digests was chosen over a Merkle tree because the whole manifest is small next to the
chunks it describes and is read once per reader, so a proof per read would add code and bytes to save
a cost that barely registers. A format version is there for the day that changes.

Linux is what this has been tested on. Windows and macOS share the code path through the existing
positional read, but have not run it.

### Paths

`join(...)`, `parent(path)`, `fileName(path)`, `extension(path)` and `isFileName(name)` take byte
paths apart and join them by Rust's `Path` rules with the platform's separator, touching no disk,
so they need no capability. `join` starts over at a part that is absolute; `parent` of a bare
name is `''` and of a root `nil`; `isFileName` is true for exactly one plain component.

### Capabilities

Reading needs `filesystem.read` (`l3i::fs::READ_CAPABILITY`) and changing the disk needs
`filesystem.write` (`l3i::fs::WRITE_CAPABILITY`). A plan that grants neither still has the module
and its types, and each function raises a permission error naming what it lacks; the path
functions need nothing.

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
| `run(program, args?, { cwd?, env?, clearEnv?, stdin?, stdout?, stderr?, timeoutMs?, maxStdoutBytes?, maxStderrBytes? }?)` | `{ success, code?, signal?, stdout?, stderr?, held? }`; `stdout` and `stderr` are `"inherit"` (the default), `"capture"` or `"null"`. A limit that passes answers `nil, message, "timeout"` or `"outputLimit"` (see below) |
| `start(program, args?, { cwd?, env?, clearEnv?, stdin?, stdout?, stderr?, timeoutMs?, maxStdoutBytes?, maxStderrBytes? }?)` | A process handle (see below), or `nil, message, kind` when the program can't start |
| `env(name)` | An environment variable, or nil |
| `knownFolder(name)` | A folder the OS defines for the user, or nil when it can't name it. `"home"`: `HOME`, or on Windows `USERPROFILE`, then `HOMEDRIVE` with `HOMEPATH`, then `HOME`. `"documents"`: the Windows Documents known folder from `SHGetKnownFolderPath`, wherever the user (or OneDrive) moved it; nil on every other platform. What an application keeps there is its own business |
| `platform()` | The OS the host was built for, as Rust's `std::env::consts::OS` names it: `"windows"`, `"macos"`, `"linux"`, `"android"`, and so on. A call, not a constant, so compiled code never carries another platform's answer |
| `write(stream, data)` | `true`: data to `"stdout"` or `"stderr"`, unbuffered, without `print`'s newline. A closed pipe is not an error in a host that ignores `SIGPIPE`, which every Rust binary does on startup; a host that has restored the default disposition dies on it, as `| head` has always killed writers |
| `isTerminal(stream)` | Whether the stream is a terminal |
| `sleep(seconds)` | Nothing: blocks the script's thread for `seconds` (finite, at least 0). Never past the runtime's execution time limit: a sleep the deadline cuts short ends there and the watchdog stops the script at its next instruction |

The program is found the way the OS finds one, and its arguments reach it as they are, with no
shell in between. A program the OS can't start returns `nil`, the message and the kind, as
`@dream/fs` does. `run` needs `process.spawn` (`l3i::process::SPAWN_CAPABILITY`), `env` and
`knownFolder` need `process.environment` (`ENVIRONMENT_CAPABILITY`): where a user's folders are
says who the user is. `write` needs `process.stdio` (`STDIO_CAPABILITY`), so a sandboxed script's
output goes where its host routes `print`; `isTerminal`, `platform` and `sleep` need nothing.

A child inherits the host's environment only when the runtime grants `process.environment`;
without it the child starts with the host's `PATH` and exactly the variables `env` names, since
inheriting would hand the script every variable `env` refuses it. `PATH` stays so the program,
and whatever it starts in turn, is found where the host would find it; a script can learn it by
running a program that prints it. `clearEnv = true` clears `PATH` too.

`run` waits no longer than the runtime's execution time limit: a child still running at the
deadline is killed and reaped, a capture still open then (a process the child started can hold
the pipe after the child is gone) is abandoned, and the script stops with the time limit.

Three optional limits bound a run further. Each one that passes kills the child and reaps it,
discards whatever it wrote, and answers `nil, message, kind`:

| Option | Limit | Kind on failure |
|---|---|---|
| `timeoutMs` | A whole number from 1 to 86,400,000 (a day): the child may run this many milliseconds | `"timeout"` |
| `maxStdoutBytes` | A whole number from 0 to 2^53 - 1: the most bytes a captured `stdout` may produce | `"outputLimit"` |
| `maxStderrBytes` | A whole number from 0 to 2^53 - 1: the most bytes a captured `stderr` may produce | `"outputLimit"` |

A value that is not a finite whole number in its range raises, as does a ceiling for a stream that
is not `"capture"`. The raise comes before the child starts, so a refused call starts nothing. The
ceilings are separate: each applies to its own stream only. Reading stops as soon as a ceiling is
crossed, so memory stays within the ceiling however much the child writes. A captured stream given
no ceiling has one of 64 MiB: a capture is held outside the Luau heap, where the runtime's memory
limit does not see it. A run that needs more names its own ceiling.

A process the child started can keep a captured pipe open after the child has exited. On Unix `run`
does not wait for it: once the child is gone it reads until that process has written nothing for
250 milliseconds, or for 2 seconds whatever it writes, and then answers the child's own result with
`held = true`. What that process writes later is not in the result. Whether anything still holds
the pipe is asked of the pipe, not guessed from the time: a run whose captures nothing holds reads
them to their end and is never `held`, however slow the machine is to read them, and a holder that
exits while `run` is still reading leaves whole captures, not a held run. `held` is the run's, not
one stream's: with one stream held and the other not, both are read as far as they go and the run
says `held`. A run with a limit of its own
kills the child's process group at that point; one without leaves it running. Elsewhere the wait
lasts until the pipe closes or a limit passes. A `timeoutMs` that passes first still answers
`"timeout"`.

Under an execution time limit, a run that shares neither `stdout` nor `stderr` with the host is put
in a process group of its own, as a run with a limit is, so the limit reaches what it started. Such
a run is a background job to a controlling terminal and cannot use it; one that shares a stream
stays in the host's group. A `cwd` that is not a directory answers `nil, message, "notFound"` with
a message that names the `cwd`.

On Unix a bounded child starts in a process group of its own. On Linux, Android, macOS and FreeBSD
a limit signals that whole group, so a process it started and kept in the group cannot hold the
pipes open past the limit; on other systems only the child is killed. Termination covers the child's
process group and nothing more: a descendant that left the group (with `setsid` or `setpgid`) is
not stopped, and may hold an inherited pipe open; the call stops waiting for that pipe at its
limit, and answers `"timeout"` even if the child itself had exited. On Unix, whatever holds a
pipe, the call leaves no thread and no descriptor behind when it returns. On Windows a thread
blocked on a pipe that something else holds cannot be ended: the call still returns at its limit,
and that thread stays until the pipe closes. A bounded run whose child exits normally
does not signal the group: what the child left running in it keeps running. An unbounded run does
not get a group of its own, so it keeps receiving the terminal's signals; a bounded one does not
receive them, and a child that reads the terminal from its own group is stopped by it. A run holds
one of the runtime's live-child slots while it runs (see [Started processes](#started-processes)).

A host that ignores `SIGCHLD`, or reaps children itself, takes the child's exit status before this
module can read it. The call then fails, saying the child was reaped by the host, and signals
nothing: the child's pid, and the group it led, may belong to another process by then.

### Started processes

`start` launches a child and returns at once, with a handle to it. It needs `process.spawn`, as
`run` does, and takes `run`'s `cwd`, `env` and `clearEnv`, with its own stream options:

| Option | Meaning |
|---|---|
| `stdin` | `"pipe"` to write to the child, or `"null"` (the default) |
| `stdout`, `stderr` | `"inherit"` (the default), `"pipe"` to read, or `"null"` |
| `timeoutMs`, `maxStdoutBytes`, `maxStderrBytes` | The limits of `run`, checked the same way. A ceiling is a lifetime total for a piped stream |

| Method | Answers |
|---|---|
| `readStdout(maxBytes)`, `readStderr(maxBytes)` | Between 1 and `maxBytes` bytes that are buffered now. `nil, message, "wouldBlock"` when none are yet; `nil` alone at end of stream |
| `write(data)` | The number of bytes queued for the child's stdin, or `nil, message, "wouldBlock"` when the queue is full |
| `closeStdin()` | Closes the child's stdin once the queued bytes are written |
| `wait(timeoutMs?)` | `{ success, code?, signal? }` once the child has exited; `nil, message, "wouldBlock"` while it runs. It waits up to `timeoutMs` (at most 60,000, and never past the execution time limit); omitted or `0` only looks |
| `kill()` | Kills the process group, or the child where groups are unavailable. Idempotent; returns `true`. It does not reap: `close` does |
| `close()` | Kills the child if it still runs, reaps it and releases its pipes. Idempotent; every other method raises afterwards |

Reads and writes never block. `wait` is the only method that can wait, and only when given a
timeout. A `wait` with a timeout blocks the calling thread for up to that long, so a cooperative
caller, such as a frame-driven host, polls with `wait()` or `wait(0)` instead. Each piped output
keeps at most 64 KiB buffered: when that is full its reader stops, so an unread child blocks on its
own pipe instead of growing the host's memory. The stdin queue holds 1 MiB of bytes not yet written,
and `write` answers `wouldBlock` beyond that.

When a limit passes, the child is killed and reaped, its buffered output is discarded, and later
reads or writes answer with the limit's kind: `"timeout"` or `"outputLimit"`. `wait` then reports the
child as ended by signal, unless it had already exited on its own, whose status it keeps. The
handle's threads end with the limit, and its slot is free from then. Killing the process group reaches what the child started too, on Linux,
Android, macOS and FreeBSD, and only while the child is unreaped: the group is signalled before the
child is reaped, and never after. On those systems `wait` answers the child's status without reaping
it, so a `kill` or `close` after a `wait` still stops what the child left in its group, such as a
process holding one of its pipes open; `close`, a limit, collection of the handle and runtime
shutdown are what reap. Once the handle is closed nothing signals that group again. A descendant
that left the group is never stopped. On other systems only the child itself is killed, and `wait`
reaps it. Closing the handle, collecting it, or shutting the runtime down does what `close` does, so
no direct child outlives its handle. A handle with no `close` still runs its child until it is
collected, and holds an exited child's process table entry until then.

Each started child, and each `run` while it runs, holds one of the runtime's live-child slots. The
default is 32. A host sets it with `ProcessExtension::with_max_children(n)` (1 to 1024), and a
script cannot change it. A started child holds its slot until its handle is closed, a limit stops
it, its handle is collected or the runtime shuts down. An exited child that has been waited for
still holds its slot until then, because the handle can still stop what it left behind. So close a
handle when you are done with it: the collector does not know what an unclosed handle costs, and a
script that starts children without closing them meets the limit before any is collected. A start
past the limit raises, and a start that fails to spawn holds no slot. The limit is per runtime:
each runtime has its own.

The reader and writer threads of a handle are an interim design, pending the readiness substrate of
RFC #11. On Unix they end the moment the handle is closed or a limit stops the child, whoever holds
their pipes, so the live-child limit bounds them: at most four threads a slot. On Windows a thread
blocked on a pipe that something still holds stays until that pipe closes. `close`, and a `kill` or
read that meets a limit's kill in progress, wait for the child to be reaped; that is brief after a
`SIGKILL`, and is the one way a call here other than `wait` can take time.

A started child is always in a process group of its own, so the terminal's signals do not reach it
and a child that reads the terminal is stopped by it; start what needs the terminal with `run`.

A host that ignores `SIGCHLD`, or reaps children itself, takes a child's exit status first. `wait`
then fails, saying so, and `kill`, `close` and the timer signal nothing.

A child started without the environment capability gets the same environment `run` gives it.

### A secret for a child

A child's arguments are in every process listing, its environment is copied to everything it
starts, and a file is a file: none of them is a place for a credential. `run` and `start` take
`secret = { env = NAME, data = bytes }` instead:

```luau
local result = process.run('git', { 'fetch', '--quiet', 'origin' }, {
    secret = { env = 'APP_TOKEN_FD', data = token },
    timeoutMs = 60000,
})
```

The bytes are written into a new pipe and its write end is closed. The child inherits the read
end; the variable named by `env` holds its **number**, never the bytes: a file descriptor on Unix,
a handle's value on Windows. Whoever reads it first gets the secret and then end of file: the
child, or a helper it starts (a credential helper, say), which inherits it in its turn. So the
secret is in no argument, no environment value and no file, and once read it is in no pipe.

| | |
|---|---|
| `env` | A variable name of capitals, digits and underscores, at most 64 bytes, not starting with a digit; not `PATH`, and not a loader variable (`LD_*`, `DYLD_*`). Naming one that `env` also sets raises |
| `data` | A string or a buffer of 1 to 4096 bytes: what every supported system's pipe takes in one write that cannot wait |

Anything else raises before the child starts, and no refusal repeats the bytes. On a system that
is neither Unix nor Windows a call with `secret` answers `nil, message, "unsupported"`.

A reader takes the number from the variable and reads until end of file: on Unix `read(fd)`, or
from a shell `cat "/dev/fd/$APP_TOKEN_FD"`. Read it by that path, not by a redirection such as
`<&"$APP_TOKEN_FD"`: the number can be 10 or more, and a POSIX shell is only required to accept
a single digit there (dash, the `sh` of Debian and Ubuntu, refuses more). `/dev/fd` reaches every
descriptor on Linux, Android and macOS; on FreeBSD it holds only 0 to 2 unless `fdescfs` is
mounted, and other BSDs provide a fixed range, so there read the descriptor from a program, or
from a shell that accepts any number after `<&` (bash, ksh, zsh). On Windows a native program calls `ReadFile` on the handle
(or wraps it, as .NET's `SafeFileHandle` does). A shell script run by Git for Windows' bundled
shell cannot open a handle by its value, so a Windows credential helper has to be a native
program.

Whether a helper **the child starts** gets the descriptor is the child's decision, not this
module's. Git for Windows starts its own children with only their standard streams (its
`core.restrictInheritedHandles` setting, which by default restricts on Windows 7 and later; see
Git for Windows' own documentation), so a credential helper it runs
does not receive the handle unless that is turned off for the call
(`git -c core.restrictInheritedHandles=false ...`), which lets Git's children inherit every
inheritable handle Git has. Other launchers restrict inheritance the same way. Where that is not
wanted, make the program that needs the secret the direct child.

A secret needs no capability of its own: `process.spawn` is what starts a child, with a secret
or without one, and a runtime without it can pass none. Nothing about the existing capabilities
changes, and a child is given no more of the host's environment for having a secret.

**One secret, one read, one attempt.** The descriptor gives the bytes to its first reader and end
of file to every later one, so a program that asks for the credential a second time in the same
call (after a redirect, or because the first answer was rejected) gets nothing, provided the
first reader read to end of file: a reader that stops early leaves the rest for the next. A helper must
treat an empty read as "no credential" and say so, never invent or cache one; the program then
fails, and the caller sees that as an ordinary failed call. **A retry is a new call with a new
`secret`**: the caller, which holds the credential, decides whether to try again, and it can
mint a fresh token for it. Nothing here retries on its own, and nothing remembers a secret
between calls.

With Git, the helper is a credential helper, which Git starts itself:

```luau
-- The helper answers `get`, for https and the one host the token is for, from the descriptor.
-- No part of this is the token.
local helper = [[!f() { test "$1" = get || exit 0; ]]
    .. [[while IFS='=' read -r key value; do case $key in protocol) protocol=$value;; host) host=$value;; esac; done; ]]
    .. [[test "$protocol" = https && test "$host" = github.com || exit 0; ]]
    .. [[token=$(cat "/dev/fd/$APP_TOKEN_FD"); test -n "$token" || exit 1; ]]
    .. [[printf 'username=x-access-token\npassword=%s\n' "$token"; }; f]]
local result, message, kind = process.run('git',
    { '-c', 'credential.helper=', '-c', 'credential.helper=' .. helper, 'fetch', '--quiet', 'origin' },
    { secret = { env = 'APP_TOKEN_FD', data = token }, timeoutMs = 60000,
      env = { GIT_TERMINAL_PROMPT = '0', GIT_CONFIG_NOSYSTEM = '1', GIT_CONFIG_GLOBAL = '/dev/null' } })
```

The first `credential.helper=` clears any helper configured elsewhere, so the token is offered to
this one only and stored by none; `GIT_TERMINAL_PROMPT=0` makes a missing credential a failure
instead of a prompt. If the helper hangs, `timeoutMs` ends the call and the process group, the
helper with it.

**The helper must check what it is asked for.** Git tells a helper the protocol and host it wants
a credential for, on the helper's standard input, and it asks for whatever host it ends up
talking to: the one in the remote's URL, the one a `url.<base>.insteadOf` in the repository's own
configuration rewrites that to, or the one a redirect sends the first request to. A helper that
answers every request hands the token to any of them, and over `http` in clear text. So the
helper above answers only for `https` and one host, and reads the descriptor only then. The
recipe still trusts the repository's own configuration for everything else (`http.proxy`,
`http.sslVerify`): run it on repositories whose configuration you wrote, or pass the settings
that matter with `-c`. And do not give a child with a secret the host's environment unless you
know what is in it: Git's tracing variables (`GIT_TRACE_CURL` with `GIT_TRACE_REDACT=0`) print
request headers.

What this does **not** do:

- **It does not hide the secret from other processes of the same user.** On Linux such a process
  can open `/proc/<child>/fd/<n>` and read the pipe before the child does, or hold it open for
  writing so that the child never meets end of file; on Windows it can duplicate the handle out
  of the child, whose value is in the child's environment. That is no worse than the environment,
  which the same process could read, but it is not secrecy from the user's own processes. Read the
  secret first thing, treat an empty read as a secret that was taken, and give the call a
  `timeoutMs`.
- **It does not overwrite the script's copy.** The host overwrites the copy it made; the Luau
  string or buffer the secret came from lives until it is collected.
- **It does not stop the child passing it on.** Everything the child starts inherits the
  descriptor unless the child closes it, including a process that outlives the child (Git's
  credential cache daemon is one). A helper should close it after reading, and a secret should go
  only to a program whose children you would also give it to.

Keeping it to the one child:

- **Linux, Android and the BSDs**: both ends are close-on-exec from the moment they exist, so a
  child another thread starts inherits neither; the flag is cleared in this child alone.
- **macOS**: there is no call that makes the pipe close-on-exec in one step. A child another
  thread starts in the moment between making the pipe and marking it inherits both ends: it could
  read the secret later, and while it lives the intended reader never meets end of file, so it
  waits until its `timeoutMs`.
- **Windows**: a handle is inherited by any child started while it is marked inheritable, and
  there is no stable way to name the handles one child gets. Every child this module starts is
  started under one lock and the handle is inheritable only inside it, so no other child of this
  library inherits it. A child anything else in the host starts in that moment can: an ordinary
  `std::process::Command` in another crate is enough, since the standard library's own lock does
  not cover this handle. The lock also means this module starts one child at a time on Windows:
  a start the system holds up (a program on a stalled share, a scanner) holds up the others.
- The number is never 0, 1 or 2, even in a host that has closed its own standard streams: those
  are the child's own input and outputs.

The host closes its end as soon as the child has been started, or has failed to start. A child
that never reads the descriptor takes the secret with it when it and what it started have exited.

### Reading standard input

With `process.stdio`, `@dream/process` exposes `openStdin()`. It claims an exclusive
consumer across all L3i runtimes and worker threads in the host process. A second claim
returns `nil, message, "wouldBlock"`. Host code must coordinate any direct stdin reads
with L3i; stdin is a single stream, not a broadcast channel.

```luau
local process = require('@dream/process')
local input = assert(process.openStdin())
while true do
    local line, message, kind = input:readLine(65536)
    if kind == 'wouldBlock' then
        input:wait(1000) -- explicit blocking wait, suitable for standalone commands
    elseif message then
        input:close()
        error(message)
    elseif line == nil then
        break -- EOF, distinct from an empty line ('')
    else
        print(line)
    end
end
input:close()
```

For frame-driven applications, call `readLine` or `read` from the existing scheduler and
retry `wouldBlock` on a later frame. Do not call `wait` in a frame handler. The reader
never invokes Luau or schedules callbacks. Hosts should close the handle when its owning
context or generation ends; runtime destruction also releases it through userdata teardown.

- `read(maxBytes)` returns up to the requested number of bytes in a binary Luau string.
  The integer bound is 1–1,048,576. Short reads are normal.
- `readLine(maxBytes?)` defaults to 65,536 bytes before LF, with the same maximum bound.
  It strips LF and a preceding CR; a final unterminated line is returned unchanged.
  It preserves UTF-8 and arbitrary non-UTF-8 bytes exactly. Text validation is the caller's
  choice. Empty input returns EOF; an empty line returns `''`.
- Pending reads return `nil, message, "wouldBlock"`; EOF returns nil without an error.
  OS failures return `nil, message, kind`, as other process operations do.
- A pending line must be retried with the same bound before switching to raw reads.
  An oversized line returns `invalidInput`, consuming the prefix through the first excess
  byte. It does not drain an arbitrarily long line: the next `readLine` carries on with what
  is left of it, so to skip the line, call again until one comes back instead of
  `invalidInput` (that one is its tail). Or close the handle. A bound includes CR before LF.
- `wait(timeoutMs)` waits up to 60,000 milliseconds, capped by the execution time limit.
  It returns whether input, EOF, or an error is available, and does not initiate a read.
- `close()` is idempotent and immediately releases ownership. A pending partial line is
  discarded. Bytes not yet consumed remain in the process buffer for the next owner.

One lazy OS-reader thread serves the process, with at most 8 KiB of shared buffering and
one bounded partial line for the owner. It reads only on demand. Closing or destroying a
runtime never joins a thread blocked in terminal input. That single thread may remain
blocked until input arrives or the process exits; a new consumer can use its eventual
result. No new thread is created on restart.

`process.stdio` is checked when stdin is opened. The handle is then the authority to read:
passing it to less privileged code delegates reading. `Runtime::shutdown()` releases the claim
even while a script still holds the handle, whose reads then report a closed consumer.

A host can add its own check with `ProcessExtension.with_input_policy(policy)`, an
`Rc<dyn process::input::Policy>` asked about `Open` and `Wait` at every such call. What it
refuses raises in the caller. It runs after the `process.stdio` gate, which it cannot bypass;
reads and `close()` are never checked. The live-child limit is set the same way, and the two
combine: `ProcessExtension.with_max_children(8).with_input_policy(policy)`. Both return a
`process::ProcessConfig`, which is itself an extension.

Windows console reads use Rust's standard
input behavior (Unicode console input); redirected pipes preserve binary bytes on all
platforms. No terminal, raw terminal mode, or network capability is required.

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
| `shutdown("write")` | As a TCP stream's `shutdown("write")`; the only half-close TLS has. Queues close_notify and flushes; once it has left, the TCP write half is shut. Reading stays open |
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

## dream.audio

Feature `audio`; module `@dream/audio`, `l3i::audio::AudioExtension`. Decoders for the three
compressed formats a game or an application ships audio in: MP3 (and MPEG layers I and II),
Ogg Vorbis and Ogg Opus, each filling a caller's buffer with interleaved little-endian `f32`
frames. Decoding reads only the bytes it was given: no file, no device, no capability. Mixing,
resampling and playback are the host's.

```luau
local audio = require("@dream/audio")

local decoder = audio.decoder(fs.readFile("theme.ogg"))
print(decoder:format(), decoder:rate(), decoder:channels(), decoder:frames())   -- vorbis 44100 2 ...

local block = buffer.create(4096 * decoder:channels() * 4)
local frames = decoder:read(block)          -- up to 4096 frames, 0 at the end
decoder:seek(44100)                         -- one second in, exactly

local step = audio.decode(fs.readFile("step.mp3"))
print(step.frames, step.rate, buffer.len(step.data))
```

| Function | Returns |
|---|---|
| `audio.detect(data)` | `"mp3"`, `"vorbis"`, `"opus"` or nil, from the first bytes of a buffer or string |
| `audio.decoder(data, options?)` | A `dream_audio_Decoder` over a copy of the bytes; `{ format? }` names the format instead of detecting it |
| `audio.decode(data, options?)` | `{ data, format, rate, channels, frames }`: the whole stream in one buffer, at most `maxSize` bytes (default 1 GiB) |
| `decoder:format()`, `decoder:rate()`, `decoder:channels()` | The stream's format, frames per second and samples per frame |
| `decoder:frames()` | The length in frames after gapless trimming, or nil when the stream does not say |
| `decoder:position()` | The frame the next read starts at |
| `decoder:read(target, offset?, frames?)` | Decodes up to `frames` frames (default: as many whole frames as fit) into `target` from byte `offset`; the frames written, 0 at the end |
| `decoder:seek(frame)` | Moves to a frame; past the end is the end |

A frame is one sample per channel, and positions and lengths count frames. Channels come in the
file's own order, which for both Ogg formats is Vorbis order (5.1 is front left, centre, front
right, rear left, rear right, LFE). Samples are nominally from -1 to 1; a decoder does not clip.

### The formats

**MP3** decodes through [nanomp3](https://crates.io/crates/nanomp3), a safe, bit-exact port of
minimp3. ID3 and APE tags are skipped, and the encoder delay and padding a LAME or Xing header
records are trimmed, so a loop cut in an encoder loops without a gap. The length comes from
that header when there is one, as minimp3 does, and from scanning the stream when there is not:
a file cut short after it was encoded states more frames than it holds, and reading ends where
the data does. A stream whose rate, layer or channel count changes partway is an error where
the change is.

**Ogg Vorbis** decodes through [lewton](https://crates.io/crates/lewton)'s packet decoder.
Each packet's output length follows from its block size without decoding it, so the length and
every position are exact, and the first and last granule positions trim the start and the end
as the Vorbis specification and libvorbis do: a first page that states fewer frames than its
packets produce trims the start (unless it is also the last page, when the end is trimmed).

**Ogg Opus** decodes through libopus 1.6.1, the reference implementation, built from source
in its float configuration with the same clang and LTO as Luau.
Opus always decodes at 48 kHz, whatever rate the encoder was given. The stream's pre-skip is
trimmed from the start, its last granule position trims the end, and its output gain is
applied. Mapping family 0 (mono and stereo), family 1 (up to eight channels) and family 255 are
decoded; the ambisonic families 2 and 3 are refused, since they need a renderer.

An Ogg file is indexed into its packets when it opens, copied out of their pages into one
allocation about the file's size, so a seek is a binary search over packets. Only the first
logical stream is read: a chained file plays its first link, and a multiplexed one its first
stream. The framing is l3i's own: pages are checked against their CRC; a damaged one is skipped by scanning for the next
page, and a packet that spanned it is dropped whole rather than decoded half. A file cut short
plays up to the cut.

### Seeking

A seek is exact for MP3 and Vorbis: the decoder decodes what the target depends on (MP3's bit
reservoir, the Vorbis packet whose window the target's packet overlaps) and then gives exactly
what a straight run gives. Opus cannot be exact without decoding from the start, so decoding
restarts 400 ms ahead of the target. RFC 7845 asks for at least 80 ms, after which a cold
decoder still runs as much as 15% quiet: CELT codes each frame's band energies against the
previous frame's, and the error decays a few frames at a time. After 400 ms it is under a
millionth of full scale, for about twenty packets more per seek. A target within the first
400 ms decodes from the start and is exact.

### Errors

Opening fails on bytes that are no format here, on the wrong `format`, and on headers that are
broken or describe something unsupported, with an error naming the call:
`audio.decoder: bad OpusHead: not an Opus stream`. A read fails where the stream is broken: an Ogg
decoder reads on from the next packet, and an MP3 decoder stays at the end until it seeks. The
decoders are third-party Rust fed whatever a file holds: a panic in one is caught at the call and reported, and that decoder refuses to read
again until it seeks. `audio.decode` refuses an output past `maxSize` while decoding, before
the buffer exists. Damaged files are a test: mutated copies of every fixture open, read and
seek as errors or audio, never as a panic that escapes.

### Checked against the reference decoders

Real files decode to the reference decoders' length to the frame and agree with their output:
thirty MP3s, from sound effects to whole songs, at 120 to 125 dB against ffmpeg's decoder, twenty Ogg
Vorbis files at 129 to 133 dB against ffmpeg's, and fifteen Ogg Opus files at 137 to 139 dB
against `opusdec` from the system's libopus. The exceptions are the two the rules above describe: an MP3 whose
header states more than the file holds, and a Vorbis file whose first page trims its start,
which libvorbis trims and ffmpeg does not.

### What it costs

A decoder holds a copy of the file (and for Ogg its packet index) on the native heap until it
is collected; the VM sees one userdata. `decoder:read` writes straight into the caller's buffer
through a scratch block the handle keeps. `audio.decode` builds the samples natively, then one
buffer of exactly their size.

The feature adds nanomp3 and its core with `wide`, `safe_arch` and `bytemuck` for its SIMD
paths, and lewton with `byteorder` and `tinyvec`: eight crates, all Rust, under MIT,
Apache-2.0, Zlib or the Unlicense, and both decoders forbid `unsafe`. Opus is C: libopus, the
`opus` submodule, compiled by `build.rs` (137 files, the encoder among them for the linker
to drop). It is under the BSD 3-Clause license, copyright Xiph.Org and its contributors,
which asks a binary that ships it to reproduce its notice (`opus/COPYING`).

Pure-Rust Opus decoders were measured first and set aside: opus-rs drifted 38 to 64 dB from
libopus on thirteen of the fifteen real files, and ruopus matched libopus but needs an FFT
whose dependencies require a newer Rust than l3i supports.
