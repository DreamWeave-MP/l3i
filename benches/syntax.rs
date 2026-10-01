//! `@dream/luau` counted in CPU instructions (`perf_event_open`, user space, this process) per
//! source line, rather than timed: what Luau's parser costs alone, what its JSON encoding adds,
//! and what building the tree as Luau tables adds, with and without the token stream.
//!
//! The source is a typed module of ordinary script code (locals, functions, tables, control
//! flow, interpolation, comments) repeated to a few thousand lines with fresh names per copy.
//!
//! - `parse`: `Luau::Parser` alone, from Rust (`l3i::analysis::parse`, no JSON);
//! - `parse + toJson`: the same with Luau's own JSON encoding of the tree, the form a script
//!   would have to decode before it could walk anything;
//! - `luau.parse`: from Luau, the tree, comments and line starts built as tables;
//! - `luau.parse, tokens`: the same plus the token buffer;
//! - `luau.parse + walk`: the tables, then a Luau walk that touches every statement and
//!   expression once (what a checker pays to read the tree).
//!
//! `cargo bench --features syntax,analysis --bench syntax` (add `jit` to run the walk as native
//! code). Linux, `perf_event_paranoid` at 2 or lower.

#![allow(clippy::cast_precision_loss, clippy::missing_panics_doc)]

#[cfg(target_os = "linux")]
#[path = "instructions/counter.rs"]
mod counter;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    eprintln!("the syntax bench reads Linux perf counters; nothing to measure on this platform");
}

#[cfg(target_os = "linux")]
mod linux {
    use l3i::Runtime;
    use l3i::extension::{RuntimePlan, RuntimePolicy};
    use l3i::memory::GcControl;
    use l3i::syntax::SyntaxExtension;
    use l3i::value::Function;

    use crate::counter::{Counter, PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE};

    const ROUNDS: usize = 5;
    const COPIES: usize = 100;

    const MODULE: &str = r"--!strict
-- A record walker: reads spans, groups them, and reports.
local Limits = { MaxDepth = 32, MaxItems = 4096 }

export type Item@ = { name: string, offset: number, length: number, tags: { string }? }

local function collect@(items: { Item@ }, filter: ((Item@) -> boolean)?): { Item@ }
  local out, seen = {}, {}

  for index, item in items do
    if filter and not filter(item) then
      continue
    end

    if seen[item.name] then
      continue
    end

    seen[item.name] = true
    table.insert(out, item)
  end

  return out
end

local function describe@(item: Item@, depth: number): string
  if depth > Limits.MaxDepth then
    return 'too deep'
  end

  local tags = item.tags and table.concat(item.tags, ', ') or 'none'
  return `{item.name} at {item.offset} ({item.length} bytes; tags {tags})`
end

--[[ The report keeps every line it makes; a caller prints them. ]]
local function report@(items: { Item@ }): { string }
  local lines = {}
  local total = 0

  for _, item in collect@(items) do
    total += item.length
    table.insert(lines, describe@(item, 1))
  end

  table.insert(lines, string.format('%d items, %d bytes', #lines, total))
  return lines
end
";

    fn source() -> String {
        let mut out = String::new();
        for copy in 0..COPIES {
            out.push_str(&MODULE.replace('@', &copy.to_string()));
        }
        out.push_str("return {}\n");
        out
    }

    fn runtime() -> Runtime {
        let plan = RuntimePlan::builder()
            .policy(RuntimePolicy::new().compat_global("@dream/luau", "luau"))
            .extension(SyntaxExtension)
            .finalize()
            .unwrap();
        Runtime::from_plan(&plan).unwrap()
    }

    fn compile(runtime: &Runtime, body: &str) -> Function {
        runtime.load_function(&format!("return function(...) {body} end")).unwrap()
    }

    const WALK: &str = r"
        local source = ...
        local result = luau.parse(source)
        local count = 0
        local walkExpr, walkBlock

        function walkExpr(expr)
          count += 1
          local kind = expr.kind
          if kind == 'ExprCall' then
            walkExpr(expr.func)
            for _, arg in expr.args do
              walkExpr(arg)
            end
          elseif kind == 'ExprBinary' then
            walkExpr(expr.left)
            walkExpr(expr.right)
          elseif kind == 'ExprUnary' or kind == 'ExprGroup' or kind == 'ExprIndexName' then
            walkExpr(expr.expr)
          elseif kind == 'ExprIndexExpr' then
            walkExpr(expr.expr)
            walkExpr(expr.index)
          elseif kind == 'ExprFunction' then
            walkBlock(expr.body)
          elseif kind == 'ExprTable' then
            for _, item in expr.items do
              if item.key then
                walkExpr(item.key)
              end
              walkExpr(item.value)
            end
          elseif kind == 'ExprInterpString' then
            for _, part in expr.expressions do
              walkExpr(part)
            end
          end
        end

        function walkBlock(block)
          for _, stat in block.body do
            count += 1
            local kind = stat.kind
            if kind == 'StatLocal' or kind == 'StatAssign' or kind == 'StatReturn' then
              for _, value in stat.values or stat.list do
                walkExpr(value)
              end
            elseif kind == 'StatExpr' then
              walkExpr(stat.expr)
            elseif kind == 'StatIf' then
              walkExpr(stat.condition)
              walkBlock(stat.thenbody)
              if stat.elsebody then
                walkBlock(stat.elsebody)
              end
            elseif kind == 'StatForIn' then
              for _, value in stat.values do
                walkExpr(value)
              end
              walkBlock(stat.body)
            elseif kind == 'StatLocalFunction' or kind == 'StatFunction' then
              walkBlock(stat.func.body)
            elseif kind == 'StatCompoundAssign' then
              walkExpr(stat.value)
            end
          end
        end

        walkBlock(result.root)
        return count
    ";

    fn minimum(counter: &Counter, mut body: impl FnMut()) -> u64 {
        (0..ROUNDS).map(|_| counter.measure(&mut body)).min().unwrap()
    }

    pub fn main() {
        let Some(instructions) = Counter::open(PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS) else {
            eprintln!("perf_event_open failed: is perf_event_paranoid above 2?");
            return;
        };
        let source = source();
        let lines = source.lines().count() as f64;
        println!(
            "{} lines, {} bytes; instructions per line, minimum of {ROUNDS} rounds (verified toolchain: {})",
            lines,
            source.len(),
            l3i::VERIFIED_TOOLCHAIN
        );

        let parse = minimum(&instructions, || {
            let report = l3i::analysis::parse(&source, false);
            assert!(report.errors.is_empty());
        });
        let json = minimum(&instructions, || {
            let report = l3i::analysis::parse(&source, true);
            assert!(report.json.is_some());
        });
        let json_bytes = l3i::analysis::parse(&source, true).json.map_or(0, |json| json.len());

        let runtime = runtime();
        let tables = compile(&runtime, "local result = luau.parse(...) assert(#result.errors == 0)");
        let tokens = compile(&runtime, "local result = luau.parse(..., { tokens = true }) assert(result.tokens)");
        let walk = compile(&runtime, WALK);
        let measure = |function: &Function| {
            minimum(&instructions, || {
                function.invoke::<(), _>(&runtime.stack(), (source.as_str(),)).unwrap();
                runtime.gc(GcControl::Collect);
            })
        };
        let built = measure(&tables);
        let tokenized = measure(&tokens);
        let walked = measure(&walk);
        let collect = minimum(&instructions, || {
            runtime.gc(GcControl::Collect);
        });

        // The Luau rows collect after each run, so the cost of a collection with nothing to free is
        // taken off them; the Rust rows make no Luau garbage.
        let row = |name: &str, count: u64| println!("{name:<28} {:>10.0}", count as f64 / lines);
        row("parse", parse);
        row("parse + toJson", json);
        row("luau.parse", built.saturating_sub(collect));
        row("luau.parse, tokens", tokenized.saturating_sub(collect));
        row("luau.parse + walk", walked.saturating_sub(collect));
        println!("toJson text: {json_bytes} bytes for {} source bytes", source.len());
    }
}
