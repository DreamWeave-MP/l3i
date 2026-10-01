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
