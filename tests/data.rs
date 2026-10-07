//! `@dream/data` through Luau: typed spans reduced, compared into selections, moved by index,
//! filled, combined, ordered and partitioned, with the semantics `DATA_PLANE.md` states.

use l3i::Runtime;
use l3i::data::DataExtension;
use l3i::extension::{RuntimePlan, RuntimePolicy};
#[cfg(feature = "jit")]
use l3i::source::LoadScope;

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/data", "data"))
        .extension(DataExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

fn exec(source: &str) {
    runtime().exec(source).unwrap();
}

#[test]
fn reductions_match_the_handwritten_luau_loop_for_every_kind() {
    exec(
        r"
        local kinds = {'u8', 'i8', 'u16', 'i16', 'u32', 'i32', 'f32', 'f64'}
        local sizes = {u8 = 1, i8 = 1, u16 = 2, i16 = 2, u32 = 4, i32 = 4, f32 = 4, f64 = 8}
        local n = 257
        for _, kind in kinds do
            local size = sizes[kind]
            local buf = buffer.create(n * size + 3)
            local write, read = buffer['write' .. kind], buffer['read' .. kind]
            for i = 0, n - 1 do
                local v = (i * 37) % 251 - (string.sub(kind, 1, 1) == 'u' and 0 or 125)
                if kind == 'f32' or kind == 'f64' then v = v / 7 end
                write(buf, 3 + i * size, v)
            end
            local total, least, greatest, lo, hi = 0, nil, nil, nil, nil
            for i = 0, n - 1 do
                local v = read(buf, 3 + i * size)
                total += v
                if least == nil or v < least then least, lo = v, i end
                if greatest == nil or v > greatest then greatest, hi = v, i end
            end
            assert(data.sum(buf, kind, 3, n) == total, kind .. ' sum')
            assert(data.min(buf, kind, 3, n) == least and data.max(buf, kind, 3, n) == greatest, kind .. ' extrema')
            assert(data.argmin(buf, kind, 3, n) == lo and data.argmax(buf, kind, 3, n) == hi, kind .. ' arg extrema')
            assert(data.sum(buf, kind, 3, 0) == 0 and data.min(buf, kind, 3, 0) == nil and data.argmax(buf, kind, 3, 0) == nil, kind .. ' empty')
        end
    ",
    );
}

#[test]
fn nan_ordering_and_bounds_follow_the_documented_rules() {
    exec(
        r"
        local buf = buffer.create(5 * 4)
        local nan = 0 / 0
        for i, v in {nan, 1, 0, 2, nan} do buffer.writef32(buf, (i - 1) * 4, v) end
        local least = data.min(buf, 'f32', 0, 5)
        assert(least ~= least, 'a leading NaN stays, as the < loop keeps it')
        assert(data.min(buf, 'f32', 4, 4) == 0 and data.max(buf, 'f32', 4, 4) == 2, 'later NaNs never replace')
        assert(data.argmin(buf, 'f32', 4, 4) == 1, 'positions are zero-based within the span')
        -- Every comparison with NaN is false except ne.
        assert(data.count(buf, 'f32', 0, 5, 'lt', 10) == 3 and data.count(buf, 'f32', 0, 5, 'ne', 10) == 5)
        assert(data.count(buf, 'f32', 0, 5, 'ge', 1) == 2 and data.count(buf, 'f32', 0, 5, 'eq', 0) == 1)
        local ok, message = pcall(data.sum, buf, 'f32', 4, 5)
        assert(not ok and string.find(message, 'exceeds the buffer length 20', 1, true), message)
        ok, message = pcall(data.sum, buf, 'f32', -4, 1)
        assert(not ok and string.find(message, 'negative offset', 1, true), message)
        ok, message = pcall(data.sum, buf, 'f32', 0, -1)
        assert(not ok and string.find(message, 'negative count', 1, true), message)
        ok, message = pcall(data.sum, buf, 'f16', 0, 1)
        assert(not ok and string.find(message, 'unknown element kind', 1, true), message)
        ok, message = pcall(data.count, buf, 'f32', 0, 5, 'between', 1)
        assert(not ok and string.find(message, 'unknown comparison', 1, true), message)
        assert(data.sum(buf, 'f32', 20, 0) == 0, 'an empty span at the very end is in bounds')
    ",
    );
}

#[test]
fn selections_compose_without_materializing_and_reuse_their_storage() {
    exec(
        r"
        local n = 130
        local buf = buffer.create(n * 4)
        for i = 0, n - 1 do buffer.writei32(buf, i * 4, i) end
        local big = data.compare(buf, 'i32', 0, n, 'ge', 100)
        local even = data.compare(buf, 'i32', 0, n, 'eq', 0)
        assert(big:len() == n and big:count() == 30 and not big:get(99) and big:get(100) and not big:get(1000))
        -- Reused output: the same selection object comes back, resized and overwritten.
        local scratch = data.selection(3)
        local reused = data.compare(buf, 'i32', 0, n, 'lt', 10, scratch)
        assert(reused == scratch and scratch:len() == n and scratch:count() == 10)
        local both = big:intersect(reused)
        assert(both:count() == 0 and not both:any())
        local either = big:union(reused, scratch)
        assert(either == scratch and either:count() == 40 and either:any() and not either:all())
        local complement = either:complement()
        assert(complement:count() == 90 and not complement:get(5) and complement:get(50))
        assert(complement:xor(either):all(), 'a selection and its complement cover everything')
        assert(big:difference(reused):count() == 0 and either:difference(big):count() == 10, 'difference: either is big with the first ten')
        assert(either:complement(either) == either and either:count() == 90, 'in-place complement')
        -- Indices: zero-based positions, into a caller buffer or a fresh exact one.
        local indices, count = big:indices()
        assert(count == 30 and buffer.len(indices) == 120 and buffer.readu32(indices, 0) == 100 and buffer.readu32(indices, 116) == 129)
        local out = buffer.create(4 + 30 * 4)
        local same, count2 = big:indices(out, 4)
        assert(same == out and count2 == 30 and buffer.readu32(out, 4) == 100)
        local ok, message = pcall(big.indices, big, buffer.create(8))
        assert(not ok and string.find(message, 'exceed the buffer length', 1, true), message)
        ok, message = pcall(big.intersect, big, data.selection(5))
        assert(not ok and string.find(message, 'different lengths', 1, true), message)
        either:clear()
        assert(either:count() == 0 and data.selection(0):all(), 'all over nothing is true')
    ",
    );
}

#[test]
fn gather_and_scatter_check_every_index_first_and_define_aliasing_sequentially() {
    exec(
        r"
        local src = buffer.create(6 * 2)
        for i = 0, 5 do buffer.writeu16(src, i * 2, (i + 1) * 100) end
        local order = buffer.create(4 * 4)
        for i, v in {5, 0, 5, 2} do buffer.writeu32(order, (i - 1) * 4, v) end
        local dst = buffer.create(2 + 4 * 2)
        assert(data.gather(src, 'u16', 0, 6, order, dst, 2) == 4)
        assert(buffer.readu16(dst, 2) == 600 and buffer.readu16(dst, 4) == 100 and buffer.readu16(dst, 6) == 600 and buffer.readu16(dst, 8) == 300)
        assert(data.gather(src, 'u16', 0, 6, order, dst, 2, 2) == 2, 'an explicit index count')
        -- Out of range: nothing is written.
        buffer.writeu32(order, 12, 6)
        buffer.fill(dst, 0, 0)
        local ok, message = pcall(data.gather, src, 'u16', 0, 6, order, dst, 2)
        assert(not ok and string.find(message, 'index 6 at position 3 is outside the source span of 6', 1, true), message)
        assert(buffer.readu16(dst, 2) == 0, 'the failed gather wrote nothing')
        -- Scatter.
        buffer.writeu32(order, 12, 1)
        local target = buffer.create(6 * 2)
        local four = buffer.create(4 * 2)
        for i = 0, 3 do buffer.writeu16(four, i * 2, i + 1) end
        assert(data.scatter(four, 'u16', 0, 4, order, target, 0, 6) == 4)
        assert(buffer.readu16(target, 10) == 3 and buffer.readu16(target, 0) == 2 and buffer.readu16(target, 2) == 4, 'later writes win on repeated indices')
        ok, message = pcall(data.scatter, four, 'u16', 0, 4, order, target, 0, 3)
        assert(not ok and string.find(message, 'outside the destination span of 3', 1, true), message)
        -- Aliasing: a gather from a buffer into itself reads then writes each element in order.
        local shift = buffer.create(4 * 4)
        for i = 0, 3 do buffer.writeu32(shift, i * 4, i + 1) end
        local rotate = buffer.create(4 * 4)
        for i, v in {1, 2, 3, 0} do buffer.writeu32(rotate, (i - 1) * 4, v) end
        data.gather(shift, 'u32', 0, 4, rotate, shift, 0)
        assert(buffer.readu32(shift, 0) == 2 and buffer.readu32(shift, 4) == 3 and buffer.readu32(shift, 8) == 4 and buffer.readu32(shift, 12) == 2)
    ",
    );
}

#[test]
fn fill_add_scale_and_clamp_convert_like_buffer_writes_and_allow_in_place() {
    exec(
        r"
        local buf = buffer.create(4 * 4)
        data.fill(buf, 'f32', 0, 4, 1.5)
        assert(buffer.readf32(buf, 12) == 1.5)
        data.fill(buf, 'u8', 0, 4, 300)
        assert(buffer.readu8(buf, 0) == 44 and buffer.readu8(buf, 3) == 44, 'wraps to the width like buffer.writeu8')
        data.fill(buf, 'i8', 0, 2, -1.9)
        assert(buffer.readi8(buf, 0) == -1, 'truncates toward zero')
        data.fill(buf, 'u8', 0, 1, 0 / 0)
        assert(buffer.readu8(buf, 0) == 0, 'NaN writes zero')
        local a, b = buffer.create(3 * 8), buffer.create(3 * 8)
        for i = 0, 2 do buffer.writef64(a, i * 8, i + 0.25) buffer.writef64(b, i * 8, 10 * i) end
        data.add(a, 'f64', 0, b, 0, a, 0, 3)
        assert(buffer.readf64(a, 0) == 0.25 and buffer.readf64(a, 8) == 11.25 and buffer.readf64(a, 16) == 22.25, 'in-place add')
        data.scale(a, 'f64', 8, 2, 2, b, 0)
        assert(buffer.readf64(b, 0) == 22.5 and buffer.readf64(b, 8) == 44.5 and buffer.readf64(b, 16) == 20, 'scale into another span')
        data.clamp(a, 'f64', 0, 3, 1, 20, a, 0)
        assert(buffer.readf64(a, 0) == 1 and buffer.readf64(a, 8) == 11.25 and buffer.readf64(a, 16) == 20)
        local ok, message = pcall(data.clamp, a, 'f64', 0, 3, 5, 1, a, 0)
        assert(not ok and string.find(message, 'is not at most high', 1, true), message)
        local half = buffer.create(4)
        data.fill(half, 'f32', 0, 1, 0.1)
        assert(buffer.readf32(half, 0) == buffer.readf32(half, 0) and buffer.readf32(half, 0) ~= 0.1, 'f32 rounds on write')
        ok, message = pcall(data.add, a, 'f64', 0, b, 8, a, 0, 3)
        assert(not ok and string.find(message, 'exceeds the buffer length', 1, true), message)
    ",
    );
}

#[test]
fn argsort_is_stable_with_nan_last_and_partition_keeps_order() {
    exec(
        r"
        local keys = buffer.create(8 * 4)
        local nan = 0 / 0
        for i, v in {3, 1, nan, 2, 1, 3, nan, 0} do buffer.writef32(keys, (i - 1) * 4, v) end
        local order = buffer.create(8 * 4)
        assert(data.argsort(keys, 'f32', 0, 8, order) == 8)
        local got = {}
        for i = 0, 7 do got[i + 1] = buffer.readu32(order, i * 4) end
        assert(table.concat(got, ',') == '7,1,4,3,0,5,2,6', table.concat(got, ','))
        -- Into an offset, over a sub-span.
        local shifted = buffer.create(4 + 3 * 4)
        assert(data.argsort(keys, 'f32', 0, 3, shifted, 4) == 3)
        assert(buffer.readu32(shifted, 4) == 1 and buffer.readu32(shifted, 8) == 0 and buffer.readu32(shifted, 12) == 2)
        local ok, message = pcall(data.argsort, keys, 'f32', 0, 8, buffer.create(8))
        assert(not ok and string.find(message, 'exceed the buffer length', 1, true), message)
        -- Partition: accepted positions first, each half in original order.
        local parts = buffer.create(8 * 4)
        assert(data.partition(keys, 'f32', 0, 8, 'ge', 2, parts) == 3)
        got = {}
        for i = 0, 7 do got[i + 1] = buffer.readu32(parts, i * 4) end
        assert(table.concat(got, ',') == '0,3,5,1,2,4,6,7', table.concat(got, ','))
        -- Large tied-key input stays stable.
        local n = 5000
        local ties = buffer.create(n * 4)
        for i = 0, n - 1 do buffer.writei32(ties, i * 4, i % 7) end
        local perm = buffer.create(n * 4)
        data.argsort(ties, 'i32', 0, n, perm)
        local previousKey, previousIndex = -1, -1
        for i = 0, n - 1 do
            local index = buffer.readu32(perm, i * 4)
            local key = buffer.readi32(ties, index * 4)
            assert(key > previousKey or (key == previousKey and index > previousIndex), 'stable ascending')
            previousKey, previousIndex = key, index
        end
    ",
    );
}

#[test]
fn the_module_is_also_published_for_lowered_pipelines() {
    exec(
        r"
        assert(__l3i_data == data and type(__l3i_data.sum) == 'function')
        assert(__l3i_data == require('@dream/data'))
    ",
    );
}

/// The same JSL source run with and without the extension installed; both runtimes must agree
/// on every value and every error, since the recognized path is the scalar loop's equal.
fn agree(source: &str) {
    let plain = Runtime::new().unwrap();
    let with_data = runtime();
    let script = format!("return function() local report = {{}} {source} return table.concat(report, '|') end");
    let run = |runtime: &Runtime| -> l3i::error::Result<String> {
        let function = runtime.load_function(&script)?;
        function.invoke::<String, _>(&runtime.stack(), ())
    };
    let expected = run(&plain);
    let actual = run(&with_data);
    match (expected, actual) {
        (Ok(expected), Ok(actual)) => assert_eq!(expected, actual, "{source}"),
        (Err(expected), Err(actual)) => assert_eq!(expected.to_string(), actual.to_string(), "{source}"),
        (expected, actual) => panic!("{source}: plain {expected:?} vs data {actual:?}"),
    }
}

#[test]
fn recognized_buffer_pipelines_agree_with_the_scalar_loop_and_call_the_data_plane_once() {
    agree(
        r"
        local buf = buffer.create(256)
        for i = 0, 255 do buffer.writeu8(buf, i, (i * 37) % 256) end
        local function note(v) table.insert(report, tostring(v)) end
        note(sum[for x in buf[1:256] => x])
        note(sum[for x in buf[10:20] => x])
        note(sum[for x in buf[300:400] => x])
        note(sum[for x in buf[20:10] => x])
        note(sum[for x in buf[-5:3] => x])
        note(#[for x in buf[1:256] => x])
        note(#[for x in buf[1:256] if x > 127 => x])
        note(#[for x in buf[1:256] if x >= 127 => x])
        note(#[for x in buf[1:256] if x == 0 => x])
        note(#[for x in buf[1:256] if x ~= 0 => x])
        note(#[for x in buf[1:256] if x < 1e1 => x])
        note(#[for x in buf[1:256] if x <= 0x10 => x])
        note(#[for x in buf[1:256] if x > -1 => x])
        note(#[for x in buf[5:5] if x > 0 => x])
        note(min[for x in buf[1:256] => x])
        note(max[for x in buf[3:9] => x])
        note(min[for x in buf[200:300] => x])
    ",
    );
    for source in [
        "return min[for x in buffer.create(4)[5:9] => x]",
        "return max[for x in buffer.create(0)[1:1] => x]",
        "return sum[for x in buffer.create(4)[1.5:2] => x]",
        "return sum[for x in buffer.create(4)['a':2] => x]",
        "return #[for x in 5[1:2] => x]",
    ] {
        agree(&format!(
            "local ok, message = pcall(function() {source} end) table.insert(report, tostring(ok)) table.insert(report, tostring(message))"
        ));
    }
    // Each chunk snapshots the alias global when it loads and takes one byte receiver from it;
    // every recognized call runs on that receiver. A chunk loaded after the alias is wrapped
    // shows that single access; one loaded after it is removed takes the scalar loop with the
    // same results.
    let runtime = runtime();
    runtime
        .exec(
            r"
            calls = 0
            local real = __l3i_data
            __l3i_data = setmetatable({}, {__index = function(_, name)
                return function(...) calls += 1 return real[name](...) end
            end})
            buf = buffer.create(1000)
            for i = 0, 999 do buffer.writeu8(buf, i, i % 7) end
        ",
        )
        .unwrap();
    let pipelines = "return sum[for x in buf[a:b] => x], #[for x in buf[a:b] if x > 3 => x], min[for x in buf[a:b] => x], #[for x in buf[a:b] => x]";
    runtime
        .exec(&format!("local a, b = 1, 1000 local total, hot, least, n = (function() {pipelines} end)() assert(total == 2997 and hot == 428 and least == 0 and n == 1000, tostring(total) .. ' ' .. tostring(hot)) assert(calls == 1, 'one receiver per chunk carries every recognized call: ' .. calls)"))
        .unwrap();
    runtime.exec("__l3i_data = nil").unwrap(); // A chunk snapshots the alias before its own statements run.
    runtime
        .exec(&format!("local a, b = 1, 1000 local total, hot, least, n = (function() {pipelines} end)() assert(total == 2997 and hot == 428 and least == 0 and n == 1000) assert(calls == 1, 'no alias, no receiver')"))
        .unwrap();
}

#[test]
fn kind_receiver_methods_match_the_module_functions() {
    exec(
        r"
        local n = 300
        local buf = buffer.create(n * 4 + 8)
        for i = 0, n - 1 do buffer.writef32(buf, 8 + i * 4, ((i * 37) % 251) / 7 - 10) end
        local F32 = data.kind('f32')
        assert(F32:sum(buf, 8, n) == data.sum(buf, 'f32', 8, n))
        assert(F32:min(buf, 8, n) == data.min(buf, 'f32', 8, n) and F32:max(buf, 8, n) == data.max(buf, 'f32', 8, n))
        assert(F32:min(buf, 8, 0) == nil and F32:sum(buf, 8, 0) == 0)
        assert(F32:countGt(buf, 8, n, 0) == data.count(buf, 'f32', 8, n, 'gt', 0))
        assert(F32:countNe(buf, 8, n, 0 / 0) == n and F32:countEq(buf, 8, n, 0 / 0) == 0)
        local ok, message = pcall(F32.sum, F32, buf, 8, n + 1)
        assert(not ok and string.find(message, 'Kind:sum: span of 301 f32', 1, true), message)
        ok, message = pcall(data.kind, 'f16')
        assert(not ok and string.find(message, 'unknown element kind', 1, true), message)
    ",
    );
}

#[cfg(feature = "jit")]
#[test]
fn kind_receiver_reductions_lower_to_native_loops_with_the_bound_semantics() {
    use l3i::data::lowering::lowered_sites;
    use l3i::extension::NativeCodePolicy;
    use l3i::native_code::NativeCodeMode;

    let policy = RuntimePolicy::new().compat_global("@dream/data", "data").native_code(NativeCodePolicy {
        mode: NativeCodeMode::Eager,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    let plan = RuntimePlan::builder().policy(policy).extension(DataExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let generator = runtime.native_code().expect("built with native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    // A safe environment, or every global access would exit native code to the interpreter.
    runtime.sandbox_globals();
    let before = lowered_sites();
    // `Runtime::exec` never compiles natively; a chunk loaded through `LoadScope` does under
    // Eager, and `--!native` makes the compiler emit the type info the hook needs.
    let stack = runtime.stack();
    let chunk = stack
        .load_source("=kinds", r"--!native
            local kinds = {'u8', 'i8', 'u16', 'i16', 'u32', 'i32', 'f32', 'f64'}
            local sizes = {u8 = 1, i8 = 1, u16 = 2, i16 = 2, u32 = 4, i32 = 4, f32 = 4, f64 = 8}
            local n = 1000
            local nan = 0 / 0
            for _, name in kinds do
                local size = sizes[name]
                local buf = buffer.create(n * size + 5)
                local write = buffer['write' .. name]
                for i = 0, n - 1 do
                    local v = (i * 37) % 251 - (string.sub(name, 1, 1) == 'u' and 0 or 125)
                    if name == 'f32' or name == 'f64' then v = v / 7 end
                    write(buf, 5 + i * size, v)
                end
                local K: dream_data_Kind = data.kind(name)
                -- The lowered calls against the module functions (the bound path), per kind.
                assert(K:sum(buf, 5, n) == data.sum(buf, name, 5, n), name .. ' sum')
                assert(K:sum(buf, 5 + 10 * size, 0) == 0, name .. ' empty sum')
                if name == 'f32' or name == 'f64' then
                    write(buf, 5 + 400 * size, nan)
                    local total = K:sum(buf, 5, n)
                    assert(total ~= total and data.sum(buf, name, 5, n) ~= total, name .. ' NaN sum')
                end
                local least, greatest = K:min(buf, 5, n), K:max(buf, 5, n)
                assert(least == data.min(buf, name, 5, n) and greatest == data.max(buf, name, 5, n), name .. ' extrema')
                assert(K:min(buf, 5, 0) == nil and K:max(buf, 5, 0) == nil, name .. ' empty extrema')
                for _, op in {'Eq', 'Ne', 'Lt', 'Le', 'Gt', 'Ge'} do
                    local method = 'count' .. op
                    local expected = data.count(buf, name, 5, n, string.lower(op), 3)
                    assert(K[method](K, buf, 5, n, 3) == expected, name .. ' ' .. method)
                    assert(K[method](K, buf, 5, n, nan) == data.count(buf, name, 5, n, string.lower(op), nan), name .. ' ' .. method .. ' NaN')
                end
                if name == 'f32' or name == 'f64' then
                    -- A leading NaN stays in min/max on both paths; a later one never replaces.
                    assert(K:min(buf, 5 + 400 * size, 10) ~= K:min(buf, 5 + 400 * size, 10), name .. ' leading NaN')
                    assert(K:min(buf, 5, 500) == data.min(buf, name, 5, 500), name .. ' later NaN')
                end
                -- Misuse leaves native code for the binder, whose errors are the contract.
                local ok, message = pcall(function() return K:sum(buf, 5, n + 1) end)
                assert(not ok and string.find(message, 'Kind:sum: span of', 1, true), name .. ': ' .. tostring(message))
                ok, message = pcall(function() return K:sum(buf, 2.5, 1) end)
                assert(not ok, name .. ' fractional offset')
                ok, message = pcall(function() return K:sum('text', 0, 1) end)
                assert(not ok, name .. ' not a buffer')
                ok, message = pcall(function() return K:sum(buf, -1, 1) end)
                assert(not ok and string.find(message, 'negative offset', 1, true), name .. ': ' .. tostring(message))
            end
        ", &runtime.compile_options())
        .unwrap();
    let stats_before = generator.execution_stats(&stack);
    chunk.invoke::<(), _>(&stack, ()).unwrap();
    let stats_after = generator.execution_stats(&stack);
    assert!(lowered_sites() > before, "the annotated receiver's calls were lowered");
    // Eight kinds, each thousands of loop iterations in native blocks (an interpreted run
    // records a few dozen); the slow paths exit to the binder.
    let blocks = stats_after.regular_blocks_executed - stats_before.regular_blocks_executed;
    assert!(blocks > 10_000, "native blocks executed: {blocks}");
}
