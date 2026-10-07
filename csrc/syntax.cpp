// `@dream/luau`'s parser: Luau's own `Luau::Parser`, its tree built as Luau tables directly on the
// calling thread's stack, with original comments, hot comments, errors and optional tokens.
// The l3i dialect substitutes genuine surface nodes for precisely identified generated calls;
// stock Luau nodes and their original-source spans otherwise retain their API shapes.
//
// Every table is made with its final size, and every key and enumeration string is pushed once
// per call and copied from its slot, so building a node hashes no string.

#include "lua.h"
#include "lualib.h"

#include "lgc.h"
#include "lobject.h"
#include "lstate.h"
#include "lstring.h"
#include "ltable.h"

#include "Luau/Ast.h"
#include "Luau/Lexer.h"
#include "Luau/ParseOptions.h"
#include "Luau/ParseResult.h"
#include "Luau/Parser.h"

#include "surface_syntax.h"
#include "source_locations.h"

#include <algorithm>
#include <cctype>
#include <unordered_set>

#include <stdint.h>
#include <string.h>

#include <exception>
#include <string>
#include <unordered_map>
#include <vector>

namespace {

using namespace Luau;

// Every string a tree uses: field names, node kinds, and enumeration values. One stack slot each.
#define L3I_SYNTAX_STRINGS(X) \
    X(clauses, "clauses") \
    X(projection, "projection") \
    X(binding, "binding") \
    X(bindings, "bindings") \
    X(sourceField, "source") \
    X(openLocation, "openLocation") \
    X(closeLocation, "closeLocation") \
    X(arrowLocation, "arrowLocation") \
    X(keywordLocation, "keywordLocation") \
    X(inLocation, "inLocation") \
    X(hasClose, "hasClose") \
    X(hasArrow, "hasArrow") \
    X(hasIn, "hasIn") \
    X(complete, "complete") \
    X(KExprReduction, "ExprReduction") \
    X(KExprSink, "ExprSink") \
    X(destination, "destination") \
    X(elementKind, "elementKind") \
    X(offsetField, "offset") \
    X(KExprRange, "ExprRange") \
    X(KExprEnumerate, "ExprEnumerate") \
    X(KExprZip, "ExprZip") \
    X(KExprSlice, "ExprSlice") \
    X(SSum, "sum") \
    X(SMin, "min") \
    X(SMax, "max") \
    X(SAny, "any") \
    X(SAll, "all") \
    X(KExprComprehension, "ExprComprehension") \
    X(KComprehensionGenerator, "ComprehensionGenerator") \
    X(KComprehensionFilter, "ComprehensionFilter") \
    X(kind, "kind") \
    X(line, "line") \
    X(column, "column") \
    X(endLine, "endLine") \
    X(endColumn, "endColumn") \
    X(hasSemicolon, "hasSemicolon") \
    X(expr, "expr") \
    X(value, "value") \
    X(quoteStyle, "quoteStyle") \
    X(local, "local") \
    X(upvalue, "upvalue") \
    X(name, "name") \
    X(func, "func") \
    X(args, "args") \
    X(strict, "strict") \
    X(first, "first") \
    X(last, "last") \
    X(self, "self") \
    X(typeArguments, "typeArguments") \
    X(argLocation, "argLocation") \
    X(index, "index") \
    X(indexLocation, "indexLocation") \
    X(op, "op") \
    X(attributes, "attributes") \
    X(generics, "generics") \
    X(genericPacks, "genericPacks") \
    X(vararg, "vararg") \
    X(varargAnnotation, "varargAnnotation") \
    X(returnAnnotation, "returnAnnotation") \
    X(body, "body") \
    X(debugname, "debugname") \
    X(functionDepth, "functionDepth") \
    X(loopDepth, "loopDepth") \
    X(items, "items") \
    X(itemKind, "itemKind") \
    X(key, "key") \
    X(left, "left") \
    X(right, "right") \
    X(annotation, "annotation") \
    X(condition, "condition") \
    X(hasThen, "hasThen") \
    X(trueExpr, "trueExpr") \
    X(hasElse, "hasElse") \
    X(falseExpr, "falseExpr") \
    X(conditionLocal, "conditionLocal") \
    X(strings, "strings") \
    X(expressions, "expressions") \
    X(messageIndex, "messageIndex") \
    X(hasEnd, "hasEnd") \
    X(thenbody, "thenbody") \
    X(elsebody, "elsebody") \
    X(thenLocation, "thenLocation") \
    X(elseLocation, "elseLocation") \
    X(hasDo, "hasDo") \
    X(list, "list") \
    X(vars, "vars") \
    X(values, "values") \
    X(isConst, "isConst") \
    X(isExported, "isExported") \
    X(equalsSignLocation, "equalsSignLocation") \
    X(var, "var") \
    X(from, "from") \
    X(to, "to") \
    X(step, "step") \
    X(nameLocation, "nameLocation") \
    X(type, "type") \
    X(exported, "exported") \
    X(params, "params") \
    X(paramNames, "paramNames") \
    X(retTypes, "retTypes") \
    X(superName, "superName") \
    X(props, "props") \
    X(indexer, "indexer") \
    X(isMethod, "isMethod") \
    X(access, "access") \
    X(statements, "statements") \
    X(super, "super") \
    X(open, "open") \
    X(prefix, "prefix") \
    X(hasParameterList, "hasParameterList") \
    X(parameters, "parameters") \
    X(isExact, "isExact") \
    X(argTypes, "argTypes") \
    X(argNames, "argNames") \
    X(returnTypes, "returnTypes") \
    X(types, "types") \
    X(tailType, "tailType") \
    X(isMissing, "isMissing") \
    X(typeList, "typeList") \
    X(variadicType, "variadicType") \
    X(genericName, "genericName") \
    X(defaultValue, "defaultValue") \
    X(indexType, "indexType") \
    X(resultType, "resultType") \
    X(shadow, "shadow") \
    X(root, "root") \
    X(errors, "errors") \
    X(message, "message") \
    X(comments, "comments") \
    X(hotComments, "hotComments") \
    X(header, "header") \
    X(content, "content") \
    X(lineStarts, "lineStarts") \
    X(tokens, "tokens") \
    X(KAttr, "Attr") \
    X(KGenericType, "GenericType") \
    X(KGenericTypePack, "GenericTypePack") \
    X(KLocal, "Local") \
    X(KExprGroup, "ExprGroup") \
    X(KExprConstantNil, "ExprConstantNil") \
    X(KExprConstantBool, "ExprConstantBool") \
    X(KExprConstantNumber, "ExprConstantNumber") \
    X(KExprConstantInteger, "ExprConstantInteger") \
    X(KExprConstantString, "ExprConstantString") \
    X(KExprLocal, "ExprLocal") \
    X(KExprGlobal, "ExprGlobal") \
    X(KExprVarargs, "ExprVarargs") \
    X(KExprCall, "ExprCall") \
    X(KExprIndexName, "ExprIndexName") \
    X(KExprIndexExpr, "ExprIndexExpr") \
    X(KExprFunction, "ExprFunction") \
    X(KExprTable, "ExprTable") \
    X(KExprUnary, "ExprUnary") \
    X(KExprBinary, "ExprBinary") \
    X(KExprTypeAssertion, "ExprTypeAssertion") \
    X(KExprIfElse, "ExprIfElse") \
    X(KExprInterpString, "ExprInterpString") \
    X(KExprInstantiate, "ExprInstantiate") \
    X(KExprError, "ExprError") \
    X(KStatBlock, "StatBlock") \
    X(KStatIf, "StatIf") \
    X(KStatWhile, "StatWhile") \
    X(KStatRepeat, "StatRepeat") \
    X(KStatBreak, "StatBreak") \
    X(KStatContinue, "StatContinue") \
    X(KStatReturn, "StatReturn") \
    X(KStatExpr, "StatExpr") \
    X(KStatLocal, "StatLocal") \
    X(KStatFor, "StatFor") \
    X(KStatForIn, "StatForIn") \
    X(KStatAssign, "StatAssign") \
    X(KStatCompoundAssign, "StatCompoundAssign") \
    X(KStatFunction, "StatFunction") \
    X(KStatLocalFunction, "StatLocalFunction") \
    X(KStatTypeAlias, "StatTypeAlias") \
    X(KStatTypeFunction, "StatTypeFunction") \
    X(KStatDeclareGlobal, "StatDeclareGlobal") \
    X(KStatDeclareFunction, "StatDeclareFunction") \
    X(KStatDeclareExternType, "StatDeclareExternType") \
    X(KStatClass, "StatClass") \
    X(KStatError, "StatError") \
    X(KTypeReference, "TypeReference") \
    X(KTypeTable, "TypeTable") \
    X(KTypeFunction, "TypeFunction") \
    X(KTypeTypeof, "TypeTypeof") \
    X(KTypeOptional, "TypeOptional") \
    X(KTypeUnion, "TypeUnion") \
    X(KTypeIntersection, "TypeIntersection") \
    X(KTypeSingletonBool, "TypeSingletonBool") \
    X(KTypeSingletonString, "TypeSingletonString") \
    X(KTypeGroup, "TypeGroup") \
    X(KTypeError, "TypeError") \
    X(KTypePackExplicit, "TypePackExplicit") \
    X(KTypePackVariadic, "TypePackVariadic") \
    X(KTypePackGeneric, "TypePackGeneric") \
    X(KTableItem, "TableItem") \
    X(KTableProp, "TableProp") \
    X(KTableIndexer, "TableIndexer") \
    X(KDeclaredProp, "DeclaredProp") \
    X(KArgumentName, "ArgumentName") \
    X(KComment, "Comment") \
    X(KHotComment, "HotComment") \
    X(KError, "Error") \
    X(SDouble, "double") \
    X(SSingle, "single") \
    X(SBacktick, "backtick") \
    X(SLong, "long") \
    X(SUnquoted, "unquoted") \
    X(SListItem, "list") \
    X(SRecord, "record") \
    X(SGeneral, "general") \
    X(SNot, "not") \
    X(SMinus, "-") \
    X(SLen, "#") \
    X(SAdd, "+") \
    X(SSub, "-") \
    X(SMul, "*") \
    X(SDiv, "/") \
    X(SFloorDiv, "//") \
    X(SMod, "%") \
    X(SPow, "^") \
    X(SConcat, "..") \
    X(SNe, "~=") \
    X(SEq, "==") \
    X(SLt, "<") \
    X(SLe, "<=") \
    X(SGt, ">") \
    X(SGe, ">=") \
    X(SAnd, "and") \
    X(SOr, "or") \
    X(SDot, ".") \
    X(SColon, ":") \
    X(SRead, "read") \
    X(SWrite, "write") \
    X(SReadWrite, "readwrite") \
    X(SLineComment, "line") \
    X(SBlockComment, "block") \
    X(SBrokenComment, "broken") \
    X(SChecked, "checked") \
    X(SNative, "native") \
    X(SDeprecated, "deprecated") \
    X(SDebugNoinline, "debugnoinline") \
    X(SUnknown, "unknown")

enum Str : int
{
#define L3I_SYNTAX_ENUM(id, text) id,
    L3I_SYNTAX_STRINGS(L3I_SYNTAX_ENUM)
#undef L3I_SYNTAX_ENUM
        StrCount
};

const char* const kStrings[] = {
#define L3I_SYNTAX_TEXT(id, text) text,
    L3I_SYNTAX_STRINGS(L3I_SYNTAX_TEXT)
#undef L3I_SYNTAX_TEXT
};

const size_t kStringLengths[] = {
#define L3I_SYNTAX_LENGTH(id, text) sizeof(text) - 1,
    L3I_SYNTAX_STRINGS(L3I_SYNTAX_LENGTH)
#undef L3I_SYNTAX_LENGTH
};

// Fields every node has: kind and the four span numbers.
constexpr int kNodeFields = 5;

// Token kinds, in the order `luau.tokenKinds` numbers them (src/syntax.rs mirrors the names).
enum TokenKind : uint32_t
{
    TokenName = 1,
    TokenKeyword,
    TokenNumber,
    TokenString,
    TokenLongString,
    TokenInterpBegin,
    TokenInterpMid,
    TokenInterpEnd,
    TokenInterpSimple,
    TokenComment,
    TokenBlockComment,
    TokenAttribute,
    TokenSymbol,
    TokenError,
};

using SurfaceRange = L3i::Surface::Range;

void ensureRoot(ParseResult& result, std::string_view source, Allocator& allocator)
{
    if (result.root)
        return;
    Position end(0, 0);
    for (char c : source)
        if (c == '\n') end = Position(end.line + 1, 0);
        else ++end.column;
    const Location entire(Position(0, 0), end);
    if (result.errors.empty())
        result.errors.emplace_back(entire, "parser recovery could not construct a tree");
    auto** body = static_cast<AstStat**>(allocator.allocate(sizeof(AstStat*)));
    body[0] = allocator.alloc<AstStatError>(entire, AstArray<AstExpr*>{}, AstArray<AstStat*>{}, 0);
    result.root = allocator.alloc<AstStatBlock>(entire, AstArray<AstStat*>{body, 1}, false);
}

// Index actual AST identities in generated coordinates, before any location is remapped.
// A site's emitted expression can end in trivia: select the largest expression with the
// same begin that is wholly contained in the site, not an exact end or a temporary name.
class SiteIndex final : public AstVisitor
{
public:
    explicit SiteIndex(const std::string& generated)
    {
        starts.push_back(0);
        for (size_t i = 0; i < generated.size(); ++i)
            if (generated[i] == '\n')
                starts.push_back(i + 1);
    }

    size_t offset(Position p) const { return starts.at(p.line) + p.column; }
    bool visit(AstType*) override { return true; }
    bool visit(AstTypePack*) override { return true; }
    bool visit(AstTypeFunction* n) override
    {
        for (auto* attr : n->attributes) attr->visit(this);
        for (auto* generic : n->generics) generic->visit(this);
        for (auto* generic : n->genericPacks) generic->visit(this);
        return true;
    }
    bool visit(AstExpr* expr) override
    {
        expressions[offset(expr->location.begin)].push_back(expr);
        return true;
    }
    void local(AstLocal* value)
    {
        if (value)
            bindings[offset(value->location.begin)] = value;
    }
    bool visit(AstNode* node) override
    {
        if (auto* n = node->as<AstStatTypeAlias>())
        {
            for (auto* generic : n->generics) generic->visit(this);
            for (auto* generic : n->genericPacks) generic->visit(this);
        }
        if (auto* n = node->as<AstStatDeclareFunction>())
        {
            for (auto* attr : n->attributes) attr->visit(this);
            for (auto* generic : n->generics) generic->visit(this);
            for (auto* generic : n->genericPacks) generic->visit(this);
        }
        if (auto* n = node->as<AstStatLocal>())
            for (auto* v : n->vars) local(v);
        else if (auto* n = node->as<AstStatFor>()) local(n->var);
        else if (auto* n = node->as<AstStatForIn>())
            for (auto* v : n->vars) local(v);
        else if (auto* n = node->as<AstStatLocalFunction>()) local(n->name);
        return true;
    }
    AstExpr* expression(SurfaceRange range) const
    {
        auto it = expressions.find(range.begin);
        AstExpr* best = nullptr;
        size_t bestEnd = 0;
        if (it != expressions.end())
            for (AstExpr* expr : it->second)
            {
                size_t end = offset(expr->location.end);
                if (end <= range.end && (!best || end > bestEnd))
                    best = expr, bestEnd = end;
            }
        return best;
    }
    bool visit(AstExprFunction* n) override
    {
        visit(static_cast<AstExpr*>(n));
        for (auto* attr : n->attributes) attr->visit(this);
        for (auto* generic : n->generics) generic->visit(this);
        for (auto* generic : n->genericPacks) generic->visit(this);
        if (n->self && n->self->annotation) n->self->annotation->visit(this);
        for (auto* arg : n->args)
            if (arg->annotation) arg->annotation->visit(this);
        if (n->varargAnnotation) n->varargAnnotation->visit(this);
        if (n->returnAnnotation) n->returnAnnotation->visit(this);
        n->body->visit(this);
        return false;
    }
    AstLocal* binding(SurfaceRange range) const
    {
        auto it = bindings.find(range.begin);
        return it != bindings.end() && offset(it->second->location.end) == range.end ? it->second : nullptr;
    }
    std::vector<size_t> starts;
    std::unordered_map<size_t, std::vector<AstExpr*>> expressions;
    std::unordered_map<size_t, AstLocal*> bindings;
};

struct SurfaceClause
{
    AstLocal* binding = nullptr;
    std::vector<AstLocal*> bindings;
    AstExpr* expression = nullptr;
    std::vector<AstExpr*> rangeArguments;
    std::vector<AstExpr*> zipArguments;
    bool zipStrict = false;
    AstExpr* sliceSource = nullptr;
    AstExpr* sliceFirst = nullptr;
    AstExpr* sliceLast = nullptr;
};
struct SurfaceExpression
{
    const L3i::Surface::Comprehension* record = nullptr;
    std::vector<SurfaceClause> clauses;
    AstExpr* projection = nullptr;
    AstExpr* sink = nullptr;
    AstExpr* sinkOffset = nullptr;
};
using SurfaceExpressions = std::unordered_map<AstExprCall*, SurfaceExpression>;
struct SurfaceSlice
{
    const L3i::Surface::Slice* record = nullptr;
    AstExpr* source = nullptr;
    AstExpr* first = nullptr;
    AstExpr* last = nullptr;
};
using SurfaceSlices = std::unordered_map<AstExprCall*, SurfaceSlice>;

// Traverse the source tree, not the synthetic IIFE/loops. Keep stock AstLocal pointers and
// binding resolution, but recompute scope metadata using only source functions and loops.
class SourceMetadata final : public AstVisitor
{
public:
    SourceMetadata(const SurfaceExpressions& surfaces, const SurfaceSlices& slices) : surfaces(surfaces), slices(slices) {}
    void walk(AstNode* n) { if (n) n->visit(this); }
    void local(AstLocal* n, bool annotation = true)
    {
        if (!n || !seen.insert(n).second) return;
        n->functionDepth = functionDepth;
        n->loopDepth = loopDepth;
        if (annotation) walk(n->annotation);
    }
    bool visit(AstType*) override { return true; }
    bool visit(AstTypePack*) override { return true; }
    bool visit(AstTypeFunction* n) override
    {
        for (auto* attr : n->attributes) attr->visit(this);
        for (auto* generic : n->generics) generic->visit(this);
        for (auto* generic : n->genericPacks) generic->visit(this);
        return true;
    }
    bool visit(AstNode* n) override
    {
        if (auto* s = n->as<AstStatTypeAlias>())
        {
            for (auto* generic : s->generics) walk(generic);
            for (auto* generic : s->genericPacks) walk(generic);
        }
        if (auto* s = n->as<AstStatDeclareFunction>())
        {
            for (auto* attr : s->attributes) walk(attr);
            for (auto* generic : s->generics) walk(generic);
            for (auto* generic : s->genericPacks) walk(generic);
        }
        if (auto* s = n->as<AstStatLocal>())
            for (auto* v : s->vars) local(v);
        else if (auto* s = n->as<AstStatLocalFunction>()) local(s->name);
        else if (auto* s = n->as<AstStatIf>()) local(s->conditionLocal);
        else if (auto* s = n->as<AstStatClass>()) local(s->name);
        return true;
    }
    bool visit(AstExprLocal* n) override
    {
        n->upvalue = n->local->functionDepth != functionDepth;
        return false;
    }
    bool visit(AstExprIfElse* n) override
    {
        local(n->conditionLocal);
        walk(n->condition); walk(n->trueExpr); walk(n->falseExpr);
        return false;
    }
    bool visit(AstExprCall* n) override
    {
        auto it = surfaces.find(n);
        if (it == surfaces.end())
        {
            auto slice = slices.find(n);
            if (slice == slices.end()) return true;
            walk(slice->second.source); walk(slice->second.first); walk(slice->second.last);
            return false;
        }
        unsigned saved = loopDepth;
        for (size_t i = 0; i < it->second.clauses.size(); ++i)
        {
            const auto& clause = it->second.clauses[i];
            if (clause.rangeArguments.empty()) walk(clause.expression);
            else for (AstExpr* argument : clause.rangeArguments) walk(argument);
            for (AstExpr* argument : clause.zipArguments) walk(argument);
            walk(clause.sliceSource); walk(clause.sliceFirst); walk(clause.sliceLast);
            if (it->second.record->clauses[i].kind == L3i::Surface::ClauseKind::Generator)
            {
                ++loopDepth;
                if (clause.bindings.empty()) local(clause.binding);
                else for (AstLocal* binding : clause.bindings) local(binding);
            }
        }
        walk(it->second.projection);
        loopDepth = saved;
        return false;
    }
    bool visit(AstExprFunction* n) override
    {
        for (auto* attr : n->attributes) walk(attr);
        for (auto* generic : n->generics) walk(generic);
        for (auto* generic : n->genericPacks) walk(generic);
        // Stock Luau parses signature annotations in the enclosing function, before
        // introducing parameter locals and entering the function body.
        if (n->self) walk(n->self->annotation);
        for (auto* arg : n->args) walk(arg->annotation);
        walk(n->varargAnnotation); walk(n->returnAnnotation);
        unsigned savedLoop = loopDepth;
        ++functionDepth;
        loopDepth = 0;
        n->functionDepth = functionDepth;
        local(n->self, false);
        for (auto* arg : n->args) local(arg, false);
        walk(n->body);
        --functionDepth;
        loopDepth = savedLoop;
        return false;
    }
    bool visit(AstStatWhile* n) override
    {
        walk(n->condition);
        ++loopDepth; walk(n->body); --loopDepth;
        return false;
    }
    bool visit(AstStatRepeat* n) override
    {
        ++loopDepth; walk(n->body); --loopDepth; walk(n->condition);
        return false;
    }
    bool visit(AstStatFor* n) override
    {
        if (n->var) walk(n->var->annotation);
        walk(n->from); walk(n->to); walk(n->step);
        ++loopDepth;
        local(n->var, false);
        walk(n->body); --loopDepth;
        return false;
    }
    bool visit(AstStatForIn* n) override
    {
        for (auto* var : n->vars) walk(var->annotation);
        for (auto* value : n->values) walk(value);
        ++loopDepth;
        for (auto* var : n->vars) local(var, false);
        walk(n->body); --loopDepth;
        return false;
    }
    const SurfaceExpressions& surfaces;
    const SurfaceSlices& slices;
    std::unordered_set<AstLocal*> seen;
    unsigned functionDepth = 0;
    unsigned loopDepth = 0;
};

class Builder final : public AstVisitor
{
public:
    Builder(lua_State* L, const char* source, size_t length)
        : L(L)
        , source(source)
        , length(length)
    {
        lineStarts.push_back(0);
        for (const char* at = source; (at = static_cast<const char*>(memchr(at, '\n', source + length - at))) != nullptr; ++at)
            lineStarts.push_back(uint32_t(at - source + 1));
    }

    // Pushes every string, then the locals table, onto the stack, where they stay anchored while
    // the tree is built; the caller checked the space.
    void begin()
    {
        base = lua_gettop(L) + 1;
        for (int i = 0; i < StrCount; ++i)
        {
            lua_pushlstring(L, kStrings[i], kStringLengths[i]);
            keys[i] = tsvalue(L->top - 1);
        }
        lua_createtable(L, 0, 0);
        locals = hvalue(L->top - 1);
    }

    // The source offset of a position, clamped to the source.
    size_t offset(const Position& position) const
    {
        if (position.line >= lineStarts.size())
            return length;
        size_t at = size_t(lineStarts[position.line]) + position.column;
        return at < length ? at : length;
    }

    // A new table on top of the stack. The collector steps here and nowhere else in the build,
    // while everything built so far is reachable from the stack; the thread barrier then keeps
    // the values pushed after the step visible to the collector.
    void newTable(int narray, int nhash)
    {
        // Each nesting level holds its table and at most one pending value.
        if (!lua_checkstack(L, 8))
            luaL_error(L, "luau.parse: the tree is nested too deeply");
        luaC_checkGC(L);
        luaC_threadbarrier(L);
        sethvalue(L, L->top, luaH_new(L, narray, nhash));
        L->top++;
    }

    void store(LuaTable* table, TValue* slot, const TValue* value)
    {
        setobj2t(L, slot, value);
        luaC_barriert(L, table, value);
    }

    // Sets key in the table below the top to the top value, and pops it.
    void popInto(Str key)
    {
        LuaTable* table = hvalue(L->top - 2);
        store(table, luaH_setstr(L, table, keys[key]), L->top - 1);
        L->top--;
    }

    // Sets index in the table below the top to the top value, and pops it.
    void popAt(int index)
    {
        LuaTable* table = hvalue(L->top - 2);
        store(table, luaH_setnum(L, table, index), L->top - 1);
        L->top--;
    }

    void pushText(const char* data, size_t size)
    {
        setsvalue(L, L->top, luaS_newlstr(L, data, size));
        L->top++;
    }

    void pushNil()
    {
        setnilvalue(L->top);
        L->top++;
    }

    void setString(Str key, Str value)
    {
        LuaTable* table = hvalue(L->top - 1);
        TValue text;
        setsvalue(L, &text, keys[value]);
        store(table, luaH_setstr(L, table, keys[key]), &text);
    }

    void setNumber(Str key, double value)
    {
        setnvalue(luaH_setstr(L, hvalue(L->top - 1), keys[key]), value);
    }

    void setBool(Str key, bool value)
    {
        setbvalue(luaH_setstr(L, hvalue(L->top - 1), keys[key]), value);
    }

    void setName(Str key, AstName name)
    {
        if (name.value == nullptr)
            return;
        pushText(name.value, strlen(name.value));
        popInto(key);
    }

    void setText(Str key, const char* data, size_t size)
    {
        pushText(data, size);
        popInto(key);
    }

    // The span fields: 1-based lines and columns, the end column inclusive.
    void spanFields(const Location& location)
    {
        setNumber(line, double(location.begin.line) + 1);
        setNumber(column, double(location.begin.column) + 1);
        setNumber(endLine, double(location.end.line) + 1);
        setNumber(endColumn, double(location.end.column));
    }

    // A new table with a kind and a span, and room for `fields` more keys.
    void open(Str kindName, const Location& location, int fields)
    {
        newTable(0, kNodeFields + fields);
        setString(kind, kindName);
        spanFields(location);
    }

    void span(Str key, const Location& location)
    {
        newTable(0, 4);
        spanFields(location);
        popInto(key);
    }

    void optionalSpan(Str key, const std::optional<Location>& location)
    {
        if (location)
            span(key, *location);
    }

    template<typename T>
    void node(T* node)
    {
        if (node == nullptr)
            pushNil();
        else
            node->visit(this);
    }

    template<typename T>
    void setNode(Str key, T* value)
    {
        if (value == nullptr)
            return;
        value->visit(this);
        popInto(key);
    }

    template<typename T>
    void setArray(Str key, const AstArray<T*>& array)
    {
        newTable(int(array.size), 0);
        for (size_t i = 0; i < array.size; ++i)
        {
            node(array.data[i]);
            popAt(int(i + 1));
        }
        popInto(key);
    }

    void setTypeOrPacks(Str key, const AstArray<AstTypeOrPack>& array)
    {
        newTable(int(array.size), 0);
        for (size_t i = 0; i < array.size; ++i)
        {
            if (array.data[i].type != nullptr)
                node(array.data[i].type);
            else
                node(array.data[i].typePack);
            popAt(int(i + 1));
        }
        popInto(key);
    }

    void typeList(Str key, const AstTypeList& list)
    {
        newTable(0, 2);
        setArray(types, list.types);
        setNode(tailType, list.tailType);
        popInto(key);
    }

    // The one table of a local, shared by its declaration and every use.
    void pushLocal(AstLocal* local)
    {
        auto [it, inserted] = localSlots.try_emplace(local, int(localSlots.size() + 1));
        if (!inserted)
        {
            setobj2s(L, L->top, luaH_getnum(locals, it->second));
            L->top++;
            return;
        }
        int slot = it->second;
        open(KLocal, local->location, 6);
        setName(name, local->name);
        setBool(isConst, local->isConst);
        setNumber(functionDepth, double(local->functionDepth));
        setNumber(loopDepth, double(local->loopDepth));
        // Registered before the annotation and shadow are built: neither can reach this local,
        // but the table must exist before anything refers to it.
        store(locals, luaH_setnum(L, locals, slot), L->top - 1);
        setNode(annotation, local->annotation);
        if (local->shadow != nullptr)
        {
            pushLocal(local->shadow);
            popInto(shadow);
        }
    }

    void setLocal(Str key, AstLocal* local)
    {
        if (local == nullptr)
            return;
        pushLocal(local);
        popInto(key);
    }

    void setLocals(Str key, const AstArray<AstLocal*>& list)
    {
        newTable(int(list.size), 0);
        for (size_t i = 0; i < list.size; ++i)
        {
            pushLocal(list.data[i]);
            popAt(int(i + 1));
        }
        popInto(key);
    }

    void stat(AstStat* node, Str kindName, int fields)
    {
        open(kindName, node->location, fields + 1);
        setBool(hasSemicolon, node->hasSemicolon);
    }

    Str access(AstTableAccess value)
    {
        switch (value)
        {
        case AstTableAccess::Read:
            return SRead;
        case AstTableAccess::Write:
            return SWrite;
        default:
            return SReadWrite;
        }
    }

    void tableIndexer(AstTableIndexer* indexer)
    {
        if (indexer == nullptr)
            return;
        open(KTableIndexer, indexer->location, 3);
        setNode(indexType, indexer->indexType);
        setNode(resultType, indexer->resultType);
        setString(Str::access, access(indexer->access));
        popInto(Str::indexer);
    }

    static Str binaryOp(AstExprBinary::Op op)
    {
        switch (op)
        {
        case AstExprBinary::Add:
            return SAdd;
        case AstExprBinary::Sub:
            return SSub;
        case AstExprBinary::Mul:
            return SMul;
        case AstExprBinary::Div:
            return SDiv;
        case AstExprBinary::FloorDiv:
            return SFloorDiv;
        case AstExprBinary::Mod:
            return SMod;
        case AstExprBinary::Pow:
            return SPow;
        case AstExprBinary::Concat:
            return SConcat;
        case AstExprBinary::CompareNe:
            return SNe;
        case AstExprBinary::CompareEq:
            return SEq;
        case AstExprBinary::CompareLt:
            return SLt;
        case AstExprBinary::CompareLe:
            return SLe;
        case AstExprBinary::CompareGt:
            return SGt;
        case AstExprBinary::CompareGe:
            return SGe;
        case AstExprBinary::And:
            return SAnd;
        default:
            return SOr;
        }
    }

    // Attributes, expressions and statements: each visit pushes exactly one table and returns
    // false, so Luau's own traversal never descends; the builder walks the children itself.

    bool visit(AstAttr* node) override
    {
        open(KAttr, node->location, 3);
        Str type;
        switch (node->type)
        {
        case AstAttr::Type::Checked:
            type = SChecked;
            break;
        case AstAttr::Type::Native:
            type = SNative;
            break;
        case AstAttr::Type::Deprecated:
            type = SDeprecated;
            break;
        case AstAttr::Type::DebugNoinline:
            type = SDebugNoinline;
            break;
        default:
            type = SUnknown;
            break;
        }
        setString(Str::type, type);
        setName(name, node->name);
        setArray(args, node->args);
        return false;
    }

    bool visit(AstGenericType* node) override
    {
        open(KGenericType, node->location, 2);
        setName(name, node->name);
        setNode(defaultValue, node->defaultValue);
        return false;
    }

    bool visit(AstGenericTypePack* node) override
    {
        open(KGenericTypePack, node->location, 2);
        setName(name, node->name);
        setNode(defaultValue, node->defaultValue);
        return false;
    }

    bool visit(AstExprGroup* node) override
    {
        open(KExprGroup, node->location, 1);
        setNode(expr, node->expr);
        return false;
    }

    bool visit(AstExprConstantNil* node) override
    {
        open(KExprConstantNil, node->location, 0);
        return false;
    }

    bool visit(AstExprConstantBool* node) override
    {
        open(KExprConstantBool, node->location, 1);
        setBool(value, node->value);
        return false;
    }

    bool visit(AstExprConstantNumber* node) override
    {
        open(KExprConstantNumber, node->location, 1);
        setNumber(value, node->value);
        return false;
    }

    bool visit(AstExprConstantInteger* node) override
    {
        open(KExprConstantInteger, node->location, 1);
        setlvalue(L->top, node->value);
        L->top++;
        popInto(value);
        return false;
    }

    // What the string was written with. Luau's tree folds single quotes, double quotes and
    // backticks into one style; the string's first byte tells them apart.
    Str quote(AstExprConstantString* node)
    {
        switch (node->quoteStyle)
        {
        case AstExprConstantString::QuoteStyle::QuotedRaw:
            return SLong;
        case AstExprConstantString::QuoteStyle::Unquoted:
            return SUnquoted;
        default:
        {
            size_t at = offset(node->location.begin);
            char first = at < length ? source[at] : '"';
            return first == '\'' ? SSingle : first == '`' ? SBacktick : SDouble;
        }
        }
    }

    bool visit(AstExprConstantString* node) override
    {
        open(KExprConstantString, node->location, 2);
        setText(value, node->value.data, node->value.size);
        setString(quoteStyle, quote(node));
        return false;
    }

    bool visit(AstExprLocal* node) override
    {
        open(KExprLocal, node->location, 2);
        setLocal(local, node->local);
        setBool(upvalue, node->upvalue);
        return false;
    }

    bool visit(AstExprGlobal* node) override
    {
        open(KExprGlobal, node->location, 1);
        setName(name, node->name);
        return false;
    }

    bool visit(AstExprVarargs* node) override
    {
        open(KExprVarargs, node->location, 0);
        return false;
    }

    bool visit(AstExprCall* node) override
    {
        auto surface = surfaces.find(node);
        if (surface != surfaces.end())
        {
            comprehension(surface->second);
            return false;
        }
        auto slice = slices.find(node);
        if (slice != slices.end())
        {
            open(KExprSlice, location(slice->second.record->range), 4);
            surfaceExpr(slice->second.source, slice->second.record->source);
            popInto(sourceField);
            surfaceExpr(slice->second.first, slice->second.record->first);
            popInto(first);
            surfaceExpr(slice->second.last, slice->second.record->last);
            popInto(last);
            if (const auto kind = slice->second.record->kind; !kind.empty())
            {
                lua_pushlstring(L, source + kind.begin + 1, kind.end - kind.begin - 2);
                popInto(elementKind);
            }
            return false;
        }
        open(KExprCall, node->location, 5);
        setNode(func, node->func);
        setArray(args, node->args);
        setBool(self, node->self);
        setTypeOrPacks(typeArguments, node->typeArguments);
        span(argLocation, node->argLocation);
        return false;
    }

    bool visit(AstExprIndexName* node) override
    {
        open(KExprIndexName, node->location, 4);
        setNode(expr, node->expr);
        setName(index, node->index);
        span(indexLocation, node->indexLocation);
        setString(op, node->op == ':' ? SColon : SDot);
        return false;
    }

    bool visit(AstExprIndexExpr* node) override
    {
        open(KExprIndexExpr, node->location, 2);
        setNode(expr, node->expr);
        setNode(index, node->index);
        return false;
    }

    bool visit(AstExprFunction* node) override
    {
        open(KExprFunction, node->location, 12);
        setArray(attributes, node->attributes);
        setArray(generics, node->generics);
        setArray(genericPacks, node->genericPacks);
        setLocal(self, node->self);
        setLocals(args, node->args);
        setBool(vararg, node->vararg);
        setNode(varargAnnotation, node->varargAnnotation);
        setNode(returnAnnotation, node->returnAnnotation);
        setNode(body, node->body);
        setNumber(functionDepth, double(node->functionDepth));
        setName(debugname, node->debugname);
        optionalSpan(argLocation, node->argLocation);
        return false;
    }

    bool visit(AstExprTable* node) override
    {
        open(KExprTable, node->location, 1);
        newTable(int(node->items.size), 0);
        for (size_t i = 0; i < node->items.size; ++i)
        {
            const AstExprTable::Item& item = node->items.data[i];
            Location location = item.key != nullptr ? Location(item.key->location, item.value->location) : item.value->location;
            open(KTableItem, location, 3);
            setString(itemKind,
                item.kind == AstExprTable::Item::Kind::List     ? SListItem
                : item.kind == AstExprTable::Item::Kind::Record ? SRecord
                                                          : SGeneral);
            setNode(key, item.key);
            setNode(value, item.value);
            popAt(int(i + 1));
        }
        popInto(items);
        return false;
    }

    bool visit(AstExprUnary* node) override
    {
        open(KExprUnary, node->location, 2);
        setString(op, node->op == AstExprUnary::Op::Not ? SNot : node->op == AstExprUnary::Op::Minus ? SMinus : SLen);
        setNode(expr, node->expr);
        return false;
    }

    bool visit(AstExprBinary* node) override
    {
        open(KExprBinary, node->location, 3);
        setString(op, binaryOp(node->op));
        setNode(left, node->left);
        setNode(right, node->right);
        return false;
    }

    bool visit(AstExprTypeAssertion* node) override
    {
        open(KExprTypeAssertion, node->location, 2);
        setNode(expr, node->expr);
        setNode(annotation, node->annotation);
        return false;
    }

    bool visit(AstExprIfElse* node) override
    {
        open(KExprIfElse, node->location, 6);
        setNode(condition, node->condition);
        setBool(hasThen, node->hasThen);
        setNode(trueExpr, node->trueExpr);
        setBool(hasElse, node->hasElse);
        setNode(falseExpr, node->falseExpr);
        setLocal(conditionLocal, node->conditionLocal);
        return false;
    }

    bool visit(AstExprInterpString* node) override
    {
        open(KExprInterpString, node->location, 2);
        newTable(int(node->strings.size), 0);
        for (size_t i = 0; i < node->strings.size; ++i)
        {
            pushText(node->strings.data[i].data, node->strings.data[i].size);
            popAt(int(i + 1));
        }
        popInto(strings);
        setArray(expressions, node->expressions);
        return false;
    }

    bool visit(AstExprInstantiate* node) override
    {
        open(KExprInstantiate, node->location, 2);
        setNode(expr, node->expr);
        setTypeOrPacks(typeArguments, node->typeArguments);
        return false;
    }

    bool visit(AstExprError* node) override
    {
        open(KExprError, node->location, 2);
        setArray(expressions, node->expressions);
        setNumber(messageIndex, double(node->messageIndex));
        return false;
    }

    bool visit(AstStatBlock* node) override
    {
        stat(node, KStatBlock, 2);
        setArray(body, node->body);
        setBool(hasEnd, node->hasEnd);
        return false;
    }

    bool visit(AstStatIf* node) override
    {
        stat(node, KStatIf, 6);
        setNode(condition, node->condition);
        setNode(thenbody, node->thenbody);
        setNode(elsebody, node->elsebody);
        optionalSpan(thenLocation, node->thenLocation);
        optionalSpan(elseLocation, node->elseLocation);
        setLocal(conditionLocal, node->conditionLocal);
        return false;
    }

    bool visit(AstStatWhile* node) override
    {
        stat(node, KStatWhile, 3);
        setNode(condition, node->condition);
        setNode(body, node->body);
        setBool(hasDo, node->hasDo);
        return false;
    }

    bool visit(AstStatRepeat* node) override
    {
        stat(node, KStatRepeat, 2);
        setNode(condition, node->condition);
        setNode(body, node->body);
        return false;
    }

    bool visit(AstStatBreak* node) override
    {
        stat(node, KStatBreak, 0);
        return false;
    }

    bool visit(AstStatContinue* node) override
    {
        stat(node, KStatContinue, 0);
        return false;
    }

    bool visit(AstStatReturn* node) override
    {
        stat(node, KStatReturn, 1);
        setArray(list, node->list);
        return false;
    }

    bool visit(AstStatExpr* node) override
    {
        stat(node, KStatExpr, 1);
        setNode(expr, node->expr);
        return false;
    }

    bool visit(AstStatLocal* node) override
    {
        stat(node, KStatLocal, 5);
        setLocals(vars, node->vars);
        setArray(values, node->values);
        setBool(isConst, node->isConst);
        setBool(isExported, node->isExported);
        optionalSpan(equalsSignLocation, node->equalsSignLocation);
        return false;
    }

    bool visit(AstStatFor* node) override
    {
        stat(node, KStatFor, 6);
        setLocal(var, node->var);
        setNode(from, node->from);
        setNode(to, node->to);
        setNode(step, node->step);
        setNode(body, node->body);
        setBool(hasDo, node->hasDo);
        return false;
    }

    bool visit(AstStatForIn* node) override
    {
        stat(node, KStatForIn, 4);
        setLocals(vars, node->vars);
        setArray(values, node->values);
        setNode(body, node->body);
        setBool(hasDo, node->hasDo);
        return false;
    }

    bool visit(AstStatAssign* node) override
    {
        stat(node, KStatAssign, 2);
        setArray(vars, node->vars);
        setArray(values, node->values);
        return false;
    }

    bool visit(AstStatCompoundAssign* node) override
    {
        stat(node, KStatCompoundAssign, 3);
        setString(op, binaryOp(node->op));
        setNode(var, node->var);
        setNode(value, node->value);
        return false;
    }

    bool visit(AstStatFunction* node) override
    {
        stat(node, KStatFunction, 2);
        setNode(name, node->name);
        setNode(func, node->func);
        return false;
    }

    bool visit(AstStatLocalFunction* node) override
    {
        stat(node, KStatLocalFunction, 3);
        setLocal(name, node->name);
        setNode(func, node->func);
        setBool(isConst, node->isConst);
        return false;
    }

    bool visit(AstStatTypeAlias* node) override
    {
        stat(node, KStatTypeAlias, 6);
        setName(name, node->name);
        span(nameLocation, node->nameLocation);
        setArray(generics, node->generics);
        setArray(genericPacks, node->genericPacks);
        setNode(type, node->type);
        setBool(exported, node->exported);
        return false;
    }

    bool visit(AstStatTypeFunction* node) override
    {
        stat(node, KStatTypeFunction, 4);
        setName(name, node->name);
        span(nameLocation, node->nameLocation);
        setNode(body, node->body);
        setBool(exported, node->exported);
        return false;
    }

    bool visit(AstStatDeclareGlobal* node) override
    {
        stat(node, KStatDeclareGlobal, 3);
        setName(name, node->name);
        span(nameLocation, node->nameLocation);
        setNode(type, node->type);
        return false;
    }

    bool visit(AstStatDeclareFunction* node) override
    {
        stat(node, KStatDeclareFunction, 9);
        setArray(attributes, node->attributes);
        setName(name, node->name);
        span(nameLocation, node->nameLocation);
        setArray(generics, node->generics);
        setArray(genericPacks, node->genericPacks);
        typeList(params, node->params);
        newTable(int(node->paramNames.size), 0);
        for (size_t i = 0; i < node->paramNames.size; ++i)
        {
            open(KArgumentName, node->paramNames.data[i].second, 1);
            setName(name, node->paramNames.data[i].first);
            popAt(int(i + 1));
        }
        popInto(paramNames);
        setBool(vararg, node->vararg);
        setNode(retTypes, node->retTypes);
        return false;
    }

    bool visit(AstStatDeclareExternType* node) override
    {
        stat(node, KStatDeclareExternType, 4);
        setName(name, node->name);
        if (node->superName)
            setName(superName, *node->superName);
        newTable(int(node->props.size), 0);
        for (size_t i = 0; i < node->props.size; ++i)
        {
            const AstDeclaredExternTypeProperty& prop = node->props.data[i];
            open(KDeclaredProp, prop.location, 5);
            setName(name, prop.name);
            span(nameLocation, prop.nameLocation);
            setNode(type, prop.ty);
            setBool(isMethod, prop.isMethod);
            setString(Str::access, access(prop.access));
            popAt(int(i + 1));
        }
        popInto(props);
        tableIndexer(node->indexer);
        return false;
    }

    // User-defined classes are behind a debug flag; only their outline is built.
    bool visit(AstStatClass* node) override
    {
        stat(node, KStatClass, 4);
        setLocal(name, node->name);
        setNode(super, node->super);
        setBool(exported, node->exported);
        setBool(Str::open, node->open);
        return false;
    }

    bool visit(AstStatError* node) override
    {
        stat(node, KStatError, 3);
        setArray(expressions, node->expressions);
        setArray(statements, node->statements);
        setNumber(messageIndex, double(node->messageIndex));
        return false;
    }

    bool visit(AstTypeReference* node) override
    {
        open(KTypeReference, node->location, 5);
        if (node->prefix)
            setName(prefix, *node->prefix);
        setName(name, node->name);
        span(nameLocation, node->nameLocation);
        setBool(hasParameterList, node->hasParameterList);
        setTypeOrPacks(parameters, node->parameters);
        return false;
    }

    bool visit(AstTypeTable* node) override
    {
        open(KTypeTable, node->location, 3);
        newTable(int(node->props.size), 0);
        for (size_t i = 0; i < node->props.size; ++i)
        {
            const AstTableProp& prop = node->props.data[i];
            open(KTableProp, prop.location, 3);
            setName(name, prop.name);
            setNode(type, prop.type);
            setString(Str::access, access(prop.access));
            popAt(int(i + 1));
        }
        popInto(props);
        tableIndexer(node->indexer);
        setBool(isExact, node->isExact);
        return false;
    }

    bool visit(AstTypeFunction* node) override
    {
        open(KTypeFunction, node->location, 6);
        setArray(attributes, node->attributes);
        setArray(generics, node->generics);
        setArray(genericPacks, node->genericPacks);
        typeList(argTypes, node->argTypes);
        newTable(int(node->argNames.size), 0);
        for (size_t i = 0; i < node->argNames.size; ++i)
        {
            const std::optional<AstArgumentName>& argument = node->argNames.data[i];
            if (argument)
            {
                open(KArgumentName, argument->second, 1);
                setName(name, argument->first);
            }
            else
            {
                setbvalue(L->top, false);
                L->top++;
            }
            popAt(int(i + 1));
        }
        popInto(argNames);
        setNode(returnTypes, node->returnTypes);
        return false;
    }

    bool visit(AstTypeTypeof* node) override
    {
        open(KTypeTypeof, node->location, 1);
        setNode(expr, node->expr);
        return false;
    }

    bool visit(AstTypeOptional* node) override
    {
        open(KTypeOptional, node->location, 0);
        return false;
    }

    bool visit(AstTypeUnion* node) override
    {
        open(KTypeUnion, node->location, 1);
        setArray(types, node->types);
        return false;
    }

    bool visit(AstTypeIntersection* node) override
    {
        open(KTypeIntersection, node->location, 1);
        setArray(types, node->types);
        return false;
    }

    bool visit(AstTypeSingletonBool* node) override
    {
        open(KTypeSingletonBool, node->location, 1);
        setBool(value, node->value);
        return false;
    }

    bool visit(AstTypeSingletonString* node) override
    {
        open(KTypeSingletonString, node->location, 1);
        setText(value, node->value.data, node->value.size);
        return false;
    }

    bool visit(AstTypeGroup* node) override
    {
        open(KTypeGroup, node->location, 1);
        setNode(type, node->type);
        return false;
    }

    bool visit(AstTypeError* node) override
    {
        open(KTypeError, node->location, 3);
        setArray(types, node->types);
        setBool(isMissing, node->isMissing);
        setNumber(messageIndex, double(node->messageIndex));
        return false;
    }

    bool visit(AstTypePackExplicit* node) override
    {
        open(KTypePackExplicit, node->location, 1);
        typeList(typeList_, node->typeList);
        return false;
    }

    bool visit(AstTypePackVariadic* node) override
    {
        open(KTypePackVariadic, node->location, 1);
        setNode(variadicType, node->variadicType);
        return false;
    }

    bool visit(AstTypePackGeneric* node) override
    {
        open(KTypePackGeneric, node->location, 1);
        setName(genericName, node->genericName);
        return false;
    }

    Position position(size_t at) const
    {
        at = std::min(at, length);
        auto it = std::upper_bound(lineStarts.begin(), lineStarts.end(), at);
        size_t index = size_t(it - lineStarts.begin() - 1);
        return Position(unsigned(index), unsigned(at - lineStarts[index]));
    }
    Location location(SurfaceRange range) const { return Location(position(range.begin), position(range.end)); }

    void surfaceExpr(AstExpr* expr, SurfaceRange range)
    {
        const Location expected = location(range);
        if (!range.empty() && expr && expr->location == expected)
        {
            node(expr);
            return;
        }
        open(KExprError, location(range), 3);
        const bool child = expr && !range.empty();
        newTable(child ? 1 : 0, 0);
        if (child)
        {
            node(expr);
            popAt(1);
        }
        popInto(expressions);
        setBool(isMissing, range.empty());
        // Error indices are stock Luau's zero-based indices. Match a structural diagnostic
        // at the insertion point when available; -1 explicitly denotes no matching message.
        int index = -1;
        if (parseErrors)
            for (size_t i = 0; i < parseErrors->size(); ++i)
            {
                const auto& loc = (*parseErrors)[i].getLocation();
                if (loc.begin >= expected.begin && loc.begin <= expected.end) { index = int(i); break; }
            }
        setNumber(messageIndex, index);
    }

    void comprehension(const SurfaceExpression& surface)
    {
        const auto& record = *surface.record;
        // The reducer is surface syntax, independent of any local/global named sum.
        // It shares the comprehension's generated site and adds no source function scope.
        // A sink is likewise surface syntax: `into(destination)` wraps the comprehension and
        // the destination is an ordinary source expression evaluated before the pipeline.
        const bool sink = !record.sinkPrefix.empty();
        if (sink)
        {
            open(KExprSink, location({record.sinkPrefix.begin, record.range.end}), 4);
            surfaceExpr(surface.sink, record.sinkDestination);
            popInto(destination);
            if (!record.sinkKind.empty())
            {
                lua_pushlstring(L, source + record.sinkKind.begin + 1, record.sinkKind.end - record.sinkKind.begin - 2);
                popInto(elementKind);
            }
            if (!record.sinkOffset.empty())
            {
                surfaceExpr(surface.sinkOffset, record.sinkOffset);
                popInto(offsetField);
            }
        }
        const bool reduction = record.reducer != L3i::Surface::Reducer::None;
        if (reduction)
        {
            open(KExprReduction, location({record.reducerPrefix.begin, record.range.end}), 2);
            switch (record.reducer)
            {
            case L3i::Surface::Reducer::Min: setString(op, SMin); break;
            case L3i::Surface::Reducer::Max: setString(op, SMax); break;
            case L3i::Surface::Reducer::Any: setString(op, SAny); break;
            case L3i::Surface::Reducer::All: setString(op, SAll); break;
            case L3i::Surface::Reducer::Sum:
            case L3i::Surface::Reducer::None: setString(op, SSum); break;
            }
        }
        open(KExprComprehension, location(record.range), 8);
        span(openLocation, location(record.open));
        if (!record.close.empty()) span(closeLocation, location(record.close));
        if (!record.arrow.empty()) span(arrowLocation, location(record.arrow));
        setBool(hasClose, !record.close.empty());
        setBool(hasArrow, !record.arrow.empty());
        bool ready = record.complete;
        if (parseErrors)
            for (const auto& error : *parseErrors)
                if (error.getLocation().begin >= position(record.range.begin) && error.getLocation().begin <= position(record.range.end))
                    ready = false;
        setBool(complete, ready);
        newTable(int(record.clauses.size()), 0);
        for (size_t i = 0; i < record.clauses.size(); ++i)
        {
            const auto& clause = record.clauses[i];
            const auto& ast = surface.clauses[i];
            bool generator = clause.kind == L3i::Surface::ClauseKind::Generator;
            open(generator ? KComprehensionGenerator : KComprehensionFilter, location(clause.range), generator ? 6 : 2);
            span(keywordLocation, location(clause.keyword));
            if (generator)
            {
                if (!clause.binding.empty()) setLocal(binding, ast.binding);
                newTable(int(ast.bindings.size()), 0);
                for (size_t binding = 0; binding < ast.bindings.size(); ++binding)
                {
                    if (ast.bindings[binding]) pushLocal(ast.bindings[binding]);
                    else lua_pushnil(L);
                    popAt(int(binding + 1));
                }
                popInto(bindings);
                if (!clause.in.empty()) span(inLocation, location(clause.in));
                setBool(hasIn, !clause.in.empty());
            }
            if (!clause.sliceSource.empty())
            {
                open(KExprSlice, location(clause.expression), 4);
                surfaceExpr(ast.sliceSource, clause.sliceSource);
                popInto(sourceField);
                surfaceExpr(ast.sliceFirst, clause.sliceFirst);
                popInto(first);
                surfaceExpr(ast.sliceLast, clause.sliceLast);
                popInto(last);
                if (!clause.sliceKind.empty())
                {
                    lua_pushlstring(L, source + clause.sliceKind.begin + 1, clause.sliceKind.end - clause.sliceKind.begin - 2);
                    popInto(elementKind);
                }
            }
            else if (!clause.zipArguments.empty())
            {
                open(KExprZip, location(clause.expression), 2);
                setBool(strict, clause.zipStrict);
                newTable(int(clause.zipArguments.size()), 0);
                for (size_t argument = 0; argument < clause.zipArguments.size(); ++argument)
                {
                    surfaceExpr(ast.zipArguments[argument], clause.zipArguments[argument]);
                    popAt(int(argument + 1));
                }
                popInto(args);
            }
            else if (!clause.enumerateArgument.empty())
            {
                open(KExprEnumerate, location(clause.expression), 1);
                surfaceExpr(ast.expression, clause.enumerateArgument);
                popInto(expr);
            }
            else if (clause.rangeArguments.empty())
                surfaceExpr(ast.expression, clause.expression);
            else
            {
                open(KExprRange, location(clause.expression), 1);
                newTable(int(clause.rangeArguments.size()), 0);
                for (size_t argument = 0; argument < clause.rangeArguments.size(); ++argument)
                {
                    surfaceExpr(ast.rangeArguments[argument], clause.rangeArguments[argument]);
                    popAt(int(argument + 1));
                }
                popInto(args);
            }
            popInto(generator ? sourceField : condition);
            popAt(int(i + 1));
        }
        popInto(clauses);
        surfaceExpr(surface.projection, record.projection);
        popInto(projection);
        if (reduction || sink) popInto(expr);
    }

    // Always lex the original input for surface trivia, including comments the emitter elides.
    void originalTrivia(AstNameTable& names)
    {
        std::vector<Comment> comments;
        std::vector<HotComment> hot;
        Lexer lexer(source, length, names);
        bool header = true;
        for (;;)
        {
            const auto& token = lexer.next(false, true);
            if (token.type == Lexeme::Eof) break;
            if (token.type == Lexeme::Comment || token.type == Lexeme::BlockComment || token.type == Lexeme::BrokenComment)
            {
                comments.push_back({token.type, token.location});
                if (token.type == Lexeme::Comment && token.getLength() && token.data[0] == '!')
                {
                    unsigned end = token.getLength();
                    while (end > 1 && std::isspace(static_cast<unsigned char>(token.data[end - 1]))) --end;
                    hot.push_back({header, token.location, std::string(token.data + 1, token.data + end)});
                }
            }
            else header = false;
        }
        this->comments(comments);
        hotComments(hot);
    }

    // Comments, hot comments and errors, each an array field of the result on top.
    void comments(const std::vector<Comment>& list)
    {
        newTable(int(list.size()), 0);
        for (size_t i = 0; i < list.size(); ++i)
        {
            const Comment& comment = list[i];
            newTable(0, kNodeFields);
            setString(Str::kind,
                comment.type == Lexeme::BlockComment ? SBlockComment : comment.type == Lexeme::Comment ? SLineComment : SBrokenComment);
            spanFields(comment.location);
            popAt(int(i + 1));
        }
        popInto(Str::comments);
    }

    void hotComments(const std::vector<HotComment>& list)
    {
        newTable(int(list.size()), 0);
        for (size_t i = 0; i < list.size(); ++i)
        {
            const HotComment& comment = list[i];
            open(KHotComment, comment.location, 2);
            setBool(header, comment.header);
            setText(content, comment.content.data(), comment.content.size());
            popAt(int(i + 1));
        }
        popInto(Str::hotComments);
    }

    void errors(const std::vector<ParseError>& list)
    {
        newTable(int(list.size()), 0);
        for (size_t i = 0; i < list.size(); ++i)
        {
            const ParseError& error = list[i];
            open(KError, error.getLocation(), 1);
            setText(message, error.getMessage().data(), error.getMessage().size());
            popAt(int(i + 1));
        }
        popInto(Str::errors);
    }

    void starts()
    {
        newTable(int(lineStarts.size()), 0);
        LuaTable* table = hvalue(L->top - 1);
        for (size_t i = 0; i < lineStarts.size(); ++i)
            setnvalue(luaH_setnum(L, table, int(i + 1)), double(lineStarts[i]) + 1);
        popInto(lineStarts_);
    }

    static uint32_t tokenKind(Lexeme::Type type)
    {
        if (type >= Lexeme::Reserved_BEGIN && type < Lexeme::Reserved_END)
            return TokenKeyword;
        switch (type)
        {
        case Lexeme::Name:
            return TokenName;
        case Lexeme::Number:
            return TokenNumber;
        case Lexeme::QuotedString:
            return TokenString;
        case Lexeme::RawString:
            return TokenLongString;
        case Lexeme::InterpStringBegin:
            return TokenInterpBegin;
        case Lexeme::InterpStringMid:
            return TokenInterpMid;
        case Lexeme::InterpStringEnd:
            return TokenInterpEnd;
        case Lexeme::InterpStringSimple:
            return TokenInterpSimple;
        case Lexeme::Comment:
            return TokenComment;
        case Lexeme::BlockComment:
            return TokenBlockComment;
        case Lexeme::Attribute:
        case Lexeme::AttributeOpen:
            return TokenAttribute;
        case Lexeme::BrokenString:
        case Lexeme::BrokenComment:
        case Lexeme::BrokenUnicode:
        case Lexeme::BrokenInterpDoubleBrace:
        case Lexeme::Error:
            return TokenError;
        default:
            return TokenSymbol;
        }
    }

    // Luau's lexer over the whole source, comments included, as 12-byte records: the kind, then
    // the 1-based indices of the token's first and last bytes (`string.sub(source, first, last)`).
    void tokens(AstNameTable& names, bool surfaceMode)
    {
        std::vector<uint32_t> records;
        records.reserve(length / 2);
        Lexer lexer(source, length, names);
        for (;;)
        {
            const Lexeme& lexeme = lexer.next(/* skipComments= */ false, /* updatePrevLocation= */ true);
            if (lexeme.type == Lexeme::Eof)
                break;
            size_t first = offset(lexeme.location.begin);
            size_t end = offset(lexeme.location.end);
            // Merge only consecutive lexer tokens, never text inside strings or comments.
            if (surfaceMode && lexeme.type == '>' && records.size() >= 3 && first > 0 && source[first - 1] == '=' &&
                records[records.size() - 3] == TokenSymbol && records[records.size() - 2] == first &&
                records.back() == first)
            {
                records.back() = uint32_t(end);
                continue;
            }
            records.push_back(tokenKind(lexeme.type));
            records.push_back(uint32_t(first + 1));
            records.push_back(uint32_t(end));
        }
        void* data = lua_newbuffer(L, records.size() * sizeof(uint32_t));
        if (!records.empty())
            memcpy(data, records.data(), records.size() * sizeof(uint32_t));
        popInto(tokens_);
    }

    SurfaceExpressions surfaces;
    SurfaceSlices slices;
    const std::vector<ParseError>* parseErrors = nullptr;

    lua_State* L;
    const char* source;
    size_t length;
    int base = 0;
    TString* keys[StrCount] = {};
    LuaTable* locals = nullptr;
    std::vector<uint32_t> lineStarts;
    std::unordered_map<AstLocal*, int> localSlots;

    // Field names that collide with the builder's own methods.
    static constexpr Str typeList_ = Str::typeList;
    static constexpr Str lineStarts_ = Str::lineStarts;
    static constexpr Str tokens_ = Str::tokens;
};

} // namespace

extern "C" {

// Flags for l3i_luau_parse.
enum
{
    L3I_PARSE_DECLARATIONS = 1,
    L3I_PARSE_TOKENS = 2,
    L3I_PARSE_LUAU = 4,
};

// Parses `source` and pushes the result table. Raises (a Luau error, through C++ exceptions) only
// when the VM cannot allocate; a parse error is data in the result, never a raise.
int l3i_luau_parse(lua_State* L, const char* source, size_t length, int flags)
{
    // Strings, the locals table, the result, and the walk's first levels.
    if (!lua_checkstack(L, StrCount + 16))
        luaL_error(L, "luau.parse: out of stack space");

    // The parse happens before any Luau value is made: a parser failure that is not a parse
    // error (an internal assertion surfaced as an exception) is turned into a Luau error with
    // nothing half-built on the stack.
    Allocator allocator;
    AstNameTable names(allocator);
    ParseOptions options;
    options.captureComments = true;
    options.allowDeclarationSyntax = (flags & L3I_PARSE_DECLARATIONS) != 0;
    ParseResult result;
    L3i::Surface::LoweredSource lowered;
    SurfaceExpressions surfaces;
    SurfaceSlices sliceSurfaces;
    bool surfaceMode = (flags & L3I_PARSE_LUAU) == 0;
    std::string failure;
    try
    {
        if (surfaceMode)
        {
            lowered = L3i::Surface::lower(std::string_view(source, length), true, false);
            result = Parser::parse(lowered.source.data(), lowered.source.size(), names, allocator, options);
            ensureRoot(result, lowered.source, allocator);
            SiteIndex index(lowered.source);
            if (result.root) result.root->visit(&index);
            for (const auto& site : lowered.sites)
            {
                if (site.comprehension >= lowered.document.comprehensions.size()) continue;
                auto* expr = index.expression(site.call);
                auto* call = expr ? expr->as<AstExprCall>() : nullptr;
                if (!call) continue;
                SurfaceExpression surface;
                surface.record = &lowered.document.comprehensions[site.comprehension];
                surface.clauses.resize(surface.record->clauses.size());
                for (size_t i = 0; i < surface.clauses.size() && i < site.clauses.size(); ++i)
                {
                    surface.clauses[i].binding = index.binding(site.clauses[i].binding);
                    surface.clauses[i].expression = index.expression(site.clauses[i].expression);
                    for (const auto& binding : site.clauses[i].bindings)
                        surface.clauses[i].bindings.push_back(index.binding(binding));
                    for (const auto& argument : site.clauses[i].rangeArguments)
                        surface.clauses[i].rangeArguments.push_back(index.expression(argument));
                    for (const auto& argument : site.clauses[i].zipArguments)
                        surface.clauses[i].zipArguments.push_back(index.expression(argument));
                    surface.clauses[i].zipStrict = lowered.document.comprehensions[site.comprehension].clauses[i].zipStrict;
                    surface.clauses[i].sliceSource = index.expression(site.clauses[i].sliceSource);
                    surface.clauses[i].sliceFirst = index.expression(site.clauses[i].sliceFirst);
                    surface.clauses[i].sliceLast = index.expression(site.clauses[i].sliceLast);
                }
                surface.projection = index.expression(site.projection);
                surface.sink = index.expression(site.sink);
                surface.sinkOffset = index.expression(site.sinkOffset);
                surfaces.emplace(call, std::move(surface));
            }
            for (const auto& site : lowered.sliceSites)
            {
                if (site.slice >= lowered.document.slices.size()) continue;
                auto* expr = index.expression(site.call);
                auto* call = expr ? expr->as<AstExprCall>() : nullptr;
                if (!call) continue;
                SurfaceSlice surface;
                surface.record = &lowered.document.slices[site.slice];
                surface.source = index.expression(site.source);
                surface.first = index.expression(site.first);
                surface.last = index.expression(site.last);
                sliceSurfaces.emplace(call, surface);
            }
            if (!surfaces.empty() || !sliceSurfaces.empty())
            {
                SourceMetadata metadata(surfaces, sliceSurfaces);
                metadata.walk(result.root);
            }
            if (result.root && lowered.preludeStatements <= result.root->body.size)
            {
                result.root->body.data += lowered.preludeStatements;
                result.root->body.size -= lowered.preludeStatements;
            }
            L3i::Surface::remapLocations(result, lowered.map);
        }
        else
        {
            result = Parser::parse(source, length, names, allocator, options);
            ensureRoot(result, std::string_view(source, length), allocator);
        }
    }
    catch (const std::exception& error)
    {
        failure = error.what();
    }
    if (!failure.empty())
        luaL_error(L, "luau.parse: %s", failure.c_str());

    Builder builder(L, source, length);
    builder.surfaces = std::move(surfaces);
    builder.slices = std::move(sliceSurfaces);
    if (surfaceMode)
        for (const auto& error : lowered.document.errors)
            result.errors.emplace_back(builder.location(error.range), error.message);
    builder.parseErrors = &result.errors;
    builder.begin();
    lua_createtable(L, 0, 8);
    builder.setNode(root, result.root);
    builder.errors(result.errors);
    if (surfaceMode && (!lowered.document.comprehensions.empty() || !lowered.document.slices.empty())) builder.originalTrivia(names);
    else
    {
        builder.comments(result.commentLocations);
        builder.hotComments(result.hotcomments);
    }
    builder.starts();
    if (flags & L3I_PARSE_TOKENS)
        builder.tokens(names, surfaceMode);

    // Leave only the result: drop the strings and the locals table below it.
    lua_replace(L, builder.base);
    lua_settop(L, builder.base);
    return 1;
}

} // extern "C"
