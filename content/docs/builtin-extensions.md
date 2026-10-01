+++
title = "Built-in extensions"
description = "The extensions l3i ships: the dream.net bridge every plan carries, packed rotations in dream.quat, colors and clip rectangles in dream.raster, and the dream.soft_render device behind the soft-render feature."
weight = 90

[extra]
kind = "guide"
+++

Four extensions come with the crate. `dream.net` is in every plan; the other three are added
with `RuntimePlan::builder().extension(..)`. Each is an ordinary `Extension` built on the
[primitives](@/docs/primitives.md), with a Luau signature on every member, so a strict script
that requires its module type checks against the plan's definitions.

| Extension | Module | Rust | Feature |
|---|---|---|---|
| `dream.net` | `@dream/net` | `l3i::net` | always |
| `dream.quat` | `@dream/quat` | `l3i::quat::QuatExtension` | always (`quat.math()` needs `jit`) |
| `dream.raster` | `@dream/raster` | `l3i::raster::RasterExtension` | always |
| `dream.soft_render` | `@dream/soft-render` | `l3i::soft_render::SoftRenderExtension` | `soft-render` |

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
whose `rotate(q, v)`, `mul(a, b)`, `slerp(a, b, t)`, `key(q, flags)`, `keyRotation(k)` and
`keyFlags(k)` are ordinary bound methods on the interpreter path and lower to IR through
`quat::lowering::Lowering` when the compiler knows the receiver's type:

```luau
--!native
local quat = require("@dream/quat")
local Q: dream_quat_Math = quat.math()
local a = quat.axisAngle(vector.create(0, 0, 1), 0.3)
local b = quat.axisAngle(vector.create(1, 0, 0), 0.7)
local m = Q:mul(a, b)
local r = Q:rotate(m, vector.create(1, 2, 3))
local k = Q:key(a, 3)
local back = Q:keyRotation(k)
```

No C call: the integer is unpacked with shifts and masks, the arithmetic runs on doubles, and the
result is stored as a vector, a number, or a fresh packed integer. The lowering checks the
receiver's tag, the operands' integer tags and packed kinds, a non-finite weight and a fractional
flag value; a mismatch exits to the interpreter, whose bound method raises the same error. Only
single-result, fixed-arity call sites lower: `return Q:mul(a, b)` and `Q:keyRotation(Q:key(q, 3))`
run through the bound method, so bind the inner result to a local first. The type declares
`TagPolicy::Required` and `CompilerTypePolicy::Required`, so a plan that cannot give it a tag and
a compiler slot fails instead of leaving the path interpreted.

Measured per call inside native code: `rotate` 21 ns and `mul` 45 ns, against 99 ns and 150 ns
through the binder and 46 ns and 111 ns for an f32 quaternion userdata (the latter allocating);
`slerp` 86 ns against 167 ns and 214 ns; `key` plus `keyRotation` together 5 ns against 220 ns
through the binder.

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
| `toHex(data)`, `fromHex(text)` | Lower-case hex and back; whitespace between digits is ignored |
| `readCString(source, offset, fieldLength?)` | The text up to the first NUL and the offset after the terminator, or after the fixed-width field when `fieldLength` is given |
| `writeCString(target, offset, text, fieldLength?)` | Writes the text and a NUL, NUL-padded to the field; returns the offset after it |
| `readVarint`, `readSignedVarint(source, offset)` | LEB128: the value as an integer and the offset after it |
| `writeVarint`, `writeSignedVarint(target, offset, value)` | The offset after the encoding |

Every bounds failure names the call, the width and the offset: `bytes.readu32be: 4 bytes at
offset 30 past the end (length 32)`.

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
