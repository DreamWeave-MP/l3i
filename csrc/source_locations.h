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
// Call only once on a parse result. CST coordinates, diagnostic message text, and
// ParseResult::lines are not changed; SourceMap does not expose original line count.
void remapLocations(Luau::ParseResult& result, const SourceMap& map);
}
