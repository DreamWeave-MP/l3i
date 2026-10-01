+++
title = "Built-in extensions"
description = "The extensions l3i ships: the dream.net bridge every plan carries, packed rotations in dream.quat, colors and clip rectangles in dream.raster, the dream.soft_render device, bytes for parsing in dream.bytes, textual identity as numbers in dream.intern, Luau's own parser in dream.luau, the host filesystem in dream.fs, and child processes in dream.process."
weight = 90

[extra]
kind = "guide"
+++

Nine extensions come with the crate. `dream.net` is in every plan; the others are added with
`RuntimePlan::builder().extension(..)`. Each is an ordinary `Extension` built on the
[primitives](@/docs/primitives.md), with a Luau signature on every member, so a strict script
that requires its module type checks against the plan's definitions.

| Extension | Module | Rust | Feature |
|---|---|---|---|
| `dream.net` | `@dream/net` | `l3i::net` | always |
| `dream.quat` | `@dream/quat` | `l3i::quat::QuatExtension` | always (`quat.math()` needs `jit`) |
| `dream.raster` | `@dream/raster` | `l3i::raster::RasterExtension` | always |
| `dream.soft_render` | `@dream/soft-render` | `l3i::soft_render::SoftRenderExtension` | `soft-render` |
| `dream.bytes` | `@dream/bytes` | `l3i::bytes::BytesExtension` | `bytes` |
| `dream.intern` | `@dream/intern` | `l3i::intern::InternExtension` | `intern` |
| `dream.luau` | `@dream/luau` | `l3i::syntax::SyntaxExtension` | `syntax` |
| `dream.fs` | `@dream/fs` | `l3i::fs::FsExtension` | `fs` |
| `dream.process` | `@dream/process` | `l3i::process::ProcessExtension` | `process` |

The samples below reach the modules as compat globals (`RuntimePolicy::new().compat_global("@dream/quat", "quat")`),
which is what the tests do; `require("@dream/quat")` is the canonical path.

## dream.net

Networking is runtime infrastructure, not a feature: l3i depends on dream-net and owns the Luau
bridge. The planner adds `NetExtension` to every plan and reserves the id `dream.net`, so no
runtime lacks the network and the policy's capabilities decide what scripts may do with it.
dream-net stays pure Rust and knows peers, event ids, channels and bytes; this module owns the
Luau-facing shape.

- Peer, event, channel and client ids are Luau integers, exact and compared with `==`; a peer id is a `Bits64` pattern, all 64 bits. Sizes and counters are plain numbers, because scripts threshold them.
- Payloads are Luau buffers, or strings on send. `pollInto` copies one received payload into a caller-owned buffer and `sendEvent` copies out of one before returning, so the transport retains no Lua memory.
- The transport clock is the plan's: `update()` reads a monotonic clock started with the bridge, or the clock the host gave `RuntimePlanBuilder::network_clock`, so a script cannot spoof time and a simulation can drive it.
- The server private key never reaches Luau: the host creates `dream_net::Server`s in Rust and hands them over as `net::Server` handles (`Server::new(server, clock)`, `Server::push(scope, server, clock)`, or a returned `Owned<Server>`). Scripts create clients with `net.client{}` only when the policy grants the `network.transport` capability (`net::TRANSPORT_CAPABILITY`).
- Nothing calls into Luau from inside dream-net; the host drives one network phase per frame.

### The module

| Member | Type |
|---|---|
| `schema(options)` | `(options: { version: number, maxMessagesPerPacket: number?, channels: { { [string]: any } }, events: { { [string]: any } } }) -> dream_net_Schema` |
| `client(options)` | `(options: { schema: dream_net_Schema, bind: string? }) -> dream_net_Client`, bound at install because it depends on the capability |
| `CONNECT_TOKEN_BYTES`, `MAX_EVENT_PAYLOAD`, `MAX_CHANNELS` | Folded numbers: 2048, dream-net's payload limit, 64 |

`net.schema{}` is a strict option table. A channel is `{ name, delivery, capacity?, overflow?,
packetBudget?, resendInterval? }` with `delivery` one of `'reliableOrdered'` and
`'unreliableUnordered'` and `overflow` one of `'fail'`, `'dropOldest'` and `'dropNewest'`; an
event is `{ name, channel, maxPayload, codecVersion? }` naming its channel. Errors carry the path
(`net.schema.channels[1].delivery`, `unknown channel 'missing'`).

```luau
local net = require("@dream/net")
local schema = net.schema{
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

`dream_net_Schema` is untagged: getters `version`, `fingerprint` (32 hex digits), `eventCount`,
`channelCount`; methods `eventId(name)`, `channelId(name)` (`integer?`), `eventName(id)`,
`channelName(id)` (`string?`), `maxPayload(eventId)` (`integer?`), `fingerprintHalves()`.

`dream_net_Client` and `dream_net_Server` are tagged when tags allow. The hot calls are the same
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

Misuse is a script error prefixed `dream.net:`, never a panic: an unknown event id, a payload
range past its buffer (`payload range 0..40 exceeds the 7-byte buffer`), a peer that is not
connected. `benches/net.rs` measures the bridge over localhost UDP.

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

Measured by `benches/bytes.rs` (`cargo bench --bench bytes --features bytes,bytes-codecs,bytes-digests,bytes-text,jit`)
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

Counted by `benches/intern.rs` (`cargo bench --bench intern --features intern,jit`) on an
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
| `luau.parse(source, options?)` | Parses a string or buffer. `options.declarations` allows definition-file syntax (`declare`, `declare extern type`); `options.tokens` adds the token stream |
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
their expressions, or `interpolatedSimple` with none; `longString` is `[[...]]`.

### What it costs

Every table is made at its final size and every key string is pushed once per call, so building
a node hashes no string. Counted by `benches/syntax.rs` (`cargo bench --bench syntax --features
syntax,analysis`) in retired instructions per source line, on a typed module of ordinary script
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
| `readFile(path)`, `readFileString(path)` | The whole file as a buffer or a string |
| `readAt(path, offset, length)` | `length` bytes from `offset` as a buffer, fewer at the end |
| `open(path)` | A `dream_fs_Reader` over a memory map of the file |
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
so a walk of a hundred thousand files makes a handful of tables. With `skipErrors`, what can't be
read below the root goes into `errors` as `{ path, message }`; without it, the first one is the
walk's answer.

A reader's `read`, `readInto`, `readAt` and `readAtInto` copy from the map; its position moves
with `read`, `readInto`, `seek` and `skip`. A writer's `write`, `writeAt`, `seek`, `truncate`,
`flush` and `close` answer the same way as the module's functions.

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
| `write(stream, data)` | `true`: data to `"stdout"` or `"stderr"`, unbuffered, without `print`'s newline; a closed pipe is not an error |
| `isTerminal(stream)` | Whether the stream is a terminal |

The program is found the way the OS finds one, and its arguments reach it as they are, with no
shell in between. A program the OS can't start returns `nil`, the message and the kind, as
`@dream/fs` does. `run` needs `process.spawn` (`l3i::process::SPAWN_CAPABILITY`) and `env` needs
`process.environment`; `write` and `isTerminal` need nothing, since `print` already reaches
standard output.

