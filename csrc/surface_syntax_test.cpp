// Lowering and provenance tests using the canonical stock-token surface frontend.

#include "surface_syntax.h"

#include "Luau/Parser.h"

#include <algorithm>
#include <cassert>
#include <cstdint>
#include <utility>
#include <vector>
#include <cstdlib>
#include <iostream>
#include <string>
#include <string_view>


namespace
{
std::string rewrite(std::string_view source, bool* changed = nullptr)
{
    const auto lowered = L3i::Surface::lower(source);
    if (changed)
        *changed = !lowered.map.empty();
    return lowered.source;
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
    {
        const auto lowered = L3i::Surface::lower("--!native\n-- leading\nreturn [for x in xs => x]");
        assert(lowered.source.rfind("--!native\n-- leading\nlocal __l3i_comp_", 0) == 0);
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
        contains(out, "_table_create(__l3i_comp_7_g0_len)");
        contains(out, "local __l3i_comp_7_value = x * 2");
        contains(out, "_error(\"L3i comprehension projection produced nil; filter nil explicitly\") end");
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
        contains(out, "_error(\"L3i comprehension projection produced nil; filter nil explicitly\") end");
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
    {
        const std::string out = rewrite("return min[for x in xs => cost(x)]");
        contains(out, "local __l3i_comp_10_min = nil");
        contains(out, "if __l3i_comp_10_min == nil or __l3i_comp_10_value < __l3i_comp_10_min then __l3i_comp_10_min = __l3i_comp_10_value end");
        contains(out, "end return if __l3i_comp_10_min == nil then __l3i_comp_34_error(\"JSL min reducer received no elements\") else __l3i_comp_10_min end)()");
        assert(occurrences(out, "cost(x)") == 1 && out.find("table.create") == std::string::npos);
        const std::string every = rewrite("return all[for x in xs => ok(x)]");
        contains(every, "if not __l3i_comp_10_value then return false end");
        contains(every, "end return true end)()");
        assert(every.find("_n = 0") == std::string::npos && every.find("_out") == std::string::npos);
        const std::string some = rewrite("return any[for x in xs if p(x) => q(x)]");
        contains(some, "if __l3i_comp_10_value then return true end");
        contains(some, "end end return false end)()");
    }
    {
        const std::string out = rewrite("return into(out)[for x in xs => f(x)]");
        contains(out, "local __l3i_comp_16_into = out if __l3i_comp_37_typeof(__l3i_comp_16_into) ~= \"table\" then");
        contains(out, "local __l3i_comp_16_into_len = #__l3i_comp_16_into ");
        contains(out, "if __l3i_comp_37_rawequal(__l3i_comp_16_g0_src, __l3i_comp_16_into) then");
        contains(out, "__l3i_comp_16_n += 1 __l3i_comp_16_into[__l3i_comp_16_n] = __l3i_comp_16_value end");
        contains(out, "for __l3i_comp_16_i = __l3i_comp_16_n + 1, __l3i_comp_16_into_len do __l3i_comp_16_into[__l3i_comp_16_i] = nil end return __l3i_comp_16_into end)()");
        assert(out.find("table.create") == std::string::npos && out.find("_out") == std::string::npos);
        assert(occurrences(out, "f(x)") == 1);
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
        contains(out, "-- source\n");
        contains(out, "-- filter\n");
        contains(out, "-- project\n");
        contains(out, "if x then");
        const std::string quoted = rewrite("return [for x in xs => '-- not a comment']");
        contains(quoted, "= '-- not a comment' if");
    }

    // Length fusion may only consume a hash the scanner saw as code, not one in trivia.
    {
        const std::string out = rewrite("local out = -- #\n [for x in xs => x]");
        contains(out, "-- #\n (function()");
        contains(out, "table.create");
        const std::string fused = rewrite("return # -- comment\n [for x in xs => x]");
        contains(fused, "-- comment\n");
        assert(fused.find("table.create") == std::string::npos);
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

    // Strict lowering never emits executable recovery scaffolding. The compiler reports
    // the canonical document errors, rather than falling back to another recognizer.
    unchanged("return [for in xs => x]");
    unchanged("return [for x xs => x]");
    unchanged("return [for x in => x]");
    unchanged("return [for x in xs =>]");
    unchanged("return [for x in xs if => x]");

    {
        // Record pattern lowerings: copied segments are exact copies of the original, and the
        // provenance is sorted, disjoint and covers every generated byte, strict and recovered.
        for (std::string_view source : {"local {position: {x, y}, id: key}: Entity = e\nreturn x",
                 "local function f({x}: P, dt, {y: {z}}): number\n  return x + z\nend",
                 "return [for i, {id, position: {z}} in enumerate(xs) if z > 0 => function({a}) local {b} = a return b end]",
                 "local {\n  -- note\n  a, --[[ x ]] b: c,\n} = t; local {d} = [for x in xs => x]", "local {position: {x",
                 "function f({x,", "return [for {x in xs => x]"})
            for (bool recovery : {false, true})
            {
                const auto lowered = L3i::Surface::lower(source, recovery);
                if (lowered.map.empty()) continue; // Strict lowering of a structural error.
                size_t covered = 0;
                for (const auto& segment : lowered.map.provenance())
                {
                    assert(segment.begin == covered && segment.end > segment.begin);
                    covered = segment.end;
                    if (segment.copied)
                        assert(lowered.source.substr(segment.begin, segment.end - segment.begin) ==
                            source.substr(segment.originalBegin, segment.originalEnd - segment.originalBegin));
                    assert(segment.originalBegin <= segment.originalEnd && segment.originalEnd <= source.size());
                }
                assert(covered == lowered.source.size());
            }
    }
    {
        // The source map's affine fast path agrees with the envelope algorithm it shortcuts,
        // for every span and point of several lowerings (CRLF, UTF-8, multiline patterns).
        const auto lineStarts = [](std::string_view text) {
            std::vector<size_t> starts{0};
            for (size_t i = 0; i < text.size(); ++i)
                if (text[i] == '\n') starts.push_back(i + 1);
            return starts;
        };
        const auto toPosition = [](const std::vector<size_t>& starts, size_t at) {
            const size_t line = size_t(std::upper_bound(starts.begin(), starts.end(), at) - starts.begin()) - 1;
            return L3i::Surface::Position{unsigned(line), unsigned(at - starts[line])};
        };
        for (std::string_view source : {"local a = 1\r\nreturn [for x in xs if x > a =>\r\n  x * 2]\r\n",
                 "-- é🦀\nlocal {\n  position: {x, y},\n  id: key,\n} = e\nreturn [for {v} in vs => v + x], key\n",
                 "local function f({a}, b)\n  return sum[for i in range(1, b) => a * i]\nend\nreturn f\n"})
        {
            const auto lowered = L3i::Surface::lower(source, true);
            const auto& map = lowered.map;
            const auto& segments = map.provenance();
            const auto original = lineStarts(source), generated = lineStarts(lowered.source);
            // The reference: the envelope of every intersecting segment, as before the fast path.
            const auto reference = [&](size_t begin, size_t end) {
                size_t first = SIZE_MAX, last = 0;
                for (const auto& s : segments)
                {
                    if (s.end <= begin || s.begin >= end) continue;
                    const size_t lo = std::max(begin, s.begin), hi = std::min(end, s.end);
                    first = std::min(first, s.copied ? s.originalBegin + lo - s.begin : s.originalBegin);
                    last = std::max(last, s.copied ? s.originalBegin + hi - s.begin : s.originalEnd);
                }
                return std::pair{toPosition(original, first), toPosition(original, last)};
            };
            const auto pointReference = [&](size_t at) {
                const L3i::Surface::Segment* owner = nullptr;
                for (const auto& s : segments)
                    if (s.begin <= at) owner = &s;
                return toPosition(original, owner->copied ? owner->originalBegin + at - owner->begin : owner->originalBegin);
            };
            for (size_t begin = 0; begin < lowered.source.size(); begin += 3)
                for (size_t end = begin; end <= lowered.source.size(); end += 7)
                {
                    const auto span = map.originalSpan({toPosition(generated, begin), toPosition(generated, end)});
                    if (end == begin)
                    {
                        const auto at = pointReference(begin);
                        assert(span.begin.line == at.line && span.begin.column == at.column && span.end.line == at.line);
                        continue;
                    }
                    const auto [first, last] = reference(begin, end);
                    assert(span.begin.line == first.line && span.begin.column == first.column);
                    assert(span.end.line == last.line && span.end.column == last.column);
                }
        }
    }
    {
        // Estimated boundaries: whenever Luau's parse confirms them, the lowering is the located
        // one byte for byte; common shapes are confirmed, and every other shape still ends with
        // exactly the located lowering.
        struct Shape { std::string_view source; bool fast; };
        const Shape shapes[] = {
            {"local {x, y} = position\nreturn x + y\n", true},
            {"local {x}: P = getPoint(1, {2}) local {y} = t.inner[1]:m'k'\nreturn x, y\n", true},
            {"local {a} = t; (g)()\nreturn a\n", true},
            {"local {a} = if c then t else u\nlocal {b} = function() if a then return 1 end end\nreturn a, b\n", true},
            {"local {a} = `{t}` .. s :: string\nreturn a\n", true},
            {"local function f({x, y}: P, dt: number): number return x + dt end\n", true},
            {"function T:m({p}): (number, string) return p, '' end\n", true},
            {"local g = @native function({a}: { a: number }): () -> () return function() end end\n", true},
            {"return [for {id} in xs => function({a}) local {b} = a return b + id end]\n", true},
            {"local function h({x}): Map<string, { v: number }>? return x end\n", true},
            // The value continues on the next line: Luau reads a call, and so must the lowering.
            {"local {a} = f\n(g)()\nreturn a\n", false},
            {"local {a} = t, u\n", false},
            {"local function k({x}): A | B return x end\n", true},
            {"local {a} = f 'str' return a\n", true},
        };
        for (const Shape& shape : shapes)
        {
            const auto exact = L3i::Surface::lower(shape.source, false, true, {}, true);
            const auto estimate = L3i::Surface::lower(shape.source, false, true, {}, true, {}, true);
            Luau::Allocator allocator;
            Luau::AstNameTable names(allocator);
            const auto parsed = Luau::Parser::parse(estimate.source.data(), estimate.source.size(), names, allocator);
            const bool fast = estimate.estimated && L3i::Surface::boundariesHold(estimate, parsed);
            if (fast != shape.fast)
            {
                std::cerr << "fast path " << fast << " for:\n" << shape.source << '\n' << estimate.source << '\n';
                return 1;
            }
            if (fast)
            {
                assert(estimate.source == exact.source);
                assert(estimate.map.provenance().size() == exact.map.provenance().size());
            }
            if (!exact.document.errors.empty())
                assert(estimate.document.errors.size() >= exact.document.errors.size());
        }
    }
    const auto strict = L3i::Surface::lower("return [for x in xs =>]");
    assert(!strict.document.errors.empty() && strict.sites.empty());
    const auto recovered = L3i::Surface::lower("return [for x in xs =>]", true);
    assert(!recovered.document.errors.empty() && recovered.sites.size() == 1);

    std::cout << "surface syntax tests passed\n";
}
