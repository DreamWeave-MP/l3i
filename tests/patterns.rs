//! JSL record binding patterns against checked handwritten Luau. Every case runs a JSL body and
//! the Luau a careful author would write instead, in the same VM, and compares their results,
//! normalized error messages, and the order of every field read (logged through `__index`).

use std::cell::Cell;
use std::rc::Rc;

use l3i::Runtime;
use l3i::debug::{DebugAction, DebugInfo, DebugScope, HookSet, RuntimeHooks};
use l3i::source::{CompileOptions, LoweringMode, disassemble, lowering_snapshot};
use l3i::stack::Stack;

/// The shared harness: `rec(name, fields)` is a record whose reads log `name.key`, and a field
/// whose value is the `FAIL` sentinel raises when read. `run` captures results, a location-free
/// error, and the read log.
const HARNESS: &str = r#"
local log = {}
local FAIL = newproxy()
local function rec(name, fields)
    return setmetatable({}, { __index = function(_, key)
        table.insert(log, name .. "." .. tostring(key))
        local value = fields[key]
        if value == FAIL then error("read " .. name .. "." .. tostring(key) .. " failed", 0) end
        return value
    end })
end
local function run(body)
    table.clear(log)
    local results = table.pack(pcall(body))
    local parts = {}
    for i = 1, results.n do
        local value = results[i]
        if i == 2 and not results[1] then value = string.gsub(tostring(value), "^.-:%d+: ", "") end
        table.insert(parts, typeof(value) == "table" and "table" or tostring(value))
    end
    return table.concat(parts, ",") .. " | " .. table.concat(log, " ")
end
"#;

/// One oracle case: shared setup, then the JSL body and its handwritten Luau equivalent. Both
/// bodies are function bodies over the setup's locals.
struct Case {
    name: &'static str,
    setup: &'static str,
    jsl: &'static str,
    luau: &'static str,
}

fn check(cases: &[Case]) {
    let runtime = Runtime::new().unwrap();
    for case in cases {
        let program = format!(
            "{HARNESS}\n{setup}\nlocal jsl = run(function()\n{jsl}\nend)\nlocal luau = run(function()\n{luau}\nend)\n\
             assert(jsl == luau, `{name}\\n  JSL:  {{jsl}}\\n  Luau: {{luau}}`)\nassert(not string.find(jsl, \"^false,\") or {failing}, \
             `{name} raised: {{jsl}}`)",
            setup = case.setup,
            jsl = case.jsl,
            luau = case.luau,
            name = case.name,
            failing = case.name.contains("raises"),
        );
        if let Err(error) = runtime.exec(&program) {
            panic!("{}: {error}", case.name);
        }
    }
}

#[test]
fn local_declarations_match_handwritten_luau() {
    check(&[
        Case {
            name: "source evaluated once",
            setup: "local calls = 0 local function source() calls += 1 return { x = 1, y = 2 } end",
            jsl: "calls = 0 local {x, y} = source() return x, y, calls",
            luau: "calls = 0 local s = source() local x = s.x local y = s.y return x, y, calls",
        },
        Case {
            name: "left to right, depth first",
            setup: "local function entity() return rec('e', { transform = rec('t', { position = rec('p', { x = 1, y = 2 }) }), id = 7 }) end",
            jsl: "local {transform: {position: {x, y}}, id} = entity() return x, y, id",
            luau: "local e = entity() local t = e.transform local p = t.position local x = p.x local y = p.y local id = e.id return x, y, id",
        },
        Case {
            name: "aliases",
            setup: "local function entity() return rec('e', { health = 3, name = 'n' }) end",
            jsl: "local {health: hp, name} = entity() return hp, name",
            luau: "local e = entity() local hp = e.health local name = e.name return hp, name",
        },
        Case {
            name: "missing field binds nil",
            setup: "",
            jsl: "local {x, missing} = { x = 1 } return x, missing == nil",
            luau: "local s = { x = 1 } local x = s.x local missing = s.missing return x, missing == nil",
        },
        Case {
            name: "nested nil raises the ordinary indexing error",
            setup: "",
            jsl: "local {a: {b}} = {} return b",
            luau: "local s = {} local a = s.a local b = a.b return b",
        },
        Case {
            name: "nil source raises the ordinary indexing error",
            setup: "",
            jsl: "local {a} = nil return a",
            luau: "local s = nil local a = (s :: any).a return a",
        },
        Case {
            name: "a failing read raises and stops later reads",
            setup: "local function entity() return rec('e', { a = 1, b = FAIL, c = 3 }) end",
            jsl: "local {a, b, c} = entity() return a, b, c",
            luau: "local e = entity() local a = e.a local b = e.b local c = e.c return a, b, c",
        },
        Case {
            name: "identity is preserved",
            setup: "local inner = {} local handle = newproxy()",
            jsl: "local {t, u} = { t = inner, u = handle } return rawequal(t, inner), rawequal(u, handle)",
            luau: "local s = { t = inner, u = handle } local t = s.t local u = s.u return rawequal(t, inner), rawequal(u, handle)",
        },
        Case {
            name: "the first of multiple returns",
            setup: "local function two() return { x = 1 }, { x = 2 } end",
            jsl: "local {x} = two() return x",
            luau: "local s = two() local x = s.x return x",
        },
        Case {
            name: "ordinary indexing: vectors, strings and metatables",
            setup: "",
            jsl: "local {x, y, z} = vector.create(1, 2, 3) local {upper} = 'abc' return x, y, z, upper == string.upper",
            luau: "local v = vector.create(1, 2, 3) local x = v.x local y = v.y local z = v.z local s = 'abc' local upper = s.upper \
                   return x, y, z, upper == string.upper",
        },
        Case {
            name: "empty pattern evaluates its source and reads nothing",
            setup: "local calls = 0 local function source() calls += 1 return rec('e', {}) end",
            jsl: "calls = 0 local {} = source() return calls",
            luau: "calls = 0 local s = source() return calls",
        },
        Case {
            name: "shadowing: the value sees the enclosing scope",
            setup: "",
            jsl: "local x = 10 local {x, y} = { x = x + 1, y = x } return x, y",
            luau: "local x = 10 local s = { x = x + 1, y = x } local x = s.x local y = s.y return x, y",
        },
        Case {
            name: "closures capture the bound locals",
            setup: "",
            jsl: "local {a} = { a = 1 } local get = function() return a end a = 5 return get()",
            luau: "local s = { a = 1 } local a = s.a local get = function() return a end a = 5 return get()",
        },
        Case {
            name: "whole-pattern annotation",
            setup: "type Point = { x: number, y: number }",
            jsl: "local {x, y}: Point = { x = 1, y = 2 } return x + y",
            luau: "local p: Point = { x = 1, y = 2 } local x = p.x local y = p.y return x + y",
        },
        Case {
            name: "a raising source raises before any read",
            setup: "local function source() error('no source', 0) end",
            jsl: "local {a} = source() return a",
            luau: "local s = source() local a = s.a return a",
        },
        Case {
            name: "statement boundaries and semicolons",
            setup: "",
            jsl: "local {a} = { a = 1 }; local {b} = { b = a + 1 } local {c} = { c = b } return a, b, c",
            luau: "local s1 = { a = 1 }; local a = s1.a local s2 = { b = a + 1 } local b = s2.b local s3 = { c = b } local c = s3.c return a, b, c",
        },
        Case {
            name: "a declaration inside a nested function and loop",
            setup: "local function entities() return { rec('a', { v = 1 }), rec('b', { v = 2 }) } end",
            jsl: "local total = 0 for _, e in entities() do local {v} = e total += v end return total",
            luau: "local total = 0 for _, e in entities() do local v = e.v total += v end return total",
        },
    ]);
}

#[test]
fn function_parameters_match_handwritten_luau() {
    check(&[
        Case {
            name: "typed parameter pattern",
            setup: "type Vec3 = { x: number, y: number, z: number }",
            jsl: "local function move({x, y, z}: Vec3, dt: number) return x + dt, y + dt, z + dt end return move({ x = 1, y = 2, z = 3 }, 1)",
            luau: "local function move(v: Vec3, dt: number) local x = v.x local y = v.y local z = v.z return x + dt, y + dt, z + dt end \
                   return move({ x = 1, y = 2, z = 3 }, 1)",
        },
        Case {
            name: "parameters destructure in order, before the body",
            setup: "local function arg(name) return rec(name, { x = name .. 'x', y = name .. 'y' }) end",
            jsl: "local function f({x}, middle, {y: {}}) table.insert(log, 'body') return x, middle end return f(arg('a'), 'm', arg('b'))",
            luau: "local function f(a, middle, b) local x = a.x local y = b.y table.insert(log, 'body') return x, middle end \
                   return f(arg('a'), 'm', arg('b'))",
        },
        Case {
            name: "a nil argument raises at entry",
            setup: "",
            jsl: "local function f({x}) return x end return f(nil)",
            luau: "local function f(p) local x = p.x return x end return f(nil)",
        },
        Case {
            name: "a nested parameter pattern raises the ordinary indexing error",
            setup: "",
            jsl: "local function f({position: {x}}) return x end return f({})",
            luau: "local function f(p) local position = p.position local x = position.x return x end return f({})",
        },
        Case {
            name: "methods keep implicit self",
            setup: "local T = {}",
            jsl: "function T:at({index}) return self == T, index end return T:at({ index = 4 })",
            luau: "function T:at(p) local index = p.index return self == T, index end return T:at({ index = 4 })",
        },
        Case {
            name: "function expressions and varargs",
            setup: "",
            jsl: "local f = function({a}, ...) return a, select('#', ...) end return f({ a = 1 }, 2, 3)",
            luau: "local f = function(p, ...) local a = p.a return a, select('#', ...) end return f({ a = 1 }, 2, 3)",
        },
        Case {
            name: "recursion and closures over parameter bindings",
            setup: "",
            jsl: "local function fact({n}) if n <= 1 then return 1 end return n * fact({ n = n - 1 }) end \
                  local function counter({start}) return function() start += 1 return start end end \
                  local tick = counter({ start = 10 }) tick() return fact({ n = 5 }), tick()",
            luau: "local function fact(p) local n = p.n if n <= 1 then return 1 end return n * fact({ n = n - 1 }) end \
                   local function counter(p) local start = p.start return function() start += 1 return start end end \
                   local tick = counter({ start = 10 }) tick() return fact({ n = 5 }), tick()",
        },
        Case {
            name: "the public arity is unchanged",
            setup: "",
            jsl: "local function f({x, y}, z) return x end return debug.info(f, 'a')",
            luau: "local function f(p, z) local x = p.x local y = p.y return x end return debug.info(f, 'a')",
        },
        Case {
            name: "generic functions and attributes",
            setup: "type Box<T> = { value: T }",
            jsl: "@native local function unbox<T>({value}: Box<T>, fallback: T): T return value or fallback end \
                  return unbox({ value = 4 }, 0), unbox({}, 7)",
            luau: "@native local function unbox<T>(b: Box<T>, fallback: T): T local value = b.value return value or fallback end \
                   return unbox({ value = 4 }, 0), unbox({}, 7)",
        },
        Case {
            name: "a failing parameter read raises before the body",
            setup: "",
            jsl: "local function f({a, b}) table.insert(log, 'body') return a end return f(rec('p', { a = 1, b = FAIL }))",
            luau: "local function f(p) local a = p.a local b = p.b table.insert(log, 'body') return a end return f(rec('p', { a = 1, b = FAIL }))",
        },
    ]);
}

#[test]
fn comprehension_generators_match_handwritten_luau() {
    const ENTITIES: &str = "local function entities() return { \
        rec('e1', { id = 1, active = true, position = rec('p1', { z = 5 }) }), \
        rec('e2', { id = 2, active = false, position = rec('p2', { z = 9 }) }), \
        rec('e3', { id = 3, active = true, position = rec('p3', { z = -1 }) }) } end";
    check(&[
        Case {
            name: "fields are read before the filter",
            setup: ENTITIES,
            jsl: "local ids = [for {id, active, position: {z}} in entities() if active and z > 0 => id] return #ids, ids[1]",
            luau: "local src = entities() local ids = {} for i = 1, #src do local e = src[i] local id = e.id local active = e.active \
                   local position = e.position local z = position.z if active and z > 0 then table.insert(ids, id) end end return #ids, ids[1]",
        },
        Case {
            name: "count, sum, min, max",
            setup: ENTITIES,
            jsl: "return #[for {id} in entities() => id], sum[for {id} in entities() => id], min[for {id} in entities() => id], \
                  max[for {position: {z}} in entities() => z]",
            luau: "local n, s, lo, hi = 0, 0, nil, nil local src = entities() for i = 1, #src do local id = src[i].id n += 1 end \
                   src = entities() for i = 1, #src do local id = src[i].id s += id end \
                   src = entities() for i = 1, #src do local id = src[i].id if lo == nil or id < lo then lo = id end end \
                   src = entities() for i = 1, #src do local position = src[i].position local z = position.z if hi == nil or z > hi then hi = z end end \
                   return n, s, lo, hi",
        },
        Case {
            name: "any and all stop at the deciding element",
            setup: ENTITIES,
            jsl: "return any[for {active} in entities() => not active], all[for {id} in entities() => id < 2]",
            luau: "local function anyInactive() local src = entities() for i = 1, #src do local active = src[i].active if not active then return true end end return false end \
                   local function allSmall() local src = entities() for i = 1, #src do local id = src[i].id if not (id < 2) then return false end end return true end \
                   return anyInactive(), allSmall()",
        },
        Case {
            name: "into sinks",
            setup: ENTITIES,
            jsl: "local out = { 9, 9, 9, 9 } local result = into(out)[for {id, active} in entities() if active => id] \
                  return rawequal(result, out), #out, out[1], out[2], out[3]",
            luau: "local out = { 9, 9, 9, 9 } local previous = #out local n = 0 local src = entities() for i = 1, #src do \
                   local e = src[i] local id = e.id local active = e.active if active then n += 1 out[n] = id end end \
                   for i = n + 1, previous do out[i] = nil end return true, #out, out[1], out[2], out[3]",
        },
        Case {
            name: "dependent generators see earlier pattern bindings",
            setup: "local function parents() return { rec('a', { enabled = true, children = { rec('c1', { id = 1 }), rec('c2', { id = 2 }) } }), \
                    rec('b', { enabled = false, children = { rec('c3', { id = 3 }) } }) } end",
            jsl: "local ids = [for {children, enabled} in parents() if enabled for {id} in children => id] return #ids, ids[1], ids[2]",
            luau: "local ids = {} local src = parents() for i = 1, #src do local p = src[i] local children = p.children local enabled = p.enabled \
                   if enabled then for j = 1, #children do local id = children[j].id table.insert(ids, id) end end end return #ids, ids[1], ids[2]",
        },
        Case {
            name: "enumerate and zip slots",
            setup: "",
            jsl: "local a = [for i, {v} in enumerate({ { v = 10 }, { v = 20 } }) => i * v] \
                  local b = [for {x}, {y} in zipStrict({ { x = 1 }, { x = 2 } }, { { y = 3 }, { y = 4 } }) => x + y] \
                  return a[1], a[2], b[1], b[2]",
            luau: "return 10, 40, 4, 6",
        },
        Case {
            name: "a generator source is outside its own pattern's scope",
            setup: "",
            jsl: "local id = { { id = 'inner' } } local out = [for {id} in id => id] return out[1], typeof(id)",
            luau: "local id = { { id = 'inner' } } local out = {} for i = 1, #id do local e = id[i] table.insert(out, e.id) end return out[1], typeof(id)",
        },
        Case {
            name: "a scalar element raises the ordinary indexing error",
            setup: "",
            jsl: "return [for {x} in range(1, 2) => x]",
            luau: "for i = 1, 2 do local x = (i :: any).x end return nil",
        },
        Case {
            name: "a nil projection raises the comprehension error",
            setup: "",
            jsl: "return [for {missing} in { {} } => missing]",
            luau: "error('L3i comprehension projection produced nil; filter nil explicitly')",
        },
        Case {
            name: "a failing read raises and stops the pipeline",
            setup: "",
            jsl: "return [for {a, b} in { rec('x', { a = 1, b = 2 }), rec('y', { a = 1, b = FAIL }), rec('z', { a = 1, b = 3 }) } => a]",
            luau: "local src = { rec('x', { a = 1, b = 2 }), rec('y', { a = 1, b = FAIL }), rec('z', { a = 1, b = 3 }) } \
                   for i = 1, #src do local e = src[i] local a = e.a local b = e.b end return nil",
        },
        Case {
            name: "nested comprehensions over pattern bindings",
            setup: "",
            jsl: "local rows = [for {cells} in { { cells = { { v = 1 }, { v = 2 } } } } => [for {v} in cells => v * 2]] return #rows, rows[1][2]",
            luau: "return 1, 4",
        },
    ]);
}

#[test]
fn extraction_order_is_source_order_and_depth_first() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(&format!(
            "{HARNESS}\n{}",
            r"
        local function entity()
            return rec('e', { transform = rec('t', { position = rec('p', { x = 1, y = 2 }) }), id = 7 })
        end
        local text = run(function() local {transform: {position: {x, y}}, id} = entity() return x, y, id end)
        assert(text == 'true,1,2,7 | e.transform t.position p.x p.y e.id', text)
        text = run(function()
            local function f({transform: {position: {x}}}, {id}) return x, id end
            return f(entity(), entity())
        end)
        assert(text == 'true,1,7 | e.transform t.position p.x e.id', text)
        text = run(function()
            return #[for {id, active, position: {z}} in { rec('a', { id = 1, active = false, position = rec('pa', { z = 1 }) }) } if active => id]
        end)
        -- `id` is read before the filter rejects the element, and nothing is read lazily.
        assert(text == 'true,0 | a.id a.active a.position pa.z', text)
    "
        ))
        .unwrap();
}

#[test]
fn generated_holders_never_capture_or_shadow_source_names() {
    let runtime = Runtime::new().unwrap();
    // The holder of `{a}` is keyed by its brace's offset; a source name spelled like that stem,
    // or extending it with '_', salts it rather than being shadowed by it.
    for user in ["", "_1", "_1_x", "_g0_src"] {
        // The brace's offset depends on the name's length: solve for the fixed point.
        let mut offset = 0;
        let (name, source) = loop {
            let name = format!("__l3i_comp_{offset}{user}");
            let source = format!(
                "local {name} = 'user' local {{a}} = {{ a = {name} }} assert(a == 'user' and {name} == 'user')"
            );
            let brace = source.find("{a}").unwrap();
            if brace == offset {
                break (name, source);
            }
            offset = brace;
        };
        let lowered = lowering_snapshot(&source, LoweringMode::Compile).unwrap();
        let generated = lowered.lines().next().unwrap();
        // The holder is the local the table constructor initializes.
        let holder = generated.split(" = { a = ").next().unwrap().rsplit("local ").next().unwrap();
        assert!(holder.starts_with(&format!("__l3i_comp_{offset}_")) && holder != name, "{generated}");
        assert_eq!(generated.matches(&format!("local {name} ")).count(), 1, "{generated}");
        runtime.exec(&source).unwrap_or_else(|error| panic!("{source}: {error}"));
    }
}

#[test]
fn incomplete_patterns_are_rejected_before_anything_runs() {
    let runtime = Runtime::new().unwrap();
    runtime.exec("effects = 0 function effect() effects += 1 end").unwrap();
    for prefix in [
        "local {",
        "local {x",
        "local {x,",
        "local {position:",
        "local {position: {",
        "local {position: {x",
        "local {x}",
        "local {x} =",
        "local {x}: Point",
        "function f({x,",
        "function f({x}",
        "return [for {x",
        "return [for {x} in",
        "local {x, x} = t",
        "local {id, ...rest} = t",
        "local {health = 100} = t",
        "local {[\"k\"]: v} = t",
        "local [a, b] = t",
        "local {x} = a, b",
        "for {id} in pairs(t) do end",
    ] {
        let source = format!("effect()\n{prefix}");
        let error = runtime.exec(&source).expect_err(&source).to_string();
        assert!(error.contains(":2:"), "{prefix}: the error is on the pattern's line: {error}");
    }
    runtime.exec("assert(effects == 0, 'a rejected chunk ran ' .. effects .. ' effects')").unwrap();
}

#[test]
fn runtime_errors_report_the_original_line_of_the_failing_field() {
    let runtime = Runtime::new().unwrap();
    let cases = [
        ("local {\n    a: {\n        b,\n    },\n} = {}\nreturn b", 3),
        ("local function f({\n    position: {\n        x,\n    },\n})\n    return x\nend\nreturn f({})", 3),
        (
            "local ids = [\n    for {\n        id,\n        position: {\n            z,\n        },\n    } in { {} }\n    => z\n]",
            5,
        ),
    ];
    for (source, line) in cases {
        let error = runtime.exec(source).expect_err(source).to_string();
        assert!(error.contains(&format!(":{line}: attempt to index nil")), "{source}\n{error}");
    }
}

#[test]
fn comments_and_interpolation_do_not_disturb_patterns() {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(
            r"
        local {
            -- the first field
            a, --[[ an inline note ]] b: renamed, -- trailing
            c, --[==[ long ]==]
        } = { a = 1, b = 2, c = 3 }
        assert(a == 1 and renamed == 2 and c == 3)
        local text = `{(function({x}) return x end)({ x = 'in' })}-{a}`
        assert(text == 'in-1', text)
        local s = '{x} = local {y}' -- strings and comments are not patterns: local {z}
        assert(s == '{x} = local {y}')
        local function f({v} --[[ after ]]: { v: number }, --[[ before ]] {w})
            return v + w
        end
        assert(f({ v = 1 }, { w = 2 }) == 3)
    ",
        )
        .unwrap();
}

struct InstructionCounter(Rc<Cell<u64>>);

impl RuntimeHooks for InstructionCounter {
    fn debug_step(&self, _stack: &Stack<'_>, _info: &DebugInfo) -> DebugAction {
        self.0.set(self.0.get() + 1);
        DebugAction::Continue
    }
}

/// Executed VM instructions of `body` over `values`, an array of `items` records.
fn executed_instructions(body: &str, items: u32) -> u64 {
    let runtime = Runtime::new().unwrap();
    runtime
        .exec(&format!(
            "values = table.create({items}) for i = 1, {items} do values[i] = {{ x = i, y = i * 2, z = i * 3, pos = {{ x = i }} }} end"
        ))
        .unwrap();
    let function = runtime.load_function(&format!("return function() {body} end")).unwrap();
    let steps = Rc::new(Cell::new(0));
    runtime.set_hooks(InstructionCounter(steps.clone()), HookSet::DEBUGGER);
    let thread = runtime.new_thread().unwrap();
    let stack = runtime.stack();
    thread
        .with_stack(&stack, |scope| {
            scope.single_step(true);
            Ok(())
        })
        .unwrap();
    assert!(matches!(thread.start(&stack, &function, ()).unwrap(), l3i::thread::Resume::Finished(_)));
    drop(stack);
    runtime.clear_hooks();
    steps.get()
}

/// (name, JSL, the same-contract handwritten Luau, extra executed instructions per element)
const SHAPES: &[(&str, &str, &str, u64)] = &[
    (
        "two fields from a call",
        "local total = 0 for i = 1, #values do local {x, y} = values[i] total += x + y end return total",
        "local total = 0 for i = 1, #values do local s = values[i] local x = s.x local y = s.y total += x + y end return total",
        0,
    ),
    (
        "three fields",
        "local total = 0 for i = 1, #values do local {x, y, z} = values[i] total += x + y + z end return total",
        "local total = 0 for i = 1, #values do local s = values[i] local x = s.x local y = s.y local z = s.z total += x + y + z end return total",
        0,
    ),
    (
        "nested",
        "local total = 0 for i = 1, #values do local {pos: {x}, y} = values[i] total += x + y end return total",
        "local total = 0 for i = 1, #values do local s = values[i] local p = s.pos local x = p.x local y = s.y total += x + y end return total",
        0,
    ),
    (
        // The holder of a local value is free: Luau aliases a local initialized from a local
        // when neither is assigned, so `local {x, y} = e` reads straight from `e`'s register.
        "two fields from a local",
        "local total = 0 for i = 1, #values do local e = values[i] local {x, y} = e total += x + y end return total",
        "local total = 0 for i = 1, #values do local e = values[i] local x = e.x local y = e.y total += x + y end return total",
        0,
    ),
    (
        "typed parameter",
        "type R = { x: number, y: number } local function f({x, y}: R) return x + y end \
         local total = 0 for i = 1, #values do total += f(values[i]) end return total",
        "type R = { x: number, y: number } local function f(p: R) local x = p.x local y = p.y return x + y end \
         local total = 0 for i = 1, #values do total += f(values[i]) end return total",
        0,
    ),
];

#[test]
fn executed_instructions_match_handwritten_extraction() {
    for items in [0, 1, 32, 1024] {
        for (name, jsl, luau, extra) in SHAPES {
            let surface = executed_instructions(jsl, items);
            let handwritten = executed_instructions(luau, items);
            println!("{name}, {items} records: JSL {surface}, Luau {handwritten}");
            assert_eq!(surface, handwritten + extra * u64::from(items), "{name}, {items} records");
        }
        // Generators: the comprehension's one setup instruction, nothing per element.
        let guard =
            "if value == nil then error('L3i comprehension projection produced nil; filter nil explicitly') end";
        let sum = executed_instructions("return sum[for {x, y} in values if x % 2 == 0 => x + y]", items);
        let sum_loop = executed_instructions(
            &format!(
                "local src = values local total = 0 for i = 1, #src do local e = src[i] local x = e.x local y = e.y \
                 if x % 2 == 0 then local value = x + y {guard} total += value end end return total"
            ),
            items,
        );
        let dense = executed_instructions("return [for {pos: {x}} in values => x]", items);
        let dense_loop = executed_instructions(
            &format!(
                "local src = values local n = #src local out = table.create(n) for i = 1, n do local e = src[i] \
                 local p = e.pos local x = p.x local value = x {guard} out[i] = value end return out"
            ),
            items,
        );
        println!("generators, {items} records: sum {sum} / {sum_loop}, dense {dense} / {dense_loop}");
        assert_eq!(sum, sum_loop + 1, "{items} records");
        assert_eq!(dense, dense_loop + 1, "{items} records");
    }
}

/// The opcodes of the function a chunk returns, operands dropped: the code shape to compare.
/// It is the listing's second to last function; the last is the chunk itself, and a
/// comprehension's inlined wrapper leaves a dead prototype before it.
fn opcodes(listing: &str) -> Vec<String> {
    let functions: Vec<&str> = listing.split("Function ").collect();
    let entry = functions[functions.len() - 2];
    entry
        .lines()
        .filter_map(|line| {
            let code = line.split_once(": ").map_or(line, |(_, rest)| rest).trim();
            let op = code.split_whitespace().next()?;
            (op.chars().all(|c| c.is_ascii_uppercase() || c == '_') && op.len() > 2).then(|| op.to_owned())
        })
        .collect()
}

#[test]
fn bytecode_is_the_handwritten_extraction() {
    let options = CompileOptions::default();
    let pairs = [
        (
            "return function(p) local {x, y} = p.inner return x + y end",
            "return function(p) local s = p.inner local x = s.x local y = s.y return x + y end",
        ),
        (
            "return function({x, y: {z}}) return x + z end",
            "return function(p) local x = p.x local s = p.y local z = s.z return x + z end",
        ),
        (
            "return function(xs) return sum[for {x, y} in xs if x > 0 => x + y] end",
            "return function(xs) local t = 0 for i = 1, #xs do local e = xs[i] local x = e.x local y = e.y if x > 0 then \
             local v = x + y if v == nil then error('L3i comprehension projection produced nil; filter nil explicitly') end t += v end end return t end",
        ),
    ];
    for (jsl, luau) in pairs {
        let surface = disassemble(jsl, &options).unwrap();
        let handwritten = disassemble(luau, &options).unwrap();
        let (jsl_ops, luau_ops) = (opcodes(&surface), opcodes(&handwritten));
        for op in ["NEWTABLE", "DUPTABLE", "NEWCLOSURE", "DUPCLOSURE", "CALL", "FASTCALL", "GETTABLEKS"] {
            let count = |ops: &[String]| ops.iter().filter(|o| *o == op).count();
            assert_eq!(count(&jsl_ops), count(&luau_ops), "{op} in {jsl}\n{surface}\n{handwritten}");
        }
        assert!(jsl_ops.len() <= luau_ops.len() + 1, "{jsl}\n{surface}\n{handwritten}");
    }
}

#[test]
fn the_exit_program_runs() {
    Runtime::new()
        .unwrap()
        .exec(
            r"
        type Position = {
            x: number,
            y: number,
            z: number,
        }

        type Entity = {
            id: number,
            active: boolean,
            position: Position,
        }

        function projectedIds(entities: {Entity})
            return [
                for {id, active, position: {z}} in entities
                if active and z > 0
                => id
            ]
        end

        local ids = projectedIds({
            { id = 1, active = true, position = { x = 0, y = 0, z = 2 } },
            { id = 2, active = false, position = { x = 0, y = 0, z = 3 } },
            { id = 3, active = true, position = { x = 0, y = 0, z = -1 } },
            { id = 4, active = true, position = { x = 0, y = 0, z = 9 } },
        })
        assert(#ids == 2 and ids[1] == 1 and ids[2] == 4)
    ",
        )
        .unwrap();
}

/// The locals `lua_getlocal` lists in the probe's caller, with their values as text.
fn listed_locals(source: &str, debug_level: u8) -> Vec<String> {
    use l3i::bind::Call;
    let runtime = Runtime::new().unwrap();
    let seen: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    let record = seen.clone();
    let probe = runtime
        .bind_function("dreamweave.probe", move |call: &Call| {
            let mut n = 1;
            while let Some((name, view)) = call.local(1, n) {
                let value = view.read::<f64>().map_or_else(|_| "record".to_owned(), |v| v.to_string());
                record.borrow_mut().push(format!("{name}={value}"));
                n += 1;
            }
        })
        .unwrap();
    runtime.set_global("probe", &probe).unwrap();
    let options = CompileOptions { optimization_level: 1, debug_level, ..CompileOptions::default() };
    runtime
        .stack()
        .with_frame(|frame| {
            runtime.load(frame, "=locals", source, &options)?.as_function()?.invoke::<(), ()>(frame, ())
        })
        .unwrap();
    seen.take()
}

#[test]
fn debuggers_see_bound_names_and_honestly_named_holders() {
    let source = "local function f({x, y: {z}}, k)\n  local {a} = { a = x + z + k }\n  probe()\n  return a\nend\nf({ x = 1, y = { z = 2 } }, 3)";
    // Debug level 1, the default, records no local names at all: nothing to hide or to see.
    assert_eq!(listed_locals(source, 1), Vec::<String>::new());
    // Debug level 2 lists every register-allocated local, generated or not; Luau has no way to
    // mark one hidden. The holders appear under their generated names holding the very record
    // they destructure (the first is the real argument), and every bound name holds its field.
    assert_eq!(
        listed_locals(source, 2),
        ["__l3i_comp_17=record", "k=3", "x=1", "__l3i_comp_24=record", "z=2", "__l3i_comp_41=record", "a=6"]
    );
}
