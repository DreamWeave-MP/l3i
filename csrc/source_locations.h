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
void remapLocations(Luau::ParseResult& result, const SourceMap& map);
std::string originalParseMessage(std::string text, Position generatedContext, const SourceMap& map);
}
