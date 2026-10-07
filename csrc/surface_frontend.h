#pragma once

#include <cstddef>
#include <string>
#include <string_view>
#include <vector>

namespace L3i::Surface
{
struct Range
{
    size_t begin = 0;
    size_t end = 0;
    bool empty() const { return begin == end; }
};

struct Diagnostic
{
    Range range;
    std::string message;
};

enum class ClauseKind { Generator, Filter };

// The JSL reducer consuming a comprehension: `sum`, `min`, `max` accumulate every accepted
// projection; `any` and `all` are language-level short-circuit reducers.
enum class Reducer { None, Sum, Min, Max, Any, All };

struct Clause
{
    ClauseKind kind = ClauseKind::Generator;
    Range range;
    Range keyword;
    // Ordered source bindings. Empty when missing; never contains invented identifiers.
    // Ordinary and range generators have one binding; enumerate has exactly two.
    std::vector<Range> bindings;
    Range binding; // First binding, retained as the compatibility/tooling shorthand.
    Range in;
    Range expression;
    // Direct JSL range(first, last[, step]) generator arguments. Empty for
    // ordinary source expressions; each range excludes commas and parentheses.
    std::vector<Range> rangeArguments;
    // Direct JSL enumerate(source) generator argument. Empty for ordinary
    // source expressions. The range excludes the call parentheses.
    Range enumerateArgument;
    // Direct JSL zipShortest/zipStrict generator arguments. Empty for other sources.
    std::vector<Range> zipArguments;
    bool zipStrict = false;
    // Direct dense-table slice source `source[first:last]`, with inclusive,
    // 1-based bounds. Empty for ordinary sources.
    Range sliceSource;
    Range sliceFirst;
    Range sliceLast;
    // Recovery-only unmatched ordinary delimiters, closed at the original fence.
    // Not part of expression's source range; strict consumers reject recovery.
    std::string expressionSuffix;
};

struct Comprehension
{
    Range range;
    // Immediately preceding real '#' token, ignoring comment trivia. Empty
    // when absent; unary length stays separate from the comprehension range.
    Range lengthPrefix;
    // Immediately preceding code Name naming a reducer (`sum`, `min`, `max`, `any`,
    // `all`), ignoring trivia, unless preceded by '.' or ':'. JSL-owned reducer syntax,
    // not a lexical/global helper call. Empty when absent; the consumer is excluded from
    // the comprehension range.
    Range reducerPrefix;
    Reducer reducer = Reducer::None;
    Range open;
    Range close;
    Range arrow;
    Range projection;
    std::string projectionSuffix; // Recovery-only, like Clause::expressionSuffix.
    std::vector<Clause> clauses;
    bool postfix = false; // Invalid unparenthesized collection-index usage.
    bool complete = false;
};

struct Slice
{
    Range range;
    Range source;
    Range first;
    Range last;
    bool complete = false;
};

struct Document
{
    // Flat, sorted by opening offset; nested ranges retain their original offsets.
    std::vector<Comprehension> comprehensions;
    // Standalone dense-table slices, in source order. Comprehension generator
    // slices remain represented on their Clause and are excluded here.
    std::vector<Slice> slices;
    std::vector<Diagnostic> errors;
    // Original Comment/BlockComment/BrokenComment token ranges, in source order.
    // The no-surface fast path may leave this empty; stock parsing owns trivia
    // when no rewrite is needed. Resource truncation retains only lexed trivia.
    std::vector<Range> comments;
};

// Tolerant structural parse using stock Luau tokens. Ordinary expression/type checking
// belongs to Luau; strict consumers reject structural recovery before emitting bytecode.
Document parseSurface(std::string_view source);
}
