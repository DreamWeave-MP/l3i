// Semantic lowering of the canonical surface document. Recognition and recovery live
// exclusively in surface_frontend.cpp; there is no legacy comprehension grammar here.
#include "surface_syntax.h"

#include <algorithm>
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
                shift(clause.expression, begin);
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
        : source(source), document(document), recovery(recovery), fuse(fuse) {}

    Text range(Range input)
    {
        Text result;
        size_t copied = input.begin;
        auto it = std::lower_bound(document.comprehensions.begin(), document.comprehensions.end(), input.begin,
            [](const Comprehension& node, size_t begin) { return node.open.begin < begin; });
        for (; it != document.comprehensions.end() && it->open.begin < input.end; ++it)
        {
            if (it->range.begin < copied || it->range.end > input.end || it->range.end <= it->range.begin)
                continue;
            const bool count = fuse && !it->lengthPrefix.empty() && it->lengthPrefix.begin >= copied;
            const bool sum = !it->sumPrefix.empty() && it->sumPrefix.begin >= copied;
            const size_t begin = sum ? it->sumPrefix.begin : count ? it->lengthPrefix.begin : it->range.begin;
            result.copy(source, {copied, begin});
            result.append(comprehension(size_t(it - document.comprehensions.begin()), count, sum));
            copied = it->range.end;
        }
        result.copy(source, {copied, input.end});
        return result;
    }

private:
    std::string_view source;
    const Document& document;
    bool recovery;
    bool fuse;

    std::string stem(size_t offset) const
    {
        for (size_t salt = 0;; ++salt)
        {
            std::string name = "__l3i_comp_" + std::to_string(offset);
            if (salt)
                name += "_" + std::to_string(salt);
            if (source.find(name) == std::string_view::npos)
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

    void projection(Text& out, const Comprehension& node, const std::string& name, ComprehensionSite& site)
    {
        out.anchor = node.projection;
        out += "local " + name + "_value = ";
        site.projection = expression(out, node.projection, "(nil :: any)", node.projectionSuffix);
        out += " if " + name + "_value == nil then error(\"L3i comprehension projection produced nil; filter nil explicitly\") end ";
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
        if (generators == 1)
        {
            const Clause& generator = node.clauses.front();
            out.anchor = generator.range;
            out += "local " + name + "_g0_src = ";
            site.clauses[0].expression = expression(out, generator.expression, "({} :: {any})", generator.expressionSuffix);
            out += " local " + name + "_g0_len = #" + name + "_g0_src ";
            if (!scalar)
                out += "local " + output + " = table.create(" + name + "_g0_len) :: typeof({}) ";
            if (!exact || scalar)
                out += "local " + cursor + " = 0 ";
            out += "for " + name + "_g0_i = 1, " + name + "_g0_len do local ";
            const size_t binding = out.size();
            if (generator.binding.empty())
                out += name + "_missing";
            else
                out.copy(source, generator.binding);
            site.clauses[0].binding = {binding, out.size()};
            out += " = " + name + "_g0_src[" + name + "_g0_i] ";
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
                out += "local " + prefix + "_src = ";
                site.clauses[i].expression = expression(out, clause.expression, "({} :: {any})", clause.expressionSuffix);
                out += " local " + prefix + "_len = #" + prefix + "_src for " + prefix + "_i = 1, " + prefix + "_len do local ";
                const size_t binding = out.size();
                if (clause.binding.empty())
                    out += prefix + "_missing";
                else
                    out.copy(source, clause.binding);
                site.clauses[i].binding = {binding, out.size()};
                out += " = " + prefix + "_src[" + prefix + "_i] ";
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
    if (document.comprehensions.empty() || (!recovery && !document.errors.empty()))
        return {std::string(source), {}, std::move(document), {}};
    Lowerer lowerer(source, document, recovery, fuseLength);
    Text output = lowerer.range({0, source.size()});
    SourceMap map(source, output.text, std::move(output.segments));
    return {std::move(output.text), std::move(map), std::move(document), std::move(output.sites)};
}
}
