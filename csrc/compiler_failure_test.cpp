// Standalone allocation-failure probe; never link this global-new override into the VM host.
// Link with bytecode.cpp, surface_frontend.cpp, surface_syntax.cpp, source_map.cpp, source_locations.cpp and the
// Luau Compiler/Ast/Bytecode/Common archives. See SOURCE_MAPPING.md for the command.
#include "luacode.h"

#include <cassert>
#include <cstdlib>
#include <iostream>
#include <limits>
#include <new>
#include <string>

namespace
{
size_t allocations = 0;
size_t remaining = std::numeric_limits<size_t>::max();
bool active = false;
}

void* operator new(size_t size)
{
    if (active)
    {
        ++allocations;
        if (remaining == 0)
            throw std::bad_alloc();
        --remaining;
    }
    if (void* result = std::malloc(size ? size : 1))
        return result;
    throw std::bad_alloc();
}

void* operator new[](size_t size) { return ::operator new(size); }
void operator delete(void* memory) noexcept { std::free(memory); }
void operator delete[](void* memory) noexcept { std::free(memory); }
void operator delete(void* memory, size_t) noexcept { std::free(memory); }
void operator delete[](void* memory, size_t) noexcept { std::free(memory); }

extern "C" char* l3i_luau_compile(const char*, size_t, lua_CompileOptions*, size_t*);
extern "C" char* l3i_luau_disassemble(const char*, size_t, lua_CompileOptions*, size_t*);
int main()
{
    using Compile = decltype(&l3i_luau_compile);
    const std::string parseError = "local xs = [for x in {1} => x]\n\nlocal broken =";
    const std::string compileError = "return [for x in {1} => x]";
    const char* types[257];
    for (size_t i = 0; i < 256; ++i)
        types[i] = "UnusedType";
    types[256] = nullptr;
    for (Compile compile : {&l3i_luau_compile, &l3i_luau_disassemble})
        for (bool parser : {true, false})
        {
            const std::string& source = parser ? parseError : compileError;
            lua_CompileOptions options{};
            options.optimizationLevel = 2;
            options.debugLevel = 1;
            if (!parser)
                options.userdataTypes = types; // Deterministic CompileError, after parsing succeeds.
            size_t size = 99;
            allocations = 0;
            remaining = std::numeric_limits<size_t>::max();
            active = true;
            char* result = compile(source.data(), source.size(), &options, &size);
            active = false;
            assert(result && size && result[0] == '\0');
            const size_t count = allocations;
            std::free(result);
            size_t nulls = 0;
            for (size_t budget = 0; budget <= count + 1; ++budget)
            {
                size = 99;
                remaining = budget;
                active = true;
                try
                {
                    result = compile(source.data(), source.size(), &options, &size);
                }
                catch (...)
                {
                    active = false;
                    std::abort(); // Any escaping exception violates the C ABI.
                }
                active = false;
                if (result)
                    assert(size && result[0] == '\0');
                else
                {
                    assert(size == 0);
                    ++nulls;
                }
                std::free(result);
            }
            assert(nulls > 0);
            std::cout << (parser ? "parse" : "compile") << " failure: " << count + 2 << " allocation budgets passed\n";
        }
}
