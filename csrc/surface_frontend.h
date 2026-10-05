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
    // Immediately preceding code Name 'sum', ignoring trivia, unless preceded
    // by '.' or ':'. JSL-owned reducer syntax, not a lexical/global helper call.
    // Empty when absent; the consumer is excluded from the comprehension range.
    Range sumPrefix;
    Range open;
    Range close;
    Range arrow;
    Range projection;
    std::string projectionSuffix; // Recovery-only, like Clause::expressionSuffix.
    std::vector<Clause> clauses;
    bool postfix = false; // Invalid unparenthesized collection-index usage.
    bool complete = false;
};

struct Document
{
    // Flat, sorted by opening offset; nested ranges retain their original offsets.
    std::vector<Comprehension> comprehensions;
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
