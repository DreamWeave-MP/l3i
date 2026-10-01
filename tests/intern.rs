//! `@dream/intern` through Luau: equivalent spellings are one token whether they arrive as
//! strings or buffer spans, tokens are dense numbers that key tables, resolve gives the first
//! spelling back, and bad input fails with the call's name.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::intern::InternExtension;

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/intern", "intern"))
        .extension(InternExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn spellings_and_spans_are_one_integer_identity() {
    runtime()
        .exec(
            "local ids = intern.new('ascii-nocase') \
             local id = ids:intern('cAiUs CoSaDeS') \
             assert(typeof(id) == 'number' and id == 1, typeof(id)) \
             assert(id == ids:intern('Caius Cosades') and id == ids:intern('CAIUS COSADES'), 'case folds') \
             local record = buffer.fromstring('NAME\\0caius cosades\\0FLAG') \
             assert(ids:intern(record, 5, 13) == id, 'buffer span') \
             assert(ids:intern('xx CAIUS COSADES xx', 3, 13) == id, 'string span') \
             assert(ids:resolve(id) == 'cAiUs CoSaDeS', 'first spelling kept') \
             assert(ids:find('fargoth') == nil and ids:count() == 1, 'find never adds') \
             local fargoth = ids:intern('Fargoth') \
             assert(fargoth ~= id and ids:find('FARGOTH') == fargoth and ids:count() == 2, 'distinct') \
             local records = {} records[id] = 'npc' \
             assert(records[ids:intern('caius cosades')] == 'npc' and #records == 1, 'dense keys, the array part') \
             assert(ids:policy() == 'ascii-nocase' and ids:memory() > 0, 'introspection') \
             local internId = ids:interner() \
             assert(internId('CAIUS COSADES') == id and internId(record, 5, 13) == id and internId('Fargoth') == fargoth, 'the bound form is the method') \
             assert(internId('Vivec') == 3 and ids:count() == 3, 'and adds to the same pool') \
             local exact = intern.new() \
             assert(exact:policy() == 'exact' and exact:intern('A') ~= exact:intern('a'), 'exact by default') \
             assert(exact:intern('A') == 1 and exact:intern('a') == 2, 'tokens are pool-relative and dense') \
             assert(exact:resolve(2) == 'a' and exact:resolve(2i) == 'a', 'resolve takes a number or an integer')",
        )
        .unwrap();
}

#[test]
fn rules_normalize_keys_before_they_compare() {
    runtime()
        .exec(
            "local paths = intern.new({ nocase = true, replace = { ['\\\\'] = '/' }, collapse = '/', trimStart = '/' }) \
             local id = paths:intern('\\\\Meshes\\\\X//Rock.NIF') \
             assert(paths:resolve(id) == 'meshes/x/rock.nif', 'the normal form is kept') \
             for _, spelling in { 'meshes/x/rock.nif', 'MESHES/X/ROCK.NIF', '/meshes//x///rock.nif', 'Meshes\\\\x\\\\Rock.nif' } do \
               assert(paths:intern(spelling) == id and paths:find(spelling) == id, spelling) \
             end \
             assert(paths:intern('meshes/x/rock.nif/') ~= id, 'a trailing separator is kept') \
             assert(paths:policy() == 'rules' and paths:count() == 2) \
             local ok, err = pcall(intern.new, { replace = { a = 'b', c = 'd', e = 'f' } }) assert(not ok and err:find('at most 2'), err) \
             ok, err = pcall(intern.new, { collapse = '//' }) assert(not ok and err:find('one byte'), err) \
             ok, err = pcall(intern.new, { colapse = '/' }) assert(not ok and err:find('colapse'), err) \
             ok, err = pcall(intern.new, 'rules') assert(not ok and err:find('rules table'), err) \
             ok, err = pcall(intern.new, { replace = { ['\\0'] = 'x' } }) assert(not ok and err:find('NUL'), err)",
        )
        .unwrap();
}

#[test]
fn a_bound_interner_keeps_its_pool_across_a_collection() {
    let runtime = runtime();
    runtime.exec("orphan = intern.new():interner() assert(orphan('x') == 1)").unwrap();
    runtime.gc(l3i::memory::GcControl::Collect);
    runtime.exec("assert(orphan('x') == 1 and orphan('y') == 2)").unwrap();
}

#[test]
fn bad_input_names_the_call() {
    runtime()
        .exec(
            "local ids = intern.new('exact') \
             local ok, err = pcall(intern.new, 'unicode') assert(not ok and err:find('unknown policy'), err) \
             ok, err = pcall(ids.intern, ids, 'abc', 2, 5) assert(not ok and err:find('Pool:intern: 5 bytes at offset 2'), err) \
             ok, err = pcall(ids.intern, ids, 'abc', -1) assert(not ok and err:find('negative offset'), err) \
             ok, err = pcall(ids.resolve, ids, 1) assert(not ok and err:find('not an identity'), err) \
             ok, err = pcall(ids.resolve, ids, 1.5) assert(not ok, 'a fraction is not a token') \
             ok, err = pcall(ids.intern, ids, 42) assert(not ok, 'not bytes')",
        )
        .unwrap();
}

/// Lowered `pool:intern` and `pool:find` against the binder (`pool.intern(pool, ...)` is a call,
/// not a namecall, so it is never lowered): every spelling, length and fallback gives the same
/// answer both ways, and a loop over duplicates never reaches the binder.
#[cfg(feature = "jit")]
#[test]
fn lowered_lookups_match_the_binder_and_stay_native_on_duplicates() {
    use l3i::extension::NativeCodePolicy;
    use l3i::intern::lowering::{binder_calls, lowered_sites};
    use l3i::native_code::{NativeCodeMode, NativeCodeStatus};
    use l3i::runtime::{CallContext, MemoryCategory};
    use l3i::sandbox::{InstanceSpec, SandboxOptions};

    let policy = RuntimePolicy::new().compat_global("@dream/intern", "intern").native_code(NativeCodePolicy {
        mode: NativeCodeMode::Eager,
        record_counters: true,
        ..NativeCodePolicy::default()
    });
    let plan = RuntimePlan::builder().policy(policy).extension(InternExtension).finalize().unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    let generator = runtime.native_code().expect("built with native code");
    if !generator.is_available() {
        eprintln!("no Luau code generator on this platform; skipping");
        return;
    }
    let sandbox = runtime
        .sandbox(|_| {}, SandboxOptions { compile_options: runtime.compile_options(), ..SandboxOptions::default() })
        .unwrap();
    let before = lowered_sites();
    let template = sandbox
        .load_template(
            &runtime,
            "intern.lua",
            r"--!native
local failures = {}

local function expect(ok: boolean, what: string)
  if not ok then
    table.insert(failures, what)
  end
end

local function lowered(ids: dream_intern_Pool, source: any, offset: any, length: any): any
  return ids:intern(source, offset, length)
end

local function lowered1(ids: dream_intern_Pool, source: any): any
  return ids:intern(source)
end

local function lowered2(ids: dream_intern_Pool, source: any, offset: any): any
  return ids:intern(source, offset)
end

local function found(ids: dream_intern_Pool, source: any, offset: any, length: any): any
  return ids:find(source, offset, length)
end

local function bound(ids: dream_intern_Pool, source: any, offset: any, length: any): any
  return ids.intern(ids, source, offset, length)
end

local function boundFind(ids: dream_intern_Pool, source: any, offset: any, length: any): any
  return ids.find(ids, source, offset, length)
end

-- Every length from 0 to 40, mixed case, a non-ASCII byte now and then.
local letters = 'aBcDeFgHiJkLmNoPqRsTuVwXyZ_0123456789@[`{\195/\\'
local words = {}
for length = 0, 40 do
  for variant = 0, 2 do
    local parts = {}
    for i = 1, length do
      local at = ((i * 7 + length * 3 + variant * 11) % #letters) + 1
      table.insert(parts, string.sub(letters, at, at))
    end
    table.insert(words, table.concat(parts))
  end
end

local function flip(word: string): string
  return (string.gsub(word, '%a', function(c)
    return if c == string.upper(c) then string.lower(c) else string.upper(c)
  end))
end

local Rules = { nocase = true, replace = { ['\\'] = '/', ['@'] = '_' }, collapse = '/', trimStart = '/', trimEnd = '_' }

for _, policy in { 'exact', 'ascii-nocase', 'rules' } do
  local spec = if policy == 'rules' then Rules else policy
  local a: dream_intern_Pool = intern.new(spec)
  local b: dream_intern_Pool = intern.new(spec)

  -- First sight: the lowered site misses and inserts through the binder; same tokens as a
  -- pool fed only through the binder. The pool grows across many rehashes on the way.
  for _, word in words do
    expect(lowered1(a, word) == bound(b, word), policy .. ' first ' .. word)
  end

  -- Duplicates, as strings, flipped case, and buffer spans.
  local text = buffer.fromstring('xyz' .. table.concat(words) .. 'xyz')
  local at = 3
  for _, word in words do
    local token = bound(b, word)
    expect(lowered1(a, word) == token, policy .. ' again ' .. word)
    expect(found(a, word) == token, policy .. ' find ' .. word)
    expect(lowered(a, text, at, #word) == token, policy .. ' span ' .. word)
    expect(lowered(a, 'xyz' .. word .. 'xyz', 3, #word) == token, policy .. ' string span ' .. word)
    local flipped = flip(word)
    expect(lowered1(a, flipped) == bound(b, flipped), policy .. ' flipped ' .. flipped)
    local slashed = '//' .. string.gsub(flipped, '/', '\\') .. '\\'
    expect(lowered1(a, slashed) == bound(b, slashed), policy .. ' slashed ' .. slashed)
    expect(found(a, slashed) == boundFind(b, slashed), policy .. ' find slashed ' .. slashed)
    expect(found(a, flipped .. '!') == boundFind(b, flipped .. '!'), policy .. ' absent ' .. flipped)
    at += #word
  end

  -- Offset only: the rest of the source.
  expect(lowered2(a, 'xxCaius', 2) == bound(b, 'xxCaius', 2), policy .. ' offset only')
  expect(lowered2(a, 'xx', 2) == bound(b, 'xx', 2), policy .. ' offset at the end')

  -- A miss followed by a hit, then a table grown past several rehashes between lookups.
  local fresh = lowered1(a, 'Fargoth the first')
  expect(lowered1(a, 'Fargoth the first') == fresh and a:count() == b:count() + 1, policy .. ' miss then hit')
  for i = 1, 2000 do
    lowered1(a, 'filler_' .. i)
  end
  expect(lowered1(a, 'Fargoth the first') == fresh, policy .. ' after growth')
  expect(lowered1(a, words[40]) == bound(b, words[40]), policy .. ' old token after growth')

  -- Every fallback raises what the binder raises.
  local cases = {
    { 42 },
    { nil },
    { 'abc', 2, 5 },
    { 'abc', -1 },
    { 'abc', 1.5 },
    { 'abc', 0, -1 },
    { 'abc', 4 },
    { text, buffer.len(text), 1 },
    { 'abc', 'x' },
  }
  for i, case in cases do
    local okL, errL = pcall(lowered, a, case[1], case[2], case[3])
    local okB, errB = pcall(bound, a, case[1], case[2], case[3])
    expect(okL == okB and (okL and errL == errB or string.match(errL, ':%d+: (.*)') == string.match(errB, ':%d+: (.*)')), policy .. ' fallback ' .. i .. ' ' .. tostring(errL) .. ' / ' .. tostring(errB))
  end

  -- An integer offset is a number the lowering does not take: the slow path, same answer.
  expect(lowered(a, 'xxCaius', 2i, 5i) == bound(b, 'xxCaius', 2, 5), policy .. ' integer offset')
end

return table.concat(failures, '\n')
",
        )
        .unwrap();
    let native = template.native_code().expect("compiled");
    assert_eq!(native.status, NativeCodeStatus::Success, "{native:?}");
    assert!(lowered_sites() - before >= 5, "the namecall sites lowered");

    // The same lowering compiles for A64, Luau's assertions on (no device needed to emit it).
    let function = runtime
        .load_function(
            "--!native\nreturn function(ids: dream_intern_Pool, t: buffer, s: string) \
             local a = ids:intern(t, 0, 3) local b = ids:intern(s) local c = ids:find(t, 1) return a, b, c end",
        )
        .unwrap();
    let before_a64 = lowered_sites();
    let a64 = runtime
        .stack()
        .with_frame(|frame| {
            let view = function.push_to(frame)?;
            generator.assembly(
                frame,
                view.index(),
                l3i::native_code::AssemblyOptions {
                    target: l3i::native_code::AssemblyTarget::A64,
                    ..l3i::native_code::AssemblyOptions::default()
                },
            )
        })
        .unwrap();
    assert!(a64.contains("ldr"), "A64 code for the lowered sites");
    assert_eq!(lowered_sites() - before_a64, 3, "all three sites lowered for A64");
    let loader = runtime.load_function("return function(name) error('module ' .. name .. ' not found') end").unwrap();
    let instance = sandbox
        .new_instance(&runtime, &InstanceSpec { name: "i", packages: &[], hidden_data: None, loader: &loader })
        .unwrap();
    let results =
        sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let failures: String = results[0].push_to(&runtime.stack().frame()).unwrap().read().unwrap();
    let failures: Vec<&str> = failures.lines().collect();
    assert!(failures.is_empty(), "{} differences: {:#?}", failures.len(), &failures[..failures.len().min(20)]);

    // Duplicates in a native loop never reach the binder.
    let template = sandbox
        .load_template(
            &runtime,
            "duplicates.lua",
            r"--!native
local ids: dream_intern_Pool = intern.new('ascii-nocase')
local text = buffer.fromstring('Caius_Cosades_x1Fargoth')
ids:intern(text, 0, 16)
ids:intern(text, 16, 7)
local paths: dream_intern_Pool = intern.new({ nocase = true, replace = { ['\\'] = '/' }, collapse = '/', trimStart = '/' })
paths:intern('meshes/x/rock_01.nif')
paths:intern('Caius_Cosades_x1')
paths:intern('Textures\\A.dds')
return function()
  local sum = 0
  for i = 1, 1000 do
    sum += ids:intern(text, 0, 16) + ids:intern(text, 16, 7) + ids:intern('caius_cosades_X1')
    sum += paths:intern('Meshes\\X\\Rock_01.NIF') + paths:intern(text, 0, 16) + paths:find('textures/a.dds')
  end
  return sum
end
",
        )
        .unwrap();
    let results =
        sandbox.run(&runtime, &template, &instance, CallContext { id: 1, category: MemoryCategory(0) }).unwrap();
    let function = l3i::value::Function::from_value(results.into_iter().next().unwrap()).unwrap();
    let calls = binder_calls();
    let exits = generator.execution_stats(&runtime.stack()).vm_exits_taken;
    let sum: f64 = function.invoke(&runtime.stack(), ()).unwrap();
    assert_eq!(sum, 1000.0 * (1.0 + 2.0 + 1.0 + 1.0 + 2.0 + 3.0));
    assert_eq!(binder_calls() - calls, 0, "a duplicate never reaches the binder");
    assert_eq!(generator.execution_stats(&runtime.stack()).vm_exits_taken - exits, 0, "nor leaves native code");
}
