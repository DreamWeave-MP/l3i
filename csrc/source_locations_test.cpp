// Remapping proof: skipping subtrees that map to themselves leaves every location exactly
// where the full walk puts it. Link with source_locations.cpp, source_map.cpp,
// surface_frontend.cpp, surface_syntax.cpp and the Cargo-built Ast/Common archives.

#include "source_locations.h"
#include "surface_syntax.h"

#include "Luau/Ast.h"
#include "Luau/Parser.h"

#include <cassert>
#include <iostream>
#include <string>
#include <vector>

namespace
{
// Every location a parse result carries that the remapper owns, in traversal order.
struct Collect final : Luau::AstVisitor
{
    std::vector<Luau::Location> seen;
    void local(Luau::AstLocal* value)
    {
        if (value) seen.push_back(value->location);
    }
    bool visit(Luau::AstType* node) override { return visit(static_cast<Luau::AstNode*>(node)); }
    bool visit(Luau::AstTypePack* node) override { return visit(static_cast<Luau::AstNode*>(node)); }
    bool visit(Luau::AstNode* node) override
    {
        seen.push_back(node->location);
        if (auto* n = node->as<Luau::AstExprLocal>()) local(n->local);
        if (auto* n = node->as<Luau::AstExprCall>()) seen.push_back(n->argLocation);
        if (auto* n = node->as<Luau::AstExprIndexName>()) seen.push_back(n->indexLocation);
        if (auto* n = node->as<Luau::AstStatLocal>())
            for (auto* v : n->vars) local(v);
        if (auto* n = node->as<Luau::AstStatFor>()) local(n->var);
        if (auto* n = node->as<Luau::AstStatForIn>())
            for (auto* v : n->vars) local(v);
        if (auto* n = node->as<Luau::AstExprFunction>())
        {
            local(n->self);
            for (auto* a : n->args) local(a);
            for (auto* attribute : n->attributes) seen.push_back(attribute->location);
            if (n->argLocation) seen.push_back(*n->argLocation);
        }
        return true;
    }
};

std::vector<Luau::Location> remapped(const std::string& source, bool skip)
{
    const auto lowered = L3i::Surface::lower(source, true);
    Luau::Allocator allocator;
    Luau::AstNameTable names(allocator);
    Luau::ParseOptions options;
    options.captureComments = true;
    Luau::ParseResult result = Luau::Parser::parse(lowered.source.data(), lowered.source.size(), names, allocator, options);
    L3i::Surface::remapLocations(result, lowered.map, skip);
    Collect collect;
    if (result.root) result.root->visit(&collect);
    for (const auto& comment : result.commentLocations) collect.seen.push_back(comment.location);
    for (const auto& error : result.errors) collect.seen.push_back(error.getLocation());
    return collect.seen;
}
}

int main()
{
    const std::vector<std::string> sources = {
        "local values = {1, 2}\nreturn [for x in values if x > 1 => x * 2]\n",
        "@native\nfunction f(a) return a end\nreturn [for x in xs => f(x)]\n",
        "@native function g(b) return b end return #[for x in xs => g(x)]\n",
        "local h = @checked function(c) return c end\nlocal t = sum[for v in vs => h(v)]\n",
        // An attribute on the prelude's shifted line, its function on the next: Luau's span of
        // an attributed function starts at the attribute, so no location escapes its subtree.
        "local h = @checked\nfunction(c) return c end\nlocal t = sum[for v in vs => h(v)]\n",
        "type Box<T> = { value: T,\n  next: Box<T>? }\nlocal b: Box<number> = { value = 1 }\nlocal v = if b.next then typeof(b) else nil\n"
        "local text = `{b.value} and {[for x in xs => x]}`\nlocal n = 0 n += 1\nfor i = 1, 3 do if i == 2 then continue end end\n"
        "return [for i in range(1, n) => i], text, v\n",
        "local {\n  position: {x, y},\n  id: key,\n} = e\nlocal function k({a}: {a: number}, b: number): number\n  return a + b + x\nend\nreturn k, key\n",
        "type P = { x: number, y: number }\r\nlocal p: P = { x = 1, y = 2 }\r\nlocal ids = [for {x} in { p } => x]\r\nreturn ids\r\n",
        "-- é🦀 comment\nlocal s = 'é🦀' return [for c in cs => s .. c], function<T>(v: T): T return v end\n",
        "local rows = [for r in rs =>\n  [for c in r.cells if c.ok =>\n    c.value]]\nlocal after = rows[1]\nreturn after\n",
        "local xs = values[2:3]\nlocal f = function(a, ...) return a, ... end\nreturn f(xs)\n",
        "return [for x in xs => x.\n",
        "local {x\nlocal y = 1\nreturn [for z in zs =>]\n",
    };
    for (const auto& source : sources)
    {
        const auto full = remapped(source, false);
        const auto skipped = remapped(source, true);
        assert(full.size() == skipped.size());
        for (size_t i = 0; i < full.size(); ++i)
            if (!(full[i] == skipped[i]))
            {
                std::cerr << "location " << i << " differs in:\n" << source << "\n";
                return 1;
            }
    }
    std::cout << "source location tests passed\n";
}
