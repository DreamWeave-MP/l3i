// Standalone, dependency-free tests for L3i's source-level surface syntax pass.
// Build directly with either GCC or Clang; no Rust/Cargo or Luau checkout is required.

#include "surface_syntax.h"

#include <cassert>
#include <cstdlib>
#include <iostream>
#include <string>
#include <string_view>


namespace
{
std::string rewrite(std::string_view source, bool* changed = nullptr)
{
    size_t size = 0;
    char* memory = l3i_rewrite_surface_syntax(source.data(), source.size(), &size);
    if (changed)
        *changed = memory != nullptr;
    if (!memory)
        return std::string(source);
    std::string result(memory, size);
    std::free(memory);
    return result;
}

size_t occurrences(std::string_view text, std::string_view needle)
{
    size_t count = 0;
    size_t at = 0;
    while ((at = text.find(needle, at)) != std::string_view::npos)
    {
        ++count;
        at += needle.size();
    }
    return count;
}

void contains(std::string_view text, std::string_view needle)
{
    if (text.find(needle) == std::string_view::npos)
    {
        std::cerr << "missing: " << needle << "\nin: " << text << "\n";
        std::abort();
    }
}

void unchanged(std::string_view source)
{
    bool changed = true;
    const std::string result = rewrite(source, &changed);
    assert(!changed);
    assert(result == source);
}

L3i::Surface::Position positionAt(std::string_view text, size_t offset)
{
    L3i::Surface::Position result{0, 0};
    for (size_t i = 0; i < offset; ++i)
        if (text[i] == '\n')
            result = {result.line + 1, 0};
        else
            ++result.column;
    return result;
}

void samePosition(L3i::Surface::Position actual, L3i::Surface::Position expected)
{
    assert(actual.line == expected.line && actual.column == expected.column);
}

void copiedSpan(std::string_view original, const L3i::Surface::LoweredSource& lowered, std::string_view token)
{
    const size_t input = original.find(token);
    const size_t output = lowered.source.find(token);
    assert(input != std::string_view::npos && output != std::string::npos);
    const auto mapped = lowered.map.originalSpan({positionAt(lowered.source, output), positionAt(lowered.source, output + token.size())});
    samePosition(mapped.begin, positionAt(original, input));
    samePosition(mapped.end, positionAt(original, input + token.size()));
    samePosition(lowered.map.generatedPosition(positionAt(original, input)), positionAt(lowered.source, output));
    samePosition(lowered.map.generatedPosition(positionAt(original, input + token.size())), positionAt(lowered.source, output + token.size()));
}
} // namespace

int main()
{
    // Provenance is compositional across nested rewrites, exact at exclusive token ends,
    // and byte-based (CRLF and UTF-8 are not normalized).
    {
        const std::string source = "local prefix = 'é'\r\nlocal out = [for row in sourceRows()\r\n"
            " if row.keep => [for item in row.items if item.active => item.missing]]; local tail = 42\r\n";
        const auto lowered = L3i::Surface::lower(source);
        for (std::string_view token : {"'é'", "sourceRows()", "row.keep", "row.items", "item.active", "item.missing", "local tail = 42"})
            copiedSpan(source, lowered, token);
        samePosition(lowered.map.originalPosition(positionAt(lowered.source, lowered.source.size())), positionAt(source, source.size()));
        const auto whole = lowered.map.originalSpan({{0, 0}, positionAt(lowered.source, lowered.source.size())});
        samePosition(whole.begin, {0, 0});
        samePosition(whole.end, positionAt(source, source.size()));
        assert(lowered.map.generatedName("__l3i_comp_123_value"));
        assert(!lowered.map.generatedName("sourceRows"));
        assert(!lowered.map.originalLine(1)); // Several original clause lines share this generated line.
        const std::string message = "User number 123, text 'at line 999'; opener at line 2";
        const std::string mappedMessage = lowered.map.referenceText(message, message.rfind("at line 2"), {1, 0});
        contains(mappedMessage, "User number 123, text 'at line 999'");
        contains(mappedMessage, "ambiguous source reference");
    }
    {
        const std::string source = "local total = sum[for x in xs if x.keep => x.value]";
        const auto lowered = L3i::Surface::lower(source);
        assert(!lowered.map.empty());
        copiedSpan(source, lowered, "xs");
        copiedSpan(source, lowered, "x.keep");
        copiedSpan(source, lowered, "x.value");
        contains(lowered.source, "_sum +=");
        assert(lowered.source.find("table.create") == std::string::npos);
    }
    {
        const auto plain = L3i::Surface::lower("local xs = {1}\nreturn xs");
        assert(plain.map.empty());
        samePosition(plain.map.originalPosition({1, 7}), {1, 7});
        samePosition(plain.map.generatedPosition({1, 7}), {1, 7});
    }
    // Ordinary Luau and the old Python spelling remain untouched.  Most importantly, Luau's long
    // string syntax no longer has any relationship to comprehension recognition.
    unchanged("return xs[1]");
    unchanged("return [[long string [for x in xs => x]]]");
    unchanged("return [=[another long string => for]=]");
    unchanged("return '[for x in xs => x]'");
    unchanged("return \"[for x in xs => x]\"");
    unchanged("return `opaque [for x in xs => x] interpolation prototype`");
    unchanged("return [x * 2 for x in xs]");
    unchanged("-- [for x in xs => x]\nreturn xs");

    {
        const std::string out = rewrite("return [for x in values => x * 2]");
        contains(out, "local __l3i_comp_7_g0_src = values");
        contains(out, "table.create(__l3i_comp_7_g0_len)");
        contains(out, "local __l3i_comp_7_value = x * 2");
        contains(out, "if __l3i_comp_7_value == nil then error(\"L3i comprehension projection produced nil; filter nil explicitly\") end");
        contains(out, "__l3i_comp_7_out[__l3i_comp_7_g0_i] = __l3i_comp_7_value");
        assert(occurrences(out, "x * 2") == 1); // projection evaluated exactly once
        assert(occurrences(out, "values") == 1); // source evaluated exactly once
    }

    {
        const std::string out = rewrite("return [for x in makeValues() if x.active => x.id]");
        contains(out, "local __l3i_comp_7_g0_src = makeValues()");
        contains(out, "local __l3i_comp_7_n = 0");
        contains(out, "if x.active then");
        contains(out, "local __l3i_comp_7_value = x.id");
        contains(out, "__l3i_comp_7_n += 1");
        contains(out, "__l3i_comp_7_out[__l3i_comp_7_n] = __l3i_comp_7_value");
        assert(occurrences(out, "x.id") == 1);
        assert(occurrences(out, "makeValues()") == 1);
    }


    // Comprehensions are dense and non-nil by contract. Projection evaluation is explicit, happens
    // exactly once per accepted element, and is checked before insertion.
    {
        const std::string out = rewrite("return [for x in xs => maybe(x)]");
        contains(out, "local __l3i_comp_7_value = maybe(x)");
        contains(out, "if __l3i_comp_7_value == nil then error(\"L3i comprehension projection produced nil; filter nil explicitly\") end");
        assert(occurrences(out, "maybe(x)") == 1);
    }

    // Unary length is fused: do not allocate the comprehension result, but do preserve projection
    // evaluation and the non-nil invariant so side effects/throws match materialization followed by #.
    {
        const std::string out = rewrite("return #[for x in xs if accept(x) => effect(x)]");
        assert(out.find("table.create") == std::string::npos);
        assert(out.find("_out") == std::string::npos);
        contains(out, "if accept(x) then");
        contains(out, "local __l3i_comp_8_value = effect(x)");
        contains(out, "__l3i_comp_8_n += 1");
        contains(out, "return __l3i_comp_8_n");
        assert(occurrences(out, "effect(x)") == 1);
    }

    // Whitespace between # and the collection expression is still a unary-length expression and is
    // eligible for fusion.
    {
        const std::string out = rewrite("return #   [for x in xs => x]");
        assert(out.find("table.create") == std::string::npos);
        contains(out, "return __l3i_comp_11_n");
    }

    // JSL-owned sum reduction fuses the comprehension into a scalar accumulator. The projection
    // still executes exactly once and keeps the dense/non-nil comprehension contract.
    {
        const std::string out = rewrite("return sum[for x in xs if accept(x) => effect(x)]");
        assert(out.find("table.create") == std::string::npos);
        assert(out.find("_out") == std::string::npos);
        contains(out, "local __l3i_comp_10_sum = 0");
        contains(out, "if accept(x) then");
        contains(out, "local __l3i_comp_10_value = effect(x)");
        contains(out, "__l3i_comp_10_sum += __l3i_comp_10_value");
        contains(out, "return __l3i_comp_10_sum");
        assert(occurrences(out, "effect(x)") == 1);
    }

    // Empty sums are zero, nested generators remain nested, and inner sources stay data-dependent.
    {
        const std::string out = rewrite("return sum[for x in xs for y in children(x) if y.keep => y.value]");
        assert(out.find("table.create") == std::string::npos);
        contains(out, "local __l3i_comp_10_sum = 0");
        const size_t outerLoop = out.find("for __l3i_comp_10_g0_i");
        const size_t innerSource = out.find("local __l3i_comp_10_g1_src = children(x)");
        const size_t accumulation = out.find("__l3i_comp_10_sum += __l3i_comp_10_value");
        assert(outerLoop != std::string::npos && innerSource > outerLoop && accumulation > innerSource);
    }

    // Whitespace and comments are reducer trivia, so formatting does not silently disable fusion.
    {
        const std::string out = rewrite("return sum   [for x in xs => x]");
        assert(out.find("table.create") == std::string::npos);
        contains(out, "return __l3i_comp_13_sum");
        const std::string commented = rewrite("return sum -- reducer trivia\n [for x in xs => x]");
        assert(commented.find("table.create") == std::string::npos);
        contains(commented, "_sum = 0");
    }

    // Multiple filters remain one traversal and preserve short-circuit order through nested ifs.
    {
        const std::string out = rewrite("return [for x in xs if cheap(x) if expensive(x) => f(x)]");
        const size_t cheap = out.find("if cheap(x) then");
        const size_t expensive = out.find("if expensive(x) then");
        const size_t project = out.find("= f(x)");
        assert(cheap != std::string::npos && expensive > cheap && project > expensive);
    }

    // Nested comprehensions have no [[ lexical collision under the [for ... => ...] grammar.
    {
        const std::string out = rewrite("return [for row in rows => [for x in row => x * 2]]");
        assert(occurrences(out, "(function()") == 2);
        contains(out, "local __l3i_comp_27_g0_src = row");
        assert(out.find("[for") == std::string::npos);
    }

    // Multiple generators become actual nested numeric loops.  An inner data-dependent source is
    // emitted inside the outer loop, once per outer binding, not hoisted or duplicated.
    {
        const std::string out = rewrite("return [for x in xs for y in neighbors(x) => pair(x, y)]");
        const size_t outerLoop = out.find("for __l3i_comp_7_g0_i");
        const size_t innerSource = out.find("local __l3i_comp_7_g1_src = neighbors(x)");
        const size_t innerLoop = out.find("for __l3i_comp_7_g1_i");
        assert(outerLoop != std::string::npos && innerSource > outerLoop && innerLoop > innerSource);
        assert(occurrences(out, "neighbors(x)") == 1);
    }

    // Filters bind to the generator immediately preceding them.
    {
        const std::string out = rewrite(
            "return [for x in xs if visible(x) for y in children(x) if enabled(y) => {x, y}]");
        const size_t outerFilter = out.find("if visible(x) then");
        const size_t innerSource = out.find("children(x)");
        const size_t innerFilter = out.find("if enabled(y) then");
        assert(outerFilter != std::string::npos && innerSource > outerFilter && innerFilter > innerSource);
    }

    // Comprehensions are recursively recognized in sources, filters, and projections.
    {
        const std::string out = rewrite("return [for x in [for y in ys => y] => x]");
        assert(occurrences(out, "(function()") == 2);
        assert(out.find("[for") == std::string::npos);
    }
    {
        const std::string out = rewrite("return [for x in xs if #[for y in ys => y] > 0 => x]");
        assert(occurrences(out, "(function()") == 2);
        assert(out.find("[for") == std::string::npos);
    }

    // Nested punctuation, strings, comments, and arrows do not terminate expressions early.
    {
        const std::string out = rewrite(
            "return [for x in get({a = '[=>]'}, fn(1, 2)) if pred(x, {k = 'for if =>'}) => f(x, '=>')]");
        contains(out, "get({a = '[=>]'}, fn(1, 2))");
        contains(out, "pred(x, {k = 'for if =>'})");
        contains(out, "f(x, '=>')");
    }

    // A conditional source expression is unambiguous when grouped, even though `if` is also the
    // filter introducer at comprehension-clause depth zero.
    {
        const std::string out = rewrite("return [for x in (if flag then left else right) => x]");
        contains(out, "= (if flag then left else right)");
    }

    // A trailing line comment must end before generated code; quoted -- is not a comment.
    {
        const std::string out = rewrite("return [for x in xs -- source\n if x -- filter\n => x -- project\n]");
        contains(out, "xs -- source\n local");
        contains(out, "if x -- filter\n then");
        contains(out, "= x -- project\n if");
        const std::string quoted = rewrite("return [for x in xs => '-- not a comment']");
        contains(quoted, "= '-- not a comment' if");
    }

    // Length fusion may only consume a hash the scanner saw as code, not one in trivia.
    {
        const std::string out = rewrite("local out = -- #\n [for x in xs => x]");
        contains(out, "-- #\n (function()");
        contains(out, "table.create");
        const std::string unfused = rewrite("return # -- comment\n [for x in xs => x]");
        contains(unfused, "# -- comment\n (function()");
        contains(unfused, "table.create");
    }

    // Trivia between the opening bracket and sentinel is accepted.
    {
        const std::string out = rewrite("return [ -- hello\n for x in xs => x]");
        contains(out, "_g0_src = xs");
    }

    // Generated names do not collide textually with names already present in the original source.
    {
        const std::string out = rewrite("local __l3i_comp_32 = 1; return [for x in xs => x]");
        contains(out, "__l3i_comp_32_1_g0_src");
    }

    // Malformed surface forms are left for stock Luau diagnostics instead of being partially
    // rewritten into misleading generated code.
    unchanged("return [for in xs => x]");
    unchanged("return [for x xs => x]");
    unchanged("return [for x in => x]");
    unchanged("return [for x in xs =>]");
    unchanged("return [for x in xs if => x]");

    // C API edge behavior.
    {
        size_t n = 123;
        assert(l3i_rewrite_surface_syntax(nullptr, 0, &n) == nullptr);
        assert(n == 0);
        assert(l3i_rewrite_surface_syntax("x", 1, nullptr) == nullptr);
    }

    std::cout << "surface syntax tests passed\n";
}
