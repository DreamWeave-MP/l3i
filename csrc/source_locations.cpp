// This file is part of l3i and is licensed under MIT OR Apache-2.0.
#include "source_locations.h"

#include "Luau/Ast.h"
#include "Luau/ParseResult.h"

#include <optional>
#include <unordered_set>

namespace L3i::Surface
{
namespace
{
class LocationRemapper final : public Luau::AstVisitor
{
public:
    explicit LocationRemapper(const SourceMap& map)
        : map(map)
    {
    }

    void remap(Luau::Location& location) const
    {
        // Map the whole range: provenance may be non-monotonic, so independently
        // mapping its endpoints would not necessarily produce an ordered span.
        const Span original = map.originalSpan({
            {location.begin.line, location.begin.column},
            {location.end.line, location.end.column},
        });
        location = Luau::Location(
            Luau::Position(original.begin.line, original.begin.column),
            Luau::Position(original.end.line, original.end.column)
        );
    }

    bool visit(Luau::AstNode* node) override
    {
        if (!nodes.insert(node).second)
            return false;

        remap(node->location);

        if (auto* expr = node->as<Luau::AstExprLocal>())
            remapLocal(expr->local);
        else if (auto* expr = node->as<Luau::AstExprCall>())
            remap(expr->argLocation);
        else if (auto* expr = node->as<Luau::AstExprIndexName>())
        {
            remap(expr->indexLocation);
            remap(expr->opPosition);
        }
        else if (auto* expr = node->as<Luau::AstExprFunction>())
        {
            if (expr->vararg)
                remap(expr->varargLocation);
            remap(expr->argLocation);
            remapLocal(expr->self);
            for (Luau::AstLocal* arg : expr->args)
                remapLocal(arg);
            visitFunctionMetadata(expr);
            // Stock traversal visits the body, including its independent end location.
        }
        else if (auto* expr = node->as<Luau::AstExprIfElse>())
        {
            remap(expr->conditionKeywordLocation);
            remap(expr->conditionEqualsLocation);
            remapLocal(expr->conditionLocal);
        }
        else if (auto* stat = node->as<Luau::AstStatIf>())
        {
            remap(stat->thenLocation);
            remap(stat->elseLocation);
            remap(stat->conditionKeywordLocation);
            remap(stat->conditionEqualsLocation);
            remapLocal(stat->conditionLocal);
        }
        else if (auto* stat = node->as<Luau::AstStatWhile>())
        {
            if (stat->hasDo)
                remap(stat->doLocation);
        }
        else if (auto* stat = node->as<Luau::AstStatFor>())
        {
            remapLocal(stat->var);
            if (stat->hasDo)
                remap(stat->doLocation);
        }
        else if (auto* stat = node->as<Luau::AstStatForIn>())
        {
            for (Luau::AstLocal* var : stat->vars)
                remapLocal(var);
            if (stat->hasIn)
                remap(stat->inLocation);
            if (stat->hasDo)
                remap(stat->doLocation);
        }
        else if (auto* stat = node->as<Luau::AstStatLocal>())
        {
            remap(stat->keywordLocation);
            remap(stat->equalsSignLocation);
            for (Luau::AstLocal* var : stat->vars)
                remapLocal(var);
        }
        else if (auto* stat = node->as<Luau::AstStatLocalFunction>())
        {
            remap(stat->constKeywordBegin);
            remapLocal(stat->name);
        }
        else if (auto* stat = node->as<Luau::AstStatTypeAlias>())
            remap(stat->nameLocation);
        else if (auto* stat = node->as<Luau::AstStatTypeFunction>())
            remap(stat->nameLocation);
        else if (auto* stat = node->as<Luau::AstStatDeclareGlobal>())
            remap(stat->nameLocation);
        else if (auto* stat = node->as<Luau::AstStatDeclareFunction>())
        {
            remap(stat->nameLocation);
            if (stat->vararg)
                remap(stat->varargLocation);
            for (size_t i = 0; i < stat->paramNames.size; ++i)
                remap(stat->paramNames.data[i].second);
            visitFunctionMetadata(stat);
        }
        else if (auto* stat = node->as<Luau::AstStatClass>())
        {
            remapLocal(stat->name);
            for (size_t i = 0; i < stat->members.size; ++i)
            {
                Luau::visit(
                    Luau::overloaded{
                        [&](Luau::AstClassProperty& property)
                        {
                            remap(property.qualifierLocation);
                            remap(property.nameLocation);
                            remap(property.typeColonLocation);
                        },
                        [&](Luau::AstClassMethod& method)
                        {
                            remap(method.qualifierLocation);
                            remap(method.keywordLocation);
                            remap(method.nameLocation);
                        },
                    },
                    stat->members.data[i]
                );
            }
        }
        else if (auto* stat = node->as<Luau::AstStatDeclareExternType>())
        {
            for (size_t i = 0; i < stat->props.size; ++i)
            {
                remap(stat->props.data[i].location);
                remap(stat->props.data[i].nameLocation);
            }
            // Stock traversal omits the extern type's indexer entirely.
            remapIndexer(stat->indexer);
        }
        else if (auto* type = node->as<Luau::AstTypeReference>())
        {
            remap(type->prefixLocation);
            remap(type->nameLocation);
            remapLocal(type->prefixLocal);
        }
        else if (auto* type = node->as<Luau::AstTypeTable>())
        {
            for (size_t i = 0; i < type->props.size; ++i)
            {
                remap(type->props.data[i].location);
                remap(type->props.data[i].accessLocation);
            }
            remapIndexer(type->indexer);
        }
        else if (auto* type = node->as<Luau::AstTypeFunction>())
        {
            for (size_t i = 0; i < type->argNames.size; ++i)
            {
                if (type->argNames.data[i])
                    remap(type->argNames.data[i]->second);
            }
            visitFunctionMetadata(type);
        }

        return true;
    }

    // Luau intentionally disables annotation traversal in its base visitor.
    bool visit(Luau::AstType* node) override { return visit(static_cast<Luau::AstNode*>(node)); }
    bool visit(Luau::AstTypePack* node) override { return visit(static_cast<Luau::AstNode*>(node)); }

private:
    const SourceMap& map;
    std::unordered_set<Luau::AstNode*> nodes;
    std::unordered_set<Luau::AstLocal*> locals;
    std::unordered_set<Luau::AstTableIndexer*> indexers;

    void remap(Luau::Position& position) const
    {
        if (position == Luau::Position::missing())
            return;
        const Position original = map.originalPosition({position.line, position.column});
        position = Luau::Position(original.line, original.column);
    }

    void remap(std::optional<Luau::Location>& location) const
    {
        if (location)
            remap(*location);
    }

    void visitNode(Luau::AstNode* node)
    {
        if (node)
            node->visit(this);
    }

    void remapLocal(Luau::AstLocal* local)
    {
        if (!local || !locals.insert(local).second)
            return;
        remap(local->location);
        visitNode(local->annotation);
        // Shadow links can reference locals outside the immediate subtree.
        remapLocal(local->shadow);
    }

    void remapIndexer(Luau::AstTableIndexer* indexer)
    {
        if (!indexer || !indexers.insert(indexer).second)
            return;
        remap(indexer->location);
        remap(indexer->accessLocation);
        visitNode(indexer->indexType);
        visitNode(indexer->resultType);
    }

    template<typename Function>
    void visitFunctionMetadata(Function* function)
    {
        // These edges are omitted by the stock function visitors. Generic
        // visitors in turn traverse their default types/type packs.
        for (Luau::AstAttr* attribute : function->attributes)
            visitNode(attribute);
        for (Luau::AstGenericType* generic : function->generics)
            visitNode(generic);
        for (Luau::AstGenericTypePack* generic : function->genericPacks)
            visitNode(generic);
    }
};
} // namespace

std::string originalParseMessage(std::string text, Position context, const SourceMap& map)
{
    // Match known Luau parser templates, never arbitrary numbers in quoted input.
    const size_t close = text.find(" (to close ");
    const size_t got = text.find(", got ");
    if (text.substr(0, 9) == "Expected " && close != std::string::npos && close < got)
    {
        const size_t at = text.find(" at ", close);
        if (at != std::string::npos && at < got)
            text = map.referenceText(std::move(text), at + 1, context);
        const size_t suggestion = text.rfind("; did you forget to close ");
        if (suggestion != std::string::npos && text.back() == '?')
        {
            const size_t suggestedAt = text.find(" at ", suggestion);
            if (suggestedAt != std::string::npos)
                text = map.referenceText(std::move(text), suggestedAt + 1, context);
        }
    }
    if (text.find("refers to a class and cannot be used as a variable name") != std::string::npos)
    {
        const size_t at = text.rfind("on line ");
        text = map.referenceText(std::move(text), at, context);
    }
    return text;
}

void remapLocations(Luau::ParseResult& result, const SourceMap& map)
{
    if (map.empty())
        return;
    LocationRemapper remapper(map);
    if (result.root)
        result.root->visit(&remapper);

    for (Luau::Comment& comment : result.commentLocations)
        remapper.remap(comment.location);
    for (Luau::HotComment& comment : result.hotcomments)
        remapper.remap(comment.location);
    for (Luau::ParseError& error : result.errors)
    {
        Luau::Location location = error.getLocation();
        std::string message = originalParseMessage(error.getMessage(), {location.begin.line, location.begin.column}, map);
        remapper.remap(location);
        error = Luau::ParseError(location, std::move(message));
    }
    result.lines = map.originalLines();
}
} // namespace L3i::Surface
