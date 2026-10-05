#pragma once

#include "source_map.h"
#include "surface_frontend.h"

namespace L3i::Surface
{
struct ClauseSite
{
    Range binding; // Generated declaration token, not a guessed temporary spelling.
    std::vector<Range> bindings;
    Range expression;
    std::vector<Range> rangeArguments;
    std::vector<Range> zipArguments;
    bool zipStrict = false;
    Range sliceSource;
    Range sliceFirst;
    Range sliceLast;
};

struct ComprehensionSite
{
    size_t comprehension = 0;
    Range call;
    std::vector<ClauseSite> clauses;
    Range projection;
};

struct SliceSite
{
    size_t slice = 0;
    Range call;
    Range source;
    Range first;
    Range last;
};

struct LoweredSource
{
    std::string source;
    SourceMap map;
    Document document;
    std::vector<ComprehensionSite> sites;
    std::vector<SliceSite> sliceSites;
    size_t preludeStatements = 0;
};

// Shared compiler/tooling seam: all coordinates in map refer to the original input.
// Recovery is tooling-only. Strict compilation must reject document errors before parsing.
// Syntax tooling disables count fusion to retain unary length in the source tree.
LoweredSource lower(std::string_view source, bool recovery = false, bool fuseLength = true,
    const std::vector<Range>& bufferGenerators = {}, bool dynamicGenerators = false);
}
