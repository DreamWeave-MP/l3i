// This file is part of l3i and is licensed under MIT OR Apache-2.0.
#pragma once

#include "source_map.h"

namespace Luau
{
struct ParseResult;
}

namespace L3i::Surface
{
// Mutates generated coordinates to original coordinates once per reachable object.
// Call only once on a parse result. CST is deliberately not captured by these callers.
// Known parser coordinate references in prose are translated or explicitly qualified.
// Subtrees lying where the lowering left every coordinate in place are not walked: their
// locations are already original. A local is remapped where it is declared, so call this
// before removing any declaration from the tree. `skipUnchanged = false` walks everything.
void remapLocations(Luau::ParseResult& result, const SourceMap& map, bool skipUnchanged = true);
std::string originalParseMessage(std::string text, Position generatedContext, const SourceMap& map);
}
