#pragma once

#include <cstddef>
#include <cstdint>
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

struct PatternField;

// A binding pattern: a name, or a record pattern `{field, field: target, ...}` whose fields
// bind the same-named field of one source value (`{x}`), rename it (`{x: px}`), or bind it to a
// nested record pattern (`{position: {x, y}}`). Fields are read with ordinary Luau indexing, in
// source order, depth first. Records introduce no runtime record; names are ordinary locals.
struct Pattern
{
    // The name, or `{` through `}`; an unclosed record ends at its last consumed token. A
    // missing name is an empty range at the insertion point.
    Range range;
    bool record = false;
    Range open;  // Records: `{`.
    Range close; // Records: `}`; empty when missing.
    std::vector<PatternField> fields;
    // Records: closed, with every field and nested pattern present and well formed.
    bool complete = false;
};

struct PatternField
{
    Range range; // The key through the target.
    Range key;   // The source field name; empty when missing.
    Range colon; // Empty for the shorthand `{key}`, whose target is a name spelled like the key.
    Pattern target;
};

// A record pattern declaration, `local {...}[: Type] = value`: one pattern and one value. The
// value's first value is destructured, exactly as `local holder[: Type] = value` would bind it.
struct LocalPattern
{
    Range keyword; // `local`
    Pattern pattern;
    Range colon;  // The whole-pattern annotation's ':'; empty without an annotation.
    Range equals; // '='; empty when missing.
    // Filled by lowering from Luau's own parse of the declaration: where the value ends, and so
    // where the field reads go. SIZE_MAX until known (or when the declaration did not parse).
    size_t extract = SIZE_MAX;
};

// A record pattern parameter of a function: the argument in that position is destructured on
// entry, before the body, in parameter order. The function's public type is unchanged.
struct ParameterPattern
{
    Range function;       // The declaring function's `function` keyword.
    size_t parameter = 0; // Zero-based position in the parameter list.
    Pattern pattern;
    Range colon; // The parameter annotation's ':'; empty without one.
    // Filled by lowering: the function body's first offset. SIZE_MAX until known.
    size_t extract = SIZE_MAX;
};

// The JSL reducer consuming a comprehension: `sum`, `min`, `max` accumulate every accepted
// projection; `any` and `all` are language-level short-circuit reducers.
enum class Reducer { None, Sum, Min, Max, Any, All };

struct Clause
{
    ClauseKind kind = ClauseKind::Generator;
    Range range;
    Range keyword;
    // Ordered source bindings. Empty when missing; never contains invented identifiers.
    // Ordinary and range generators have one binding; enumerate has exactly two. A binding slot
    // may be a record pattern; its range is then the pattern's.
    std::vector<Range> bindings;
    Range binding; // First binding, retained as the compatibility/tooling shorthand.
    // One pattern per binding slot, parallel to `bindings`: a name or a record pattern.
    std::vector<Pattern> patterns;
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
    // A typed slice `source[first:last, "kind"]`: the kind string literal (quotes included; a
    // kind or `kind@stride` layout). The source must then be a buffer read as that layout.
    Range sliceKind;
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
    // Immediately preceding `into(destination)`, ignoring trivia, unless `into` follows '.'
    // or ':'. JSL-owned sink syntax: the pipeline fills the destination table in place and
    // evaluates to it. Empty when absent. The destination range excludes the parentheses.
    Range sinkPrefix;
    Range sinkDestination;
    // A buffer sink, `into(buffer, "kind"[, offset])`: the kind string literal (quotes
    // included; a kind or `kind@stride` layout) and the optional byte offset expression.
    // Both empty for a table sink.
    Range sinkKind;
    Range sinkOffset;
    // A sink whose `into` follows a token that can end an expression statement, so it begins
    // a new statement; the lowered call then needs a separator from the previous statement.
    bool sinkStatement = false;
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
    Range kind; // The kind literal of a typed slice `source[first:last, "kind"]`; empty otherwise.
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
    // Record pattern declarations and parameters, each in source order. Generator patterns
    // remain on their Clause.
    std::vector<LocalPattern> locals;
    std::vector<ParameterPattern> parameters;
    // Original Comment/BlockComment/BrokenComment token ranges, in source order.
    // The no-surface fast path may leave this empty; stock parsing owns trivia
    // when no rewrite is needed. Resource truncation retains only lexed trivia.
    std::vector<Range> comments;
};

// Tolerant structural parse using stock Luau tokens. Ordinary expression/type checking
// belongs to Luau; strict consumers reject structural recovery before emitting bytecode.
Document parseSurface(std::string_view source);
}
