// This file is part of l3i and is licensed under MIT OR Apache-2.0.
#include "Luau/BytecodeBuilder.h"
#include "Luau/Compiler.h"
#include "luacode.h"

#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>

extern "C" char* l3i_luau_disassemble(const char* source, size_t size, lua_CompileOptions* options, size_t* outsize)
{
    if (!outsize)
        return nullptr;

    std::string output;
    try
    {
        Luau::BytecodeBuilder bytecode;
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
        Luau::compileOrThrow(bytecode, std::string(source, size), compileOptions);
        output = bytecode.dumpEverything();
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
