+++
title = "l3i"
description = "The Luau runtime for DreamWeave: a Rust binder that owns its Luau 0.740 build, plans every VM before it exists, and lowers hot script calls to native code."

[taxonomies]
tags = ["Rust", "Luau", "Bindings", "Game development", "Native code"]

[extra]
sections = ["overview", "install", "compatibility", "releases", "credits"]
+++

A game engine that scripts in Luau needs three things from its binder. Scripts must reach engine
objects at the cost of a table lookup, not a registry walk. The engine must be able to run
thousands of script instances in sandboxes it can measure, limit and profile. And the surface a
script sees has to be declared once, typed, and impossible to drift from what the runtime does.

l3i is that binder. It is a Rust port of the OpenMW Luau binder, with the parts a multi-crate
engine needs on top: a planner that composes a VM from extensions before the VM exists, a packed
integer vocabulary for values that should never allocate, the network bridge in every runtime,
and Luau's native code generator with lowering hooks written in Rust.

{{ schematic(data_path="data/schematics/plan.json") }}

{% features() %}
- **Its own Luau.** Luau 0.740 is a git submodule compiled by `build.rs` with clang, lld and
  cross-language thin LTO, so the Rust thunks and the Luau API inline into each other. No `mlua`,
  no separate binding crate, nothing fetched at build time.
- **Arguments read from the value layout.** The typed binder reads stack slots straight from
  Luau's 16-byte `TValue`, and each runtime proves the mirror against the API once at creation. A
  bound `(f64, f64) -> f64` call retires 369 instructions against 277 for a bare `lua_CFunction`.
- **Tags, atoms and slots are plan data.** A `RuntimePlan` assigns userdata tags, Luau's 32
  compiler type slots, atoms and direct-access slots per VM, so the same Rust type can be tag 8
  in one runtime and untagged in another, and one handler serves both.
- **Typed by construction.** Every module member carries a Luau signature or is marked untyped.
  The plan renders the `.d.luau`, and the `analysis` feature type checks strict scripts against
  it in the crate's tests, so the declared API and the runtime cannot disagree.
- **Values that never allocate.** Rotations, animation keys, colours and clip rectangles travel
  as one Luau integer each, with a kind nibble checked on every read. A packed `rotate` runs in
  21 ns lowered to native code.
- **The network is not optional.** Every plan carries the `dream.net` bridge; the policy's
  capabilities decide what a script may do with it. Nothing calls into Luau from inside the
  transport.
- **Sandboxes the engine can account for.** OpenMW's prelude, per-script instances cloned from
  compiled templates, a watchdog on time and heap, memory categories, call scopes with self-time
  accounting, and a sampling profiler.
- **Lowering hooks in Rust.** With `jit`, hosts write Luau's userdata and vector lowering hooks
  against an `IrBuilder` C ABI. The quaternion, colour and vertex-writer paths compile to IR with
  no C call and exit to the interpreter only on malformed input.
{% end %}

## What a host writes

```rust
use l3i::Runtime;
use l3i::userdata::{Owned, Userdata, tagged};

struct Vec3 { x: f32, y: f32, z: f32 }

unsafe impl Userdata for Vec3 {
    const NAME: &'static str = "dreamweave.Vec3";
}

fn main() -> l3i::Result<()> {
    let runtime = Runtime::new()?;
    // The host picks the tag, per runtime, at registration.
    tagged::register::<Vec3>(&runtime, 10, |ty| {
        ty.property("x", |v: &Vec3| v.x)?;
        ty.method("length", |v: &Vec3| (v.x * v.x + v.y * v.y + v.z * v.z).sqrt())
    })?;
    let make = runtime.bind_function("dreamweave.vec3", |x: f32, y: f32, z: f32| Owned(Vec3 { x, y, z }))?;
    runtime.set_global("vec3", &make)?;
    runtime.exec("local v = vec3(3, 4, 0) assert(v.x == 3 and v:length() == 5)")?;
    Ok(())
}
```

That is the hand-assembled runtime. An engine composes one from extensions instead:

```rust
use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::quat::QuatExtension;
use l3i::raster::RasterExtension;

let plan = RuntimePlan::builder()
    .policy(RuntimePolicy::new().compat_global("@dream/quat", "quat"))
    .extension(QuatExtension)
    .extension(RasterExtension)
    .finalize()?;
// One immutable plan, any number of runtimes, each with its own tags and atoms.
let runtime = Runtime::from_plan(&plan)?;
std::fs::write("dream.d.luau", plan.type_definitions())?;
```

## What a script sees

```luau
--!strict
--!native
local quat = require("@dream/quat")
local raster = require("@dream/raster")

-- A rotation is one integer: 18 bits per component, smallest three, exact identity.
local spin = quat.axisAngle(vector.create(0, 0, 1), math.pi / 2)
local key = quat.key(spin, 5)          -- an AnimationKey: the rotation plus four flag bits
assert(quat.keyFlags(key) == 5)

-- Annotate the receiver and its methods lower to native code: no C call, no allocation.
local Q: dream_quat_Math = quat.math()
local forward = Q:rotate(spin, vector.create(1, 0, 0))

local C: dream_raster_Math = raster.math()
local tint = C:lerp(raster.rgb8(255, 240, 255), raster.rgba8(198, 160, 246, 255), 0.5)
```

Every name above is declared in the plan's definitions, so a strict script that misspells one, or
passes a key where a rotation is expected, fails in the checker before it runs. At runtime the same
mistake is a type error naming the kind: `Quaternion expected, got AnimationKey`.

## What it costs

Retired instructions per call, read from the CPU's counters on the pinned Luau 0.740, with the
loop subtracted. The rest of a call is Luau's own machinery.

| Per call | Instructions | Cycles |
|---|---:|---:|
| hand-written `lua_CFunction (f64, f64)` | 277 | 69 |
| bound `(f64, f64) -> f64` | 369 | 89 |
| typed direct namecall, the VM's leanest method path | 368 | 98 |
| planned method `() -> f64` | 431 | 107 |
| planned direct field | 78 | 12 |

Every scenario reports fewer than 0.005 cache misses, TLB misses and branch misses per call: the
whole path stays in L1 and predicts. [Compatibility and performance](@/docs/performance.md) has the
Criterion tables, the memory per runtime, and the toolchain measurements behind the build rule.

## Documentation

- **[Start here](@/docs/start-here.md)**: the toolchain, the first runtime, a bound function and a
  userdata type.
- **[Guide](@/docs/_index.md)**: the stack model, userdata, sandboxes, runtime options, direct
  access, extensions and plans, packed primitives, the built-in extensions, and native code.
- **[Rust API](@/docs/api/_index.md)**: every public module.
- **[Safety model](@/docs/safety.md)**: what unwinds, what aborts, what panics, and the deliberate
  divergences from the C++ binder.
