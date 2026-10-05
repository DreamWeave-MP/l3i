#include "surface_frontend.h"
#include "Luau/Allocator.h"
#include "Luau/Lexer.h"
#include "Luau/Parser.h"
#include <algorithm>
#include <utility>

namespace L3i::Surface
{
namespace
{
using T = Luau::Lexeme;
// Per-document budgets prevent repeated failed prefixes from quadratic work.
constexpr size_t maxTokens = 262144, maxDepth = 128;
constexpr size_t maxPrefixBytes = 8388608, maxPrefixCalls = 4096;
struct Token { T::Type type; Range range; };
class Frontend
{
public:
    explicit Frontend(std::string_view source): source(source) {}
    Document run()
    {
        // Negative-only shortcut: substring hits are never treated as syntax.
        if (source.find('[') == std::string_view::npos || source.find("for") == std::string_view::npos)
            return std::move(document);
        std::vector<size_t> lines{0};
        for (size_t i = 0; i < source.size(); ++i)
            if (source[i] == '\n') lines.push_back(i + 1);
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::Lexer lexer(source.data(), source.size(), names);
        lexer.setSkipComments(false);
        bool surfaceSeen = false;
        Token previous{T::Eof, {0, 0}};
        Token beforePrevious{T::Eof, {0, 0}};
        Token beforeBeforePrevious{T::Eof, {0, 0}};
        for (;;)
        {
            const auto& token = lexer.next();
            const auto offset = [&](Luau::Position p) { return lines[p.line] + p.column; };
            Token current{token.type, {offset(token.location.begin), offset(token.location.end)}};
            if (current.type == T::Comment || current.type == T::BlockComment || current.type == T::BrokenComment)
            {
                document.comments.push_back(current.range);
                // Trivia doesn't interrupt [ ... for detection or consume the
                // extension token budget, before or after the first opener.
                continue;
            }
            if (!surfaceSeen)
            {
                if (previous.type == '[' && current.type == T::ReservedFor)
                {
                    surfaceSeen = true;
                    // The first opener is token zero; preserve its real consumer
                    // predecessor without charging it to the extension budget.
                    if (beforePrevious.type == '#') firstLengthPrefix = beforePrevious.range;
                    if (isSumPrefix(beforePrevious, beforeBeforePrevious.type)) firstSumPrefix = beforePrevious.range;
                    firstPostfix = firstLengthPrefix.empty() && firstSumPrefix.empty() &&
                        isPostfix(beforePrevious, beforeBeforePrevious);
                    tokens.push_back(previous);
                }
                else
                {
                    // Plain source has no surface token budget, and doesn't
                    // need retained tokens. Literal/comment hits stay inert.
                    if (current.type == T::Eof) return std::move(document);
                    beforeBeforePrevious = beforePrevious;
                    beforePrevious = previous;
                    previous = current;
                    continue;
                }
            }
            tokens.push_back(current);
            if (token.type == T::Eof) break;
            if (tokens.size() >= maxTokens)
            {
                error(tokens.back().range, "surface token limit exceeded (262144 tokens from first comprehension)");
                tokens.push_back({T::Eof, {tokens.back().range.end, tokens.back().range.end}});
                break;
            }
        }
        prepareConditionalShapes();
        size_t i = 0;
        while (!stopped && type(i) != T::Eof)
            if (isComprehension(i)) comprehension(i, 0);
            else ++i;
        return std::move(document);
    }
private:
    std::string_view source;
    std::vector<Token> tokens;
    Document document;
    Range firstLengthPrefix;
    Range firstSumPrefix;
    bool firstPostfix = false;
    std::vector<unsigned char> conditionalShapes;
    size_t prefixBytes = 0, prefixCalls = 0;
    bool stopped = false;
    T::Type type(size_t i) const { return tokens[std::min(i, tokens.size() - 1)].type; }
    Range point(size_t i) const { return {tokens[i].range.begin, tokens[i].range.begin}; }
    void error(Range r, std::string message) { document.errors.push_back({r, std::move(message)}); }
    bool isComprehension(size_t i) const { return type(i) == '[' && type(i + 1) == T::ReservedFor; }
    bool isSumPrefix(const Token& token, T::Type preceding) const
    {
        return token.type == T::Name && preceding != '.' && preceding != ':' &&
            source.substr(token.range.begin, token.range.end - token.range.begin) == "sum";
    }
    bool arrow(size_t i) const
    {
        return type(i) == '=' && type(i + 1) == '>' && tokens[i].range.end == tokens[i + 1].range.begin;
    }
    bool limit(size_t i, size_t depth)
    {
        if (depth < maxDepth) return false;
        error(point(i), "surface nesting limit exceeded (128)");
        stopped = true;
        return true;
    }
    bool isPostfix(const Token& previous, const Token& before) const
    {
        const int t = previous.type;
        if (t == '>') return before.type == '>' && before.range.end == previous.range.begin;
        return t == T::Name || t == T::Number || t == T::QuotedString || t == T::RawString ||
            t == T::InterpStringSimple || t == T::InterpStringEnd || t == T::ReservedNil ||
            t == T::ReservedTrue || t == T::ReservedFalse || t == ')' || t == ']' || t == '}';
    }
    // Linear shape hints, not an expression parser. A failed prefix may still
    // precede an ordinary conditional; then/else at the same delimiter level
    // distinguish it from a damaged expression followed by a filter. Nested
    // delimiters/comprehensions have separate scopes, so their keywords cannot
    // lend a conditional shape to an enclosing filter.
    void prepareConditionalShapes()
    {
        struct Pending { size_t index; bool thenSeen; };
        struct Scope { int closer; std::vector<Pending> pending; std::vector<size_t> awaitingThen; };
        std::vector<Scope> scopes(1);
        conditionalShapes.resize(tokens.size());
        for (size_t i = 0; i < tokens.size(); ++i)
        {
            const int t = type(i);
            int close = 0;
            if (t == '(') close = ')';
            if (t == '[') close = ']';
            if (t == '{') close = '}';
            if (t == T::InterpStringBegin) close = T::InterpStringEnd;
            if (close) scopes.push_back({close, {}, {}});
            else if (scopes.size() > 1 && t == scopes.back().closer) scopes.pop_back();
            else if (arrow(i) || t == ';' || t == T::Eof || t == ')' || t == ']' || t == '}' ||
                t == T::InterpStringMid || t == T::InterpStringEnd)
            {
                scopes.back().pending.clear();
                scopes.back().awaitingThen.clear();
            }
            else if (t == T::ReservedIf)
            {
                scopes.back().awaitingThen.push_back(scopes.back().pending.size());
                scopes.back().pending.push_back({i, false});
            }
            else if (t == T::ReservedThen && !scopes.back().awaitingThen.empty())
            {
                scopes.back().pending[scopes.back().awaitingThen.back()].thenSeen = true;
                scopes.back().awaitingThen.pop_back();
            }
            else if (t == T::ReservedElse)
            {
                // A conservative hint is enough; Luau still owns validity.
                // Mark enclosing then-arms too, including an outer conditional
                // whose branch contains a nested conditional/function literal.
                while (!scopes.back().pending.empty() && scopes.back().pending.back().thenSeen)
                {
                    conditionalShapes[scopes.back().pending.back().index] = 1;
                    scopes.back().pending.pop_back();
                }
            }
        }
    }

    bool validPrefix(Range r)
    {
        if (r.empty()) return false;
        if (++prefixCalls > maxPrefixCalls || r.end - r.begin > maxPrefixBytes - prefixBytes)
        {
            error({r.end, r.end}, "surface expression prefix work limit exceeded (4096 calls / 8388608 bytes)");
            stopped = true;
            return false;
        }
        prefixBytes += r.end - r.begin;
        std::string text(source.substr(r.begin, r.end - r.begin));
        // Opaque placeholders ONLY for stock validation, never returned source.
        auto it = std::lower_bound(document.comprehensions.begin(), document.comprehensions.end(), r.begin,
            [](const Comprehension& c, size_t offset) { return c.open.begin < offset; });
        size_t covered = r.begin;
        for (; it != document.comprehensions.end() && it->open.begin < r.end; ++it)
        {
            if (it->range.begin < covered || it->range.end > r.end || it->range.end - it->range.begin < 3) continue;
            // The JSL sum consumer has no stock-Luau expression spelling. Hide
            // it together with its comprehension ONLY in validation; otherwise
            // `sum <trivia> nil` would reject a valid enclosing clause prefix.
            const size_t originalBegin = !it->sumPrefix.empty() && it->sumPrefix.begin >= r.begin
                ? it->sumPrefix.begin : it->range.begin;
            const size_t begin = originalBegin - r.begin, end = it->range.end - r.begin;
            for (size_t j = begin; j < end; ++j)
                if (text[j] != '\n' && text[j] != '\r') text[j] = ' ';
            text.replace(begin, 3, "nil");
            covered = it->range.end;
        }
        while (!text.empty() && Luau::isSpace(text.back())) text.pop_back();
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        const auto parsed = Luau::Parser::parseExpr(text.data(), text.size(), names, allocator);
        if (!parsed.root || !parsed.errors.empty()) return false;
        // parseExpr's EOF check advances once; requiring the AST to reach the
        // final token also rejects a single unconsumed trailing token.
        Luau::Position end{0, 0};
        for (char ch : text)
            if (ch == '\n') { ++end.line; end.column = 0; }
            else ++end.column;
        return parsed.root->location.end == end;
    }
    // Delimiters shield nested syntax; Luau owns expression grammar. Prefix
    // Checks occur at potential separators, statement recovery points, and
    // function-end candidates (which establish where separator shielding ends).
    Range expression(size_t& i, size_t depth, bool projection, std::string& suffix)
    {
        const size_t start = i;
        suffix.clear();
        size_t end = point(i).begin;
        std::vector<int> closers;
        bool functionSeen = false;
        size_t functionStart = i;
        while (!stopped && type(i) != T::Eof)
        {
            const int t = type(i);
            if (isComprehension(i))
            {
                comprehension(i, depth + closers.size() + 1);
                end = i > start ? tokens[i - 1].range.end : end;
                continue;
            }
            // No ordinary expression can own =>, even with unmatched opening
            // delimiters. Genuine nested comprehensions consumed theirs above;
            // strings/comments are opaque lexer tokens, not character scans.
            if (arrow(i)) break;
            // Statement fences cannot belong to an ordinary parenthesized
            // expression. Only function bodies (and table-field semicolons)
            // own these tokens; an unmatched '(' must not swallow the file.
            if (!functionSeen && (t == T::ReservedLocal || t == T::ReservedReturn ||
                (t == ';' && std::find(closers.begin(), closers.end(), '}') == closers.end()))) break;
            const bool separator = !projection && (t == T::ReservedFor || t == T::ReservedIf);
            const bool sync = t == T::ReservedLocal || t == T::ReservedReturn || t == ';' || t == ',';
            if (closers.empty())
            {
                if (t == ']' || t == ')' || t == '}' || t == T::InterpStringMid || t == T::InterpStringEnd) break;

                // Unlike 'if', 'for' cannot start an ordinary expression. Even
                // a malformed prefix (xs.) must not hide the next generator.
                // Function bodies remain protected by stock prefix validation.
                if (!projection && t == T::ReservedFor && !functionSeen) break;
                if (separator && ((i == start && t != T::ReservedIf) ||
                    (i != start && validPrefix({tokens[start].range.begin, end})))) break;
                if (!projection && t == T::ReservedIf && i != start && !functionSeen &&
                    !conditionalShapes[i]) break;
                // A complete prefix proves local/return/semicolon/comma is outside
                // a function literal; its body isn't a comprehension clause.
                if (sync && (!functionSeen || validPrefix({tokens[start].range.begin, end}))) break;
            }
            if (stopped) break;
            if (t == T::ReservedFunction && !functionSeen)
            {
                functionSeen = true;
                functionStart = i;
            }
            int close = 0;
            if (t == '(') close = ')';
            if (t == '[') close = ']';
            if (t == '{') close = '}';
            if (t == T::InterpStringBegin) close = T::InterpStringEnd;
            if (close)
            {
                if (limit(i, depth + closers.size() + 1)) break;
                closers.push_back(close);
            }
            else if (t == ')' || t == ']' || t == '}' || t == T::InterpStringEnd)
            {
                if (closers.empty() || closers.back() != t) break;
                closers.pop_back();
            }
            end = tokens[i++].range.end;
            // Validate the literal itself, not the whole surrounding prefix:
            // malformed syntax AFTER a completed literal must not continue
            // shielding a subsequent generator or parent comma. Inner block
            // and nested-function ends fail this check until the outer end.
            if (functionSeen && t == T::ReservedEnd &&
                validPrefix({tokens[functionStart].range.begin, end})) functionSeen = false;
        }
        for (auto closer = closers.rbegin(); closer != closers.rend(); ++closer)
        {
            if (*closer == ')' || *closer == ']' || *closer == '}')
            {
                suffix += char(*closer);
                error(point(i), std::string("expected '") + char(*closer) + "' to close expression delimiter");
            }
            else error(point(i), "expected end of interpolated string");
        }
        return i == start ? point(i) : Range{tokens[start].range.begin, end};
    }
    void recognizeRange(size_t begin, size_t end, Clause& clause)
    {
        if (end < begin + 5 || type(begin) != T::Name || type(begin + 1) != '(' || type(end - 1) != ')' ||
            source.substr(tokens[begin].range.begin, tokens[begin].range.end - tokens[begin].range.begin) != "range")
            return;
        std::vector<Range> arguments;
        size_t start = begin + 2;
        int depth = 1;
        for (size_t at = start; at + 1 < end; ++at)
        {
            const int token = type(at);
            if (token == '(' || token == '[' || token == '{') ++depth;
            else if (token == ')' || token == ']' || token == '}') --depth;
            if (token == ',' && depth == 1)
            {
                if (at == start) return;
                arguments.push_back({tokens[start].range.begin, tokens[at - 1].range.end});
                start = at + 1;
            }
        }
        if (start >= end - 1) return;
        arguments.push_back({tokens[start].range.begin, tokens[end - 2].range.end});
        if (arguments.size() == 2 || arguments.size() == 3)
            clause.rangeArguments = std::move(arguments);
        else
            error(clause.expression, "JSL range generator expects two or three arguments");
    }
    void recognizeEnumerate(size_t begin, size_t end, Clause& clause)
    {
        if (end < begin + 4 || type(begin) != T::Name || type(begin + 1) != '(' || type(end - 1) != ')' ||
            source.substr(tokens[begin].range.begin, tokens[begin].range.end - tokens[begin].range.begin) != "enumerate")
            return;
        int depth = 1;
        for (size_t at = begin + 2; at + 1 < end; ++at)
        {
            const int token = type(at);
            if (token == '(' || token == '[' || token == '{') ++depth;
            else if (token == ')' || token == ']' || token == '}') --depth;
            if (token == ',' && depth == 1)
            {
                error(clause.expression, "JSL enumerate generator expects one argument");
                return;
            }
        }
        if (begin + 2 == end - 1)
            error(clause.expression, "JSL enumerate generator expects one argument");
        else
            clause.enumerateArgument = {tokens[begin + 2].range.begin, tokens[end - 2].range.end};
    }
    void recognizeZip(size_t begin, size_t end, Clause& clause)
    {
        if (end < begin + 4 || type(begin) != T::Name || type(begin + 1) != '(' || type(end - 1) != ')')
            return;
        const auto name = source.substr(tokens[begin].range.begin, tokens[begin].range.end - tokens[begin].range.begin);
        if (name != "zipShortest" && name != "zipStrict") return;
        std::vector<Range> arguments;
        size_t start = begin + 2;
        int depth = 1;
        for (size_t at = start; at + 1 < end; ++at)
        {
            const int token = type(at);
            if (token == '(' || token == '[' || token == '{') ++depth;
            else if (token == ')' || token == ']' || token == '}') --depth;
            if (token == ',' && depth == 1)
            {
                if (at == start) return;
                arguments.push_back({tokens[start].range.begin, tokens[at - 1].range.end});
                start = at + 1;
            }
        }
        if (start >= end - 1) return;
        arguments.push_back({tokens[start].range.begin, tokens[end - 2].range.end});
        if (arguments.size() < 2)
        {
            error(clause.expression, "JSL zip generator expects at least two arguments");
            return;
        }
        clause.zipArguments = std::move(arguments);
        clause.zipStrict = name == "zipStrict";
    }
    void comprehension(size_t& i, size_t depth)
    {
        if (limit(i, depth)) return;
        const size_t index = document.comprehensions.size();
        document.comprehensions.emplace_back(); // Parent before children.
        Comprehension c;
        c.lengthPrefix = point(i);
        if (i == 0 && !firstLengthPrefix.empty()) c.lengthPrefix = firstLengthPrefix;
        else if (i > 0 && type(i - 1) == '#') c.lengthPrefix = tokens[i - 1].range;
        c.sumPrefix = point(i);
        if (i == 0 && !firstSumPrefix.empty()) c.sumPrefix = firstSumPrefix;
        else if (i > 0 && isSumPrefix(tokens[i - 1], i > 1 ? type(i - 2) : T::Eof)) c.sumPrefix = tokens[i - 1].range;
        c.postfix = c.lengthPrefix.empty() && c.sumPrefix.empty() &&
            (i == 0 ? firstPostfix : isPostfix(tokens[i - 1], i > 1 ? tokens[i - 2] : Token{T::Eof, {0, 0}}));
        c.open = tokens[i++].range;
        // Set the opening immediately so binary search remains ordered even
        // while an ancestor record is still being constructed.
        document.comprehensions[index].open = c.open;
        const size_t errorsBefore = document.errors.size();
        if (c.postfix) error(c.open, "invalid postfix comprehension; use an explicit parenthesized collection index, e.g. values[([for ...])]");
        while (!stopped && (type(i) == T::ReservedFor || type(i) == T::ReservedIf))
        {
            Clause clause;
            clause.kind = type(i) == T::ReservedFor ? ClauseKind::Generator : ClauseKind::Filter;
            clause.keyword = tokens[i++].range;
            clause.binding = clause.in = point(i);
            if (clause.kind == ClauseKind::Generator)
            {
                if (type(i) == T::Name)
                {
                    clause.bindings.push_back(tokens[i++].range);
                    while (type(i) == ',')
                    {
                        ++i;
                        if (type(i) != T::Name)
                        {
                            error(point(i), "expected generator binding name after ','");
                            break;
                        }
                        clause.bindings.push_back(tokens[i++].range);
                    }
                    clause.binding = clause.bindings.front();
                }
                else error(point(i), "expected generator binding name after 'for'");
                clause.in = point(i);
                if (type(i) == T::ReservedIn) clause.in = tokens[i++].range;
                else error(point(i), "expected 'in' after generator binding");
            }
            const size_t expressionBegin = i;
            clause.expression = expression(i, depth, false, clause.expressionSuffix);
            if (clause.kind == ClauseKind::Generator && clause.expressionSuffix.empty())
            {
                recognizeRange(expressionBegin, i, clause);
                recognizeEnumerate(expressionBegin, i, clause);
                recognizeZip(expressionBegin, i, clause);
                if (!clause.enumerateArgument.empty() && clause.bindings.size() != 2)
                    error(clause.binding, "JSL enumerate generator requires two bindings");
                else if (!clause.zipArguments.empty() && clause.bindings.size() != clause.zipArguments.size())
                    error(clause.binding, "JSL zip generator binding count must match its arguments");
                else if (clause.enumerateArgument.empty() && clause.zipArguments.empty() && clause.bindings.size() > 1)
                    error(clause.binding, "multiple generator bindings require a recognized multi-value source");
            }
            if (clause.expression.empty())
                error(clause.expression, clause.kind == ClauseKind::Generator ? "expected generator expression" : "expected filter expression");
            clause.range = {clause.keyword.begin, clause.expression.end};
            c.clauses.push_back(clause);
        }
        c.arrow = c.projection = point(i);
        if (!stopped && arrow(i))
        {
            c.arrow = {tokens[i].range.begin, tokens[i + 1].range.end};
            i += 2;
            c.projection = expression(i, depth, true, c.projectionSuffix);
            if (c.projection.empty()) error(c.projection, "expected projection expression after '=>'");
        }
        else error(c.arrow, "expected '=>' and projection expression");
        c.close = point(i);
        if (!stopped && type(i) == ']') c.close = tokens[i++].range;
        else error(c.close, "expected ']' to close comprehension");
        c.range = {c.open.begin, c.close.end};
        c.complete = !c.postfix && !c.close.empty() && document.errors.size() == errorsBefore;
        document.comprehensions[index] = std::move(c);
    }
};
}
Document parseSurface(std::string_view source) { return Frontend(source).run(); }
}
