// Semantic lowering of the canonical surface document. Recognition and recovery live
// exclusively in surface_frontend.cpp; there is no legacy comprehension grammar here.
#include "surface_syntax.h"

#include <algorithm>
#include <cctype>
#include <unordered_set>
#include <utility>

namespace L3i::Surface
{
namespace
{
void shift(Range& range, size_t offset)
{
    range.begin += offset;
    range.end += offset;
}

struct Text
{
    std::string text;
    std::vector<Segment> segments;
    std::vector<ComprehensionSite> sites;
    Range anchor;

    size_t size() const { return text.size(); }
    void record(Segment segment)
    {
        if (segment.begin == segment.end)
            return;
        if (!segments.empty())
        {
            auto& previous = segments.back();
            if (previous.end == segment.begin && previous.copied == segment.copied
                && (segment.copied ? previous.originalEnd == segment.originalBegin
                                   : previous.originalBegin == segment.originalBegin && previous.originalEnd == segment.originalEnd))
            {
                previous.end = segment.end;
                previous.originalEnd = segment.originalEnd;
                return;
            }
        }
        segments.push_back(segment);
    }
    Text& operator+=(std::string_view value)
    {
        const size_t begin = size();
        text.append(value.data(), value.size());
        record({begin, size(), anchor.begin, anchor.end, false});
        return *this;
    }
    void append(const Text& value)
    {
        const size_t begin = size();
        text += value.text;
        for (auto segment : value.segments)
        {
            segment.begin += begin;
            segment.end += begin;
            record(segment);
        }
        for (auto site : value.sites)
        {
            shift(site.call, begin);
            shift(site.projection, begin);
            for (auto& clause : site.clauses)
            {
                shift(clause.binding, begin);
                for (auto& binding : clause.bindings)
                    shift(binding, begin);
                shift(clause.expression, begin);
                for (auto& argument : clause.rangeArguments)
                    shift(argument, begin);
                for (auto& argument : clause.zipArguments)
                    shift(argument, begin);
                shift(clause.sliceSource, begin);
                shift(clause.sliceFirst, begin);
                shift(clause.sliceLast, begin);
            }
            sites.push_back(std::move(site));
        }
    }
    void copy(std::string_view source, Range range)
    {
        const size_t begin = size();
        text.append(source.data() + range.begin, range.end - range.begin);
        record({begin, size(), range.begin, range.end, true});
    }
};

class Lowerer
{
public:
    Lowerer(std::string_view source, const Document& document, bool recovery, bool fuse)
        : source(source), document(document), recovery(recovery), fuse(fuse)
    {
        for (size_t begin = 0; begin < source.size();)
        {
            const unsigned char first = static_cast<unsigned char>(source[begin]);
            if (!(first == '_' || std::isalpha(first)))
            {
                ++begin;
                continue;
            }
            size_t end = begin + 1;
            while (end < source.size())
            {
                const unsigned char next = static_cast<unsigned char>(source[end]);
                if (!(next == '_' || std::isalnum(next))) break;
                ++end;
            }
            names.emplace(source.substr(begin, end - begin));
            begin = end;
        }
        operations = stem(source.size());
    }

    Text lower()
    {
        Text result;
        result.anchor = document.comprehensions.empty() ? document.slices.front().range : document.comprehensions.front().range;
        size_t prefix = 0;
        auto comment = document.comments.begin();
        for (;;)
        {
            while (prefix < source.size() && std::isspace(static_cast<unsigned char>(source[prefix])))
                ++prefix;
            while (comment != document.comments.end() && comment->end <= prefix)
                ++comment;
            if (comment == document.comments.end() || comment->begin != prefix)
                break;
            prefix = comment->end;
            ++comment;
        }
        result.copy(source, {0, prefix});
        result += "local " + operations + "_error = error ";
        const bool needsTable = std::any_of(document.comprehensions.begin(), document.comprehensions.end(), [&](const Comprehension& node) {
            const size_t generators = std::count_if(node.clauses.begin(), node.clauses.end(),
                [](const Clause& clause) { return clause.kind == ClauseKind::Generator; });
            return node.sumPrefix.empty() && (!fuse || node.lengthPrefix.empty()) && generators == 1 &&
                !node.clauses.empty() && node.clauses.front().rangeArguments.empty();
        });
        if (needsTable)
        {
            result += "local " + operations + "_table_create = table.create ";
            preludeStatements = 2;
        }
        if (!document.slices.empty() && !needsTable)
        {
            result += "local " + operations + "_table_create = table.create ";
            preludeStatements = 2;
        }
        result.append(range({prefix, source.size()}));
        return result;
    }

    size_t prelude() const { return preludeStatements; }

    Text range(Range input)
    {
        Text result;
        size_t copied = input.begin;
        auto it = std::lower_bound(document.comprehensions.begin(), document.comprehensions.end(), input.begin,
            [](const Comprehension& node, size_t begin) { return node.open.begin < begin; });
        auto sliceIt = std::lower_bound(document.slices.begin(), document.slices.end(), input.begin,
            [](const Slice& node, size_t begin) { return node.range.begin < begin; });
        while ((it != document.comprehensions.end() && it->open.begin < input.end) ||
            (sliceIt != document.slices.end() && sliceIt->range.begin < input.end))
        {
            const bool useComprehension = it != document.comprehensions.end() && it->open.begin < input.end &&
                (sliceIt == document.slices.end() || sliceIt->range.begin >= it->open.begin);
            if (useComprehension)
            {
                if (it->range.begin < copied || it->range.end > input.end || it->range.end <= it->range.begin)
                {
                    ++it;
                    continue;
                }
                const bool count = fuse && !it->lengthPrefix.empty() && it->lengthPrefix.begin >= copied;
                const bool sum = !it->sumPrefix.empty() && it->sumPrefix.begin >= copied;
                const size_t begin = sum ? it->sumPrefix.begin : count ? it->lengthPrefix.begin : it->range.begin;
                result.copy(source, {copied, begin});
                result.append(comprehension(size_t(it - document.comprehensions.begin()), count, sum));
                copied = it->range.end;
                ++it;
            }
            else
            {
                if (sliceIt->range.begin < copied || sliceIt->range.end > input.end || sliceIt->range.end <= sliceIt->range.begin)
                {
                    ++sliceIt;
                    continue;
                }
                result.copy(source, {copied, sliceIt->range.begin});
                result.append(slice(size_t(sliceIt - document.slices.begin())));
                copied = sliceIt->range.end;
                ++sliceIt;
            }
        }
        result.copy(source, {copied, input.end});
        return result;
    }

private:
    std::string_view source;
    const Document& document;
    bool recovery;
    bool fuse;
    std::unordered_set<std::string_view> names;
    std::string operations;
    size_t preludeStatements = 1;

    std::string stem(size_t offset) const
    {
        for (size_t salt = 0;; ++salt)
        {
            std::string name = "__l3i_comp_" + std::to_string(offset);
            if (salt)
                name += "_" + std::to_string(salt);
            if (!names.count(name))
                return name;
        }
    }

    Range expression(Text& out, Range original, std::string_view missing, std::string_view suffix = {})
    {
        out.anchor = original;
        if (recovery && !original.empty())
        {
            out.anchor = {original.begin, original.begin};
            out += "("; // Fence ordinary parser recovery away from generated statements.
            out.anchor = original;
        }
        const size_t begin = out.size();
        if (original.empty())
            out += missing; // Tooling only: strict lowering never reaches a structural hole.
        else
            out.append(range(original));
        if (!suffix.empty())
        {
            out.anchor = {original.end, original.end};
            out += suffix;
            out.anchor = original;
        }
        const size_t end = out.size();
        if (recovery && !original.empty())
        {
            out.anchor = {original.end, original.end};
            out += ")";
            out.anchor = original;
        }
        return {begin, end};
    }

    Text slice(size_t index)
    {
        const Slice& node = document.slices[index];
        Text out;
        out.anchor = node.range;
        out += "(function() local __slice_src = ";
        expression(out, node.source, "({} :: {any})");
        out += " local __slice_len = #__slice_src local __slice_first = ";
        expression(out, node.first, "(1 :: number)");
        out += " local __slice_last = ";
        expression(out, node.last, "(0 :: number)");
        out += " if __slice_first < 1 then __slice_first = 1 end if __slice_last > __slice_len then __slice_last = __slice_len end local __slice_n = 0 if __slice_last >= __slice_first then __slice_n = __slice_last - __slice_first + 1 end local __slice_out = " + operations + "_table_create(__slice_n) local __slice_j = 0 for __slice_i = __slice_first, __slice_last do __slice_j += 1 __slice_out[__slice_j] = __slice_src[__slice_i] end return __slice_out end)()";
        return out;
    }

    void projection(Text& out, const Comprehension& node, const std::string& name, ComprehensionSite& site)
    {
        out.anchor = node.projection;
        out += "local " + name + "_value = ";
        site.projection = expression(out, node.projection, "(nil :: any)", node.projectionSuffix);
        out += " if " + name + "_value == nil then " + operations
            + "_error(\"L3i comprehension projection produced nil; filter nil explicitly\") end ";
    }

    void comments(Text& out, const Comprehension& node, size_t begin)
    {
        // Preserve trivia removed with surface punctuation. Comments inside expressions
        // remain copied there; nested expressions preserve their own clause trivia.
        auto it = std::lower_bound(document.comments.begin(), document.comments.end(), begin,
            [](Range comment, size_t at) { return comment.begin < at; });
        bool newline = false;
        for (; it != document.comments.end() && it->end <= node.range.end; ++it)
        {
            bool copied = it->begin >= node.projection.begin && it->end <= node.projection.end;
            for (const Clause& clause : node.clauses)
                copied = copied || (it->begin >= clause.expression.begin && it->end <= clause.expression.end);
            if (copied)
                continue;
            if (!newline)
            {
                out += "\n";
                newline = true;
            }
            out.copy(source, *it);
            out += "\n";
        }
    }

    Text comprehension(size_t index, bool count, bool sum)
    {
        const Comprehension& node = document.comprehensions[index];
        const std::string name = stem(node.open.begin);
        const bool scalar = count || sum;
        const std::string output = name + "_out", cursor = name + (sum ? "_sum" : "_n");
        Text out;
        out.anchor = {sum ? node.sumPrefix.begin : count ? node.lengthPrefix.begin : node.range.begin, node.range.end};
        if (node.postfix)
            out += "["; // Recovery-only index fence; strict compilation rejects the document.
        const size_t callBegin = out.size();
        out += "(function() ";
        comments(out, node, out.anchor.begin);
        ComprehensionSite site;
        site.comprehension = index;
        site.clauses.resize(node.clauses.size());
        const bool exact = node.clauses.size() == 1 && node.clauses[0].kind == ClauseKind::Generator;
        size_t generators = 0;
        for (const Clause& clause : node.clauses)
            generators += clause.kind == ClauseKind::Generator;
        if (generators != 1 && !scalar)
        {
            out += "local " + output + " = {} ";
        }
        // The one-source cases need the source before allocation. Emit them separately
        // to preserve the proven bytecode shape, not a second recognition path.
        if (generators == 1 && !node.clauses.front().rangeArguments.empty())
        {
            const Clause& generator = node.clauses.front();
            if (!scalar)
                out += "local " + output + " = {} ";
            out += "local " + cursor + " = 0 ";
            const std::string prefix = name + "_g0";
            for (size_t i = 0; i < generator.rangeArguments.size(); ++i)
            {
                out += "local " + prefix + "_r" + std::to_string(i) + " = ";
                const size_t begin = out.size();
                expression(out, generator.rangeArguments[i], "(0 :: number)");
                site.clauses[0].rangeArguments.push_back({begin, out.size()});
                out += " ";
            }
            if (generator.rangeArguments.size() == 2)
                out += "local " + prefix + "_r2 = 1 ";
            out += "if " + prefix + "_r2 == 0 then " + operations + "_error(\"JSL range step must not be zero\") end for ";
            const size_t binding = out.size();
            if (generator.binding.empty()) out += name + "_missing";
            else out.copy(source, generator.binding);
            site.clauses[0].binding = {binding, out.size()};
            out += " = " + prefix + "_r0, " + prefix + "_r1, " + prefix + "_r2 do ";
            for (size_t i = 1; i < node.clauses.size(); ++i)
            {
                out += "if ";
                site.clauses[i].expression = expression(out, node.clauses[i].expression, "true", node.clauses[i].expressionSuffix);
                out += " then ";
            }
            projection(out, node, name, site);
            out += cursor + " += " + (sum ? name + "_value " : "1 ");
            if (!scalar)
                out += output + "[" + cursor + "] = " + name + "_value ";
            for (size_t i = 1; i < node.clauses.size(); ++i)
                out += "end ";
            out += "end ";
        }
        else if (generators == 1 && !node.clauses.front().sliceSource.empty())
        {
            const Clause& generator = node.clauses.front();
            const std::string prefix = name + "_g0";
            out.anchor = generator.range;
            out += "local " + prefix + "_src = ";
            const size_t sourceBegin = out.size();
            expression(out, generator.sliceSource, "({} :: {any})");
            site.clauses[0].sliceSource = {sourceBegin, out.size()};
            out += " local " + prefix + "_len = #" + prefix + "_src local " + prefix + "_first = ";
            const size_t firstBegin = out.size();
            expression(out, generator.sliceFirst, "(1 :: number)");
            site.clauses[0].sliceFirst = {firstBegin, out.size()};
            out += " local " + prefix + "_last = ";
            const size_t lastBegin = out.size();
            expression(out, generator.sliceLast, "(0 :: number)");
            site.clauses[0].sliceLast = {lastBegin, out.size()};
            out += " if " + prefix + "_first < 1 then " + prefix + "_first = 1 end if " + prefix + "_last > " + prefix + "_len then " + prefix + "_last = " + prefix + "_len end local " + prefix + "_n = 0 if " + prefix + "_last >= " + prefix + "_first then " + prefix + "_n = " + prefix + "_last - " + prefix + "_first + 1 end ";
            if (!scalar)
                out += "local " + output + " = " + operations + "_table_create(" + prefix + "_n) :: typeof({}) ";
            if (!exact || scalar || !generator.sliceSource.empty())
                out += "local " + cursor + " = 0 ";
            out += "for " + prefix + "_i = " + prefix + "_first, " + prefix + "_last do local ";
            const size_t binding = out.size();
            out.copy(source, generator.binding);
            site.clauses[0].binding = {binding, out.size()};
            site.clauses[0].bindings.push_back(site.clauses[0].binding);
            out += " = " + prefix + "_src[" + prefix + "_i] ";
            for (size_t i = 1; i < node.clauses.size(); ++i)
            {
                out += "if ";
                site.clauses[i].expression = expression(out, node.clauses[i].expression, "true", node.clauses[i].expressionSuffix);
                out += " then ";
            }
            projection(out, node, name, site);
            if (!exact || scalar || !generator.sliceSource.empty())
                out += cursor + " += " + (sum ? name + "_value " : "1 ");
            if (!scalar)
                out += output + "[" + (exact && generator.sliceSource.empty() ? prefix + "_i" : cursor) + "] = " + name + "_value ";
            for (size_t i = 1; i < node.clauses.size(); ++i) out += "end ";
            out += "end ";
        }
        else if (generators == 1 && !node.clauses.front().zipArguments.empty())
        {
            const Clause& generator = node.clauses.front();
            const std::string prefix = name + "_g0";
            out.anchor = generator.range;
            for (size_t argument = 0; argument < generator.zipArguments.size(); ++argument)
            {
                out += "local " + prefix + "_src" + std::to_string(argument) + " = ";
                const size_t begin = out.size();
                expression(out, generator.zipArguments[argument], "({} :: {any})");
                site.clauses[0].zipArguments.push_back({begin, out.size()});
                out += " local " + prefix + "_len" + std::to_string(argument) + " = #" + prefix + "_src" + std::to_string(argument) + " ";
            }
            out += "local " + prefix + "_n = " + prefix + "_len0 ";
            for (size_t argument = 1; argument < generator.zipArguments.size(); ++argument)
                out += "if " + prefix + "_len" + std::to_string(argument) + " < " + prefix + "_n then " + prefix + "_n = " + prefix + "_len" + std::to_string(argument) + " end ";
            if (generator.zipStrict)
                for (size_t argument = 1; argument < generator.zipArguments.size(); ++argument)
                    out += "if " + prefix + "_len" + std::to_string(argument) + " ~= " + prefix + "_n then " + operations + "_error(\"JSL zipStrict inputs must have equal lengths\") end ";
            if (!scalar)
                out += "local " + output + " = " + operations + "_table_create(" + prefix + "_n) :: typeof({}) ";
            if (!exact || scalar)
                out += "local " + cursor + " = 0 ";
            out += "for " + prefix + "_i = 1, " + prefix + "_n do local ";
            for (size_t binding = 0; binding < generator.bindings.size(); ++binding)
            {
                if (binding) out += ", ";
                const size_t begin = out.size();
                out.copy(source, generator.bindings[binding]);
                site.clauses[0].bindings.push_back({begin, out.size()});
            }
            site.clauses[0].binding = site.clauses[0].bindings.front();
            out += " = ";
            for (size_t argument = 0; argument < generator.zipArguments.size(); ++argument)
            {
                if (argument) out += ", ";
                out += prefix + "_src" + std::to_string(argument) + "[" + prefix + "_i]";
            }
            out += " ";
            for (size_t i = 1; i < node.clauses.size(); ++i)
            {
                out.anchor = node.clauses[i].expression;
                out += "if ";
                site.clauses[i].expression = expression(out, node.clauses[i].expression, "true", node.clauses[i].expressionSuffix);
                out += " then ";
            }
            projection(out, node, name, site);
            if (!exact || scalar)
                out += cursor + " += " + (sum ? name + "_value " : "1 ");
            if (!scalar)
                out += output + "[" + (exact ? prefix + "_i" : cursor) + "] = " + name + "_value ";
            out.anchor = node.close;
            for (size_t i = 1; i < node.clauses.size(); ++i) out += "end ";
            out += "end ";
        }
        else if (generators == 1)
        {
            const Clause& generator = node.clauses.front();
            out.anchor = generator.range;
            out += "local " + name + "_g0_src = ";
            site.clauses[0].expression = expression(out,
                generator.enumerateArgument.empty() ? generator.expression : generator.enumerateArgument,
                "({} :: {any})", generator.expressionSuffix);
            out += " local " + name + "_g0_len = #" + name + "_g0_src ";
            if (!scalar)
                out += "local " + output + " = " + operations + "_table_create(" + name + "_g0_len) :: typeof({}) ";
            if (!exact || scalar)
                out += "local " + cursor + " = 0 ";
            out += "for " + name + "_g0_i = 1, " + name + "_g0_len do local ";
            if (!generator.enumerateArgument.empty())
            {
                for (size_t i = 0; i < generator.bindings.size(); ++i)
                {
                    if (i) out += ", ";
                    const size_t begin = out.size();
                    out.copy(source, generator.bindings[i]);
                    site.clauses[0].bindings.push_back({begin, out.size()});
                }
                site.clauses[0].binding = site.clauses[0].bindings.front();
                out += " = " + name + "_g0_i, " + name + "_g0_src[" + name + "_g0_i] ";
            }
            else
            {
                const size_t binding = out.size();
                if (generator.binding.empty())
                    out += name + "_missing";
                else
                    out.copy(source, generator.binding);
                site.clauses[0].binding = {binding, out.size()};
                site.clauses[0].bindings.push_back(site.clauses[0].binding);
                out += " = " + name + "_g0_src[" + name + "_g0_i] ";
            }
            for (size_t i = 1; i < node.clauses.size(); ++i)
            {
                out.anchor = node.clauses[i].expression;
                out += "if ";
                site.clauses[i].expression = expression(out, node.clauses[i].expression, "true", node.clauses[i].expressionSuffix);
                out += " then ";
            }
            projection(out, node, name, site);
            if (!exact || scalar)
                out += cursor + " += " + (sum ? name + "_value " : "1 ");
            if (!scalar)
                out += output + "[" + (exact ? name + "_g0_i" : cursor) + "] = " + name + "_value ";
            out.anchor = node.close;
            for (size_t i = 1; i < node.clauses.size(); ++i)
                out += "end ";
            out += "end ";
        }
        else
        {
            out += "local " + cursor + " = 0 ";
            size_t level = 0;
            for (size_t i = 0; i < node.clauses.size(); ++i)
            {
                const Clause& clause = node.clauses[i];
                out.anchor = clause.range;
                if (clause.kind == ClauseKind::Filter)
                {
                    out += "if ";
                    site.clauses[i].expression = expression(out, clause.expression, "true", clause.expressionSuffix);
                    out += " then ";
                    continue;
                }
                const std::string prefix = name + "_g" + std::to_string(level++);
                if (!clause.rangeArguments.empty())
                {
                    for (size_t argument = 0; argument < clause.rangeArguments.size(); ++argument)
                    {
                        out += "local " + prefix + "_r" + std::to_string(argument) + " = ";
                        const size_t begin = out.size();
                        expression(out, clause.rangeArguments[argument], "(0 :: number)");
                        site.clauses[i].rangeArguments.push_back({begin, out.size()});
                        out += " ";
                    }
                    if (clause.rangeArguments.size() == 2) out += "local " + prefix + "_r2 = 1 ";
                    out += "if " + prefix + "_r2 == 0 then " + operations + "_error(\"JSL range step must not be zero\") end for ";
                }
                else if (!clause.sliceSource.empty())
                {
                    out += "local " + prefix + "_src = ";
                    const size_t sourceBegin = out.size();
                    expression(out, clause.sliceSource, "({} :: {any})");
                    site.clauses[i].sliceSource = {sourceBegin, out.size()};
                    out += " local " + prefix + "_len = #" + prefix + "_src local " + prefix + "_first = ";
                    const size_t firstBegin = out.size();
                    expression(out, clause.sliceFirst, "(1 :: number)");
                    site.clauses[i].sliceFirst = {firstBegin, out.size()};
                    out += " local " + prefix + "_last = ";
                    const size_t lastBegin = out.size();
                    expression(out, clause.sliceLast, "(0 :: number)");
                    site.clauses[i].sliceLast = {lastBegin, out.size()};
                    out += " if " + prefix + "_first < 1 then " + prefix + "_first = 1 end if " + prefix + "_last > " + prefix + "_len then " + prefix + "_last = " + prefix + "_len end for " + prefix + "_i = " + prefix + "_first, " + prefix + "_last do local ";
                }
                else if (!clause.zipArguments.empty())
                {
                    for (size_t argument = 0; argument < clause.zipArguments.size(); ++argument)
                    {
                        out += "local " + prefix + "_src" + std::to_string(argument) + " = ";
                        const size_t begin = out.size();
                        expression(out, clause.zipArguments[argument], "({} :: {any})");
                        site.clauses[i].zipArguments.push_back({begin, out.size()});
                        out += " local " + prefix + "_len" + std::to_string(argument) + " = #" + prefix + "_src" + std::to_string(argument) + " ";
                    }
                    out += "local " + prefix + "_n = " + prefix + "_len0 ";
                    for (size_t argument = 1; argument < clause.zipArguments.size(); ++argument)
                        out += "if " + prefix + "_len" + std::to_string(argument) + " < " + prefix + "_n then " + prefix + "_n = " + prefix + "_len" + std::to_string(argument) + " end ";
                    if (clause.zipStrict)
                        for (size_t argument = 1; argument < clause.zipArguments.size(); ++argument)
                            out += "if " + prefix + "_len" + std::to_string(argument) + " ~= " + prefix + "_n then " + operations + "_error(\"JSL zipStrict inputs must have equal lengths\") end ";
                    out += "for " + prefix + "_i = 1, " + prefix + "_n do local ";
                }
                else
                {
                    out += "local " + prefix + "_src = ";
                    site.clauses[i].expression = expression(out,
                        clause.enumerateArgument.empty() ? clause.expression : clause.enumerateArgument,
                        "({} :: {any})", clause.expressionSuffix);
                    out += " local " + prefix + "_len = #" + prefix + "_src for " + prefix + "_i = 1, " + prefix + "_len do local ";
                }
                if (!clause.rangeArguments.empty())
                {
                    const size_t binding = out.size();
                    if (clause.binding.empty()) out += prefix + "_missing";
                    else out.copy(source, clause.binding);
                    site.clauses[i].binding = {binding, out.size()};
                    site.clauses[i].bindings.push_back(site.clauses[i].binding);
                    out += " = " + prefix + "_r0, " + prefix + "_r1, " + prefix + "_r2 do ";
                }
                else if (!clause.sliceSource.empty())
                {
                    const size_t binding = out.size();
                    out.copy(source, clause.binding);
                    site.clauses[i].binding = {binding, out.size()};
                    site.clauses[i].bindings.push_back(site.clauses[i].binding);
                    out += " = " + prefix + "_src[" + prefix + "_i] ";
                }
                else if (!clause.zipArguments.empty())
                {
                    for (size_t binding = 0; binding < clause.bindings.size(); ++binding)
                    {
                        if (binding) out += ", ";
                        const size_t begin = out.size();
                        out.copy(source, clause.bindings[binding]);
                        site.clauses[i].bindings.push_back({begin, out.size()});
                    }
                    site.clauses[i].binding = site.clauses[i].bindings.front();
                    out += " = ";
                    for (size_t argument = 0; argument < clause.zipArguments.size(); ++argument)
                    {
                        if (argument) out += ", ";
                        out += prefix + "_src" + std::to_string(argument) + "[" + prefix + "_i]";
                    }
                    out += " ";
                }
                else if (!clause.enumerateArgument.empty())
                {
                    for (size_t binding = 0; binding < clause.bindings.size(); ++binding)
                    {
                        if (binding) out += ", ";
                        const size_t begin = out.size();
                        out.copy(source, clause.bindings[binding]);
                        site.clauses[i].bindings.push_back({begin, out.size()});
                    }
                    site.clauses[i].binding = site.clauses[i].bindings.front();
                    out += " = " + prefix + "_i, " + prefix + "_src[" + prefix + "_i] ";
                }
                else
                {
                    const size_t binding = out.size();
                    if (clause.binding.empty()) out += prefix + "_missing";
                    else out.copy(source, clause.binding);
                    site.clauses[i].binding = {binding, out.size()};
                    site.clauses[i].bindings.push_back(site.clauses[i].binding);
                    out += " = " + prefix + "_src[" + prefix + "_i] ";
                }
            }
            projection(out, node, name, site);
            out += cursor + " += " + (sum ? name + "_value " : "1 ");
            if (!scalar)
                out += output + "[" + cursor + "] = " + name + "_value ";
            out.anchor = node.close;
            for (size_t i = 0; i < node.clauses.size(); ++i)
                out += "end ";
        }
        out.anchor = node.close;
        out += "return " + (scalar ? cursor : output) + " end)()";
        site.call = {callBegin, out.size()};
        if (node.postfix)
            out += "]";
        out.sites.push_back(std::move(site));
        return out;
    }
};
}

LoweredSource lower(std::string_view source, bool recovery, bool fuseLength)
{
    Document document = parseSurface(source);
    if (document.comprehensions.empty() && document.slices.empty() || (!recovery && !document.errors.empty()))
        return {std::string(source), {}, std::move(document), {}, 0};
    Lowerer lowerer(source, document, recovery, fuseLength);
    Text output = lowerer.lower();
    SourceMap map(source, output.text, std::move(output.segments));
    return {std::move(output.text), std::move(map), std::move(document), std::move(output.sites), lowerer.prelude()};
}
}
