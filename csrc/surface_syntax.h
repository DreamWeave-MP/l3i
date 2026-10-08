#pragma once

#include "source_map.h"
#include "surface_frontend.h"

namespace Luau
{
struct ParseResult;
}

namespace L3i::Surface
{
// What one record pattern lowered to: a holder local receiving the source value, then one
// declaration per field read, `local target = holder.key`, depth first in source order.
struct PatternSite
{
    Range holder;                    // The holder's generated name in its declaration or parameter.
    std::vector<Range> declarations; // Every generated declaration name: targets and nested holders.
    std::vector<Range> targets;      // The bound names' generated declarations, depth first.
    std::vector<Range> reads;        // Each field's generated read `holder.key`, depth first.
    // Tooling (recovery) lowering only: the key of a probe read `holder.probe` for each record
    // that reads no field, in the order records finish (nested before enclosing). Completion in
    // such a record borrows Luau's property completion there; its diagnostics are not the user's.
    std::vector<Range> probes;
    // Declarations and parameters: the generated offset where Luau must end the holder's
    // declaration, or begin the function body, for the inserted reads to be where intended.
    size_t boundary = SIZE_MAX;
};

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
    // Parallel to the clause's binding slots; a name slot has an empty holder.
    std::vector<PatternSite> patterns;
};

struct ComprehensionSite
{
    size_t comprehension = 0;
    Range call;
    std::vector<ClauseSite> clauses;
    Range projection;
    Range sink;       // Generated destination expression of an `into(...)` sink; empty otherwise.
    Range sinkOffset; // Generated offset expression of a buffer sink; empty otherwise.
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
    // Parallel to document.locals and document.parameters. A pattern Luau did not parse into a
    // declaration or parameter (broken source) has an empty holder and no reads.
    std::vector<PatternSite> localSites;
    std::vector<PatternSite> parameterSites;
    // The reads were placed at the frontend's estimated boundaries rather than located with a
    // parse: the caller's parse of `source` must confirm them with boundariesHold().
    bool estimated = false;
    // Comprehensions whose recognized data shape compares the binding against an identifier.
    // They lower to the scalar loop until the compiler verifies the identifier is a local
    // declared outside the comprehension and lowers again naming them in `localFilters`.
    std::vector<size_t> pendingLocalFilters;
};

// Shared compiler/tooling seam: all coordinates in map refer to the original input.
// Recovery is tooling-only. Strict compilation must reject document errors before parsing.
// Syntax tooling disables count fusion to retain unary length in the source tree.
// `estimated` places record pattern reads at the frontend's estimated boundaries when every
// pattern has one, sparing the parse that otherwise locates them; the caller then verifies
// its own parse of the result with boundariesHold() and lowers again without `estimated` when
// that fails.
LoweredSource lower(std::string_view source, bool recovery = false, bool fuseLength = true,
    const std::vector<Range>& bufferGenerators = {}, bool dynamicGenerators = false,
    const std::vector<size_t>& localFilters = {}, bool estimated = false);

// Whether Luau's parse of an estimated lowering ends every holder declaration, and begins every
// body, exactly where the reads were inserted. Then the lowering is exactly the one a located
// lowering produces. Always true for a lowering that was not estimated. Call before remapping.
bool boundariesHold(const LoweredSource& lowered, const Luau::ParseResult& parsed);
}
