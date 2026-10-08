#include "surface_frontend.h"
#include <cassert>
#include <iostream>
#include <string>
#include <string_view>
#include <utility>
using namespace L3i::Surface;
namespace
{
std::string_view slice(std::string_view s, Range r)
{
    assert(r.begin <= r.end && r.end <= s.size());
    return s.substr(r.begin, r.end - r.begin);
}
void complete(std::string_view s, size_t count = 1)
{
    auto d = parseSurface(s);
    if (!d.errors.empty())
    {
        for (const auto& e : d.errors) std::cerr << e.range.begin << ": " << e.message << '\n';
        std::cerr << s << '\n';
    }
    assert(d.errors.empty() && d.comprehensions.size() == count);
    size_t previous = 0;
    for (const auto& c : d.comprehensions)
    {
        assert(c.complete && !c.postfix && c.projectionSuffix.empty() && c.open.begin >= previous);
        for (const auto& clause : c.clauses) assert(clause.expressionSuffix.empty());
        previous = c.open.begin;
        assert(slice(s, c.open) == "[" && slice(s, c.close) == "]" && slice(s, c.arrow) == "=>");
    }
}
}
int main()
{
    {
        const std::string_view s = "[for i in range(first(), last(), step()) => i]";
        const auto clause = parseSurface(s).comprehensions[0].clauses[0];
        assert(clause.rangeArguments.size() == 3);
        assert(slice(s, clause.rangeArguments[0]) == "first()");
        assert(slice(s, clause.rangeArguments[1]) == "last()");
        assert(slice(s, clause.rangeArguments[2]) == "step()");
    }
    {
        const std::string s = "local out = [for row in rows() if row.ok for x in row.items if x > 0 => x * 2]";
        complete(s);
        auto c = parseSurface(s).comprehensions[0];
        assert(c.clauses.size() == 4);
        assert(c.clauses[0].kind == ClauseKind::Generator && c.clauses[1].kind == ClauseKind::Filter);
        assert(slice(s, c.clauses[0].binding) == "row");
        assert(slice(s, c.clauses[0].in) == "in");
        assert(slice(s, c.clauses[0].expression) == "rows()");
        assert(slice(s, c.clauses[1].expression) == "row.ok");
        assert(slice(s, c.clauses[2].expression) == "row.items");
        assert(slice(s, c.projection) == "x * 2");
    }
    for (std::string_view s : {"[for", "[for x", "[for x in", "[for x in xs", "[for x in xs if",
             "[for x in xs =>", "[for x in xs => x"})
    {
        auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.errors.empty());
        const auto& c = d.comprehensions[0];
        assert(!c.complete && c.close.empty() && c.close.begin == s.size());
        assert(c.range.end == s.size());
        assert(c.clauses.size() == (s == "[for x in xs if" ? 2 : 1));
        const auto& g = c.clauses[0];
        assert(slice(s, g.binding) == (s == "[for" ? "" : "x"));
        if (s == "[for") assert(g.binding.begin == s.size());
        if (s == "[for x in xs if") assert(c.clauses[1].expression.empty());
        if (s == "[for x in xs =>") assert(!c.arrow.empty() && c.projection.empty());
        if (s == "[for x in xs => x") assert(slice(s, c.projection) == "x");
    }
    for (std::string_view sync : {"local", "return", ";", ",", ")", "}"})
    {
        const std::string s = "[for x in xs => x " + std::string(sync) + " tail\nlocal y = [for y in ys => y]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 2);
        assert(d.comprehensions[0].close.empty());
        assert(d.comprehensions[0].range.end == s.find(sync));
        assert(d.comprehensions[1].complete);
    }
    complete("[for x in [for y in ys if y.ok => y] for z in x.items => [for a in z => a]]", 3);
    complete("[for x in if yes then xs else ys if x.ok => if x.ok then x else nil]");
    complete("[for x in function() local a = 1; if a then for i in xs do a += i end end return a end if x => x]");
    complete("[for x in (function() if yes then return xs end return ys end)() => x]");
    complete("[for x in xs => function() for y in ys do if y then return y end end end]");
    complete("[for x in {a = [for y in ys => y]} if x => x]", 2);
    for (std::string_view s : {"[for x in entities. => x]", "[for x in xs if x. => x]",
             "[for x in xs if p(x) + => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        const bool filter = s.find(" if ") != std::string_view::npos;
        assert(c.complete && c.clauses.size() == (filter ? 2 : 1));
        assert(slice(s, c.clauses[0].expression) == (filter ? "xs" : "entities."));
        if (filter) assert(slice(s, c.clauses[1].expression) == (s.find("p(x)") != std::string_view::npos ? "p(x) +" : "x."));
        assert(slice(s, c.arrow) == "=>" && c.arrow.begin == s.find("=>"));
        assert(c.clauses.back().expression.end < c.arrow.begin);
        assert(slice(s, c.projection) == "x");
    }
    {
        const std::string_view s = "[for x in function() return xs => x]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        assert(slice(s, c.clauses[0].expression) == "function() return xs");
        assert(slice(s, c.arrow) == "=>" && slice(s, c.projection) == "x");
    }
    {
        const std::string_view s = "[for x in (function() return xs => xs end) => x]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        assert(slice(s, c.clauses[0].expression) == "(function() return xs");
        assert(c.clauses[0].expressionSuffix == ")");
        assert(c.arrow.begin == s.find("=>") && slice(s, c.projection) == "xs end");
        assert(!c.complete && !d.errors.empty());
    }
    {
        const std::string_view s = "[for x in [for y in ys. => y] => x]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 2);
        assert(slice(s, d.comprehensions[0].clauses[0].expression) == "[for y in ys. => y]");
        assert(d.comprehensions[0].arrow.begin == s.rfind("=>"));
        assert(slice(s, d.comprehensions[1].clauses[0].expression) == "ys.");
        assert(slice(s, d.comprehensions[1].projection) == "y");
    }
    for (std::string_view s : {"f([for x in xs => x, trailing, [for y in ys => y])",
             "local t = {[for x in xs => x, trailing = 1, [for y in ys => y]}"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 2 && !d.comprehensions[0].complete && d.comprehensions[1].complete);
        assert(d.comprehensions[0].close.empty() && d.comprehensions[0].close.begin == s.find(','));
        assert(slice(s, d.comprehensions[0].projection) == "x");
    }
    for (std::string_view s : {"[for x in xs. for y in ys => y]", "[for x in xs if x. for y in ys => y]"})
    {
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        assert(c.clauses.size() == (s.find(" if ") == std::string_view::npos ? 2 : 3));
        assert(slice(s, c.clauses.back().binding) == "y");
        assert(slice(s, c.clauses.back().expression) == "ys");
        assert(slice(s, c.clauses[c.clauses.size() - 2].expression) == (c.clauses.size() == 2 ? "xs." : "x."));
    }
    complete("[for x in function() local a, b = 1, 2 for y in ys do a += y end return a, b end for z in zs => z]");
    complete("[for x in xs => function() local a, b = 1, 2 return a, b end]");
    complete("[for x in function() local f = function() for y in ys do return y end end return f end for z in zs => z]");
    for (std::string_view s : {"[for x in function() end + xs. for y in ys => y]",
             "[for x in (function() end)() + xs. for y in ys => y]"})
    {
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions[0].clauses.size() == 2);
        assert(slice(s, d.comprehensions[0].clauses[1].binding) == "y");
    }
    {
        const std::string_view s = "f([for x in xs => function() end + x., trailing)";
        const auto d = parseSurface(s);
        assert(d.comprehensions[0].close.begin == s.find(','));
        assert(slice(s, d.comprehensions[0].projection) == "function() end + x.");
    }
    complete("[for x in xs if if a then x else nil if x => x]");
    complete("[for x in xs if (function() for y in ys do if y then return true end end return false end)() => x]");
    for (std::string_view prefix : {"[for", "[for x", "[for x in", "[for x in xs", "[for x in xs if", "[for x in xs =>"})
    {
        const std::string s = std::string(prefix) + "\nlocal next = [for y in ys => y]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 2 && !d.comprehensions[0].complete && d.comprehensions[1].complete);
        assert(d.comprehensions[0].close.begin == s.find("local"));
    }
    {
        const auto d = parseSurface("[for in xs => 1]");
        assert(d.comprehensions.size() == 1 && !d.errors.empty());
        assert(d.comprehensions[0].clauses[0].binding.empty());
    }
    {
        const std::string_view s = "[for i, value in enumerate(values) => value]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& clause = d.comprehensions[0].clauses[0];
        assert(clause.bindings.size() == 2);
        assert(slice(s, clause.bindings[0]) == "i" && slice(s, clause.bindings[1]) == "value");
        assert(slice(s, clause.enumerateArgument) == "values");
    }
    for (std::string_view s : {"[for value in enumerate(values) => value]",
             "[for a, b in values => a]", "[for a, in enumerate(values) => a]"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.errors.empty() && !d.comprehensions[0].complete);
    }
    {
        const std::string_view s = "[for a, b in zipStrict(left, right) => a + b]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& clause = d.comprehensions[0].clauses[0];
        assert(clause.zipStrict && clause.zipArguments.size() == 2 && clause.bindings.size() == 2);
    }
    for (std::string_view s : {"[for a in zipShortest(xs, ys) => a]",
             "[for a, b in zipStrict(xs) => a]", "[for a, b in zip(xs, ys) => a]"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.errors.empty() && !d.comprehensions[0].complete);
    }
    {
        const std::string_view s = "[for x in values[first:last] => x]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& clause = d.comprehensions[0].clauses[0];
        assert(slice(s, clause.sliceSource) == "values");
        assert(slice(s, clause.sliceFirst) == "first");
        assert(slice(s, clause.sliceLast) == "last");
        assert(d.slices.empty());
    }
    {
        const std::string_view s = "return source()[(first()):(last())]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.slices.size() == 1);
        assert(slice(s, d.slices[0].source) == "source()");
        assert(slice(s, d.slices[0].first) == "(first())");
        assert(slice(s, d.slices[0].last) == "(last())");
        assert(slice(s, d.slices[0].range) == "source()[(first()):(last())]");
    }
    {
        const auto d = parseSurface("return values[obj:method()]");
        assert(d.errors.empty() && d.slices.empty());
    }
    for (std::string_view s : {"return values[:last]", "return values[first:]"})
    {
        const auto d = parseSurface(s);
        assert(d.slices.size() == 1 && d.errors.size() == 1 && !d.slices[0].complete);
        assert(d.slices[0].first.empty() != d.slices[0].last.empty());
        assert(d.errors[0].message.find("slice") != std::string::npos);
    }
    for (std::string_view s : {"[for x in values[:last] => x]", "[for x in values[first:] => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.comprehensions[0].complete && d.slices.empty());
        const auto& clause = d.comprehensions[0].clauses[0];
        assert(!clause.sliceSource.empty() && (clause.sliceFirst.empty() != clause.sliceLast.empty()));
    }
    {
        std::string s;
        for (size_t i = 0; i < 4097; ++i) s += "values[first:last];";
        const auto d = parseSurface(s);
        assert(d.slices.size() == 4096 && !d.errors.empty());
        bool found = false;
        for (const auto& e : d.errors) found |= e.message.find("work limit") != std::string::npos;
        assert(found);
    }
    {
        const std::string s = "-- [for fake in xs => fake]\r\nlocal a = '[for fake]'\nlocal b = [==[[for fake]]==]\n"
            "--[=[ [for ignored] ]=]\nlocal c = [ -- trivia\r\n for x -- name\n in xs -- expr\n if x => 'é' ]";
        complete(s);
        const auto c = parseSurface(s).comprehensions[0];
        assert(slice(s, c.clauses[0].expression) == "xs");
        assert(slice(s, c.projection) == "'é'");
    }
    complete("local s = `literal [for fake] {[for x in xs => x]} tail {[for y in ys => y]}`", 2);
    for (std::string_view s : {"sum[for x in xs => x]", "sum \n [for x in xs => x]",
             "sum -- comment\n[ -- opener trivia\nfor x in xs => x]",
             "sum --[=[ trivia ]=] [for x in xs => x]", "sum sum[for x in xs => x]",
             "#sum[for x in xs => x]", "local sum = callback; sum[for x in xs => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        assert(slice(s, c.reducerPrefix) == "sum" && c.reducerPrefix.end <= c.open.begin);
        assert(c.reducerPrefix.begin == s.rfind("sum"));
        assert(c.lengthPrefix.empty() && c.range.begin == c.open.begin);
    }
    for (std::string_view s : {"values.sum[for x in xs => x]", "values . sum [for x in xs => x]",
             "values:sum[for x in xs => x]", "values . -- member trivia\nsum[for x in xs => x]",
             "values: -- member trivia\nsum[for x in xs => x]", "resum[for x in xs => x]",
             "_sum[for x in xs => x]", "sum2[for x in xs => x]", "Sum[for x in xs => x]",
             "-- sum\n[for x in xs => x]", "local s = 'sum'\n[for x in xs => x]",
             "sum 'literal' [for x in xs => x]", "(sum)[for x in xs => x]"})
    {
        const auto d = parseSurface(s);
        // Postfix errors don't erase the underlying comprehension or turn it
        // into an intrinsic sum reduction.
        assert(d.comprehensions.size() == 1);
        const bool postfix = s.substr(0, 2) != "--";
        assert(d.comprehensions[0].postfix == postfix && d.errors.empty() == !postfix);
        assert(d.comprehensions[0].complete == !postfix);
        assert(d.comprehensions[0].reducerPrefix.empty());
        assert(d.comprehensions[0].range.begin == d.comprehensions[0].open.begin);
    }
    {
        const std::string_view s = "sum[for x in source() if accept(x) => sum -- nested\n"
            "[for y in children(x) => effect(y)]]; sum sum [for z in zs => z]; values.sum[for a in xs => a]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 4 && !d.errors.empty());
        assert(d.comprehensions[0].complete && d.comprehensions[1].complete && d.comprehensions[2].complete);
        assert(d.comprehensions[3].postfix && !d.comprehensions[3].complete);
        for (size_t i = 0; i < 3; ++i) assert(slice(s, d.comprehensions[i].reducerPrefix) == "sum");
        assert(d.comprehensions[2].reducerPrefix.begin == s.find("sum [for z"));
        assert(d.comprehensions[3].reducerPrefix.empty());
        assert(slice(s, d.comprehensions[0].clauses[0].expression) == "source()");
        assert(slice(s, d.comprehensions[0].clauses[1].expression) == "accept(x)");
        assert(slice(s, d.comprehensions[1].clauses[0].expression) == "children(x)");
        assert(slice(s, d.comprehensions[1].projection) == "effect(y)");
    }
    complete("[for x in sum -- trivia\n[for y in ys => y] if x > 0 => x]", 2);
    complete("[for x in xs if sum -- trivia\n[for y in ys => y] > 0 if x => x]", 2);
    complete("local text = `sum literal {sum -- trivia\n[for x in xs => x]}`");
    {
        const std::string_view s = "sum[for sum in {} if accept(sum) => effect(sum)]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        assert(slice(s, c.reducerPrefix) == "sum");
        assert(slice(s, c.clauses[0].binding) == "sum");
        assert(slice(s, c.clauses[0].expression) == "{}");
        assert(slice(s, c.clauses[1].expression) == "accept(sum)");
        assert(slice(s, c.projection) == "effect(sum)");
        const auto ordinary = parseSurface("[for sum in xs => sum]");
        assert(ordinary.comprehensions[0].reducerPrefix.empty());
        assert(ordinary.comprehensions[0].lengthPrefix.empty());
    }
    {
        const std::string_view s = "sum#[for x in xs => x]; #sum[for y in ys => y]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 2);
        assert(slice(s, d.comprehensions[0].lengthPrefix) == "#" && d.comprehensions[0].reducerPrefix.empty());
        assert(d.comprehensions[1].lengthPrefix.empty() && slice(s, d.comprehensions[1].reducerPrefix) == "sum");
    }
    {
        // Every reducer spelling is one record kind; member access and other names are not reducers.
        const std::pair<std::string_view, Reducer> reducers[] = {{"sum", Reducer::Sum}, {"min", Reducer::Min},
            {"max", Reducer::Max}, {"any", Reducer::Any}, {"all", Reducer::All}};
        for (const auto& [name, kind] : reducers)
        {
            const std::string s = std::string(name) + " -- trivia\n[for x in xs => x]";
            const auto d = parseSurface(s);
            assert(d.errors.empty() && d.comprehensions.size() == 1);
            assert(d.comprehensions[0].reducer == kind && slice(s, d.comprehensions[0].reducerPrefix) == name);
            const std::string member = "t." + std::string(name) + "[for x in xs => x]";
            const auto m = parseSurface(member);
            assert(m.comprehensions.size() == 1 && m.comprehensions[0].reducer == Reducer::None && m.comprehensions[0].postfix);
        }
        for (std::string_view s : {"into(out)[for x in xs => x]", "into (scratch[kind] or make()) -- note\n[for x in xs => x]",
                 "local into = 1; return #into(out)[for x in xs => x]"})
        {
            const auto sink = parseSurface(s);
            assert(sink.errors.empty() && sink.comprehensions.size() == 1 && !sink.comprehensions[0].postfix);
            const auto& c = sink.comprehensions[0];
            assert(slice(s, c.sinkPrefix).substr(0, 4) == "into" && c.sinkPrefix.end <= c.open.begin);
            assert(!c.sinkDestination.empty() && c.reducer == Reducer::None && c.lengthPrefix.empty());
        }
        {
            const std::string_view s = "into(out)[for x in xs => x]";
            assert(slice(s, parseSurface(s).comprehensions[0].sinkDestination) == "out");
        }
        for (std::string_view s : {"t.into(out)[for x in xs => x]", "t:into(out)[for x in xs => x]", "fill(out)[for x in xs => x]",
                 "into[for x in xs => x]"})
        {
            const auto m = parseSurface(s);
            assert(m.comprehensions.size() == 1 && m.comprehensions[0].sinkPrefix.empty() && m.comprehensions[0].postfix);
        }
        for (std::string_view s : {"into(out, \"f32\")[for x in xs => x]", "into(out, \"f32@16\", base + 8)[for x in xs => x]",
                 "into(f(a, b), \"u8\")[for x in xs => x]"})
        {
            const auto d = parseSurface(s);
            assert(d.errors.empty() && d.comprehensions.size() == 1);
            const auto& c = d.comprehensions[0];
            assert(!c.sinkKind.empty() && slice(s, c.sinkKind).front() == '"' && !c.sinkDestination.empty());
            assert(slice(s, c.sinkDestination).find(',') == std::string_view::npos || slice(s, c.sinkDestination) == "f(a, b)");
        }
        {
            const std::string_view s = "into(out, \"f32@16\", base + 8)[for x in xs => x]";
            const auto parsed = parseSurface(s);
            const auto& c = parsed.comprehensions[0];
            assert(slice(s, c.sinkOffset) == "base + 8" && slice(s, c.sinkKind) == "\"f32@16\"");
        }
        for (std::string_view s : {"into(out, \"f16\")[for x in xs => x]", "into(out, kind)[for x in xs => x]",
                 "into(out, \"f32\", 0, 1)[for x in xs => x]", "into(out, \"f32@2\")[for x in xs => x]", "into(out, \"f32\",)[for x in xs => x]"})
        {
            const auto d = parseSurface(s);
            assert(d.comprehensions.size() == 1 && !d.errors.empty() && !d.comprehensions[0].sinkPrefix.empty());
        }
        {
            const auto empty = parseSurface("into()[for x in xs => x]");
            assert(empty.comprehensions.size() == 1 && !empty.comprehensions[0].sinkPrefix.empty());
            assert(!empty.errors.empty() && empty.errors[0].message.find("sink destination") != std::string::npos);
        }
        {
            // Unmatched parentheses before many openers: each opener scans back at most a bounded
            // number of tokens, so hostile input stays linear, and none of them is a sink.
            std::string s;
            for (size_t i = 0; i < 6000; ++i) s += ")";
            for (size_t i = 0; i < 200; ++i) s += "[for x in xs => x] ";
            const auto d = parseSurface(s);
            assert(d.comprehensions.size() == 200);
            assert(d.comprehensions[0].postfix && d.comprehensions[0].sinkPrefix.empty());
            std::string deep = "into(";
            for (size_t i = 0; i < 5000; ++i) deep += "(";
            deep += "x";
            for (size_t i = 0; i < 5000; ++i) deep += ")";
            deep += ")[for x in xs => x]";
            const auto beyond = parseSurface(deep);
            assert(beyond.comprehensions.size() == 1 && beyond.comprehensions[0].sinkPrefix.empty() && beyond.comprehensions[0].postfix);
        }
        {
            // Typed slices: an element kind literal after the last bound, in both positions.
            const std::string_view s = "[for v in samples[a:b, \"f32@16\"] => v]; local part = samples[1:n, \"u8\"]";
            const auto d = parseSurface(s);
            assert(d.errors.empty() && d.comprehensions.size() == 1 && d.slices.size() == 1);
            const auto& clause = d.comprehensions[0].clauses[0];
            assert(slice(s, clause.sliceKind) == "\"f32@16\"" && slice(s, clause.sliceLast) == "b" && slice(s, clause.sliceSource) == "samples");
            assert(slice(s, d.slices[0].kind) == "\"u8\"" && slice(s, d.slices[0].last) == "n" && d.slices[0].complete);
            for (std::string_view bad : {"[for v in samples[a:b, \"f16\"] => v]", "[for v in samples[a:b, kind] => v]",
                     "[for v in samples[a:b, \"f32@2\"] => v]", "local p = samples[1:2, 3]", "local p = samples[1:2, \"u8\", 4]"})
            {
                const auto e = parseSurface(bad);
                assert(!e.errors.empty());
            }
            const auto missing = parseSurface("[for v in samples[a:, \"f32\"] => v]");
            assert(missing.comprehensions.size() == 1 && !missing.errors.empty() && slice("[for v in samples[a:, \"f32\"] => v]", missing.comprehensions[0].clauses[0].sliceKind) == "\"f32\"");
        }
        const auto d = parseSurface("minimum[for x in xs => x]; [for x in xs => x]");
        assert(d.comprehensions.size() == 2 && d.comprehensions[0].reducer == Reducer::None && d.comprehensions[0].postfix);
        assert(d.comprehensions[1].reducer == Reducer::None && !d.comprehensions[1].postfix);
    }
    for (std::string_view s : {"#[for x in xs => x]", "# -- hash in trivia: #\n[for x in xs => x]",
             "# --[=[ block # ]=] [ -- opener trivia #\nfor x in xs => x]", "##[for x in xs => x]"})
    {
        complete(s);
        const auto c = parseSurface(s).comprehensions[0];
        assert(slice(s, c.lengthPrefix) == "#");
        assert(c.lengthPrefix.begin == (s.substr(0, 2) == "##" ? 1 : 0));
        assert(c.range.begin == c.open.begin && c.lengthPrefix.end <= c.open.begin);
        assert(slice(s, c.range).front() == '[');
    }
    for (std::string_view s : {"[for x in xs => x]", "-- #\n[for x in xs => x]",
             "--[=[ # ]=] [for x in xs => x]", "local s = '#'\n[for x in xs => x]",
             "local s = `#`\n[for x in xs => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1);
        const bool postfix = s.substr(0, 5) == "local";
        assert(d.comprehensions[0].postfix == postfix && d.errors.empty() == !postfix);
        assert(d.comprehensions[0].lengthPrefix.empty());
        assert(d.comprehensions[0].range.begin == d.comprehensions[0].open.begin);
    }
    {
        const std::string_view s = "#[for x in #[for y in ys => y] => ##[for z in zs => z]]; ##[for a in items => a]";
        complete(s, 4);
        const auto d = parseSurface(s);
        for (const auto& c : d.comprehensions)
        {
            assert(slice(s, c.lengthPrefix) == "#");
            assert(c.lengthPrefix.begin == c.open.begin - 1);
            assert(c.range.begin == c.open.begin);
        }
        // Repeated unary length operators select only the innermost hash.
        assert(s[d.comprehensions[2].lengthPrefix.begin - 1] == '#');
        assert(s[d.comprehensions[3].lengthPrefix.begin - 1] == '#');
    }
    {
        const std::string_view s = "[for x in xs => # -- fake hash #\n[for y in ys => -- #\n[for z in zs => z]]]";
        complete(s, 3);
        const auto d = parseSurface(s);
        assert(d.comprehensions[0].lengthPrefix.empty());
        assert(slice(s, d.comprehensions[1].lengthPrefix) == "#");
        assert(d.comprehensions[1].lengthPrefix.begin == s.find("# --"));
        assert(d.comprehensions[2].lengthPrefix.empty());
    }
    {
        const std::string s = "-- header [for inert]\r\nlocal result = [ -- opener\nfor x in xs "
            "--[=[ between generator and filter ]=]\nif x -- between filter and generator\n"
            "for y in x.items -- before arrow\n=> y] -- trailing\n";
        complete(s);
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && d.comprehensions[0].clauses.size() == 3);
        const std::string_view expected[] = {"-- header [for inert]", "-- opener",
            "--[=[ between generator and filter ]=]", "-- between filter and generator",
            "-- before arrow", "-- trailing"};
        assert(d.comments.size() == 6);
        size_t previousEnd = 0;
        for (size_t i = 0; i < d.comments.size(); ++i)
        {
            assert(d.comments[i].begin >= previousEnd);
            assert(slice(s, d.comments[i]) == expected[i]);
            assert(d.comments[i].begin == s.find(expected[i]));
            previousEnd = d.comments[i].end;
        }
        assert(d.comments.front().end < d.comprehensions[0].open.begin);
        assert(d.comments.back().begin > d.comprehensions[0].close.end);
    }
    {
        const std::string_view s = "-- only [for inert]\nlocal value = 1 --[=[ another [for inert] ]=]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.empty() && d.comments.size() == 2);
        assert(slice(s, d.comments[0]) == "-- only [for inert]");
        assert(slice(s, d.comments[1]) == "--[=[ another [for inert] ]=]");
    }
    {
        const std::string_view s = "[for x in xs => x] --[=[ unfinished [for";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions[0].complete && d.comments.size() == 1);
        assert(slice(s, d.comments[0]) == "--[=[ unfinished [for");
    }
    for (bool afterOpener : {false, true})
    {
        std::string s = afterOpener ? "[for x in xs\n" : "[";
        for (size_t i = 0; i < 262145; ++i) s += "-- comment [for inert]\n";
        s += afterOpener ? "=> x] -- after" : "for x in xs => x] -- after";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1 && d.comprehensions[0].complete);
        assert(d.comments.size() == 262146);
        assert(slice(s, d.comments.front()) == "-- comment [for inert]");
        assert(slice(s, d.comments.back()) == "-- after");
    }
    complete("[for x in `{[for y in ys => y]}` if x => `value {x}`]", 2);
    complete("[for x in xs => `a { {v = [for y in ys => y]} } b`]", 2);
    assert(parseSurface("local a = '[for'; local b = [=[ [for ]=]; -- [for\nreturn `{1}`").comprehensions.empty());
    {
        std::string s;
        for (size_t i = 0; i < 140; ++i) s += "[for x in ";
        s += "xs";
        for (size_t i = 0; i < 140; ++i) s += " => x]";
        const auto d = parseSurface(s);
        assert(!d.errors.empty() && d.comprehensions.size() <= 128);
        bool found = false;
        for (const auto& e : d.errors) found |= e.message.find("nesting limit") != std::string::npos;
        assert(found);
    }
    {
        std::string s = "[for x in " + std::string(140, '(') + "xs" + std::string(140, ')') + " => x]";
        assert(!parseSurface(s).errors.empty());
        assert(parseSurface(std::string(8388609, ' ')).errors.empty());
        std::string manyTokens = "[for x in xs";
        for (size_t i = 0; i < 131073; ++i) manyTokens += " + xs";
        manyTokens += " => x]";
        const auto tokenLimited = parseSurface(manyTokens);
        assert(!tokenLimited.errors.empty());
        assert(tokenLimited.errors[0].message.find("token limit") != std::string::npos);
    }
    {
        // Both exceed the former blanket byte cap. Literal sentinels must not
        // activate the feature budget, even with more than 262144 real tokens.
        const std::string bigString = "local text = '[for " + std::string(8388609, 'x') + "'";
        const auto stringControl = parseSurface(bigString);
        assert(stringControl.errors.empty() && stringControl.comprehensions.empty());
        std::string bigBody = "-- [for comment]\nlocal q = '[for quoted]'\nlocal l = [=[ [for long] ]=]\n"
            "local s = `literal [for interpolation] {1}`\n";
        while (bigBody.size() <= 8388608) bigBody += "local a = 1\n";
        const auto bodyControl = parseSurface(bigBody);
        assert(bodyControl.errors.empty() && bodyControl.comprehensions.empty());
        // Ordinary tokens BEFORE the first genuine opener don't consume its
        // token budget, either. This also exercises offsets beyond 8 MiB.
        complete(bigBody + "local result = [for x in xs => x]");
        // A genuine opener still enforces the byte budget for prefix work.
        const std::string largeFeature = "[for x in '" + std::string(8388609, 'x') + "' if x => x]";
        const auto limited = parseSurface(largeFeature);
        assert(limited.comprehensions.size() == 1 && !limited.comprehensions[0].complete);
        bool found = false;
        for (const auto& e : limited.errors) found |= e.message.find("work limit") != std::string::npos;
        assert(found);
    }
    {
        std::string s = "[for x in function() ";
        for (size_t i = 0; i < 5000; ++i) s += "if a then end ";
        s += "end => x]";
        const auto d = parseSurface(s);
        bool found = false;
        for (const auto& e : d.errors) found |= e.message.find("work limit") != std::string::npos;
        assert(found);
    }
    for (std::string_view s : {"[for x in xs. if keep(x) => x]", "[for x in xs if x. if keep(x) => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1);
        const auto& c = d.comprehensions[0];
        const bool extraFilter = s.find("if x.") != std::string_view::npos;
        assert(c.clauses.size() == (extraFilter ? 3 : 2));
        assert(c.clauses.back().kind == ClauseKind::Filter);
        assert(slice(s, c.clauses.back().expression) == "keep(x)");
        assert(slice(s, c.clauses[c.clauses.size() - 2].expression) == (extraFilter ? "x." : "xs."));
        assert(slice(s, c.arrow) == "=>" && slice(s, c.projection) == "x");
    }
    complete("[for x in function() if keep(x) then return xs end return ys end if keep(x) => x]");
    complete("[for x in if a then if b then xs else ys else zs if keep(x) => x]");
    complete("[for x in if a then if function() if b then return true end return false end then xs else ys else zs if keep(x) => x]");
    complete("[for x in xs if if a then if b then true else false else false if keep(x) => x]");
    {
        const std::string_view s = "[for x in xs. if a then ys else zs => x]";
        const auto d = parseSurface(s);
        assert(d.comprehensions[0].clauses.size() == 1);
        assert(slice(s, d.comprehensions[0].clauses[0].expression) == "xs. if a then ys else zs");
    }
    {
        // A conditional inside a nested predicate argument must not lend its
        // then/else tokens to the damaged-source filter separator.
        const std::string_view s = "[for x in xs. if keep(if a then x else nil) => x]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions[0].clauses.size() == 2);
        assert(slice(s, d.comprehensions[0].clauses[1].expression) == "keep(if a then x else nil)");
    }
    for (std::string_view s : {"f([for x in (xs => x,42)", "[for x in a[{key = (xs => x]",
             "[for x in xs if (keep(x) => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.errors.empty());
        const auto& c = d.comprehensions[0];
        const auto& clause = c.clauses.back();
        assert(!c.complete && slice(s, c.projection) == "x");
        assert(slice(s, c.arrow) == "=>" && c.arrow.begin == s.find("=>"));
        assert(clause.expression.end < c.arrow.begin);
        const std::string suffix = s.find("a[{") == std::string_view::npos ? ")" : ")}]";
        assert(clause.expressionSuffix == suffix);
        for (char closer : suffix)
        {
            bool found = false;
            for (const auto& e : d.errors)
                found |= e.range.empty() && e.range.begin == c.arrow.begin &&
                    e.message.find(std::string("expected '") + closer + "'") != std::string::npos;
            assert(found);
        }
        if (s.front() == 'f') assert(c.close.empty() && c.close.begin == s.find(','));
    }
    for (std::string_view s : {"[for x in (xs]", "[for x in xs => (x]", "[for x in xs => ({x]",
             "[for x in xs => (x"})
    {
        const auto d = parseSurface(s);
        assert(!d.errors.empty() && d.comprehensions.size() == 1 && !d.comprehensions[0].complete);
        const auto& c = d.comprehensions[0];
        const bool projection = !c.arrow.empty();
        const std::string suffix = projection ? c.projectionSuffix : c.clauses[0].expressionSuffix;
        assert(suffix == (s.find("({") == std::string_view::npos ? ")" : "})"));
        const size_t fence = s.find(']') == std::string_view::npos ? s.size() : s.find(']');
        for (char closer : suffix)
        {
            bool found = false;
            for (const auto& e : d.errors)
                found |= e.range.empty() && e.range.begin == fence &&
                    e.message.find(std::string("expected '") + closer + "'") != std::string::npos;
            assert(found);
        }
    }
    for (std::string_view s : {"values[for x in xs => x]", "values.sum[for x in xs => x]",
             "values:sum[for x in xs => x]", "values<<number>>[for x in xs => x]",
             "values<<number>> -- trivia\n[for x in xs => x]", "(values)[for x in xs => x]",
             "values[1][for x in xs => x]", "'literal'[for x in xs => x]",
             "42[for x in xs => x]", "true[for x in xs => x]", "`text`[for x in xs => x]",
             "`value {x}`[for x in xs => x]"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.errors.empty());
        const auto& c = d.comprehensions[0];
        assert(c.postfix && !c.complete && c.reducerPrefix.empty() && c.lengthPrefix.empty());
        assert(c.range.begin == c.open.begin && slice(s, c.projection) == "x");
        assert(d.errors[0].range.begin == c.open.begin);
        assert(d.errors[0].message.find("explicit parenthesized collection index") != std::string::npos);
    }
    complete("foo([for x in xs => x])");
    for (std::string_view s : {"f([for x in xs => values[x)", "local t = {key = [for x in xs => (x}"})
    {
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 1 && !d.errors.empty());
        const auto& c = d.comprehensions[0];
        const char closer = s.front() == 'f' ? ']' : ')';
        const size_t fence = s.size() - 1;
        assert(c.projectionSuffix == std::string(1, closer));
        assert(c.close.empty() && c.close.begin == fence);
        bool found = false;
        for (const auto& e : d.errors)
            found |= e.range.empty() && e.range.begin == fence &&
                e.message.find(std::string("expected '") + closer + "'") != std::string::npos;
        assert(found);
    }
    for (std::string_view fence : {"local", "return", ";"})
    {
        const std::string s = "[for x in (xs " + std::string(fence) + " tail\nlocal y = [for y in ys => y]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 2 && !d.errors.empty());
        assert(d.comprehensions[0].clauses[0].expressionSuffix == ")");
        assert(d.comprehensions[0].close.begin == s.find(fence));
        assert(d.comprehensions[1].complete);
    }
    complete("[for x in {a; b} => x]");
    complete("[for x in {'=>', [=[=>]=]} => x]");
    complete("[for x in `literal =>` => x]");
    complete("values[([for x in xs => x])]");
    complete("left > [for x in xs => x]");
    complete("left >= [for x in xs => x]");
    complete("sum[for x in xs => x]");
    complete("#[for x in xs => x]");
    {
        const std::string_view s = "[for x in xs => values.sum[for y in ys => y]]";
        const auto d = parseSurface(s);
        assert(d.comprehensions.size() == 2 && !d.errors.empty());
        assert(d.comprehensions[1].postfix && !d.comprehensions[1].complete);
        assert(!d.comprehensions[0].postfix && !d.comprehensions[0].complete);
    }
    // Record patterns: one model for declarations, parameters and generator slots.
    {
        const std::string_view s = "local {transform: {position: pos, rotation,}, id}: Entity = entity";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.locals.size() == 1);
        const auto& local = d.locals[0];
        assert(slice(s, local.keyword) == "local" && slice(s, local.colon) == ":" && slice(s, local.equals) == "=");
        const auto& p = local.pattern;
        assert(p.record && p.complete && slice(s, p.range) == "{transform: {position: pos, rotation,}, id}");
        assert(p.fields.size() == 2 && slice(s, p.fields[0].key) == "transform" && slice(s, p.fields[0].colon) == ":");
        const auto& nested = p.fields[0].target;
        assert(nested.record && slice(s, nested.open) == "{" && slice(s, nested.close) == "}" && nested.fields.size() == 2);
        assert(slice(s, nested.fields[0].key) == "position" && slice(s, nested.fields[0].target.range) == "pos");
        assert(slice(s, nested.fields[1].range) == "rotation" && nested.fields[1].colon.empty());
        assert(!p.fields[1].target.record && slice(s, p.fields[1].target.range) == "id");
    }
    {
        const std::string_view s = "local {} = e local {x,} = e";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.locals.size() == 2 && d.locals[0].pattern.fields.empty() && d.locals[0].pattern.complete);
        assert(d.locals[1].pattern.fields.size() == 1 && slice(s, d.locals[1].pattern.range) == "{x,}");
    }
    {
        const std::string_view s = "function T:move({x, y}: Vec3, dt: Map<K, V>, {z}) end local f = function(a, {b: {c}}) end";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.parameters.size() == 3);
        assert(d.parameters[0].parameter == 0 && slice(s, d.parameters[0].colon) == ":" && d.parameters[0].function.begin == 0);
        assert(d.parameters[1].parameter == 2 && d.parameters[1].colon.empty());
        assert(d.parameters[2].parameter == 1 && slice(s, d.parameters[2].function) == "function" && d.parameters[2].function.begin > 0);
        assert(d.parameters[2].pattern.fields[0].target.record);
    }
    {
        const std::string_view s = "return [for i, {id, position: {z}} in enumerate(entities) if z > 0 => id]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions.size() == 1 && d.comprehensions[0].complete);
        const auto& g = d.comprehensions[0].clauses[0];
        assert(g.bindings.size() == 2 && g.patterns.size() == 2 && !g.patterns[0].record && g.patterns[1].record);
        assert(slice(s, g.bindings[1]) == "{id, position: {z}}" && slice(s, g.binding) == "i");
        assert(!g.enumerateArgument.empty());
    }
    {
        // A function literal with a pattern parameter inside a clause does not hide the next filter.
        const std::string_view s = "return [for x in map(xs, function({a}) return a end) if x > 0 => x]";
        const auto d = parseSurface(s);
        assert(d.errors.empty() && d.comprehensions[0].clauses.size() == 2 && d.parameters.size() == 1);
        assert(slice(s, d.comprehensions[0].clauses[0].expression) == "map(xs, function({a}) return a end)");
    }
    {
        // Every way to repeat a name in one binding list, aliases and nesting included.
        for (std::string_view s : {"local {x, other: x} = v", "local {x, n: {x}} = v", "function f({x}, x) end",
                 "function f({x}, {y: x}) end", "return [for i, {i} in enumerate(xs) => i]"})
        {
            const auto d = parseSurface(s);
            assert(d.errors.size() == 1);
            const std::string name(slice(s, d.errors[0].range));
            assert(d.errors[0].message == "duplicate binding '" + name + "' in record pattern");
            assert(name == "x" || name == "i");
        }
        // Plain duplicate parameters stay Luau's business.
        assert(parseSurface("function f(a, a) end").errors.empty());
    }
    {
        const std::pair<std::string_view, std::string_view> unsupported[] = {
            {"local {id, ...rest} = e", "rest patterns are not supported"},
            {"local {health = 100, x} = e", "defaults are not supported"},
            {"local {[\"k\"]: v, y} = e", "computed keys are not supported"},
            {"local {a: [b], c} = e", "indexed patterns are not supported"},
            {"local [a, b] = v", "indexed destructuring is not supported"},
            {"for {id} in pairs(t) do end", "not supported in generic for loops"},
            {"local {x}, y = v", "binds exactly one pattern"},
        };
        for (const auto& [s, message] : unsupported)
        {
            const auto d = parseSurface(s);
            // One mistake, one diagnostic: recovery resynchronizes at the record's ',' or '}'.
            assert(d.errors.size() == 1 && d.errors[0].message.find(message) != std::string::npos);
        }
        // A comprehension clause `for {` is not a statement loop.
        assert(parseSurface("return [for {a} in xs => a]").errors.empty());
    }
    {
        // Every prefix of representative forms: deterministic records and errors, nothing invented.
        for (std::string_view full : {"local {position: {x, y}, id: key} = entity", "function f({x, y}: P, z) return x end",
                 "return [for {x, position: {z}} in xs if z => x]"})
            for (size_t n = 0; n <= full.size(); ++n)
            {
                const std::string s(full.substr(0, n));
                const auto first = parseSurface(s), second = parseSurface(s);
                assert(first.errors.size() == second.errors.size());
                for (size_t k = 0; k < first.errors.size(); ++k)
                    assert(first.errors[k].range.begin == second.errors[k].range.begin && first.errors[k].message == second.errors[k].message);
                for (const auto& e : first.errors) assert(e.range.begin <= e.range.end && e.range.end <= s.size());
                const auto names = [&](const Pattern& pattern, auto&& self) -> void {
                    assert(pattern.range.begin <= pattern.range.end && pattern.range.end <= s.size());
                    for (const auto& field : pattern.fields)
                    {
                        assert(!field.key.empty() && field.key.end <= s.size());
                        self(field.target, self);
                    }
                };
                for (const auto& local : first.locals) names(local.pattern, names);
                for (const auto& parameter : first.parameters) names(parameter.pattern, names);
                const bool whole = n == full.size();
                assert(whole ? first.errors.empty() : true);
            }
        const auto d = parseSurface("local {position: {x");
        assert(d.locals.size() == 1 && !d.locals[0].pattern.complete && d.locals[0].pattern.close.empty());
        assert(d.locals[0].pattern.fields[0].target.fields[0].target.range.end == 19);
    }
    {
        // The record nesting budget is one fatal diagnostic, not a cascade or a deep stack.
        for (std::string_view head : {"local ", "function f(", "return [for "})
        {
            std::string s(head);
            for (int i = 0; i < 4096; ++i) s += "{a: ";
            s += "x";
            const auto d = parseSurface(s);
            assert(d.errors.size() == 1 && d.errors[0].message == "record pattern nesting limit exceeded (128)");
        }
    }
    {
        // The lexing shortcut never hides a pattern: each of these must still be parsed.
        for (std::string_view s : {"local {x} = t", "local\n\t{x} = t", "local --[[c]] {x} = t", "local [a] = t",
                 "function f({x}) end", "function f(a, {x}) end", "function T.a:b<T>(a: Map<K, V>, {x}: P) end",
                 "function f(cb: () -> (), {x}) end", "function f(cb: (number) -> string, {x}) end",
                 "local g = function(--[[)]] {x}) end", "return function(a, \"{\", {x}) end", "for {x} in t do end",
                 "local f = function\n(\n{x}\n) end", "function f(a, -- note\n  {x}) end", "function f(a, --[[ x ]] {y}) end",
                 "function f(a --[==[\n]==], {x}) end", "local -- note\n{x} = t", "local --[==[ note ]==] {x} = t",
                 "for --[[x]] {a} in t do end", "local\n\n{x} = t", "function f(\n-- a\n{x}) end"})
        {
            const auto d = parseSurface(s);
            assert(!d.locals.empty() || !d.parameters.empty() || !d.errors.empty());
        }
        // And sources without patterns lex nothing extra: no records, no errors.
        for (std::string_view s : {"local t = {1, {2}}", "f({x = 1}, {y})", "function f(t: {number}) return {t} end"})
        {
            const auto d = parseSurface(s);
            assert(d.locals.empty() && d.parameters.empty() && d.errors.empty());
        }
    }
    std::cout << "surface frontend tests passed\n";
}
