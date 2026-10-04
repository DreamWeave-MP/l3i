// This file is part of l3i and is licensed under MIT OR Apache-2.0.
#include "Luau/BytecodeBuilder.h"
#include "Luau/Compiler.h"
#include "Luau/Parser.h"
#include "luacode.h"
#include "source_locations.h"
#include "surface_syntax.h"

#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>

namespace
{
char* compileImpl(const char* source, size_t size, lua_CompileOptions* options, size_t* outsize, bool dump)
{
    if (!outsize)
        return nullptr;

    std::string output;
    try
    {
        Luau::BytecodeBuilder bytecode;
        if (dump)
            bytecode.setDumpFlags(
                Luau::BytecodeBuilder::Dump_Code | Luau::BytecodeBuilder::Dump_Lines | Luau::BytecodeBuilder::Dump_Locals |
                Luau::BytecodeBuilder::Dump_Constants
            );

        Luau::CompileOptions compileOptions;
        if (options)
        {
            static_assert(sizeof(lua_CompileOptions) == sizeof(Luau::CompileOptions), "C and C++ compile options must match");
            std::memcpy(&compileOptions, options, sizeof(compileOptions));
        }
        auto lowered = L3i::Surface::lower(std::string_view(source, size));
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::ParseResult parsed = Luau::Parser::parse(lowered.source.data(), lowered.source.size(), names, allocator);
        L3i::Surface::remapLocations(parsed, lowered.map);
        if (!parsed.errors.empty())
            throw parsed.errors.front();
        Luau::compileOrThrow(bytecode, parsed, names, compileOptions);
        output = dump ? bytecode.dumpEverything() : bytecode.getBytecode();
    }
    catch (const Luau::ParseError& error)
    {
        output = Luau::BytecodeBuilder::getError(":" + std::to_string(error.getLocation().begin.line + 1) + ": " + error.what());
    }
    catch (const Luau::CompileError& error)
    {
        output = Luau::BytecodeBuilder::getError(":" + std::to_string(error.getLocation().begin.line + 1) + ": " + error.what());
    }
    catch (const std::exception& error)
    {
        // Match luau_compile's in-band error convention so Rust can translate compiler failures
        // into ordinary Jess errors without allowing a C++ exception across the FFI boundary.
        output.push_back('\0');
        output += error.what();
    }
    catch (...)
    {
        output = std::string(1, '\0') + "unknown Luau compiler failure";
    }

    char* result = static_cast<char*>(std::malloc(output.size()));
    if (!result)
        return nullptr;
    std::memcpy(result, output.data(), output.size());
    *outsize = output.size();
    return result;
}

char* compile(const char* source, size_t size, lua_CompileOptions* options, size_t* outsize, bool dump) noexcept
{
    if (!outsize)
        return nullptr;
    *outsize = 0;
    try
    {
        return compileImpl(source, size, options, outsize, dump);
    }
    catch (...)
    {
        // Formatting an error can itself allocate and throw. This outer barrier must
        // include the inner catch handlers; its fallback performs no allocation.
        return nullptr;
    }
}
}

extern "C" char* l3i_luau_compile(const char* source, size_t size, lua_CompileOptions* options, size_t* outsize)
{
    return compile(source, size, options, outsize, false);
}

extern "C" char* l3i_luau_disassemble(const char* source, size_t size, lua_CompileOptions* options, size_t* outsize)
{
    return compile(source, size, options, outsize, true);
}
