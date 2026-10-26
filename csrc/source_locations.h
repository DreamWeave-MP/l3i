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
// For compiling a remapped tree: Luau's register reuse frees a local before a statement of its
// block that begins past the local's last read, and a lowering's synthetic statements can map
// past reads that run after them. This moves such a statement's begin back to the earliest read
// after it in its function, and raises a read's end where one local's reads would go backwards,
// as Luau asserts they never do. On a tree whose coordinates were not reordered it changes
// nothing. Call after `remapLocations`, only on a tree that will be compiled.
void orderLocalReads(Luau::ParseResult& result);
std::string originalParseMessage(std::string text, Position generatedContext, const SourceMap& map);
}
