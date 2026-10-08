#pragma once

#include <cstddef>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace L3i::Surface
{
struct Position
{
    unsigned line;
    unsigned column;
};

struct Span
{
    Position begin;
    Position end;
};

// Sorted, disjoint ranges covering generated text. Copies map affinely in byte offsets;
// synthetic ranges are attributed to an intentional original-source span.
struct Segment
{
    size_t begin;
    size_t end;
    size_t originalBegin;
    size_t originalEnd;
    bool copied;
};

class SourceMap
{
public:
    SourceMap() = default; // Identity mapping: ordinary Luau does not need provenance storage.
    SourceMap(std::string_view original, std::string_view generated, std::vector<Segment> segments);

    bool empty() const { return segments.empty(); }
    // The recorded provenance, for lowering snapshots and diagnostics; never mutated after construction.
    const std::vector<Segment>& provenance() const { return segments; }
    Position originalPosition(Position generated) const;
    Span originalSpan(Span generated) const;
    // The same, also telling whether every position inside the span maps to itself: it lies in
    // one copy whose lines are not displaced, past the copy's first line or with no column
    // shift there.
    Span originalSpan(Span generated, bool& unchanged) const;
    // The same for a caller mapping spans in roughly source order: `cursor` remembers the last
    // segment found, so the next lookup usually needs no search. Start it at 0.
    Span originalSpan(Span generated, bool& unchanged, size_t& cursor) const;
    Position generatedPosition(Position original) const;
    // Offset helpers for tooling sites in a mapped document (not a default identity map).
    Position generatedPoint(size_t offset) const;
    Span originalRange(size_t begin, size_t end) const;
    size_t originalOffset(Position position) const;
    bool generatedName(std::string_view name) const;
    unsigned originalLines() const;
    // A line-only reference can be ambiguous after several original lines were fused.
    std::optional<unsigned> originalLine(unsigned generatedLine) const;
    // Translate one explicitly identified coordinate phrase in engine diagnostic prose.
    // Never search arbitrary user text for numbers; callers identify known templates.
    std::string referenceText(std::string text, size_t at, Position generatedContext) const;

private:
    std::string original;
    size_t generatedSize = 0;
    std::vector<size_t> originalStarts;
    std::vector<size_t> generatedStarts;
    std::vector<Segment> segments;
    // Where each segment begins, generated and original. A copy is byte-identical to its
    // original, so a position inside it maps by a line delta (and a column delta on its first
    // line) with no offset or line-start search.
    std::vector<Position> generatedAnchors;
    std::vector<Position> originalAnchors;
    std::vector<Position> originalEndAnchors; // Where each segment's original range ends.
    bool exact(Position generated, size_t at) const;
    // The first segment ending after `at`, trying the cursor and its successor first.
    size_t segmentAfter(size_t at, size_t& cursor) const;
    Position shifted(Position generated, size_t segment) const;
};
}
