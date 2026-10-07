// Semantic lowering of the canonical surface document. Recognition and recovery live
// exclusively in surface_frontend.cpp; there is no legacy comprehension grammar here.
#include "surface_syntax.h"

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <optional>
#include <string_view>
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
    std::vector<SliceSite> sliceSites;
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
            shift(site.sink, begin);
            shift(site.sinkOffset, begin);
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
        for (auto site : value.sliceSites)
        {
            shift(site.call, begin);
            shift(site.source, begin);
            shift(site.first, begin);
            shift(site.last, begin);
            sliceSites.push_back(std::move(site));
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
    Lowerer(std::string_view source, const Document& document, bool recovery, bool fuse,
        const std::vector<Range>& bufferGenerators, bool dynamicGenerators, const std::vector<size_t>& localFilters)
        : source(source), document(document), recovery(recovery), fuse(fuse), bufferGenerators(bufferGenerators),
          dynamicGenerators(dynamicGenerators), localFilters(localFilters)
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
        // A comprehension inside a sink destination is lowered as part of that destination
        // expression, so the walk over nodes in opening order must not emit it first.
        sinkParent.assign(document.comprehensions.size(), SIZE_MAX);
        for (size_t k = 0; k < document.comprehensions.size(); ++k)
        {
            const Comprehension& sink = document.comprehensions[k];
            if (sink.sinkPrefix.empty()) continue;
            auto first = std::lower_bound(document.comprehensions.begin(), document.comprehensions.end(), sink.sinkDestination.begin,
                [](const Comprehension& node, size_t begin) { return node.open.begin < begin; });
            for (; first != document.comprehensions.end() && first->open.begin < sink.sinkDestination.end; ++first)
            {
                const size_t inner = size_t(first - document.comprehensions.begin());
                if (sinkParent[inner] == SIZE_MAX) sinkParent[inner] = k; // Nearest sink is visited first.
            }
        }
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
        const bool hasSlices = !document.slices.empty() ||
            std::any_of(document.comprehensions.begin(), document.comprehensions.end(), [](const Comprehension& node) {
                return std::any_of(node.clauses.begin(), node.clauses.end(),
                    [](const Clause& clause) { return !clause.sliceSource.empty(); });
            });
        const bool hasSinks = std::any_of(document.comprehensions.begin(), document.comprehensions.end(),
            [](const Comprehension& node) { return !node.sinkPrefix.empty(); });
        if (hasSlices || hasSinks)
        {
            result += "local " + operations + "_typeof = typeof ";
            ++preludeStatements;
        }
        if (hasSinks)
        {
            result += "local " + operations + "_rawequal = rawequal ";
            ++preludeStatements;
        }
        // One receiver per distinct layout the recognized pipelines read (byte slices are u8).
        std::vector<SliceLayout> layouts;
        for (size_t index = 0; dynamicGenerators && index < document.comprehensions.size(); ++index)
        {
            const Comprehension& node = document.comprehensions[index];
            if (!knownDataOp(node, plan(node, {}, fuse && !node.lengthPrefix.empty(), node.reducer), index)) continue;
            const auto layout = sliceLayout(node.clauses.front().sliceKind).value_or(SliceLayout{"u8", 1, 1});
            if (std::none_of(layouts.begin(), layouts.end(), [&](const SliceLayout& known) { return known.receiver() == layout.receiver(); }))
                layouts.push_back(layout);
        }
        if (!layouts.empty())
        {
            // The data plane's alias global (nil when the runtime did not install the extension)
            // and the receivers, annotated so Luau's code generator lowers the calls.
            result += "local " + operations + "_data = __l3i_data ";
            ++preludeStatements;
            for (const SliceLayout& layout : layouts)
            {
                result += "local " + operations + layout.receiver() + ": dream_data_Kind_" + layout.kind + " = if " + operations +
                    "_data then " + operations + "_data." + layout.kind + "(" +
                    (layout.stride == layout.size ? "" : std::to_string(layout.stride)) + ") else nil ";
                ++preludeStatements;
            }
        }


        const bool hasBufferSinks = std::any_of(document.comprehensions.begin(), document.comprehensions.end(),
            [](const Comprehension& node) { return !node.sinkKind.empty(); });
        const bool hasTypedSlices = std::any_of(document.comprehensions.begin(), document.comprehensions.end(), [](const Comprehension& node) {
            return std::any_of(node.clauses.begin(), node.clauses.end(), [](const Clause& clause) { return !clause.sliceKind.empty(); });
        });
        if (!document.slices.empty() || !bufferGenerators.empty() || dynamicGenerators || hasBufferSinks || hasTypedSlices)
        {
            result += "local " + operations + "_buffer = buffer ";
            ++preludeStatements;
        }
        // Sized materialization is the one shape that preallocates (see Pipeline::single).
        const bool needsTable = std::any_of(document.comprehensions.begin(), document.comprehensions.end(), [&](const Comprehension& node) {
            const Pipeline pipeline = plan(node, {}, fuse && !node.lengthPrefix.empty(), node.reducer);
            return pipeline.consumer == Consumer::Materialize && pipeline.single() &&
                pipeline.stages.front().kind != SourceKind::Range;
        });
        if (needsTable)
        {
            result += "local " + operations + "_table_create = table.create ";
            ++preludeStatements;
        }
        if (!document.slices.empty())
        {
            if (!needsTable)
            {
                result += "local " + operations + "_table_create = table.create ";
                ++preludeStatements;
            }
            result += "local " + operations + "_table_move = table.move ";
            ++preludeStatements;
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
                const size_t index = size_t(it - document.comprehensions.begin());
                const size_t parent = sinkParent[index];
                const bool inDestination = parent != SIZE_MAX && document.comprehensions[parent].sinkPrefix.begin >= input.begin &&
                    document.comprehensions[parent].range.end <= input.end;
                if (it->range.begin < copied || it->range.end > input.end || it->range.end <= it->range.begin || inDestination)
                {
                    ++it;
                    continue;
                }
                const bool count = fuse && !it->lengthPrefix.empty() && it->lengthPrefix.begin >= copied;
                const bool sink = !it->sinkPrefix.empty() && it->sinkPrefix.begin >= copied;
                const Reducer reducer = !it->reducerPrefix.empty() && it->reducerPrefix.begin >= copied ? it->reducer : Reducer::None;
                const size_t begin = sink ? it->sinkPrefix.begin : reducer != Reducer::None ? it->reducerPrefix.begin : count ? it->lengthPrefix.begin : it->range.begin;
                result.copy(source, {copied, begin});
                result.append(comprehension(size_t(it - document.comprehensions.begin()), count, reducer));
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
    std::vector<size_t> sinkParent;
    size_t preludeStatements = 1;
    const std::vector<Range>& bufferGenerators;
    bool dynamicGenerators;
    const std::vector<size_t>& localFilters;

public:
    std::vector<size_t> pendingLocalFilters;

private:

    bool bufferGenerator(Range sourceRange) const
    {
        return std::any_of(bufferGenerators.begin(), bufferGenerators.end(), [&](Range range) {
            return range.begin == sourceRange.begin && range.end == sourceRange.end;
        });
    }

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

    void sliceChecks(Text& out, std::string_view prefix, Range sourceRange, Range firstRange, Range lastRange,
        bool allowBuffer = false)
    {
        out.anchor = sourceRange;
        out += "local " + std::string(prefix) + "_kind = " + operations + "_typeof(" + std::string(prefix) + "_src) ";
        if (allowBuffer)
            out += "if " + std::string(prefix) + "_kind ~= \"table\" and " + std::string(prefix) +
                "_kind ~= \"buffer\" then " + operations + "_error(\"JSL slice source must be a table or buffer\") end ";
        else
            out += "if " + std::string(prefix) + "_kind ~= \"table\" then " + operations +
                "_error(\"JSL comprehension slice source must be a table\") end ";
        boundChecks(out, prefix, firstRange, lastRange);
    }

    void boundChecks(Text& out, std::string_view prefix, Range firstRange, Range lastRange)
    {
        out.anchor = firstRange;
        out += "if " + operations + "_typeof(" + std::string(prefix) + "_first) ~= \"number\" or " +
            std::string(prefix) + "_first % 1 ~= 0 then " + operations +
            "_error(\"JSL slice first bound must be a finite integer\") end ";
        out.anchor = lastRange;
        out += "if " + operations + "_typeof(" + std::string(prefix) + "_last) ~= \"number\" or " +
            std::string(prefix) + "_last % 1 ~= 0 then " + operations +
            "_error(\"JSL slice last bound must be a finite integer\") end ";
    }

    Text slice(size_t index)
    {
        const Slice& node = document.slices[index];
        const std::string prefix = stem(node.range.begin) + "_slice";
        Text out;
        out.anchor = node.range;
        SliceSite site;
        site.slice = index;
        const size_t callBegin = out.size();
        out += "(function() local " + prefix + "_src = ";
        site.source = expression(out, node.source, "({} :: {any})");
        out += " local " + prefix + "_first = ";
        site.first = expression(out, node.first, "(1 :: number)");
        out += " local " + prefix + "_last = ";
        site.last = expression(out, node.last, "(0 :: number)");
        out += " ";
        if (const auto typed = sliceLayout(node.kind))
        {
            // A typed slice copies elements of the layout into a new contiguous buffer: one
            // `buffer.copy` when the layout is contiguous, else an element loop.
            out.anchor = node.source;
            out += "if " + operations + "_typeof(" + prefix + "_src) ~= \"buffer\" then " + operations +
                "_error(\"JSL typed slice source must be a buffer\") end ";
            boundChecks(out, prefix, node.first, node.last);
            const std::string size = std::to_string(typed->size), stride = std::to_string(typed->stride);
            out += "local " + prefix + "_len = " + typed->count(operations + "_buffer.len(" + prefix + "_src)") + " if " + prefix +
                "_first < 1 then " + prefix + "_first = 1 end if " + prefix + "_last > " + prefix + "_len then " + prefix + "_last = " +
                prefix + "_len end local " + prefix + "_n = 0 if " + prefix + "_last >= " + prefix + "_first then " + prefix + "_n = " +
                prefix + "_last - " + prefix + "_first + 1 end local " + prefix + "_out = " + operations + "_buffer.create(" + prefix +
                "_n * " + size + ") ";
            if (typed->contiguous())
                out += "if " + prefix + "_n > 0 then " + operations + "_buffer.copy(" + prefix + "_out, 0, " + prefix + "_src, (" + prefix +
                    "_first - 1) * " + size + ", " + prefix + "_n * " + size + ") end ";
            else
                out += "for " + prefix + "_i = 0, " + prefix + "_n - 1 do " + operations + "_buffer.write" + typed->kind + "(" + prefix +
                    "_out, " + prefix + "_i * " + size + ", " + operations + "_buffer.read" + typed->kind + "(" + prefix + "_src, " +
                    typed->at(prefix + "_first + " + prefix + "_i") + ")) end ";
            out += "return " + prefix + "_out end)()";
            site.call = {callBegin, out.size()};
            out.sliceSites.push_back(std::move(site));
            return out;
        }
        sliceChecks(out, prefix, node.source, node.first, node.last, true);
        out += "local " + prefix + "_len = 0 if " + prefix + "_kind == \"buffer\" then " + prefix + "_len = " + operations + "_buffer.len(" + prefix + "_src :: any) else " + prefix + "_len = #(" + prefix + "_src :: any) end if " + prefix + "_first < 1 then " + prefix + "_first = 1 end if " + prefix + "_last > " + prefix + "_len then " + prefix + "_last = " + prefix + "_len end local " + prefix + "_n = 0 if " + prefix + "_last >= " + prefix + "_first then " + prefix + "_n = " + prefix + "_last - " + prefix + "_first + 1 end if " + prefix + "_kind == \"buffer\" then local " + prefix + "_out = (" + operations + "_buffer.create(" + prefix + "_n) :: any) :: typeof(" + prefix + "_src) if " + prefix + "_n > 0 then " + operations + "_buffer.copy(" + prefix + "_out :: any, 0, " + prefix + "_src :: any, " + prefix + "_first - 1, " + prefix + "_n) end return " + prefix + "_out else local " + prefix + "_out = (" + operations + "_table_create(" + prefix + "_n) :: any) :: typeof(" + prefix + "_src) if " + prefix + "_n > 0 then " + operations + "_table_move(" + prefix + "_src :: any, " + prefix + "_first, " + prefix + "_last, 1, " + prefix + "_out :: any) end return " + prefix + "_out end end)()";
        site.call = {callBegin, out.size()};
        out.sliceSites.push_back(std::move(site));
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

    // What a generator iterates. Every surface source form is one of these; lowering a
    // pipeline is then one walk over its stages rather than one emitter per combination.
    enum class SourceKind { Plain, Enumerate, Range, Zip, Slice };

    static SourceKind kindOf(const Clause& clause)
    {
        if (!clause.rangeArguments.empty()) return SourceKind::Range;
        if (!clause.sliceSource.empty()) return SourceKind::Slice;
        if (!clause.zipArguments.empty()) return SourceKind::Zip;
        if (!clause.enumerateArgument.empty()) return SourceKind::Enumerate;
        return SourceKind::Plain;
    }

    // What the pipeline feeds. Materialize builds the dense result; Count and Sum accumulate
    // a number from 0; Min and Max keep the least/greatest projection by Luau ordering and
    // reject an empty pipeline; Any and All return at the first deciding projection.
    // Sink writes accepted projections into a caller-owned table and evaluates to it.
    enum class Consumer { Materialize, Count, Sum, Min, Max, Any, All, Sink };

    static Consumer consumerOf(bool count, Reducer reducer, bool sink)
    {
        if (sink) return Consumer::Sink;
        switch (reducer)
        {
        case Reducer::Sum: return Consumer::Sum;
        case Reducer::Min: return Consumer::Min;
        case Reducer::Max: return Consumer::Max;
        case Reducer::Any: return Consumer::Any;
        case Reducer::All: return Consumer::All;
        case Reducer::None: break;
        }
        return count ? Consumer::Count : Consumer::Materialize;
    }

    // The accumulator's generated suffix; empty for consumers that keep no state.
    static const char* accumulator(Consumer consumer)
    {
        switch (consumer)
        {
        case Consumer::Sum: return "_sum";
        case Consumer::Min: return "_min";
        case Consumer::Max: return "_max";
        case Consumer::Any:
        case Consumer::All: return "";
        case Consumer::Materialize:
        case Consumer::Count:
        case Consumer::Sink: break;
        }
        return "_n";
    }

    // One clause of the pipeline: a generator over a source, or a filter.
    struct Stage
    {
        size_t clause = 0; // Index into the comprehension's clauses.
        bool generator = false;
        SourceKind kind = SourceKind::Plain; // Generators only.
        std::string prefix;                  // Generators only: the loop's generated name stem.
    };

    // The collection pipeline a comprehension denotes: ordered stages feeding one consumer.
    struct Pipeline
    {
        Consumer consumer = Consumer::Materialize;
        std::vector<Stage> stages;
        size_t generators = 0;
        // One generator leading the pipeline: its element count is known before allocation
        // and (with no filters) its loop index is the output index.
        bool single() const { return generators == 1 && !stages.empty() && stages.front().generator; }
    };

    Pipeline plan(const Comprehension& node, const std::string& name, bool count, Reducer reducer) const
    {
        Pipeline pipeline;
        pipeline.consumer = consumerOf(count, reducer, !node.sinkPrefix.empty());
        for (size_t i = 0; i < node.clauses.size(); ++i)
        {
            const Clause& clause = node.clauses[i];
            Stage stage;
            stage.clause = i;
            stage.generator = clause.kind == ClauseKind::Generator;
            if (stage.generator)
            {
                stage.kind = kindOf(clause);
                stage.prefix = name + "_g" + std::to_string(pipeline.generators++);
            }
            pipeline.stages.push_back(std::move(stage));
        }
        return pipeline;
    }

    // A pipeline whose buffer path is a known data operation: a lone leading slice generator,
    // the projection exactly its binding, no filter or one `binding <op> literal` filter, and a
    // consumer the data plane computes bit for bit as the scalar loop would (sum, min, max,
    // count). Everything else keeps the fused scalar loop: arbitrary expressions have effects.
    // How many identifier-threshold candidates one chunk may hand the compiler's second pass.
    static constexpr size_t maxLocalFilterCandidates = 256;
    // Below this many elements the receiver's unrolled native sum beats the bound call under
    // jit; above it the bound call's vectorized loop wins (DATA_PLANE.md §2.4).
    static constexpr size_t boundSumThreshold = 4096;

    struct KnownDataOp
    {
        Consumer consumer = Consumer::Count;
        std::string comparison; // Data-plane comparison name; empty without a filter.
        std::string threshold;  // The literal or identifier, as written.
        bool identifier = false; // The threshold is an identifier verified to be an outer local.
    };

    static bool identifierName(std::string_view text)
    {
        if (text.empty() || !(std::isalpha(static_cast<unsigned char>(text.front())) || text.front() == '_')) return false;
        for (const char c : text)
            if (!(std::isalnum(static_cast<unsigned char>(c)) || c == '_')) return false;
        static constexpr std::string_view keywords[] = {"and", "break", "do", "else", "elseif", "end", "false", "for", "function",
            "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "continue"};
        for (const auto keyword : keywords)
            if (text == keyword) return false;
        return true;
    }

    // `gt` to `Gt`: the receiver's comparison-count method suffix.
    static std::string capitalized(std::string_view name)
    {
        std::string result(name);
        if (!result.empty()) result[0] = static_cast<char>(std::toupper(static_cast<unsigned char>(result[0])));
        return result;
    }

    static std::string_view trim(std::string_view text)
    {
        while (!text.empty() && std::isspace(static_cast<unsigned char>(text.front()))) text.remove_prefix(1);
        while (!text.empty() && std::isspace(static_cast<unsigned char>(text.back()))) text.remove_suffix(1);
        return text;
    }

    // `name <op> [-]<number literal>` and nothing else; the data plane's comparison name.
    static bool literalComparison(std::string_view text, std::string_view name, std::string& comparison, std::string& threshold)
    {
        text = trim(text);
        if (text.substr(0, name.size()) != name) return false;
        text.remove_prefix(name.size());
        if (!text.empty() && (std::isalnum(static_cast<unsigned char>(text.front())) || text.front() == '_')) return false;
        text = trim(text);
        static constexpr std::pair<std::string_view, std::string_view> operators[] = {
            {"<=", "le"}, {">=", "ge"}, {"==", "eq"}, {"~=", "ne"}, {"<", "lt"}, {">", "gt"}};
        comparison.clear();
        for (const auto& [spelling, named] : operators)
            if (text.substr(0, spelling.size()) == spelling)
            {
                comparison = named;
                text.remove_prefix(spelling.size());
                break;
            }
        if (comparison.empty()) return false;
        text = trim(text);
        if (!numberLiteral(text) && !(identifierName(text) && text != name)) return false;
        threshold = std::string(text);
        return true;
    }

    // A whole Luau number literal, optionally negated: decimal with fraction/exponent, hex, or
    // binary, with digit separators. Conservative: anything else is not recognized.
    static bool numberLiteral(std::string_view text)
    {
        size_t at = 0;
        if (at < text.size() && text[at] == '-') ++at;
        const auto digitsWhile = [&](auto accept) {
            const size_t start = at;
            while (at < text.size() && (accept(static_cast<unsigned char>(text[at])) || text[at] == '_')) ++at;
            return at > start;
        };
        if (at + 1 < text.size() && text[at] == '0' && (text[at + 1] == 'x' || text[at + 1] == 'X'))
        {
            at += 2;
            if (!digitsWhile([](unsigned char c) { return std::isxdigit(c) != 0; })) return false;
        }
        else if (at + 1 < text.size() && text[at] == '0' && (text[at + 1] == 'b' || text[at + 1] == 'B'))
        {
            at += 2;
            if (!digitsWhile([](unsigned char c) { return c == '0' || c == '1'; })) return false;
        }
        else
        {
            const bool whole = digitsWhile([](unsigned char c) { return std::isdigit(c) != 0; });
            bool fraction = false;
            if (at < text.size() && text[at] == '.')
            {
                ++at;
                fraction = digitsWhile([](unsigned char c) { return std::isdigit(c) != 0; });
            }
            if (!whole && !fraction) return false;
            if (at < text.size() && (text[at] == 'e' || text[at] == 'E'))
            {
                ++at;
                if (at < text.size() && (text[at] == '+' || text[at] == '-')) ++at;
                if (!digitsWhile([](unsigned char c) { return std::isdigit(c) != 0; })) return false;
            }
        }
        return at == text.size();
    }


    std::optional<KnownDataOp> knownDataOp(const Comprehension& node, const Pipeline& pipeline, size_t index)
    {
        if (!pipeline.single() || pipeline.stages.front().kind != SourceKind::Slice || node.clauses.size() > 2) return std::nullopt;
        const Clause& generator = node.clauses.front();
        if (generator.binding.empty() || generator.bindings.size() > 1 || node.projection.empty()) return std::nullopt;
        const auto text = [&](Range range) { return std::string_view(source).substr(range.begin, range.end - range.begin); };
        const std::string_view name = text(generator.binding);
        KnownDataOp op;
        op.consumer = pipeline.consumer;
        switch (op.consumer)
        {
        case Consumer::Sum:
        case Consumer::Min:
        case Consumer::Max:
        case Consumer::Count:
            // The projection is the binding; an optional filter compares it against a threshold.
            if (trim(text(node.projection)) != name) return std::nullopt;
            if (node.clauses.size() == 2 && !literalComparison(text(node.clauses[1].expression), name, op.comparison, op.threshold))
                return std::nullopt;
            break;
        case Consumer::Any:
        case Consumer::All:
            // No filter; the projection is the comparison, and the language-level short-circuit
            // of any/all is the receiver's early exit.
            if (node.clauses.size() != 1 || !literalComparison(text(node.projection), name, op.comparison, op.threshold))
                return std::nullopt;
            break;
        case Consumer::Materialize:
        case Consumer::Sink:
            return std::nullopt;
        }
        if (!op.comparison.empty() && !numberLiteral(op.threshold))
        {
            // An identifier: only once the compiler has verified it names an outer local, read
            // once instead of per element with no observable difference. The candidate list is
            // bounded so hostile input cannot make the compiler's second pass unbounded.
            op.identifier = true;
            if (std::find(localFilters.begin(), localFilters.end(), index) == localFilters.end())
            {
                if (pendingLocalFilters.size() < maxLocalFilterCandidates &&
                    std::find(pendingLocalFilters.begin(), pendingLocalFilters.end(), index) == pendingLocalFilters.end())
                    pendingLocalFilters.push_back(index);
                return std::nullopt;
            }
        }
        return op;
    }

    // The loop variable declaration for a generator: its user bindings copied verbatim, or

    // the recovery-only placeholder when a binding is missing and a placeholder is requested.
    void bindings(Text& out, const Clause& clause, ClauseSite& site, const std::string& missing = {})
    {
        if (clause.bindings.size() > 1)
        {
            for (size_t i = 0; i < clause.bindings.size(); ++i)
            {
                if (i) out += ", ";
                const size_t begin = out.size();
                out.copy(source, clause.bindings[i]);
                site.bindings.push_back({begin, out.size()});
            }
            site.binding = site.bindings.front();
            return;
        }
        const size_t begin = out.size();
        if (clause.binding.empty() && !missing.empty()) out += missing;
        else out.copy(source, clause.binding);
        site.binding = {begin, out.size()};
        site.bindings.push_back(site.binding);
    }

    // Everything a pipeline walk needs besides the stage list.
    struct Emission
    {
        Emission(const Comprehension& node, const Pipeline& pipeline, ComprehensionSite& site, std::string name, std::string output,
            std::string cursor, size_t index)
            : node(node), pipeline(pipeline), site(site), name(std::move(name)), output(std::move(output)), cursor(std::move(cursor)),
              index(index)
        {
        }
        const Comprehension& node;
        const Pipeline& pipeline;
        ComprehensionSite& site;
        std::string name;
        std::string output;
        std::string cursor;
        size_t index; // The comprehension's index in the document.
        bool scalar = false;
        bool sized = false;       // A lone leading generator: allocate once its count is known.
        bool needsCursor = false; // False when the loop index is the output index, or no state is kept.
        bool directIndex = false;
        std::string seed = "0";   // The accumulator's initial value.
        std::string sink;         // The destination local of a sink consumer; empty otherwise.
        std::string sinkWrite;    // A buffer sink's `buffer.write<kind>` member; empty for a table sink.
        size_t sinkSize = 0;      // A buffer sink's element size and stride in bytes.
        size_t sinkStride = 0;
    };

    // A sink destination may not also be a pipeline source: the pipeline would read elements
    // it has already overwritten. Checked once per source evaluation, never per element.
    void aliasCheck(Text& out, const Emission& e, const std::string& sourceLocal)
    {
        if (e.sink.empty()) return;
        out += "if " + operations + "_rawequal(" + sourceLocal + ", " + e.sink + ") then " + operations +
            "_error(\"JSL sink destination must not be a pipeline source\") end ";
    }

    // The result allocation and accumulator, emitted once the element count is known (sized)
    // or up front when it is not.
    void allocation(Text& out, const Emission& e, const std::string& size = {})
    {
        if (!e.scalar)
            out += "local " + e.output + " = " + (size.empty() ? "{} " : operations + "_table_create(" + size + ") :: typeof({}) ");
        if (e.needsCursor)
            out += "local " + e.cursor + " = " + e.seed + " ";
    }

    // The element layout a typed slice names: `"f32"` or `"f32@16"`, validated by the frontend.
    struct SliceLayout
    {
        std::string kind;
        size_t size = 1;
        size_t stride = 1;
        size_t offset = 0; // The field's position inside each record, `f32@16+12`.
        bool contiguous() const { return stride == size && offset == 0; }
        // The lowering-owned receiver local for this layout, `_data_f32` or `_data_f32_16`; the
        // field offset is an argument, not part of the receiver.
        std::string receiver() const { return "_data_" + kind + (stride == size ? "" : "_" + std::to_string(stride)); }
        std::string name() const { return stride == size ? kind : kind + "@" + std::to_string(stride); }
        // How many elements a buffer of `length` bytes holds: the last element's bytes must fit,
        // so `(length - offset - size) // stride + 1`, or zero when even the first does not.
        std::string count(const std::string& length) const
        {
            const std::string room = length + " - " + std::to_string(offset + size);
            return "(if " + room + " < 0 then 0 else (" + room + ") // " + std::to_string(stride) + " + 1)";
        }
        // The byte position of element `index` (a generated expression, 1-based).
        std::string at(const std::string& index) const
        {
            std::string position = "(" + index + " - 1)";
            if (stride != 1) position += " * " + std::to_string(stride);
            if (offset) position += " + " + std::to_string(offset);
            return position;
        }
    };

    std::optional<SliceLayout> sliceLayout(Range literal) const
    {
        if (literal.empty()) return std::nullopt;
        std::string_view text = std::string_view(source).substr(literal.begin + 1, literal.end - literal.begin - 2);
        SliceLayout layout;
        if (const size_t plus = text.find('+'); plus != std::string_view::npos)
        {
            layout.offset = size_t(std::stoul(std::string(text.substr(plus + 1))));
            text = text.substr(0, plus);
        }
        const size_t at = text.find('@');
        layout.kind = std::string(text.substr(0, at));
        layout.size = layout.kind == "u8" || layout.kind == "i8" ? 1 : layout.kind == "u16" || layout.kind == "i16" ? 2 : layout.kind == "f64" ? 8 : 4;
        layout.stride = at == std::string_view::npos ? layout.size : size_t(std::stoul(std::string(text.substr(at + 1))));
        return layout;
    }


    // A slice source evaluated and bounded: the loop below reads `P_src[P_i]` or, for bytes,
    // `buffer.readu8(P_src, P_i - 1)`, or for a typed slice `buffer.read<kind>(P_src, (P_i - 1)
    // * stride)`. Representation specialization exists only for a leading generator: Analysis
    // types it (bytes), the kind literal names it (typed), or compilation dispatches on the
    // runtime kind.
    void sliceSetup(Text& out, const Stage& stage, const Clause& clause, ClauseSite& site, Emission& e, bool bytes, bool dynamic,
        const SliceLayout* typed = nullptr)
    {
        const std::string& prefix = stage.prefix;
        out += "local " + prefix + "_src = ";
        const size_t sourceBegin = out.size();
        expression(out, clause.sliceSource, typed ? "(buffer.create(0) :: buffer)" : "({} :: {any})");
        site.sliceSource = {sourceBegin, out.size()};
        out += " local " + prefix + "_first = ";
        const size_t firstBegin = out.size();
        expression(out, clause.sliceFirst, "(1 :: number)");
        site.sliceFirst = {firstBegin, out.size()};
        out += " local " + prefix + "_last = ";
        const size_t lastBegin = out.size();
        expression(out, clause.sliceLast, "(0 :: number)");
        site.sliceLast = {lastBegin, out.size()};
        out += " ";
        if (typed)
        {
            out.anchor = clause.sliceSource;
            out += "if " + operations + "_typeof(" + prefix + "_src) ~= \"buffer\" then " + operations +
                "_error(\"JSL typed slice source must be a buffer\") end ";
            boundChecks(out, prefix, clause.sliceFirst, clause.sliceLast);
        }
        else
            sliceChecks(out, prefix, clause.sliceSource, clause.sliceFirst, clause.sliceLast, bytes || dynamic);
        aliasCheck(out, e, prefix + "_src");
        if (typed)
            out += "local " + prefix + "_len = " + typed->count(operations + "_buffer.len(" + prefix + "_src)") + " ";
        else if (dynamic)
            out += "local " + prefix + "_len = 0 if " + prefix + "_kind == \"buffer\" then " + prefix + "_len = " + operations + "_buffer.len(" + prefix + "_src) else " + prefix + "_len = #" + prefix + "_src end ";
        else
            out += "local " + prefix + "_len = " + (bytes ? operations + "_buffer.len(" + prefix + "_src)" : "#" + prefix + "_src") + " ";
        out += "if " + prefix + "_first < 1 then " + prefix + "_first = 1 end if " + prefix + "_last > " + prefix + "_len then " + prefix + "_last = " + prefix + "_len end ";
        if (e.sized)
        {
            out += "local " + prefix + "_n = 0 if " + prefix + "_last >= " + prefix + "_first then " + prefix + "_n = " + prefix + "_last - " + prefix + "_first + 1 end ";
            allocation(out, e, prefix + "_n");
        }
    }

    void sliceLoop(Text& out, const Stage& stage, const Clause& clause, ClauseSite& site, bool bytes, const SliceLayout* typed = nullptr)
    {
        const std::string& prefix = stage.prefix;
        out += "for " + prefix + "_i = " + prefix + "_first, " + prefix + "_last do local ";
        bindings(out, clause, site);
        if (typed)
            out += " = " + operations + "_buffer.read" + typed->kind + "(" + prefix + "_src, " + typed->at(prefix + "_i") + ") ";
        else
            out += bytes ? " = " + operations + "_buffer.readu8(" + prefix + "_src, " + prefix + "_i - 1) " : " = " + prefix + "_src[" + prefix + "_i] ";
    }

    // One generator stage: evaluate its source once, open the loop, declare the bindings.
    // Sized generators emit the result allocation between the length computation and the loop.
    void generator(Text& out, const Stage& stage, const Clause& clause, ClauseSite& site, Emission& e)
    {
        const std::string& prefix = stage.prefix;
        switch (stage.kind)
        {
        case SourceKind::Range:
        {
            for (size_t i = 0; i < clause.rangeArguments.size(); ++i)
            {
                out += "local " + prefix + "_r" + std::to_string(i) + " = ";
                const size_t begin = out.size();
                expression(out, clause.rangeArguments[i], "(0 :: number)");
                site.rangeArguments.push_back({begin, out.size()});
                out += " ";
            }
            if (clause.rangeArguments.size() == 2)
                out += "local " + prefix + "_r2 = 1 ";
            out += "if " + prefix + "_r2 == 0 then " + operations + "_error(\"JSL range step must not be zero\") end for ";
            bindings(out, clause, site, (e.sized ? e.name : prefix) + "_missing");
            out += " = " + prefix + "_r0, " + prefix + "_r1, " + prefix + "_r2 do ";
            return;
        }
        case SourceKind::Slice:
        {
            const auto typed = sliceLayout(clause.sliceKind);
            const bool bytes = !typed && e.sized && bufferGenerator(clause.sliceSource);
            sliceSetup(out, stage, clause, site, e, bytes, false, typed ? &*typed : nullptr);
            sliceLoop(out, stage, clause, site, bytes, typed ? &*typed : nullptr);
            return;
        }
        case SourceKind::Zip:
        {
            for (size_t argument = 0; argument < clause.zipArguments.size(); ++argument)
            {
                out += "local " + prefix + "_src" + std::to_string(argument) + " = ";
                const size_t begin = out.size();
                expression(out, clause.zipArguments[argument], "({} :: {any})");
                site.zipArguments.push_back({begin, out.size()});
                out += " local " + prefix + "_len" + std::to_string(argument) + " = #" + prefix + "_src" + std::to_string(argument) + " ";
                aliasCheck(out, e, prefix + "_src" + std::to_string(argument));
            }
            out += "local " + prefix + "_n = " + prefix + "_len0 ";
            for (size_t argument = 1; argument < clause.zipArguments.size(); ++argument)
                out += "if " + prefix + "_len" + std::to_string(argument) + " < " + prefix + "_n then " + prefix + "_n = " + prefix + "_len" + std::to_string(argument) + " end ";
            if (clause.zipStrict)
                for (size_t argument = 1; argument < clause.zipArguments.size(); ++argument)
                    out += "if " + prefix + "_len" + std::to_string(argument) + " ~= " + prefix + "_n then " + operations + "_error(\"JSL zipStrict inputs must have equal lengths\") end ";
            if (e.sized)
                allocation(out, e, prefix + "_n");
            out += "for " + prefix + "_i = 1, " + prefix + "_n do local ";
            bindings(out, clause, site);
            out += " = ";
            for (size_t argument = 0; argument < clause.zipArguments.size(); ++argument)
            {
                if (argument) out += ", ";
                out += prefix + "_src" + std::to_string(argument) + "[" + prefix + "_i]";
            }
            out += " ";
            return;
        }
        case SourceKind::Enumerate:
        case SourceKind::Plain:
        {
            out += "local " + prefix + "_src = ";
            site.expression = expression(out,
                stage.kind == SourceKind::Enumerate ? clause.enumerateArgument : clause.expression,
                "({} :: {any})", clause.expressionSuffix);
            out += " local " + prefix + "_len = #" + prefix + "_src ";
            aliasCheck(out, e, prefix + "_src");
            if (e.sized)
                allocation(out, e, prefix + "_len");
            out += "for " + prefix + "_i = 1, " + prefix + "_len do local ";
            if (stage.kind == SourceKind::Enumerate)
            {
                bindings(out, clause, site);
                out += " = " + prefix + "_i, " + prefix + "_src[" + prefix + "_i] ";
            }
            else
            {
                bindings(out, clause, site, (e.sized ? e.name : prefix) + "_missing");
                out += " = " + prefix + "_src[" + prefix + "_i] ";
            }
            return;
        }
        }
    }

    // The consumer's per-element step: the projection, then what the consumer does with it.
    void step(Text& out, Emission& e)
    {
        projection(out, e.node, e.name, e.site);
        const std::string value = e.name + "_value";
        switch (e.pipeline.consumer)
        {
        case Consumer::Materialize:
            if (e.needsCursor)
                out += e.cursor + " += 1 ";
            out += e.output + "[" + (e.directIndex ? e.pipeline.stages.front().prefix + "_i" : e.cursor) + "] = " + value + " ";
            return;
        case Consumer::Count:
            out += e.cursor + " += 1 ";
            return;
        case Consumer::Sum:
            out += e.cursor + " += " + value + " ";
            return;
        case Consumer::Sink:
            if (e.sinkWrite.empty())
                out += e.cursor + " += 1 " + e.sink + "[" + e.cursor + "] = " + value + " ";
            else
                // Capacity is checked per write; the write itself is the buffer library's, so a
                // non-number projection raises its ordinary error.
                out += "if " + e.sink + "_at + " + e.cursor + " * " + std::to_string(e.sinkStride) + " + " + std::to_string(e.sinkSize) +
                    " > " + e.sink + "_len then " + operations + "_error(\"JSL sink buffer destination is full\") end " + operations +
                    "_buffer." + e.sinkWrite + "(" + e.sink + ", " + e.sink + "_at + " + e.cursor + " * " + std::to_string(e.sinkStride) +
                    ", " + value + ") " + e.cursor + " += 1 ";
            return;
        case Consumer::Min:
        case Consumer::Max:
            // Luau ordering, as a handwritten loop would compare: a NaN that arrives first
            // stays; later NaNs never replace. The accumulator is nil only while empty.
            out += "if " + e.cursor + " == nil or " + value + (e.pipeline.consumer == Consumer::Min ? " < " : " > ") + e.cursor +
                " then " + e.cursor + " = " + value + " end ";
            return;
        case Consumer::Any:
            out += "if " + value + " then return true end ";
            return;
        case Consumer::All:
            out += "if not " + value + " then return false end ";
            return;
        }
    }

    // What the pipeline returns once every stage has run.
    void epilogue(Text& out, const Emission& e)
    {
        switch (e.pipeline.consumer)
        {
        case Consumer::Materialize:
            out += "return " + e.output + " end)()";
            return;
        case Consumer::Count:
        case Consumer::Sum:
            out += "return " + e.cursor + " end)()";
            return;
        case Consumer::Min:
        case Consumer::Max:
            // An if-expression, not a guard statement: its error branch has type never, so
            // both Luau solvers type the result as the element type rather than optional.
            out += "return if " + e.cursor + " == nil then " + operations + "_error(\"JSL " +
                (e.pipeline.consumer == Consumer::Min ? "min" : "max") + " reducer received no elements\") else " + e.cursor + " end)()";
            return;
        case Consumer::Any:
            out += "return false end)()";
            return;
        case Consumer::All:
            out += "return true end)()";
            return;
        case Consumer::Sink:
            if (!e.sinkWrite.empty())
            {
                // A buffer sink evaluates to the destination and the count written.
                out += "return " + e.sink + ", " + e.cursor + " end)()";
                return;
            }
            // Replace semantics: entries past the written count are cleared so the result is
            // dense; the destination's prior length was captured before the pipeline ran.
            out += "for " + e.name + "_i = " + e.cursor + " + 1, " + e.sink + "_len do " + e.sink + "[" + e.name + "_i] = nil end return " +
                e.sink + " end)()";
            return;
        }
    }

    // The data-plane form of a recognized pipeline over a slice of `layout`, opened as
    // `if <data available> then ... ` for the caller to close with the scalar fallback. The
    // receiver's native loops (DATA_PLANE.md §2.4) beat the bound calls at every size except
    // the unfiltered sum above `boundSumThreshold`, which the bound call's vectorized loop wins.
    void dataOp(Text& out, Emission& e, const Stage& stage, const KnownDataOp& op, const SliceLayout& layout)
    {
        const std::string& p = stage.prefix;
        const std::string offset = layout.contiguous() && layout.stride == 1 ? p + "_first - 1" : layout.at(p + "_first");
        const std::string span = p + "_src, " + offset + ", " + p + "_n";
        const std::string receiver = operations + layout.receiver() + ":";
        // An empty span after normalization may start past the buffer (`buf[300:400]` of 256
        // bytes); the loop runs zero times and the data plane is not consulted.
        out += "if " + operations + "_data" +
            (op.identifier ? " and " + operations + "_typeof(" + op.threshold + ") == \"number\"" : "") + " then ";
        // `sumGt(span, t)`, `min(span)`, `anyLe(span, t)`: the member and its arguments.
        const std::string suffix = op.comparison.empty() ? "" : capitalized(op.comparison);
        const std::string arguments = span + (op.comparison.empty() ? "" : ", " + op.threshold);
        const std::string guard = "if " + p + "_n > 0 then ";
        switch (op.consumer)
        {
        case Consumer::Count:
            out += op.comparison.empty() ? e.cursor + " = " + p + "_n "
                                         : guard + e.cursor + " = " + receiver + "count" + suffix + "(" + arguments + ") end ";
            break;
        case Consumer::Sum:
            if (op.comparison.empty())
                out += guard + "if " + p + "_n > " + std::to_string(boundSumThreshold) + " then " + e.cursor + " = " + operations +
                    "_data.sum(" + p + "_src, \"" + layout.name() + "\", " + offset + ", " + p + "_n) else " + e.cursor + " = " +
                    receiver + "sum(" + span + ") end end ";
            else
                out += guard + e.cursor + " = " + receiver + "sum" + suffix + "(" + arguments + ") end ";
            break;
        case Consumer::Min:
        case Consumer::Max:
            // nil for an empty span (or no accepted element) is the reducer's empty error.
            out += guard + e.cursor + " = " + receiver + (op.consumer == Consumer::Min ? "min" : "max") + suffix + "(" + arguments +
                ") end if " + e.cursor + " == nil then " + operations + "_error(\"JSL " +
                (op.consumer == Consumer::Min ? "min" : "max") + " reducer received no elements\") end ";
            break;
        case Consumer::Any:
        case Consumer::All:
            out += "return " + receiver + (op.consumer == Consumer::Any ? "any" : "all") + suffix + "(" + arguments + ") ";
            break;
        default:
            break;
        }
    }

    // Stages `from` onward, then the step, then the closers: a recursive walk so that one

    // stage may emit the rest of the pipeline under more than one representation.
    void stages(Text& out, Emission& e, size_t from)
    {
        if (from == e.pipeline.stages.size())
        {
            step(out, e);
            return;
        }
        const Stage& stage = e.pipeline.stages[from];
        const Clause& clause = e.node.clauses[stage.clause];
        ClauseSite& site = e.site.clauses[stage.clause];
        // Synthetic scaffolding of a clause is attributed to that clause; the loop closers to
        // the closing bracket. Copied user expressions keep their own provenance.
        out.anchor = clause.range;
        if (!stage.generator)
        {
            out += "if ";
            site.expression = expression(out, clause.expression, "true", clause.expressionSuffix);
            out += " then ";
            stages(out, e, from + 1);
            out.anchor = e.node.close;
            out += "end ";
            return;
        }
        const auto typed = stage.kind == SourceKind::Slice ? sliceLayout(clause.sliceKind) : std::nullopt;
        if (typed && e.sized)
        {
            // A typed slice reads a buffer as its layout: one loop, no representation dispatch,
            // and the data plane when the pipeline is a known operation.
            sliceSetup(out, stage, clause, site, e, false, false, &*typed);
            const auto op = dynamicGenerators ? knownDataOp(e.node, e.pipeline, e.index) : std::nullopt;
            if (op)
            {
                dataOp(out, e, stage, *op, *typed);
                out += "else ";
                sliceLoop(out, stage, clause, site, false, &*typed);
                stages(out, e, from + 1);
                out.anchor = e.node.close;
                out += "end end ";
            }
            else
            {
                sliceLoop(out, stage, clause, site, false, &*typed);
                stages(out, e, from + 1);
                out.anchor = e.node.close;
                out += "end ";
            }
            return;
        }
        if (stage.kind == SourceKind::Slice && e.sized && dynamicGenerators && !bufferGenerator(clause.sliceSource))
        {
            // Compilation has no types: dispatch on the runtime representation once, outside
            // the loop, and traverse each representation with its own specialized loop.
            sliceSetup(out, stage, clause, site, e, false, true);
            out += "if " + stage.prefix + "_kind == \"buffer\" then ";
            if (const auto op = knownDataOp(e.node, e.pipeline, e.index))
            {
                // JSL byte slices are u8 spans; the data plane computes this consumer over the
                // normalized span exactly as the scalar loop would.
                dataOp(out, e, stage, *op, SliceLayout{"u8", 1, 1});
                out += "else ";
                sliceLoop(out, stage, clause, site, true);
                stages(out, e, from + 1);
                out.anchor = e.node.close;
                out += "end end else ";
            }
            else
            {
                sliceLoop(out, stage, clause, site, true);
                stages(out, e, from + 1);
                out.anchor = e.node.close;
                out += "end else ";
            }

            out.anchor = clause.range;
            sliceLoop(out, stage, clause, site, false);
            stages(out, e, from + 1);
            out.anchor = e.node.close;
            out += "end end ";
            return;
        }
        generator(out, stage, clause, site, e);
        stages(out, e, from + 1);
        out.anchor = e.node.close;
        out += "end ";
    }

    Text comprehension(size_t index, bool count, Reducer reducer)
    {
        const Comprehension& node = document.comprehensions[index];
        const std::string name = stem(node.open.begin);
        const Pipeline pipeline = plan(node, name, count, reducer);
        Text out;
        const bool sink = pipeline.consumer == Consumer::Sink;
        out.anchor = {sink ? node.sinkPrefix.begin : reducer != Reducer::None ? node.reducerPrefix.begin : count ? node.lengthPrefix.begin : node.range.begin, node.range.end};
        if (node.postfix)
            out += "["; // Recovery-only index fence; strict compilation rejects the document.
        if (node.sinkStatement)
            out += ";"; // Separate the call from the previous statement; Luau would otherwise call its result.
        const size_t callBegin = out.size();
        out += "(function() ";
        comments(out, node, out.anchor.begin);
        ComprehensionSite site;
        site.comprehension = index;
        site.clauses.resize(node.clauses.size());

        // A lone leading generator knows its element count before allocating, except a
        // numeric range, which keeps the growing-table shape; with no filters its loop index
        // is the output index and no cursor is needed. Everything else counts as it goes.
        const bool single = pipeline.single();
        const SourceKind first = single ? pipeline.stages.front().kind : SourceKind::Plain;
        const Consumer consumer = pipeline.consumer;
        Emission e(node, pipeline, site, name, name + "_out", name + accumulator(consumer), index);
        e.scalar = consumer != Consumer::Materialize;
        e.sized = single && first != SourceKind::Range;
        e.directIndex = single && node.clauses.size() == 1 && first != SourceKind::Range && first != SourceKind::Slice;
        e.needsCursor = (consumer != Consumer::Any && consumer != Consumer::All) && (e.scalar || !e.directIndex);
        if (consumer == Consumer::Min || consumer == Consumer::Max)
            e.seed = "nil";
        if (sink && node.sinkKind.empty())
        {
            // The destination is evaluated exactly once, before any source, and must be a table.
            e.sink = name + "_into";
            out.anchor = node.sinkDestination;
            out += "local " + e.sink + " = ";
            site.sink = expression(out, node.sinkDestination, "({} :: {any})");
            out += " if " + operations + "_typeof(" + e.sink + ") ~= \"table\" then " + operations +
                "_error(\"JSL sink destination must be a table\") end local " + e.sink + "_len = #" + e.sink + " ";
        }
        else if (sink)
        {
            // A buffer sink: the kind literal (validated by the frontend) picks the write and
            // the stride at compile time; the destination, then the offset, evaluate once.
            e.sink = name + "_into";
            const std::string_view literal = std::string_view(source).substr(node.sinkKind.begin + 1, node.sinkKind.end - node.sinkKind.begin - 2);
            const size_t at = literal.find('@');
            const std::string_view kind = literal.substr(0, at);
            e.sinkWrite = "write" + std::string(kind);
            e.sinkSize = kind == "u8" || kind == "i8" ? 1 : kind == "u16" || kind == "i16" ? 2 : kind == "f64" ? 8 : 4;
            e.sinkStride = at == std::string_view::npos ? e.sinkSize : size_t(std::stoul(std::string(literal.substr(at + 1))));
            out.anchor = node.sinkDestination;
            out += "local " + e.sink + " = ";
            site.sink = expression(out, node.sinkDestination, "(buffer.create(0) :: buffer)");
            out += " if " + operations + "_typeof(" + e.sink + ") ~= \"buffer\" then " + operations +
                "_error(\"JSL sink buffer destination must be a buffer\") end local " + e.sink + "_len = " + operations + "_buffer.len(" + e.sink + ") ";
            out.anchor = node.sinkOffset.empty() ? node.sinkKind : node.sinkOffset;
            out += "local " + e.sink + "_at = ";
            if (node.sinkOffset.empty())
                out += "0";
            else
                site.sinkOffset = expression(out, node.sinkOffset, "(0 :: number)");
            out += " if " + operations + "_typeof(" + e.sink + "_at) ~= \"number\" or " + e.sink + "_at % 1 ~= 0 or " + e.sink +
                "_at < 0 then " + operations + "_error(\"JSL sink buffer offset must be a non-negative integer\") end ";
        }

        if (!e.sized)
            allocation(out, e);
        stages(out, e, 0);
        out.anchor = node.close;
        epilogue(out, e);
        site.call = {callBegin, out.size()};
        if (node.postfix)
            out += "]";
        out.sites.push_back(std::move(site));
        return out;
    }
};
}

LoweredSource lower(std::string_view source, bool recovery, bool fuseLength, const std::vector<Range>& bufferGenerators,
    bool dynamicGenerators, const std::vector<size_t>& localFilters)
{
    Document document = parseSurface(source);
    if ((document.comprehensions.empty() && document.slices.empty()) || (!recovery && !document.errors.empty()))
        return {std::string(source), {}, std::move(document), {}, {}, 0, {}};
    Lowerer lowerer(source, document, recovery, fuseLength, bufferGenerators, dynamicGenerators, localFilters);
    Text output = lowerer.lower();
    SourceMap map(source, output.text, std::move(output.segments));
    return {std::move(output.text), std::move(map), std::move(document), std::move(output.sites), std::move(output.sliceSites),
        lowerer.prelude(), std::move(lowerer.pendingLocalFilters)};
}
}
