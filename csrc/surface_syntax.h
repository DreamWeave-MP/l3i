#pragma once

#include <stddef.h>

// L3i-owned source surface lowering.  Returns a malloc-owned byte buffer when the source changed;
// returns nullptr when no recognized surface syntax was present or allocation failed. No C++
// exception escapes. The buffer is not NUL-terminated;
// callers must use `outsize` and release it with the same C runtime's free().
extern "C" char* l3i_rewrite_surface_syntax(const char* source, size_t size, size_t* outsize);

#include "source_map.h"

namespace L3i::Surface
{
struct LoweredSource
{
    std::string source;
    SourceMap map;
};

// Shared compiler/tooling seam: all coordinates in map refer to the original input.
LoweredSource lower(std::string_view source);
}
