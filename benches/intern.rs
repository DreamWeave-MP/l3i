//! `@dream/intern` against Luau strings, counted in CPU instructions, cycles and cache misses
//! (`perf_event_open`, user space, this process) rather than timed: wall time on a shared
//! machine is noise at these sizes, retired instructions are not.
//!
//! The workloads are parse-shaped: a text buffer of identifier occurrences and an index of
//! `(offset, length)` spans into it, what a parser walking a record file sees. Each case turns
//! every occurrence into an identity:
//!
//! - `string`: `buffer.readstring`, then a string-keyed table (exact identity);
//! - `lower`: `string.lower(buffer.readstring(...))`, then a string-keyed table (no case);
//! - `pool:intern` / `interner`: the pool's method, or the function `pool:interner()` binds,
//!   on the span itself: no Luau string at all;
//! - `str→…`: the same, from Luau strings that already exist (made outside the count);
//! - `loop only` rows: the loop and its span reads alone, to subtract;
//! - `native` rows: `Interner::intern` from Rust, no VM: the pool's own cost.
//!
//! Two passes per case: the first meets every identity for the first time somewhere in it, the
//! second sees only duplicates. Counts are per occurrence, the minimum of `ROUNDS`, and include
//! the collector work the case caused. Memory: VM bytes allocated by the first pass with the
//! collector stopped, VM bytes still held once the collector settles, the pool's native bytes,
//! the instructions one full collection spends while the structure is live (what it costs every
//! cycle), and the instructions spent freeing it (a native drop, or the collections until the
//! heap stops shrinking). Then table reads by key kind, resolve, and a per-call table.
//!
//! `cargo bench --features intern --bench intern [-- micro | <workload number>]` for the
//! interpreter; add `jit` to run every chunk as native code. Linux, `perf_event_paranoid` at 2
//! or lower.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]

#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    eprintln!("the intern bench reads Linux perf counters; nothing to measure on this platform");
}

#[cfg(target_os = "linux")]
mod linux {
    use l3i::Runtime;
    use l3i::convert::NewBuffer;
    use l3i::extension::{RuntimePlan, RuntimePolicy};
    use l3i::intern::{InternExtension, Interner, Policy};
    use l3i::memory::GcControl;
    use l3i::value::Function;

    use crate::counter::{
        CACHE_READ_MISS, Counter, PERF_COUNT_HW_CACHE_L1D, PERF_COUNT_HW_CACHE_LL, PERF_COUNT_HW_CPU_CYCLES,
        PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE, PERF_TYPE_HW_CACHE, measure_all,
    };

    const ROUNDS: usize = 3;
    const MICRO_CALLS: f64 = 200_000.0;

    const CHUNK: &str = r"
local text, index, count
local strs
local current
local lookup
local resolving

local cases = {}

function cases.loop(pass)
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
  end
end

function cases.string(pass)
  local map, n = pass.map, pass.n
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    local s = buffer.readstring(text, o, l)
    if not map[s] then
      n += 1
      map[s] = n
    end
  end
  pass.n = n
end

function cases.lower(pass)
  local map, n = pass.map, pass.n
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    local s = string.lower(buffer.readstring(text, o, l))
    if not map[s] then
      n += 1
      map[s] = n
    end
  end
  pass.n = n
end

function cases.pool(pass)
  local pool: dream_intern_Pool = pass.pool
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    local id = pool:intern(text, o, l)
  end
end

function cases.interner(pass)
  local internId = pass.pool:interner()
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    local id = internId(text, o, l)
  end
end

function cases.strLoop(pass)
  for i = 1, count do
    local s = strs[i]
  end
end

function cases.strString(pass)
  local map, n = pass.map, pass.n
  for i = 1, count do
    local s = strs[i]
    if not map[s] then
      n += 1
      map[s] = n
    end
  end
  pass.n = n
end

function cases.strLower(pass)
  local map, n = pass.map, pass.n
  for i = 1, count do
    local s = string.lower(strs[i])
    if not map[s] then
      n += 1
      map[s] = n
    end
  end
  pass.n = n
end

function cases.strPool(pass)
  local pool: dream_intern_Pool = pass.pool
  for i = 1, count do
    local id = pool:intern(strs[i])
  end
end

local ops = {}

function cases.strInterner(pass)
  local internId = pass.pool:interner()
  for i = 1, count do
    local id = internId(strs[i])
  end
end

function ops.load(t, x, c)
  text, index, count = t, x, c
  return 0, 0
end

function ops.strings()
  strs = table.create(count)
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    strs[i + 1] = buffer.readstring(text, o, l)
  end
  return 0, 0
end

function ops.dropStrings()
  strs = nil
  return 0, 0
end

function ops.prepare(case, policy)
  current = { map = {}, n = 0, pool = policy and intern.new(policy), run = cases[case] }
  return 0, 0
end

function ops.pass()
  current.run(current)
  local pool = current.pool
  return if pool then pool:count() else current.n, if pool then pool:memory() else 0
end

function ops.release()
  current = nil
  return 0, 0
end

-- Table reads by key kind over the occurrences' identities: the pool's dense number tokens,
-- the same identities as integers, and as strings.
function ops.lookupPrepare()
  local pool: dream_intern_Pool = intern.new('exact')
  local tokens, integers, strings = table.create(count), table.create(count), table.create(count)
  local byToken, byInteger, byString = {}, {}, {}
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    local s = buffer.readstring(text, o, l)
    local token = pool:intern(text, o, l)
    local asInteger = integer.create(token)
    tokens[i + 1], integers[i + 1], strings[i + 1] = token, asInteger, s
    byToken[token], byInteger[asInteger], byString[s] = 1, 1, 1
  end

  lookup = {
    token = { keys = tokens, map = byToken },
    integer = { keys = integers, map = byInteger },
    string = { keys = strings, map = byString },
  }
  return 0, 0
end

function ops.lookup(kind)
  local keys, map = lookup[kind].keys, lookup[kind].map
  local acc = 0
  for i = 1, count do
    acc += map[keys[i]]
  end
  return acc, 0
end

function ops.lookupLoop()
  local keys = lookup.token.keys
  for i = 1, count do
    local key = keys[i]
  end
  return 0, 0
end

function ops.lookupRelease()
  lookup = nil
  return 0, 0
end

function ops.resolvePrepare()
  local pool: dream_intern_Pool = intern.new('ascii-nocase')
  for i = 0, count - 1 do
    local o, l = buffer.readu32(index, i * 8), buffer.readu32(index, i * 8 + 4)
    pool:intern(text, o, l)
  end
  resolving = pool
  return pool:count(), 0
end

function ops.resolve()
  local pool = resolving
  for id = 1, pool:count() do
    local s = pool:resolve(id)
  end
  return 0, 0
end

function ops.micro(kind)
  local pool: dream_intern_Pool = intern.new('ascii-nocase')
  local key = 'Caius_Cosades_x1'
  local keyBuffer = buffer.fromstring(key)
  local map = { [string.lower(key)] = 1 }
  pool:intern(key)
  local n = 200000
  if kind == 'loop' then
    for i = 1, n do
    end
  elseif kind == 'count' then
    for i = 1, n do
      local c = pool:count()
    end
  elseif kind == 'internString' then
    for i = 1, n do
      local id = pool:intern(key)
    end
  elseif kind == 'internerSpan' then
    local internId = pool:interner()
    for i = 1, n do
      local id = internId(keyBuffer, 0, 16)
    end
  elseif kind == 'internerString' then
    local internId = pool:interner()
    for i = 1, n do
      local id = internId(key)
    end
  elseif kind == 'internSpan' then
    for i = 1, n do
      local id = pool:intern(keyBuffer, 0, 16)
    end
  elseif kind == 'readu32' then
    for i = 1, n do
      local o = buffer.readu32(keyBuffer, 0)
    end
  elseif kind == 'find' then
    for i = 1, n do
      local id = pool:find(key)
    end
  elseif kind == 'findAbsent' then
    for i = 1, n do
      local id = pool:find('Caius_Cosades_x2')
    end
  elseif kind == 'readstring' then
    for i = 1, n do
      local s = map[buffer.readstring(keyBuffer, 0, 16)]
    end
  elseif kind == 'lower' then
    for i = 1, n do
      local s = map[string.lower(buffer.readstring(keyBuffer, 0, 16))]
    end
  end
  return 0, 0
end

return function(op, a, b, c)
  return ops[op](a, b, c)
end
";

    /// splitmix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    const WORDS: [&str; 16] = [
        "Caius", "Cosades", "fargoth", "Ald", "ruhn", "Vivec", "_door", "Sword", "ebony", "Daedric", "Hlaalu",
        "Telvanni", "misc", "Ingred", "_01", "Guard",
    ];

    /// Record-id-like spellings: two or three words and a unique base-36 suffix, 7 to 28 bytes.
    fn identities(unique: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
        (0..unique)
            .map(|i| {
                let mut id = String::new();
                for _ in 0..2 + rng.below(2) {
                    id.push_str(WORDS[rng.below(WORDS.len())]);
                }
                let mut n = i;
                loop {
                    id.push(char::from_digit((n % 36) as u32, 36).unwrap());
                    n /= 36;
                    if n == 0 {
                        break;
                    }
                }
                id.into_bytes()
            })
            .collect()
    }

    #[derive(Clone, Copy)]
    enum Draw {
        /// Each identity once, shuffled.
        Once,
        Uniform,
        /// Zipf, s = 1: a few identities dominate, as base-game records do.
        Zipf,
    }

    struct Workload {
        name: String,
        text: Vec<u8>,
        index: Vec<u8>,
        count: usize,
    }

    fn workload(unique: usize, occurrences: usize, draw: Draw, mixed_case: bool) -> Workload {
        let mut rng = Rng(0x1d_2e3f + unique as u64 * 31 + occurrences as u64);
        let ids = identities(unique, &mut rng);
        let picks: Vec<usize> = match draw {
            Draw::Once => {
                let mut order: Vec<usize> = (0..unique).collect();
                for i in (1..order.len()).rev() {
                    order.swap(i, rng.below(i + 1));
                }
                order
            }
            Draw::Uniform => (0..occurrences).map(|_| rng.below(unique)).collect(),
            Draw::Zipf => {
                let mut cdf = Vec::with_capacity(unique);
                let mut total = 0.0;
                for k in 0..unique {
                    total += 1.0 / (k + 1) as f64;
                    cdf.push(total);
                }
                (0..occurrences)
                    .map(|_| {
                        let target = rng.unit() * total;
                        cdf.partition_point(|c| *c < target).min(unique - 1)
                    })
                    .collect()
            }
        };
        let mut text = Vec::new();
        let mut index = Vec::with_capacity(picks.len() * 8);
        for pick in &picks {
            let start = text.len() as u32;
            let id = &ids[*pick];
            // A quarter of the occurrences are spelled with random case.
            if mixed_case && rng.below(4) == 0 {
                text.extend(
                    id.iter()
                        .map(|b| if rng.next() & 1 == 0 { b.to_ascii_uppercase() } else { b.to_ascii_lowercase() }),
                );
            } else {
                text.extend_from_slice(id);
            }
            index.extend_from_slice(&start.to_le_bytes());
            index.extend_from_slice(&(id.len() as u32).to_le_bytes());
        }
        let draw = match draw {
            Draw::Once => "each once",
            Draw::Uniform => "uniform",
            Draw::Zipf => "zipf",
        };
        Workload {
            name: format!(
                "{}K unique, {}K occurrences, {draw}, {}",
                unique / 1000,
                picks.len() / 1000,
                if mixed_case { "a quarter in random case" } else { "one spelling each" }
            ),
            count: picks.len(),
            text,
            index,
        }
    }

    /// Instructions, cycles, L1D read misses, LLC read misses.
    const COUNTERS: &[(u32, u64)] = &[
        (PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS),
        (PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES),
        (PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_L1D | CACHE_READ_MISS),
        (PERF_TYPE_HW_CACHE, PERF_COUNT_HW_CACHE_LL | CACHE_READ_MISS),
    ];

    fn heap(runtime: &Runtime) -> f64 {
        f64::from(runtime.gc(GcControl::Count)) * 1024.0 + f64::from(runtime.gc(GcControl::CountRemainder))
    }

    /// Full collections until the heap stops shrinking: a dead table goes in one, the strings it
    /// held in the next.
    fn settle(runtime: &Runtime) {
        loop {
            let before = heap(runtime);
            runtime.gc(GcControl::Collect);
            if heap(runtime) >= before {
                break;
            }
        }
    }

    fn mib(bytes: f64) -> String {
        format!("{:.1}", bytes / (1024.0 * 1024.0))
    }

    fn min_into(best: &mut [u64], counts: &[u64]) {
        for (best, count) in best.iter_mut().zip(counts) {
            *best = (*best).min(*count);
        }
    }

    fn per(count: u64, n: usize) -> String {
        format!("{:.0}", count as f64 / n as f64)
    }

    fn per_miss(count: u64, n: usize) -> String {
        format!("{:.2}", count as f64 / n as f64)
    }

    const HEADER: &str = "| Case | first pass instr/occ | duplicate pass instr/occ | dup cycles/occ | dup L1D miss/occ | dup LLC miss/occ | identities | VM alloc, first pass (MiB) | VM retained (MiB) | native (MiB) | full GC while live (M instr) | free (M instr) |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|";

    struct Bench<'a> {
        runtime: &'a Runtime,
        function: &'a Function,
        counters: &'a [Counter],
    }

    impl Bench<'_> {
        fn call(&self, op: &str, a: Option<&str>, b: Option<&str>) -> (f64, f64) {
            self.function.invoke::<(f64, f64), _>(&self.runtime.stack(), (op, a, b)).unwrap()
        }

        fn counted(&self, op: &str, a: Option<&str>) -> Vec<u64> {
            measure_all(self.counters, &mut || {
                self.call(op, a, None);
            })
        }

        fn collect_cost(&self) -> u64 {
            measure_all(self.counters, &mut || {
                self.runtime.gc(GcControl::Collect);
            })[0]
        }
    }

    fn native_rows(load: &Workload, counters: &[Counter]) {
        let spans: Vec<(usize, usize)> = load
            .index
            .as_chunks::<8>()
            .0
            .iter()
            .map(|span| {
                let start = u32::from_le_bytes(span[..4].try_into().unwrap()) as usize;
                (start, start + u32::from_le_bytes(span[4..].try_into().unwrap()) as usize)
            })
            .collect();
        for (label, policy) in [("native exact (no VM)", Policy::Exact), ("native nocase (no VM)", Policy::AsciiNoCase)]
        {
            let (mut first, mut duplicate) = (vec![u64::MAX; COUNTERS.len()], vec![u64::MAX; COUNTERS.len()]);
            let (mut unique, mut native, mut drop_cost) = (0, 0, u64::MAX);
            for _ in 0..ROUNDS {
                let mut pool = Interner::new(policy);
                let pass = |pool: &mut Interner| {
                    for (from, to) in &spans {
                        std::hint::black_box(pool.intern(&load.text[*from..*to]).unwrap());
                    }
                };
                min_into(&mut first, &measure_all(counters, &mut || pass(&mut pool)));
                min_into(&mut duplicate, &measure_all(counters, &mut || pass(&mut pool)));
                unique = pool.len();
                native = pool.memory();
                let mut pool = Some(pool);
                drop_cost = drop_cost.min(measure_all(counters, &mut || drop(pool.take()))[0]);
            }
            println!(
                "| {label} | {} | {} | {} | {} | {} | {unique} | 0.0 | 0.0 | {} | 0.00 | {:.2} |",
                per(first[0], load.count),
                per(duplicate[0], load.count),
                per(duplicate[1], load.count),
                per_miss(duplicate[2], load.count),
                per_miss(duplicate[3], load.count),
                mib(native as f64),
                drop_cost as f64 / 1e6,
            );
        }
    }

    fn measure(bench: &Bench<'_>, load: &Workload, strings: bool) {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("loop only (span reads)", "loop", None),
            ("string (exact)", "string", None),
            ("pool:intern exact", "pool", Some("exact")),
            ("interner exact", "interner", Some("exact")),
            ("lower (nocase)", "lower", None),
            ("pool:intern nocase", "pool", Some("ascii-nocase")),
            ("interner nocase", "interner", Some("ascii-nocase")),
            ("str→ loop only", "strLoop", None),
            ("str→string (exact)", "strString", None),
            ("str→pool:intern exact", "strPool", Some("exact")),
            ("str→interner exact", "strInterner", Some("exact")),
            ("str→lower (nocase)", "strLower", None),
            ("str→pool:intern nocase", "strPool", Some("ascii-nocase")),
            ("str→interner nocase", "strInterner", Some("ascii-nocase")),
        ];
        let runtime = bench.runtime;
        println!("\n### {}\n\n{HEADER}", load.name);
        native_rows(load, bench.counters);
        for (label, case, policy) in cases {
            // The `str…` cases start from Luau strings made beforehand (`strString`, not `string`).
            let from_strings = case.starts_with("str") && case != &"string";
            if from_strings && !strings {
                continue;
            }
            if from_strings {
                bench.call("strings", None, None);
            }
            settle(runtime);
            let baseline = heap(runtime);

            // Allocation volume: the first pass with the collector stopped.
            bench.call("prepare", Some(case), *policy);
            runtime.gc(GcControl::Stop);
            let before = heap(runtime);
            bench.call("pass", None, None);
            let allocated = heap(runtime) - before;
            runtime.gc(GcControl::Restart);

            let (mut first, mut duplicate) = (vec![u64::MAX; COUNTERS.len()], vec![u64::MAX; COUNTERS.len()]);
            let mut result = (0.0, 0.0);
            for _ in 0..ROUNDS {
                bench.call("release", None, None);
                settle(runtime);
                bench.call("prepare", Some(case), *policy);
                min_into(&mut first, &bench.counted("pass", None));
                min_into(&mut duplicate, &measure_all(bench.counters, &mut || result = bench.call("pass", None, None)));
            }
            let (unique, native) = result;
            settle(runtime);
            let retained = heap(runtime) - baseline;
            let live = bench.collect_cost();
            bench.call("release", None, None);
            // A dead table goes in one full collection and the strings it held in the next:
            // collect until the heap stops shrinking, then subtract that many empty collections.
            let (mut freeing, mut collections) = (0, 0);
            loop {
                let before = heap(runtime);
                freeing += bench.collect_cost();
                collections += 1;
                if heap(runtime) >= before {
                    break;
                }
            }
            let empty = bench.collect_cost();
            let (pressure, freed) = (live.saturating_sub(empty), freeing.saturating_sub(empty * collections));
            bench.call("dropStrings", None, None);
            runtime.gc(GcControl::Collect);
            println!(
                "| {label} | {} | {} | {} | {} | {} | {unique} | {} | {} | {} | {:.2} | {:.2} |",
                per(first[0], load.count),
                per(duplicate[0], load.count),
                per(duplicate[1], load.count),
                per_miss(duplicate[2], load.count),
                per_miss(duplicate[3], load.count),
                mib(allocated),
                mib(retained),
                mib(native),
                pressure as f64 / 1e6,
                freed as f64 / 1e6,
            );
        }

        bench.call("lookupPrepare", None, None);
        let mut loop_only = vec![u64::MAX; COUNTERS.len()];
        for _ in 0..ROUNDS {
            min_into(&mut loop_only, &bench.counted("lookupLoop", None));
        }
        print!("\nTable read `map[keys[i]]` per occurrence, the key walk subtracted:");
        for kind in ["token", "integer", "string"] {
            let mut best = vec![u64::MAX; COUNTERS.len()];
            for _ in 0..ROUNDS {
                min_into(&mut best, &bench.counted("lookup", Some(kind)));
            }
            print!(
                " {kind} keys {} instr, {} cycles, {} L1D miss;",
                per(best[0].saturating_sub(loop_only[0]), load.count),
                per(best[1].saturating_sub(loop_only[1]), load.count),
                per_miss(best[2].saturating_sub(loop_only[2]), load.count),
            );
        }
        println!();
        bench.call("lookupRelease", None, None);
        runtime.gc(GcControl::Collect);

        let (identities, _) = bench.call("resolvePrepare", None, None);
        let mut best = vec![u64::MAX; COUNTERS.len()];
        for _ in 0..ROUNDS {
            min_into(&mut best, &bench.counted("resolve", None));
        }
        println!(
            "Resolve, per identity over {identities}: {} instr, {} cycles.",
            per(best[0], identities as usize),
            per(best[1], identities as usize)
        );
        runtime.gc(GcControl::Collect);
    }

    fn native_micro(counters: &[Counter]) {
        let key = b"Caius_Cosades_x1";
        let calls = MICRO_CALLS as usize;
        for (label, policy) in [("exact", Policy::Exact), ("nocase", Policy::AsciiNoCase)] {
            let mut pool = Interner::new(policy);
            for i in 0..1000 {
                pool.intern(format!("filler_{i}").as_bytes()).unwrap();
            }
            pool.intern(key).unwrap();
            let mut best = u64::MAX;
            for _ in 0..ROUNDS * 3 {
                best = best.min(
                    measure_all(&counters[..1], &mut || {
                        for _ in 0..calls {
                            std::hint::black_box(pool.intern(std::hint::black_box(key)).unwrap());
                        }
                    })[0],
                );
            }
            println!("| `Interner::intern`, {label}, duplicate (native, no VM) | {:.0} | |", best as f64 / MICRO_CALLS);
        }
    }

    fn micro(bench: &Bench<'_>) {
        let kinds = [
            ("count", "pool:count()"),
            ("internString", "pool:intern(string), duplicate"),
            ("internSpan", "pool:intern(buffer, 0, 16), duplicate"),
            ("find", "pool:find(string), present"),
            ("findAbsent", "pool:find(string), absent"),
            ("internerString", "internId(string), duplicate (pool:interner())"),
            ("internerSpan", "internId(buffer, 0, 16), duplicate (pool:interner())"),
            ("readu32", "buffer.readu32(buffer, 0)"),
            ("readstring", "map[buffer.readstring(buffer, 0, 16)]"),
            ("lower", "map[string.lower(buffer.readstring(buffer, 0, 16))]"),
        ];
        let mut loop_only = vec![u64::MAX; COUNTERS.len()];
        for _ in 0..ROUNDS * 3 {
            min_into(&mut loop_only, &bench.counted("micro", Some("loop")));
        }
        println!("\n### Per call, 16-byte identity, cache-resident (loop subtracted)\n");
        println!("| Call | instructions | cycles |");
        println!("|---|---:|---:|");
        for (kind, label) in kinds {
            let mut best = vec![u64::MAX; COUNTERS.len()];
            for _ in 0..ROUNDS * 3 {
                min_into(&mut best, &bench.counted("micro", Some(kind)));
            }
            println!(
                "| `{label}` | {:.0} | {:.0} |",
                best[0].saturating_sub(loop_only[0]) as f64 / MICRO_CALLS,
                best[1].saturating_sub(loop_only[1]) as f64 / MICRO_CALLS
            );
        }
        native_micro(bench.counters);
    }

    /// The chunk's dispatcher, compiled to native code first under `jit`.
    fn dispatcher(runtime: &Runtime) -> Function {
        let options = runtime.compile_options();
        runtime
            .stack()
            .with_frame(|frame| {
                let chunk = runtime.load(frame, "=intern-bench", CHUNK, &options)?;
                #[cfg(feature = "jit")]
                {
                    let native = runtime.native_code().expect("jit builds have a code generator");
                    let bytecode = l3i::source::compile(CHUNK, &options)?;
                    let result = native.compile(frame, chunk.index(), &bytecode)?;
                    assert!(
                        matches!(result.status, l3i::native_code::NativeCodeStatus::Success),
                        "{:?}",
                        result.status
                    );
                }
                chunk.as_function()?.invoke::<Function, ()>(frame, ())
            })
            .unwrap()
    }

    pub fn main() {
        let counters: Option<Vec<Counter>> =
            COUNTERS.iter().map(|(kind, config)| Counter::open(*kind, *config)).collect();
        let Some(counters) = counters else {
            eprintln!("perf counters unavailable (perf_event_paranoid above 2?); nothing measured");
            return;
        };
        let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
        let policy = RuntimePolicy::new().compat_global("@dream/intern", "intern");
        // With `jit`, every chunk runs as native code, as a host that enables it ships scripts.
        #[cfg(feature = "jit")]
        let policy = policy.native_code(l3i::extension::NativeCodePolicy {
            mode: l3i::native_code::NativeCodeMode::Eager,
            ..l3i::extension::NativeCodePolicy::default()
        });
        let plan = RuntimePlan::builder().policy(policy).extension(InternExtension).finalize().unwrap();
        println!(
            "Luau {}; counts are user-space, this process only.",
            if cfg!(feature = "jit") { "native code (jit, every chunk compiled)" } else { "interpreter" }
        );
        let shapes: &[(usize, usize, Draw, bool, bool)] = &[
            (350_000, 350_000, Draw::Once, false, true),
            (350_000, 5_000_000, Draw::Uniform, false, true),
            (350_000, 5_000_000, Draw::Zipf, false, true),
            (350_000, 5_000_000, Draw::Zipf, true, true),
            (10_000, 5_000_000, Draw::Zipf, true, true),
            (100_000, 1_000_000, Draw::Uniform, true, true),
            (1_000_000, 5_000_000, Draw::Uniform, true, true),
            (350_000, 20_000_000, Draw::Zipf, true, false),
        ];
        let runtime = |load: &Workload| {
            let runtime = Runtime::from_plan(&plan).unwrap();
            // A safe environment, as a host ships one: Luau's builtins (buffer.readu32,
            // string.lower) take their fast paths only there.
            runtime.sandbox_globals();
            let function = dispatcher(&runtime);
            function
                .invoke::<(f64, f64), _>(
                    &runtime.stack(),
                    ("load", NewBuffer(load.text.clone()), NewBuffer(load.index.clone()), load.count as f64),
                )
                .unwrap();
            (runtime, function)
        };
        if filter.as_deref().is_none_or(|f| f == "micro") {
            let load = workload(1000, 1000, Draw::Once, false);
            let (runtime, function) = runtime(&load);
            micro(&Bench { runtime: &runtime, function: &function, counters: &counters });
        }
        for (i, (unique, occurrences, draw, mixed, strings)) in shapes.iter().enumerate() {
            if filter.as_deref().is_some_and(|f| f.parse::<usize>().ok() != Some(i)) {
                continue;
            }
            let load = workload(*unique, *occurrences, *draw, *mixed);
            // A fresh runtime per workload, so one workload's heap is not the next one's baseline.
            let (runtime, function) = runtime(&load);
            measure(&Bench { runtime: &runtime, function: &function, counters: &counters }, &load, *strings);
        }
    }
}
