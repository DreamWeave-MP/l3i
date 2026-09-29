//! `@dream/bytes`: the per-call cost of the reads `buffer` lacks through the module, through
//! the receiver, and lowered to native code, against Luau's own `buffer.readu32` plus
//! `bit32.byteswap`; and the throughput of searching, digests, codecs and text decoding over
//! a megabyte.

#![allow(clippy::semicolon_if_nothing_returned, clippy::missing_panics_doc)]

use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use l3i::Runtime;
use l3i::bytes::BytesExtension;
use l3i::extension::{RuntimePlan, RuntimePolicy};

const CALLS: u64 = 1000;
const MIB: u64 = 1 << 20;

fn plain_runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/bytes", "bytes"))
        .extension(BytesExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

fn per_call(c: &mut Criterion) {
    let runtime = plain_runtime();
    let cases = [
        ("buffer.readu32 + bit32.byteswap (Luau builtin)", "v = bit32.byteswap(buffer.readu32(buf, i % 60))"),
        ("readu32be (module)", "v = bytes.readu32be(buf, i % 60)"),
        ("readu32be (receiver, interpreted)", "v = B:readu32be(buf, i % 60)"),
        ("readi64be (module)", "w = bytes.readi64be(buf, i % 56)"),
        ("readf16 (module)", "v = bytes.readf16(buf, i % 62)"),
        ("writeu16be (module)", "bytes.writeu16be(buf, i % 62, i)"),
        ("readCString (module)", "s = bytes.readCString(buf, 0, 16)"),
        ("readVarint (module)", "w = bytes.readVarint(buf, 0)"),
        ("find, 64 byte haystack", "v = bytes.find(buf, needle)"),
    ];
    let mut group = c.benchmark_group("bytes_per_call");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let function = runtime
            .load_function(&format!(
                "return function() local bytes = bytes local B = bytes.math() \
                 local buf = buffer.create(64) buffer.writestring(buf, 0, 'record name here') \
                 local needle = 'here' local v, w, s = 0, 0i, '' \
                 for i = 1, {CALLS} do {body} end return v end"
            ))
            .unwrap();
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

#[cfg(feature = "jit")]
fn lowered(c: &mut Criterion) {
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};
    use l3i::value::Function;

    let policy = RuntimePolicy::new()
        .compat_global("@dream/bytes", "bytes")
        .native_code(NativeCodePolicy { mode: NativeCodeMode::Eager, ..NativeCodePolicy::default() });
    let plan = RuntimePlan::builder().policy(policy).extension(BytesExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    if !runtime.native_code().is_some_and(l3i::native_code::NativeCodeGen::is_available) {
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "b", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let cases = [
        ("buffer.readu32 + bit32.byteswap (native builtin)", "v = bit32.byteswap(buffer.readu32(buf, i % 60))"),
        ("readu32be (native lowered)", "v = B:readu32be(buf, i % 60)"),
        ("readi16be (native lowered)", "v = B:readi16be(buf, i % 60)"),
        ("readu24be (native lowered)", "v = B:readu24be(buf, i % 60)"),
        ("readi64be (native lowered)", "w = B:readi64be(buf, i % 56)"),
        ("writeu16be (native lowered)", "B:writeu16be(buf, i % 62, i)"),
        ("writeu32be (native lowered)", "B:writeu32be(buf, i % 60, i)"),
        ("readu32be (native, module)", "v = bytes.readu32be(buf, i % 60)"),
    ];
    let mut group = c.benchmark_group("bytes_lowered");
    group.throughput(Throughput::Elements(CALLS));
    for (name, body) in cases {
        let template = sandbox
            .load_template(
                &runtime,
                "bench.lua",
                &format!(
                    "--!native\nlocal B: dream_bytes_Math = bytes.math()\n\
                     return function() local buf = buffer.create(64) local v, w = 0, 0i \
                     for i = 1, {CALLS} do {body} end return v end"
                ),
            )
            .unwrap();
        let results =
            sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
        let function = Function::from_value(results.into_iter().next().unwrap()).unwrap();
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

#[cfg(not(feature = "jit"))]
fn lowered(_: &mut Criterion) {}

/// One call over a megabyte: what the boundary costs against the work itself.
fn throughput(c: &mut Criterion) {
    let runtime = plain_runtime();
    let mut cases: Vec<(&str, &str)> = vec![
        ("find, absent needle", "v = bytes.find(big, 'needle') or 0"),
        ("count, byte", "v = bytes.count(big, 'z')"),
        ("equals", "v = bytes.equals(big, copy) and 1 or 0"),
        ("slice, whole", "b = bytes.slice(big, 0, buffer.len(big))"),
    ];
    if cfg!(feature = "bytes-digests") {
        cases.extend([
            ("crc32", "v = bytes.crc32(big)"),
            ("xxh3", "w = bytes.xxh3(big)"),
            ("sha256", "s = bytes.sha256(big)"),
            ("blake3", "s = bytes.blake3(big)"),
        ]);
    }
    if cfg!(feature = "bytes-codecs") {
        cases.extend([
            ("inflate zlib", "b = bytes.inflate(packed)"),
            ("deflate zlib level 1", "b = bytes.deflate(big, { level = 1 })"),
            ("lz4 block decompress", "b = bytes.lz4Decompress(lz4, buffer.len(big))"),
            ("lz4 block compress", "b = bytes.lz4Compress(big)"),
        ]);
    }
    if cfg!(feature = "bytes-text") {
        cases.extend([
            ("decode windows-1252", "s = bytes.decode(big, 'windows-1252')"),
            ("isUtf8", "v = bytes.isUtf8(big) and 1 or 0"),
        ]);
    }
    let setup = "local big = buffer.create(1048576) \
                 local text = string.rep('The quick brown fox jumps over the lazy dog. ', 23302) \
                 buffer.writestring(big, 0, text, 1048576) \
                 local copy = buffer.create(1048576) buffer.copy(copy, 0, big) \
                 local packed = bytes.deflate and bytes.deflate(big) or nil \
                 local lz4 = bytes.lz4Compress and bytes.lz4Compress(big) or nil \
                 local v, w, s, b = 0, 0i, '', nil";
    let mut group = c.benchmark_group("bytes_throughput");
    group.throughput(Throughput::Bytes(MIB));
    for (name, body) in cases {
        let function = runtime
            .load_function(&format!("local bytes = bytes {setup} return function() {body} return v end"))
            .unwrap();
        let stack = runtime.stack();
        group.bench_function(name, |b| b.iter(|| function.invoke::<f64, _>(&stack, ()).unwrap()));
    }
    group.finish();
}

fn configure() -> Criterion {
    Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3))
}

criterion_group! { name = benches; config = configure(); targets = per_call, lowered, throughput }
criterion_main!(benches);
