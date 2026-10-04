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
    Range binding; // Empty insertion point when missing; never an invented source identifier.
    Range in;
    Range expression;
};

struct Comprehension
{
    Range range;
    Range open;
    Range close;
    Range arrow;
    Range projection;
    std::vector<Clause> clauses;
    bool complete = false;
};

struct Document
{
    // Flat, sorted by opening offset; nested ranges retain their original offsets.
    std::vector<Comprehension> comprehensions;
    std::vector<Diagnostic> errors;
};

// Tolerant structural parse using stock Luau tokens. Ordinary expression/type checking
// belongs to Luau; strict consumers reject structural recovery before emitting bytecode.
Document parseSurface(std::string_view source);
}
