#include "source_map.h"

#include <algorithm>
#include <charconv>
#include <limits>
#include <stdexcept>
#include <utility>

namespace L3i::Surface
{
namespace
{
std::vector<size_t> lineStarts(std::string_view text)
{
    std::vector<size_t> starts{0};
    for (size_t i = 0; i < text.size(); ++i)
        if (text[i] == '\n')
            starts.push_back(i + 1);
    return starts;
}

size_t offset(Position position, const std::vector<size_t>& starts, size_t size)
{
    if (position.line >= starts.size())
        return size;
    const size_t begin = starts[position.line];
    const size_t end = position.line + 1 < starts.size() ? starts[position.line + 1] - 1 : size;
    return begin + std::min(size_t(position.column), end - begin);
}

Position position(size_t at, const std::vector<size_t>& starts)
{
    const size_t line = size_t(std::upper_bound(starts.begin(), starts.end(), at) - starts.begin()) - 1;
    return {unsigned(line), unsigned(at - starts[line])};
}
}

SourceMap::SourceMap(std::string_view original, std::string_view generated, std::vector<Segment> segments)
    : original(original)
    , generatedSize(generated.size())
    , originalStarts(lineStarts(original))
    , generatedStarts(lineStarts(generated))
    , segments(std::move(segments))
{
    generatedAnchors.reserve(this->segments.size());
    originalAnchors.reserve(this->segments.size());
    for (const Segment& segment : this->segments)
    {
        generatedAnchors.push_back(position(segment.begin, generatedStarts));
        originalAnchors.push_back(position(segment.originalBegin, originalStarts));
    }
}

// Whether `generated` is exactly offset `at`: on a real line, not clamped past its end.
bool SourceMap::exact(Position generated, size_t at) const
{
    return generated.line < generatedStarts.size() && generatedStarts[generated.line] + generated.column == at;
}

bool SourceMap::unchanged(Span generated) const
{
    if (empty())
        return true;
    const size_t begin = offset(generated.begin, generatedStarts, generatedSize);
    const size_t end = offset(generated.end, generatedStarts, generatedSize);
    if (end < begin || !exact(generated.begin, begin) || !exact(generated.end, end))
        return false;
    auto it = std::upper_bound(segments.begin(), segments.end(), begin,
        [](size_t value, const Segment& segment) { return value < segment.end; });
    if (it == segments.end() || !it->copied || it->begin > begin || end > it->end)
        return false;
    const size_t segment = size_t(it - segments.begin());
    const Position from = generatedAnchors[segment];
    const Position to = originalAnchors[segment];
    return from.line == to.line && (generated.begin.line > from.line || from.column == to.column);
}

Position SourceMap::shifted(Position generated, size_t segment) const
{
    const Position from = generatedAnchors[segment];
    const Position to = originalAnchors[segment];
    if (generated.line == from.line)
        return {to.line, generated.column - from.column + to.column};
    return {generated.line - from.line + to.line, generated.column};
}

Position SourceMap::originalPosition(Position generated) const
{
    if (empty())
        return generated;
    const size_t at = offset(generated, generatedStarts, generatedSize);
    if (at == generatedSize)
        return position(original.size(), originalStarts);
    auto it = std::upper_bound(segments.begin(), segments.end(), at,
        [](size_t value, const Segment& segment) { return value < segment.begin; });
    if (it == segments.begin())
        return position(0, originalStarts);
    --it;
    if (it->copied && at < it->end && exact(generated, at))
        return shifted(generated, size_t(it - segments.begin()));
    const size_t origin = it->copied ? it->originalBegin + at - it->begin : it->originalBegin;
    return position(origin, originalStarts);
}

Span SourceMap::originalSpan(Span generated) const
{
    if (empty())
        return generated;
    const size_t begin = offset(generated.begin, generatedStarts, generatedSize);
    const size_t end = offset(generated.end, generatedStarts, generatedSize);
    if (end <= begin)
    {
        const Position at = originalPosition(generated.begin);
        return {at, at};
    }
    // Lowering can reorder original coordinates. A container is the envelope of its
    // provenance, not a blindly reversed pair of endpoints. Exclusive ends use only
    // intersecting segments, so the next segment cannot steal a copied token's end.
    size_t first = std::numeric_limits<size_t>::max();
    size_t last = 0;
    auto it = std::upper_bound(segments.begin(), segments.end(), begin,
        [](size_t value, const Segment& segment) { return value < segment.end; });
    // The common case: the whole span lies in one copy, which maps both ends affinely.
    if (it != segments.end() && it->copied && it->begin <= begin && end <= it->end && exact(generated.begin, begin) &&
        exact(generated.end, end))
    {
        const size_t segment = size_t(it - segments.begin());
        return {shifted(generated.begin, segment), shifted(generated.end, segment)};
    }
    for (; it != segments.end() && it->begin < end; ++it)
    {
        const size_t lo = std::max(begin, it->begin);
        const size_t hi = std::min(end, it->end);
        const size_t originBegin = it->copied ? it->originalBegin + lo - it->begin : it->originalBegin;
        const size_t originEnd = it->copied ? it->originalBegin + hi - it->begin : it->originalEnd;
        first = std::min(first, originBegin);
        last = std::max(last, originEnd);
    }
    if (first == std::numeric_limits<size_t>::max())
    {
        const Position at = originalPosition(generated.begin);
        return {at, at};
    }
    return {position(first, originalStarts), position(last, originalStarts)};
}

Position SourceMap::generatedPosition(Position origin) const
{
    if (empty())
        return origin;
    const size_t at = offset(origin, originalStarts, original.size());
    // Exact copied positions take precedence over broad synthetic anchors. At an
    // exclusive boundary prefer the preceding copy: completion occurs at token ends.
    for (const Segment& segment : segments)
        if (segment.copied && segment.originalBegin < at && at <= segment.originalEnd)
            return position(segment.begin + at - segment.originalBegin, generatedStarts);
    for (const Segment& segment : segments)
        if (segment.copied && segment.originalBegin == at)
            return position(segment.begin, generatedStarts);
    // Removed surface punctuation maps to the nearest copied source position. This
    // is a fallback for cursor requests, not a claim that synthetic locals are public.
    size_t distance = std::numeric_limits<size_t>::max();
    size_t target = generatedSize;
    for (const Segment& segment : segments)
    {
        if (!segment.copied)
            continue;
        const size_t candidate = std::clamp(at, segment.originalBegin, segment.originalEnd);
        const size_t delta = candidate > at ? candidate - at : at - candidate;
        if (delta < distance)
        {
            distance = delta;
            target = segment.begin + candidate - segment.originalBegin;
        }
    }
    return position(target, generatedStarts);
}

bool SourceMap::generatedName(std::string_view name) const
{
    return !empty() && name.substr(0, 11) == "__l3i_comp_" && original.find(name) == std::string::npos;
}

Position SourceMap::generatedPoint(size_t at) const
{
    if (generatedStarts.empty())
        throw std::logic_error("generated offset requires a mapped surface document");
    return position(std::min(at, generatedSize), generatedStarts);
}

Span SourceMap::originalRange(size_t begin, size_t end) const
{
    if (originalStarts.empty())
        throw std::logic_error("original offset requires a mapped surface document");
    return {position(std::min(begin, original.size()), originalStarts), position(std::min(end, original.size()), originalStarts)};
}

size_t SourceMap::originalOffset(Position at) const
{
    if (originalStarts.empty())
        throw std::logic_error("original position requires a mapped surface document");
    return offset(at, originalStarts, original.size());
}

unsigned SourceMap::originalLines() const
{
    return empty() ? 0 : unsigned(originalStarts.size() - 1 + (!original.empty() && original.back() != '\n'));
}

std::optional<unsigned> SourceMap::originalLine(unsigned line) const
{
    if (empty())
        return line;
    if (line >= generatedStarts.size())
        return std::nullopt;
    const size_t begin = generatedStarts[line];
    const size_t end = line + 1 < generatedStarts.size() ? generatedStarts[line + 1] : generatedSize;
    if (begin == generatedSize)
        return position(original.size(), originalStarts).line;
    std::optional<unsigned> result;
    for (const Segment& segment : segments)
    {
        if (!segment.copied || segment.end <= begin || segment.begin >= end)
            continue;
        const size_t lo = std::max(begin, segment.begin);
        const size_t hi = std::min(end, segment.end);
        const unsigned first = position(segment.originalBegin + lo - segment.begin, originalStarts).line;
        const unsigned last = position(segment.originalBegin + hi - segment.begin - 1, originalStarts).line;
        if (first != last || (result && *result != first))
            return std::nullopt;
        result = first;
    }
    return result;
}

std::string SourceMap::referenceText(std::string text, size_t at, Position context) const
{
    if (empty() || at == std::string::npos)
        return text;
    const std::string_view tail = std::string_view(text).substr(at);
    const bool line = tail.substr(0, 8) == "at line " || tail.substr(0, 8) == "on line ";
    const bool column = tail.substr(0, 10) == "at column " || tail.substr(0, 10) == "on column ";
    if (!line && !column)
        return text;
    const size_t prefix = line ? 8 : 10;
    unsigned value = 0;
    const auto parsed = std::from_chars(tail.data() + prefix, tail.data() + tail.size(), value);
    if (parsed.ec != std::errc() || value == 0)
        return text;
    const size_t length = size_t(parsed.ptr - tail.data());
    std::string replacement;
    if (line)
    {
        const auto original = originalLine(value - 1);
        replacement = original ? std::string(tail.substr(0, 8)) + std::to_string(*original + 1)
                               : "within a lowered comprehension (ambiguous source reference)";
    }
    else
    {
        const Position original = originalPosition({context.line, value - 1});
        // Same generated line does not imply same original line. Report both coordinates.
        replacement = "at line " + std::to_string(original.line + 1) + ", column " + std::to_string(original.column + 1);
    }
    text.replace(at, length, replacement);
    return text;
}
}
