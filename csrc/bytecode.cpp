// This file is part of l3i and is licensed under MIT OR Apache-2.0.
#include "Luau/BytecodeBuilder.h"
#include "Luau/Compiler.h"
#include "Luau/Parser.h"
#include "luacode.h"
#include "source_locations.h"
#include "surface_syntax.h"

#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>
#include <string_view>
#include <vector>

namespace
{
Luau::Position positionAt(std::string_view text, size_t offset)
{
    Luau::Position result(0, 0);
    for (size_t i = 0; i < std::min(offset, text.size()); ++i)
        if (text[i] == '\n') result = Luau::Position(result.line + 1, 0);
        else ++result.column;
    return result;
}

struct ExpressionAt final : Luau::AstVisitor
{
    Luau::Location target;
    Luau::AstExpr* result = nullptr;
    explicit ExpressionAt(Luau::Location target) : target(target) {}
    bool visit(Luau::AstExpr* expression) override
    {
        if (expression->location == target) result = expression;
        return result == nullptr;
    }
};

// The compile-path lowering. A recognized pipeline whose filter compares the binding against
// an identifier is lowered as a scalar loop first; the parsed lowering then tells whether that
// identifier is a local (or upvalue) declared outside the comprehension, in which case reading
// it once is indistinguishable from reading it per element and the pipeline is lowered again
// as a data operation. Globals, and anything the parse cannot resolve, keep the loop.
L3i::Surface::LoweredSource lowerForCompile(std::string_view source)
{
    auto lowered = L3i::Surface::lower(source, false, true, {}, true);
    if (lowered.pendingLocalFilters.empty() || !lowered.document.errors.empty())
        return lowered;
    Luau::Allocator allocator;
    Luau::AstNameTable names(allocator);
    Luau::ParseResult parsed = Luau::Parser::parse(lowered.source.data(), lowered.source.size(), names, allocator);
    if (!parsed.root || !parsed.errors.empty())
        return lowered;
    std::vector<size_t> verified;
    for (const size_t index : lowered.pendingLocalFilters)
    {
        const auto site = std::find_if(lowered.sites.begin(), lowered.sites.end(),
            [&](const L3i::Surface::ComprehensionSite& site) { return site.comprehension == index; });
        if (site == lowered.sites.end()) continue;
        // A reduction's threshold sits in its filter clause; any/all compare in the projection.
        const auto range = site->clauses.size() >= 2 ? site->clauses[1].expression : site->projection;
        ExpressionAt finder({positionAt(lowered.source, range.begin), positionAt(lowered.source, range.end)});
        parsed.root->visit(&finder);
        const auto* binary = finder.result ? finder.result->as<Luau::AstExprBinary>() : nullptr;
        const auto* local = binary ? binary->right->as<Luau::AstExprLocal>() : nullptr;
        if (!local) continue;
        // Declared before the generated call begins: an outer local, not a loop binding.
        if (local->local->location.begin < positionAt(lowered.source, site->call.begin))
            verified.push_back(index);
    }
    if (verified.empty())
        return lowered;
    return L3i::Surface::lower(source, false, true, {}, true, verified);
}

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
        auto lowered = lowerForCompile(std::string_view(source, size));
        if (!lowered.document.errors.empty())
        {
            const auto& error = lowered.document.errors.front();
            Luau::Position begin(0, 0), end(0, 0);
            for (size_t i = 0; i < error.range.end; ++i)
            {
                if (i == error.range.begin)
                    begin = end;
                if (source[i] == '\n')
                    end = Luau::Position(end.line + 1, 0);
                else
                    ++end.column;
            }
            if (error.range.empty())
                begin = end;
            throw Luau::ParseError(Luau::Location(begin, end), error.message);
        }
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
        // into ordinary host errors without allowing a C++ exception across the FFI boundary.
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

namespace
{
void dumpRange(std::string& out, const char* label, L3i::Surface::Range range)
{
    out += ' ';
    out += label;
    out += '=';
    out += std::to_string(range.begin);
    out += ',';
    out += std::to_string(range.end);
}

// Every observable product of lowering, in one text: the generated source, its provenance
// segments, the tooling sites, and the document's structural errors. Snapshot tests compare
// this text, so a refactor of the lowerer proves itself by leaving every line unchanged.
std::string dumpImpl(std::string_view source, bool recovery, bool fuseLength, bool dynamic, bool bufferAll)
{
    std::vector<L3i::Surface::Range> buffers;
    if (bufferAll)
    {
        const auto document = L3i::Surface::parseSurface(source);
        for (const auto& comprehension : document.comprehensions)
            for (const auto& clause : comprehension.clauses)
                if (!clause.sliceSource.empty())
                    buffers.push_back(clause.sliceSource);
    }
    // The compile policy takes the compiler's own two-pass path so snapshots show what compiles.
    const auto lowered = !recovery && fuseLength && dynamic && !bufferAll ? lowerForCompile(source)
                                                                        : L3i::Surface::lower(source, recovery, fuseLength, buffers, dynamic);
    std::string out = lowered.source;
    out += "\n--prelude ";
    out += std::to_string(lowered.preludeStatements);
    for (const auto& segment : lowered.map.provenance())
    {
        out += "\n--segment ";
        out += std::to_string(segment.begin) + ' ' + std::to_string(segment.end) + ' ' + std::to_string(segment.originalBegin) +
            ' ' + std::to_string(segment.originalEnd) + (segment.copied ? " copied" : " synthetic");
    }
    for (const auto& site : lowered.sites)
    {
        out += "\n--site " + std::to_string(site.comprehension);
        dumpRange(out, "call", site.call);
        dumpRange(out, "projection", site.projection);
        for (const auto& clause : site.clauses)
        {
            out += "\n--clause";
            // Tooling reads `bindings` when present and falls back to `binding`: dump the effective list.
            if (clause.bindings.empty())
                dumpRange(out, "binding", clause.binding);
            else
                for (const auto& binding : clause.bindings)
                    dumpRange(out, "binding", binding);
            dumpRange(out, "expression", clause.expression);
            for (const auto& argument : clause.rangeArguments)
                dumpRange(out, "range", argument);
            for (const auto& argument : clause.zipArguments)
                dumpRange(out, clause.zipStrict ? "zipStrict" : "zip", argument);
            if (!clause.sliceSource.empty())
            {
                dumpRange(out, "sliceSource", clause.sliceSource);
                dumpRange(out, "sliceFirst", clause.sliceFirst);
                dumpRange(out, "sliceLast", clause.sliceLast);
            }
        }
    }
    for (const auto& site : lowered.sliceSites)
    {
        out += "\n--slice " + std::to_string(site.slice);
        dumpRange(out, "call", site.call);
        dumpRange(out, "source", site.source);
        dumpRange(out, "first", site.first);
        dumpRange(out, "last", site.last);
    }
    for (const auto& error : lowered.document.errors)
    {
        out += "\n--error ";
        out += std::to_string(error.range.begin) + ' ' + std::to_string(error.range.end) + ' ' + error.message;
    }
    out += '\n';
    return out;
}
}

// Lowering snapshot for tests: the generated text and every record the lowerer produces.
// Same malloc/free ownership as l3i_luau_compile; null on any failure, never an exception.
extern "C" char* l3i_surface_dump(
    const char* source, size_t size, int recovery, int fuseLength, int dynamic, int bufferAll, size_t* outsize) noexcept
{
    if (!outsize)
        return nullptr;
    *outsize = 0;
    try
    {
        const std::string out = dumpImpl(std::string_view(source, size), recovery != 0, fuseLength != 0, dynamic != 0, bufferAll != 0);
        char* result = static_cast<char*>(std::malloc(out.size()));
        if (!result)
            return nullptr;
        std::memcpy(result, out.data(), out.size());
        *outsize = out.size();
        return result;
    }
    catch (...)
    {
        return nullptr;
    }
}

