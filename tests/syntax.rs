//! `@dream/luau` through Luau: the tree has Luau's kinds and members with exact spans, a local
//! is one table for its declaration and every use, comments, hot comments and errors are data,
//! and the token stream covers every byte that is not whitespace.

use l3i::Runtime;
use l3i::extension::{RuntimePlan, RuntimePolicy};
use l3i::syntax::SyntaxExtension;

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/luau", "luau"))
        .extension(SyntaxExtension)
        .finalize()
        .unwrap();
    Runtime::from_plan(&plan).unwrap()
}

#[test]
fn the_tree_has_luaus_kinds_members_and_exact_spans() {
    runtime()
        .exec(
            r#"
            local source = "--!strict\nlocal x, y = 1, 'two'\n\nif x then\n  print(`a{x}`, \"b\", [[c]])\nend\n"
            local result = luau.parse(source)
            assert(#result.errors == 0, 'no errors')
            assert(#result.lineStarts == 7 and result.lineStarts[2] == 11, 'line starts')
            local root = result.root
            assert(root.kind == 'StatBlock' and #root.body == 2, 'two statements')

            local declare = root.body[1]
            assert(declare.kind == 'StatLocal' and declare.line == 2 and declare.column == 1, 'local span')
            assert(declare.endLine == 2 and declare.endColumn == 21, 'end column is inclusive: ' .. declare.endColumn)
            local text = string.sub(source, result.lineStarts[declare.line] + declare.column - 1, result.lineStarts[declare.endLine] + declare.endColumn - 1)
            assert(text == "local x, y = 1, 'two'", text)
            assert(declare.vars[1].kind == 'Local' and declare.vars[1].name == 'x' and declare.vars[2].name == 'y', 'locals')
            assert(declare.values[1].kind == 'ExprConstantNumber' and declare.values[1].value == 1, 'number')
            assert(declare.values[2].quoteStyle == 'single' and declare.values[2].value == 'two', 'single quotes')

            local branch = root.body[2]
            assert(branch.kind == 'StatIf' and branch.condition.kind == 'ExprLocal', 'if')
            assert(branch.condition['local'] == declare.vars[1], 'a use is its declaration')
            assert(branch.thenLocation and branch.thenLocation.line == 4, 'then keyword')
            local call = branch.thenbody.body[1].expr
            assert(call.kind == 'ExprCall' and call.func.kind == 'ExprGlobal' and call.func.name == 'print', 'call')
            assert(call.args[1].kind == 'ExprInterpString' and call.args[1].strings[1] == 'a', 'interpolated')
            assert(call.args[2].quoteStyle == 'double' and call.args[3].quoteStyle == 'long', 'quote styles')
            assert(call.argLocation.line == 5, 'argument list span')

            assert(#result.hotComments == 1 and result.hotComments[1].content == 'strict' and result.hotComments[1].header, 'hot comment')
            assert(#result.comments == 1 and result.comments[1].kind == 'line' and result.comments[1].line == 1, 'comment')
            "#,
        )
        .unwrap();
}

#[test]
fn methods_functions_tables_and_types_keep_their_shape() {
    runtime()
        .exec(
            r"
            local result = luau.parse([[
local M = {}

function M:go(a: number, ...: string): (boolean, ...any)
  local t = { 1, key = a, [a] = `x` }
  return t.key == a, ...
end

type Pair<T> = { first: T, second: T? }
export type Fn = (name: string) -> ()
return M
]])
            assert(#result.errors == 0, result.errors[1] and result.errors[1].message)
            local body = result.root.body
            local method = body[2]
            assert(method.kind == 'StatFunction' and method.name.kind == 'ExprIndexName' and method.name.op == ':', 'method name')
            local func = method.func
            assert(func.self and func.self.name == 'self' and #func.args == 1 and func.vararg, 'self, args, vararg')
            assert(func.args[1].annotation.kind == 'TypeReference' and func.args[1].annotation.name == 'number', 'annotation')
            assert(func.varargAnnotation.kind == 'TypePackVariadic', 'vararg annotation')
            assert(func.returnAnnotation.kind == 'TypePackExplicit' and func.returnAnnotation.typeList.tailType.kind == 'TypePackVariadic', 'return pack')

            local items = func.body.body[1].values[1].items
            assert(items[1].itemKind == 'list' and items[2].itemKind == 'record' and items[3].itemKind == 'general', 'item kinds')
            assert(items[2].key.quoteStyle == 'unquoted', 'a record key is unquoted')
            assert(items[3].value.kind == 'ExprConstantString' and items[3].value.quoteStyle == 'backtick', 'an interpolation with no holes is a constant')
            local returned = func.body.body[2]
            assert(returned.kind == 'StatReturn' and returned.list[1].op == '==' and returned.list[2].kind == 'ExprVarargs', 'return')

            local alias = body[3]
            assert(alias.kind == 'StatTypeAlias' and alias.name == 'Pair' and alias.generics[1].name == 'T' and not alias.exported, 'alias')
            assert(alias.type.kind == 'TypeTable' and alias.type.props[2].type.kind == 'TypeUnion', 'optional is a union')
            local exported = body[4]
            assert(exported.exported and exported.type.kind == 'TypeFunction' and exported.type.argNames[1].name == 'name', 'function type')
            ",
        )
        .unwrap();
}

#[test]
fn syntax_errors_are_data_and_the_tree_recovers() {
    runtime()
        .exec(
            r"
            local result = luau.parse('local x = \nprint(')
            assert(#result.errors >= 1, 'an error')
            local first = result.errors[1]
            assert(first.kind == 'Error' and type(first.message) == 'string' and first.line >= 1, first.message)
            assert(result.root.kind == 'StatBlock', 'a tree anyway')

            local declarations = 'declare function f(x: number): string'
            assert(#luau.parse(declarations).errors > 0, 'declarations need the option')
            local declared = luau.parse(declarations, { declarations = true })
            assert(#declared.errors == 0 and declared.root.body[1].kind == 'StatDeclareFunction', 'declarations parse with it')
            assert(declared.root.body[1].paramNames[1].name == 'x', 'parameter names')

            local ok, err = pcall(luau.parse, 'x', { token = true })
            assert(not ok and string.find(err, 'token'), 'unknown options fail: ' .. tostring(err))
            ok, err = pcall(luau.parse, 42)
            assert(not ok, 'a number is not a source')
            ",
        )
        .unwrap();
}

#[test]
fn tokens_cover_every_byte_that_is_not_whitespace() {
    runtime()
        .exec(
            r#"
            local source = "local s = 'a' -- note\n--[[ block ]] return `x{s}y`, 1.5 >= 2"
            local result = luau.parse(source, { tokens = true })
            local kinds = luau.tokenKinds
            assert(kinds.name == 1 and kinds.error == 14, 'numbered from 1')
            local tokens = result.tokens
            assert(tokens and buffer.len(tokens) % 12 == 0, 'records')
            local texts, names = {}, {}
            for at = 0, buffer.len(tokens) - 12, 12 do
              local kind, first, last = buffer.readu32(tokens, at), buffer.readu32(tokens, at + 4), buffer.readu32(tokens, at + 8)
              table.insert(texts, string.sub(source, first, last))
              for name, number in kinds do
                if number == kind then
                  table.insert(names, name)
                end
              end
            end
            local joined = table.concat(texts, ' ')
            assert(joined == "local s = 'a' -- note --[[ block ]] return `x{ s }y` , 1.5 >= 2", joined)
            assert(table.concat(names, ' ') == 'keyword name symbol string comment blockComment keyword interpolatedBegin name interpolatedEnd symbol number symbol number', table.concat(names, ' '))
            assert(luau.parse(source).tokens == nil, 'only on request')
            assert(not pcall(function() luau.tokenKinds.name = 2 end), 'read-only')
            "#,
        )
        .unwrap();
}

#[test]
fn deep_nesting_is_bounded_by_the_parser_not_the_stack() {
    let runtime = runtime();
    runtime
        .exec(
            "local deep = string.rep('(', 150) .. '1' .. string.rep(')', 150) \
             local result = luau.parse('return ' .. deep) \
             assert(#result.errors == 0, result.errors[1] and result.errors[1].message) \
             local node = result.root.body[1].list[1] \
             local depth = 0 \
             while node.kind == 'ExprGroup' do node = node.expr depth += 1 end \
             assert(depth == 150 and node.value == 1, depth) \
             local tooDeep = luau.parse('return ' .. string.rep('(', 5000) .. '1' .. string.rep(')', 5000)) \
             assert(#tooDeep.errors > 0, 'the parser refuses past its recursion limit')",
        )
        .unwrap();
}

/// The tree is built while the collector runs in its smallest steps: every node of every
/// parse survives the collections that happen between and during builds, with its fields and
/// shared locals intact.
#[test]
fn trees_survive_a_collector_that_steps_on_every_allocation() {
    use l3i::memory::GcControl;

    let runtime = runtime();
    runtime.gc(GcControl::SetGoal(100));
    runtime.gc(GcControl::SetStepMultiplier(1000));
    runtime.gc(GcControl::SetStepSize(1));
    runtime
        .exec(
            r"
            local parts = {}
            for i = 1, 200 do
              table.insert(parts, `local value{i}: number = {i}\nlocal function f{i}(a, b) return a + b * value{i}, 'x{i}', \{ a, b = b \} end\nprint(f{i}(value{i}, 2))\n`)
            end
            local source = table.concat(parts)
            local results = {}
            for round = 1, 8 do
              results[round] = luau.parse(source, { tokens = true })
              local garbage = {}
              for i = 1, 2000 do
                garbage[i] = { i, tostring(i) }
              end
            end

            local count = 0
            local function check(node)
              if type(node) ~= 'table' then
                return
              end
              if node.kind then
                assert(type(node.kind) == 'string', 'kind')
                if node.line then
                  assert(type(node.line) == 'number' and type(node.endColumn) == 'number', node.kind)
                end
                count += 1
              end
              for key, value in node do
                if key ~= 'local' and key ~= 'shadow' and type(value) == 'table' then
                  check(value)
                end
              end
            end
            for _, result in results do
              assert(#result.errors == 0, result.errors[1] and result.errors[1].message)
              assert(#result.root.body == 600, #result.root.body)
              check(result.root)
              local declared = result.root.body[1].vars[1]
              local function_ = result.root.body[2].func
              local read = function_.body.body[1].list[1].right.right
              assert(read.kind == 'ExprLocal' and read['local'] == declared and declared.name == 'value1', 'shared local')
              assert(buffer.len(result.tokens) % 12 == 0, 'tokens')
            end
            assert(count > 8 * 600 * 5, count)
            ",
        )
        .unwrap();
}

// All expectations below refer to original bytes, never to the lowering's generated source.
fn surface_test(script: &str) {
    runtime()
        .exec(&format!(
            r"
        local function text(source, result, node)
            return string.sub(source, result.lineStarts[node.line] + node.column - 1,
                result.lineStarts[node.endLine] + node.endColumn - 1)
        end
        local function span(source, result, node, expected)
            assert(node and text(source, result, node) == expected,
                expected .. ': ' .. (node and text(source, result, node) or 'nil'))
        end
        local function equal(a, b, path)
            path = path or 'root'
            assert(type(a) == type(b), path .. ': types')
            if type(a) ~= 'table' then assert(a == b, path .. ': ' .. tostring(a) .. ' ~= ' .. tostring(b)) return end
            for k, v in a do equal(v, b[k], path .. '.' .. tostring(k)) end
            for k in b do assert(a[k] ~= nil, path .. ': extra ' .. tostring(k)) end
        end
        {script}
    "
        ))
        .unwrap();
}

#[test]
fn malformed_expressions_keep_clause_boundaries_and_error_spans() {
    surface_test(
        r"
        local source = 'return [for x in xs. if keep(x) => x]'
        local result = luau.parse(source)
        local comp = result.root.body[1].list[1]
        assert(comp.kind == 'ExprComprehension' and not comp.complete and #result.errors > 0)
        assert(#comp.clauses == 2 and comp.clauses[2].kind == 'ComprehensionFilter')
        local bad = comp.clauses[1].source
        -- Stock expression recovery may retain an index with a missing name. A
        -- fallback error may retain its prefix, but a bare global cannot drop '.'.
        assert((bad.kind == 'ExprError' and not bad.isMissing) or
               (bad.kind == 'ExprIndexName' and bad.index == '%error-id%'))
        span(source, result, bad, 'xs.')
        local receiver = if bad.kind == 'ExprIndexName' then bad.expr else bad.expressions[1]
        assert(receiver.name == 'xs')
        assert(comp.clauses[2].condition.args[1]['local'] == comp.clauses[1].binding)
        local closed = luau.parse('return [for x in xs => x.]')
        assert(not closed.root.body[1].list[1].complete)
        local projection = closed.root.body[1].list[1].projection
        assert(projection.kind == 'ExprError' or (projection.kind == 'ExprIndexName' and projection.index == '%error-id%'))
        span('return [for x in xs => x.]', closed, projection, 'x.')

        local recovered = luau.parse('return f([for x in (xs => x, 42), 99)')
        assert(#recovered.errors > 0)
        local call = recovered.root.body[1].list[1]
        assert(call.kind == 'ExprCall' and #call.args == 2)
        assert(call.args[1].kind == 'ExprComprehension' and call.args[2].value == 42)
        assert(call.args[1].projection['local'] == call.args[1].clauses[1].binding)
    ",
    );
}

#[test]
fn invalid_postfix_comprehensions_never_expose_generated_functions() {
    surface_test(
        r"
        local function noHelpers(value, seen)
            if type(value) ~= 'table' then return end
            seen = seen or {}
            if seen[value] then return end
            seen[value] = true
            assert(value.kind ~= 'ExprFunction', 'generated function leaked')
            if value.kind == 'Local' or value.kind == 'ExprGlobal' then
                assert(not string.find(value.name, '__l3i_comp_', 1, true), 'generated name leaked')
            end
            for _, child in value do noHelpers(child, seen) end
        end
        for _, source in {'return values[for x in xs => x]', 'return values.sum[for x in xs => x]',
                          'return values<<number>>[for x in xs => x]'} do
            local result = luau.parse(source)
            assert(#result.errors > 0, 'invalid postfix should have a source diagnostic')
            local expr = result.root.body[1].list[1]
            assert(expr.kind == 'ExprIndexExpr' and expr.index.kind == 'ExprComprehension')
            assert(not expr.index.complete)
            noHelpers(result.root)
        end
    ",
    );
}

#[test]
fn repeat_condition_comprehension_scope_matches_enclosing_source_depth() {
    surface_test(
        r"
        local result = luau.parse('repeat until [for x in {1} => x]')
        assert(#result.errors == 0)
        local comp = result.root.body[1].condition
        assert(comp.kind == 'ExprComprehension')
        assert(comp.clauses[1].binding.loopDepth == 1)
        assert(comp.clauses[1].binding.functionDepth == 0 and not comp.projection.upvalue)
    ",
    );
}

#[test]
fn surface_comprehensions_keep_order_dependent_bindings_nested_projection_and_spans() {
    surface_test(
        r"
        local source = 'local rows = {{1, 2}}\nreturn [for row in rows if #row > 0 for x in row if x > 0 if x < 3 => [for y in {x} => y + x]]'
        local r = luau.parse(source)
        assert(#r.errors == 0, r.errors[1] and r.errors[1].message)
        local c = r.root.body[2].list[1]
        assert(c.kind == 'ExprComprehension' and c.complete and c.hasClose and c.hasArrow)
        local begin = string.find(source, '[for', 1, true)
        span(source, r, c, string.sub(source, begin))
        span(source, r, c.openLocation, '[')
        span(source, r, c.closeLocation, ']')
        span(source, r, c.arrowLocation, '=>')
        assert(#c.clauses == 5)
        local row, x = c.clauses[1], c.clauses[3]
        assert(row.kind == 'ComprehensionGenerator' and row.hasIn and row.binding.kind == 'Local')
        assert(row.source['local'] == r.root.body[1].vars[1])
        span(source, r, row.keywordLocation, 'for')
        span(source, r, row.inLocation, 'in')
        span(source, r, row.binding, 'row')
        span(source, r, row.source, 'rows')
        assert(x.kind == 'ComprehensionGenerator' and x.source['local'] == row.binding)
        for _, i in {2, 4, 5} do
            assert(c.clauses[i].kind == 'ComprehensionFilter')
            span(source, r, c.clauses[i].keywordLocation, 'if')
        end
        assert(c.clauses[2].condition.left.expr['local'] == row.binding)
        assert(c.clauses[4].condition.left['local'] == x.binding)
        assert(c.clauses[5].condition.left['local'] == x.binding)
        local nested = c.projection
        assert(nested.kind == 'ExprComprehension' and nested.complete)
        assert(nested.clauses[1].source.items[1].value['local'] == x.binding)
        assert(nested.projection.left['local'] == nested.clauses[1].binding)
        assert(nested.projection.right['local'] == x.binding and not nested.projection.right.upvalue)
        span(source, r, nested.projection, 'y + x')
    ",
    );
}

#[test]
fn surface_shadowing_and_real_function_captures_exclude_generated_scopes() {
    surface_test(
        r"
        local source = 'local x = {1}\nreturn [for x in x for x in {x} => function(p) local kept = p; return x, kept end], x'
        local r = luau.parse(source)
        assert(#r.errors == 0, r.errors[1] and r.errors[1].message)
        local outer = r.root.body[1].vars[1]
        local c = r.root.body[2].list[1]
        local first, second = c.clauses[1], c.clauses[2]
        assert(first.source['local'] == outer and first.binding ~= outer and first.binding.shadow == outer)
        assert(second.source.items[1].value['local'] == first.binding)
        assert(second.binding ~= first.binding and second.binding.shadow == first.binding)
        assert(outer.functionDepth == first.binding.functionDepth and first.binding.functionDepth == second.binding.functionDepth)
        assert(first.binding.loopDepth == outer.loopDepth + 1 and second.binding.loopDepth == first.binding.loopDepth + 1)
        assert(not first.source.upvalue and not second.source.items[1].value.upvalue)
        local fn = c.projection
        assert(fn.kind == 'ExprFunction' and fn.functionDepth == outer.functionDepth + 1)
        local capture = fn.body.body[2].list[1]
        assert(capture['local'] == second.binding and capture.upvalue)
        assert(r.root.body[2].list[2]['local'] == outer)
        span(source, r, fn, 'function(p) local kept = p; return x, kept end')
        local stock = luau.parse('return function(p) local kept = p; return x, kept end', {dialect = 'luau'}).root.body[1].list[1]
        assert(fn.args[1].loopDepth == stock.args[1].loopDepth and fn.args[1].functionDepth == stock.args[1].functionDepth)
        assert(fn.body.body[1].vars[1].loopDepth == stock.body.body[1].vars[1].loopDepth)
        assert(fn.body.body[2].list[2]['local'] == fn.body.body[1].vars[1] and not fn.body.body[2].list[2].upvalue)
    ",
    );
}

#[test]
fn surface_reduction_and_length_are_source_nodes_and_stock_is_opt_in() {
    surface_test(
        r"
        local source = 'local sum = 99; return sum[for x in {1} => x], #[for y in {2} => y]'
        local r = luau.parse(source)
        assert(#r.errors == 0, r.errors[1] and r.errors[1].message)
        local reduction, length = r.root.body[2].list[1], r.root.body[2].list[2]
        assert(reduction.kind == 'ExprReduction' and reduction.op == 'sum')
        assert(reduction.expr.kind == 'ExprComprehension' and reduction.expr.complete)
        span(source, r, reduction, 'sum[for x in {1} => x]')
        span(source, r, reduction.expr, '[for x in {1} => x]')
        assert(length.kind == 'ExprUnary' and length.op == '#' and length.expr.kind == 'ExprComprehension')
        span(source, r, length, '#[for y in {2} => y]')
        assert(#luau.parse(source, {dialect = 'luau'}).errors > 0)
        assert(not pcall(luau.parse, source, {dialect = 'unknown'}))
        local ordinary = 'local x = {1}; for _, n in x do local f = function() return n end; print(f()) end'
        equal(luau.parse(ordinary).root, luau.parse(ordinary, {dialect = 'luau'}).root)
        local adjacent = 'local a = [for n in {1} => n]; local x = {1}; for _, n in x do local f = function() return n end; print(f()) end'
        local stock = luau.parse(string.gsub(adjacent, '%[for n in {1} => n%]', '1' .. string.rep(' ', #'[for n in {1} => n]' - 1)), {dialect = 'luau'})
        local surface = luau.parse(adjacent)
        assert(#surface.errors == 0 and #stock.errors == 0)
        for i = 2, 3 do equal(surface.root.body[i], stock.root.body[i]) end
    ",
    );
}

#[test]
fn surface_tokens_cover_original_comments_interpolation_and_merged_arrows() {
    surface_test(
        r#"
        local source = "--!strict\nreturn [for x in {1} -- source\n if x > 0 --[[filter]]\n => `é{x}=>`], sum[for y in {2} => y]"
        local r = luau.parse(source, {tokens = true})
        assert(#r.errors == 0, r.errors[1] and r.errors[1].message)
        local names = {'name', 'keyword', 'number', 'string', 'longString', 'interpolatedBegin', 'interpolatedMid', 'interpolatedEnd', 'interpolatedSimple', 'comment', 'blockComment', 'attribute', 'symbol', 'error'}
        for i, name in names do assert(luau.tokenKinds[name] == i, name) end
        assert(buffer.len(r.tokens) % 12 == 0)
        local last, arrows = 0, 0
        for at = 0, buffer.len(r.tokens) - 12, 12 do
            local kind = buffer.readu32(r.tokens, at)
            local first, finish = buffer.readu32(r.tokens, at + 4), buffer.readu32(r.tokens, at + 8)
            assert(first > last and finish >= first and finish <= #source, 'ordered original spans')
            assert(not string.find(string.sub(source, last + 1, first - 1), '%S'), 'uncovered byte')
            local token = string.sub(source, first, finish)
            if token == '=>' then assert(kind == luau.tokenKinds.symbol) arrows += 1 end
            last = finish
        end
        assert(arrows == 2 and not string.find(string.sub(source, last + 1), '%S'))
        assert(#r.comments == 3 and #r.hotComments == 1 and r.hotComments[1].content == 'strict')
        span(source, r, r.comments[2], '-- source')
        span(source, r, r.comments[3], '--[[filter]]')
    "#,
    );
}

#[test]
fn every_surface_prefix_has_deterministic_errors_and_a_recoverable_ast() {
    surface_test(
        r"
        for _, source in {
            'return [for x in {1} if x > 0 if x < 3 => x + 1]',
            'return sum[for row in {{1}} for x in row => [for y in {x} => y]]',
            'return #[for x in {1} => function() return x end]',
            '[for x in xs => x]',
        } do
            for n = 0, #source do
                local prefix = string.sub(source, 1, n)
                local a, b = luau.parse(prefix), luau.parse(prefix)
                assert(a.root.kind == 'StatBlock', prefix)
                equal(a.errors, b.errors, prefix .. '.errors')
                equal(a.root, b.root, prefix .. '.ast')
                for _, err in a.errors do
                    assert(err.kind == 'Error' and #err.message > 0, prefix)
                    local begin = a.lineStarts[err.line] + err.column - 1
                    local finish = a.lineStarts[err.endLine] + err.endColumn - 1
                    assert(begin >= 1 and begin <= #prefix + 1 and finish >= begin - 1 and finish <= #prefix, prefix)
                end
                if string.sub(prefix, -1) == ']' and n == #source and string.sub(source, 1, 6) == 'return' then
                    assert(#a.errors == 0, prefix)
                elseif string.find(prefix, '[for', 1, true) and not string.find(prefix, ']', 1, true) then
                    assert(#a.errors > 0, prefix)
                end
            end
        end
        for _, source in {'return [for x in', 'return [for x in xs =>'} do
            local r = luau.parse(source)
            local c = r.root.body[1].list[1]
            assert(c.kind == 'ExprComprehension' and not c.complete and not c.hasClose and c.closeLocation == nil)
            local hole = c.hasArrow and c.projection or c.clauses[1].source
            assert(hole.kind == 'ExprError' and hole.isMissing and #hole.expressions == 0)
            assert(hole.line == 1 and hole.column == #source + 1 and hole.endColumn == #source)
        end
    ",
    );
}

#[test]
fn missing_surface_close_synchronizes_at_local_and_argument_or_table_commas() {
    surface_test(
        r"
        local source = 'local a = [for x in {1} => x\nlocal kept = 42\nreturn kept'
        local r = luau.parse(source)
        assert(#r.errors > 0 and #r.root.body == 3)
        local c = r.root.body[1].values[1]
        assert(c.kind == 'ExprComprehension' and not c.hasClose and not c.complete)
        span(source, r, c.projection, 'x')
        assert(r.root.body[2].vars[1].name == 'kept' and r.root.body[2].values[1].value == 42)
        assert(r.root.body[3].list[1]['local'] == r.root.body[2].vars[1])
        for _, source in {'return f([for x in {1} => x, 42)', 'return {[for x in {1} => x, 42}'} do
            local r = luau.parse(source)
            assert(#r.errors > 0)
            local expr = r.root.body[1].list[1]
            local c, following
            if expr.kind == 'ExprCall' then c = expr.args[1] following = expr.args[2]
            else c = expr.items[1].value following = expr.items[2].value end
            assert(c.kind == 'ExprComprehension' and not c.hasClose)
            span(source, r, c.projection, 'x')
            assert(following.kind == 'ExprConstantNumber' and following.value == 42)
        end
    ",
    );
}

#[test]
fn surface_nodes_and_shared_bindings_survive_incremental_collection() {
    use l3i::memory::GcControl;

    let runtime = runtime();
    runtime.gc(GcControl::SetGoal(100));
    runtime.gc(GcControl::SetStepMultiplier(1000));
    runtime.gc(GcControl::SetStepSize(1));
    runtime.exec(r"
        local results = {}
        for i = 1, 40 do
            results[i] = luau.parse('local xs = {1}; return sum[for x in xs if x > 0 => [for y in {x} => y + x]], #[for z in xs => z]')
            local garbage = {}
            for j = 1, 1000 do garbage[j] = {j, tostring(j)} end
        end
        for _, r in results do
            assert(#r.errors == 0)
            local sum, length = r.root.body[2].list[1], r.root.body[2].list[2]
            assert(sum.kind == 'ExprReduction' and sum.op == 'sum' and length.op == '#')
            local c = sum.expr
            assert(c.clauses[1].source['local'] == r.root.body[1].vars[1])
            assert(c.clauses[2].condition.left['local'] == c.clauses[1].binding)
            assert(c.projection.projection.left['local'] == c.projection.clauses[1].binding)
            assert(c.projection.projection.right['local'] == c.clauses[1].binding)
            assert(length.expr.projection['local'] == length.expr.clauses[1].binding)
        end
    ").unwrap();
}

#[test]
fn over_depth_surface_errors_keep_the_required_root() {
    surface_test(
        r"
        local source = 'return ' .. string.rep('[for x in {1} => ', 500) .. 'x' .. string.rep(']', 500)
        local r = luau.parse(source)
        assert(#r.errors > 0, 'bounded surface recursion reports errors')
        assert(r.root and r.root.kind == 'StatBlock', 'over-depth surface parse must retain the ParseResult.root contract')
    ",
    );
}

#[test]
fn incomplete_surface_clauses_export_optional_locations_and_zero_width_holes() {
    surface_test(
        r"
        local source = 'return [for in xs => 1]'
        local r = luau.parse(source)
        local c = r.root.body[1].list[1]
        assert(#r.errors > 0 and c.kind == 'ExprComprehension' and not c.complete)
        local g = c.clauses[1]
        assert(g.binding == nil and g.hasIn and g.source.kind == 'ExprGlobal' and g.source.name == 'xs')
        span(source, r, g.inLocation, 'in')

        source = 'return [for x xs => x]'
        r = luau.parse(source)
        c = r.root.body[1].list[1]
        g = c.clauses[1]
        assert(#r.errors > 0 and not c.complete and not g.hasIn and g.inLocation == nil)
        assert(g.binding.name == 'x' and g.source.name == 'xs' and c.projection['local'] == g.binding)

        source = 'return [for x in xs if => x]'
        r = luau.parse(source)
        c = r.root.body[1].list[1]
        local filter = c.clauses[2]
        assert(#r.errors > 0 and not c.complete and c.hasArrow and c.hasClose)
        assert(filter.kind == 'ComprehensionFilter' and filter.condition.kind == 'ExprError' and filter.condition.isMissing)
        local insertion = string.find(source, '=>', 1, true)
        assert(filter.condition.line == 1 and filter.condition.column == insertion and filter.condition.endColumn == insertion - 1)
        span(source, r, filter.keywordLocation, 'if')
        assert(c.projection['local'] == c.clauses[1].binding)

        source = 'return [for x in xs]'
        r = luau.parse(source)
        c = r.root.body[1].list[1]
        assert(#r.errors > 0 and not c.complete and c.hasClose and not c.hasArrow and c.arrowLocation == nil)
        assert(c.projection.kind == 'ExprError' and c.projection.isMissing)
        insertion = string.find(source, ']', 1, true)
        assert(c.projection.column == insertion and c.projection.endColumn == insertion - 1)
        span(source, r, c.closeLocation, ']')
    ",
    );
}
