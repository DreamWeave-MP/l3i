#include "surface_frontend.h"
#include "Luau/Allocator.h"
#include "Luau/Lexer.h"
#include "Luau/Parser.h"
#include <algorithm>
#include <unordered_set>
#include <utility>

namespace L3i::Surface
{
namespace
{
using T = Luau::Lexeme;
// Per-document budgets prevent repeated failed prefixes from quadratic work.
constexpr size_t maxTokens = 262144, maxDepth = 128;
// How far back from `[for` an `into(...)` destination may reach, in tokens: hostile input with
// unmatched parentheses before many openers must not make every opener rescan the file.
constexpr size_t maxSinkTokens = 4096;
constexpr size_t maxPrefixBytes = 8388608, maxPrefixCalls = 4096;
struct Token { T::Type type; Range range; };
class Frontend
{
public:
    explicit Frontend(std::string_view source): source(source) {}
    Document run()
    {
        // Record patterns first: prefix validation of comprehension clauses masks them.
        if (source.find('{') != std::string_view::npos || source.find('[') != std::string_view::npos)
            scanPatterns();
        scan();
        // A `for {` that no comprehension claimed as a clause is a statement loop.
        for (const Range loop : loopPatterns)
        {
            const bool clause = std::any_of(document.comprehensions.begin(), document.comprehensions.end(), [&](const Comprehension& c) {
                return std::any_of(c.clauses.begin(), c.clauses.end(), [&](const Clause& k) { return k.keyword.begin == loop.begin; });
            });
            if (!clause)
                error(loop, "record patterns are not supported in generic for loops; destructure the loop variable in the body");
        }
        return std::move(document);
    }
private:
    void scan()
    {
        // Negative-only shortcut: substring hits are never treated as syntax.
        if (source.find('[') == std::string_view::npos)
            return;
        if (source.find("for") == std::string_view::npos)
        {
            scanSlices();
            return;
        }
        bool surfaceSeen = false;
        size_t surfaceStart = 0;
        // Code tokens and comments merged back into source order.
        const std::vector<Token>& all = codeTokens();
        for (size_t next = 0, comment = 0; next < all.size();)
        {
            const bool trivia = comment < comments.size() && comments[comment].range.begin < all[next].range.begin;
            const Token& current = trivia ? comments[comment++] : all[next++];
            if (current.type == T::Comment || current.type == T::BlockComment || current.type == T::BrokenComment)
            {
                document.comments.push_back(current.range);
                // Trivia doesn't interrupt [ ... for detection or consume the
                // extension token budget, before or after the first opener.
                continue;
            }
            // Every code token is retained so a consumer prefix of any length (`into(...)`)
            // can be read back from the first opener. The extension token budget is charged
            // from that opener; plain source before it is unbudgeted.
            tokens.push_back(current);
            if (!surfaceSeen)
            {
                if (tokens.size() >= 2 && tokens[tokens.size() - 2].type == '[' && current.type == T::ReservedFor)
                {
                    surfaceSeen = true;
                    surfaceStart = tokens.size() - 2;
                }
                else
                {
                    if (current.type == T::Eof)
                    {
                        scanSlices();
                        return;
                    }
                    continue;
                }
            }
            if (current.type == T::Eof) break;
            if (tokens.size() - surfaceStart >= maxTokens)
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
        scanSlices();
    }
    std::string_view source;
    std::vector<Token> tokens;
    Document document;
    // The source lexed once into its code tokens (ending with Eof) and its comments. The
    // pattern, comprehension and slice scans all read these; none lexes again.
    std::vector<Token> code, comments;
    bool lexed = false;
    const std::vector<Token>& codeTokens()
    {
        if (lexed) return code;
        lexed = true;
        std::vector<size_t> lines{0};
        for (size_t i = 0; i < source.size(); ++i)
            if (source[i] == '\n') lines.push_back(i + 1);
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::Lexer lexer(source.data(), source.size(), names);
        lexer.setSkipComments(false);
        for (;;)
        {
            const auto& token = lexer.next();
            const auto offset = [&](Luau::Position p) { return lines[p.line] + p.column; };
            const Token current{token.type, {offset(token.location.begin), offset(token.location.end)}};
            if (current.type == T::Comment || current.type == T::BlockComment || current.type == T::BrokenComment)
                comments.push_back(current);
            else
                code.push_back(current);
            if (token.type == T::Eof) break;
        }
        return code;
    }
    // `for` keywords followed by `{`: comprehension clauses, or unsupported statement loops.
    std::vector<Range> loopPatterns;

    std::vector<unsigned char> conditionalShapes;
    size_t prefixBytes = 0, prefixCalls = 0;
    bool stopped = false;
    T::Type type(size_t i) const { return tokens[std::min(i, tokens.size() - 1)].type; }
    Range point(size_t i) const { return {tokens[i].range.begin, tokens[i].range.begin}; }
    void error(Range r, std::string message) { document.errors.push_back({r, std::move(message)}); }
    void scanSlices()
    {
        const std::vector<Token>& all = codeTokens();
        for (size_t open = 0; open < all.size(); ++open)
        {
            // A comprehension is never a slice, whatever colons its clauses contain.
            if (all[open].type != '[' || open == 0 || (open + 1 < all.size() && all[open + 1].type == T::ReservedFor)) continue;
            size_t close = open + 1;
            int depth = 1;
            size_t colon = 0;
            for (; close < all.size() && depth; ++close)
            {
                if (all[close].type == '[') ++depth;
                else if (all[close].type == ']') --depth;
                else if (all[close].type == ':' && depth == 1) colon = close;
            }
            if (depth || !colon) continue;
            // An optional `, "kind"` after the last bound makes the slice typed.
            size_t comma = 0;
            int nested = 0;
            for (size_t at = colon + 1; at + 1 < close; ++at)
            {
                const int token = all[at].type;
                if (token == '(' || token == '[' || token == '{') ++nested;
                else if (token == ')' || token == ']' || token == '}') --nested;
                else if (token == ',' && nested == 0 && !comma) comma = at;
            }
            const size_t lastEnd = comma ? comma - 1 : close - 2;
            size_t start = open - 1;
            if (all[start].type == ')' || all[start].type == ']' || all[start].type == '}')
            {
                const int closing = all[start].type;
                const int wanted = closing == ')' ? '(' : closing == ']' ? '[' : '{';
                int nested = 1;
                while (start > 0 && nested)
                {
                    --start;
                    if (all[start].type == closing) ++nested;
                    else if (all[start].type == wanted) --nested;
                }
                if (nested) continue;
                if (start > 0 && all[start - 1].type == T::Name) --start;
            }
            else if (all[start].type != T::Name && all[start].type != T::Number &&
                all[start].type != T::QuotedString && all[start].type != T::RawString)
                continue;
            Slice slice;
            slice.source = {all[start].range.begin, all[open - 1].range.end};
            const bool missingFirst = colon == open + 1;
            const bool missingLast = colon == lastEnd;
            slice.first = missingFirst ? Range{all[colon].range.begin, all[colon].range.begin}
                                       : Range{all[open + 1].range.begin, all[colon - 1].range.end};
            slice.last = missingLast ? Range{all[colon].range.end, all[colon].range.end}
                                     : Range{all[colon + 1].range.begin, all[lastEnd].range.end};
            slice.range = {slice.source.begin, all[close - 1].range.end};
            bool badKind = false;
            if (comma)
            {
                if (comma + 1 == close - 2 && all[comma + 1].type == T::QuotedString && bufferKindLiteral(all[comma + 1].range))
                    slice.kind = all[comma + 1].range;
                else
                    badKind = true;
            }
            const bool generatorSlice = std::any_of(document.comprehensions.begin(), document.comprehensions.end(),
                [&](const Comprehension& comprehension) {
                    return std::any_of(comprehension.clauses.begin(), comprehension.clauses.end(), [&](const Clause& clause) {
                        return !clause.sliceSource.empty() && clause.expression.begin == slice.range.begin &&
                            clause.expression.end == slice.range.end;
                    });
                });
            if (generatorSlice) continue;
            // A colon inside an otherwise valid Luau index can be a method call,
            // e.g. values[obj:method()]. Stock syntax wins over JSL slicing.
            if (++prefixCalls > maxPrefixCalls || slice.range.end - slice.range.begin > maxPrefixBytes - prefixBytes)
            {
                error({slice.range.end, slice.range.end},
                    "surface expression prefix work limit exceeded (4096 calls / 8388608 bytes)");
                return;
            }
            prefixBytes += slice.range.end - slice.range.begin;
            Luau::Allocator expressionAllocator;
            Luau::AstNameTable expressionNames(expressionAllocator);
            const auto text = source.substr(slice.range.begin, slice.range.end - slice.range.begin);
            const auto parsed = Luau::Parser::parseExpr(text.data(), text.size(), expressionNames, expressionAllocator);
            if (parsed.root && parsed.errors.empty()) continue;
            if (missingFirst) error(slice.first, "expected slice first bound before ':'");
            if (missingLast) error(slice.last, "expected slice last bound after ':'");
            if (badKind)
                error(comma + 1 < close - 1 ? Range{all[comma + 1].range.begin, all[close - 2].range.end} : Range{all[comma].range.begin, all[comma].range.begin},
                    "slice element kind must be a string literal naming an element kind, e.g. \"f32\" or \"f32@16\"");
            slice.complete = !missingFirst && !missingLast && !badKind;
            document.slices.push_back(slice);
        }
        std::sort(document.slices.begin(), document.slices.end(), [](const Slice& a, const Slice& b) {
            return a.range.begin < b.range.begin;
        });
    }
    static T::Type typeIn(const std::vector<Token>& list, size_t i) { return list[std::min(i, list.size() - 1)].type; }
    static Range pointIn(const std::vector<Token>& list, size_t i)
    {
        const Range at = list[std::min(i, list.size() - 1)].range;
        return {at.begin, at.begin};
    }
    std::string_view text(Range range) const { return source.substr(range.begin, range.end - range.begin); }

    // A record pattern at `list[i] == '{'`, consumed through its `}`. Recovery stops at the
    // first token that cannot continue the pattern and leaves it to the enclosing construct.
    Pattern recordPattern(const std::vector<Token>& list, size_t& i, size_t depth)
    {
        Pattern pattern;
        pattern.record = true;
        pattern.open = pattern.range = list[i].range;
        pattern.close = pointIn(list, ++i);
        const size_t errorsBefore = document.errors.size();
        if (depth >= maxDepth)
        {
            // Fatal, like the comprehension nesting limit: one diagnostic, nothing further.
            error(pattern.open, "record pattern nesting limit exceeded (128)");
            stopped = true;
            return pattern;
        }
        // Past an unsupported form to the next ',' or '}' of this record, so one mistake is one
        // diagnostic; a statement keyword means the record was never closed.
        const auto resync = [&]() {
            int nested = 0;
            for (;; ++i)
            {
                const int t = typeIn(list, i);
                if (t == T::Eof || (nested == 0 && (t == ',' || t == '}')) || (keywordStop(t) && t != T::ReservedFunction && t != T::ReservedEnd))
                    return t == ',' || t == '}';
                if (t == '(' || t == '[' || t == '{') ++nested;
                else if ((t == ')' || t == ']' || t == '}') && nested > 0) --nested;
            }
        };
        for (;;)
        {
            const int t = typeIn(list, i);
            if (t == '}')
            {
                pattern.close = list[i++].range;
                pattern.range.end = pattern.close.end;
                break;
            }
            if (t != T::Name)
            {
                pattern.close = pointIn(list, i);
                if (t == T::Dot3 || t == '[')
                {
                    error(list[i].range, t == T::Dot3 ? "rest patterns are not supported; a record pattern names every field it binds"
                                                      : "computed keys are not supported in record patterns; fields are named, e.g. {name: binding}");
                    if (!resync()) break;
                    if (typeIn(list, i) == ',') pattern.range.end = list[i++].range.end;
                    continue;
                }
                if (t == ',')
                    error(list[i].range, "expected field name in record pattern");
                else
                    error(pattern.close, "expected '}' to close record pattern");
                break;
            }
            PatternField field;
            field.key = list[i++].range;
            if (typeIn(list, i) == ':')
            {
                field.colon = list[i++].range;
                const int target = typeIn(list, i);
                if (target == T::Name)
                    field.target.range = list[i++].range;
                else if (target == '{')
                {
                    field.target = recordPattern(list, i, depth + 1);
                    if (stopped)
                    {
                        pattern.fields.push_back(std::move(field));
                        return pattern;
                    }
                }
                else if (target == '[')
                {
                    field.target.range = pointIn(list, i);
                    error(list[i].range, "indexed patterns are not supported; a field binds a name or a record pattern");
                    field.range = {field.key.begin, field.colon.end};
                    pattern.fields.push_back(std::move(field));
                    if (!resync())
                    {
                        pattern.close = pointIn(list, i);
                        break;
                    }
                    if (typeIn(list, i) == ',') pattern.range.end = list[i++].range.end;
                    continue;
                }
                else
                {
                    field.target.range = pointIn(list, i);
                    error(field.target.range, "expected binding name or record pattern after ':'");
                }
            }
            else
                field.target.range = field.key;
            field.range = {field.key.begin, field.target.range.empty() ? field.colon.end : field.target.range.end};
            pattern.range.end = field.range.end;
            pattern.fields.push_back(std::move(field));
            const int next = typeIn(list, i);
            if (next == '=')
            {
                error(list[i].range, "defaults are not supported in record patterns; a missing field binds nil");
                if (!resync())
                {
                    pattern.close = pointIn(list, i);
                    break;
                }
                if (typeIn(list, i) == ',') pattern.range.end = list[i++].range.end;
                continue;
            }
            if (next == ',')
            {
                pattern.range.end = list[i++].range.end;
                continue;
            }
            if (next != '}')
            {
                pattern.close = pointIn(list, i);
                error(pattern.close, next == T::Name || next == '{' ? "expected ',' or '}' in record pattern"
                                                                    : "expected '}' to close record pattern");
                break;
            }
        }
        pattern.complete = !pattern.close.empty() && document.errors.size() == errorsBefore;
        return pattern;
    }

    // Every name one binding list introduces, in source order: plain names and record targets.
    static void patternNames(const Pattern& pattern, std::vector<Range>& names)
    {
        if (!pattern.record)
        {
            if (!pattern.range.empty()) names.push_back(pattern.range);
            return;
        }
        for (const PatternField& field : pattern.fields)
            patternNames(field.target, names);
    }

    // One binding list may not introduce a name twice, through aliases and nesting included:
    // the later declaration would silently shadow the earlier one.
    void duplicates(const std::vector<Range>& names)
    {
        std::unordered_set<std::string_view> seen;
        for (const Range name : names)
            if (!seen.insert(text(name)).second)
                error(name, "duplicate binding '" + std::string(text(name)) + "' in record pattern");
    }

    bool keywordStop(int t) const
    {
        return t == T::ReservedLocal || t == T::ReservedReturn || t == T::ReservedFunction || t == T::ReservedIf ||
            t == T::ReservedFor || t == T::ReservedWhile || t == T::ReservedRepeat || t == T::ReservedDo || t == T::ReservedEnd ||
            t == T::ReservedThen || t == T::ReservedElse || t == T::ReservedElseif || t == T::ReservedUntil ||
            t == T::ReservedBreak || t == T::ReservedIn || t == ';' || t == T::Eof;
    }

    // Record patterns outside comprehensions: `local {...} = value` declarations and function
    // parameters. Generator patterns are parsed with their comprehension clause.
    void scanPatterns()
    {
        const std::vector<Token>& all = codeTokens();
        for (size_t i = 0; i + 1 < all.size() && !stopped; ++i)
        {
            const int t = all[i].type, next = all[i + 1].type;
            if (t == T::ReservedLocal && next == '{')
                localPattern(all, i);
            else if (t == T::ReservedLocal && next == '[')
                error(all[i + 1].range, "indexed destructuring is not supported; record patterns bind named fields, e.g. local {x, y} = value");
            else if (t == T::ReservedFunction)
                parameterPatterns(all, i);
            else if (t == T::ReservedFor && next == '{')
                loopPatterns.push_back(all[i].range);
        }
    }

    void localPattern(const std::vector<Token>& all, size_t at)
    {
        LocalPattern local;
        local.keyword = all[at].range;
        size_t i = at + 1;
        local.pattern = recordPattern(all, i, 0);
        if (stopped) return;
        std::vector<Range> bound;
        patternNames(local.pattern, bound);
        duplicates(bound);
        local.equals = pointIn(all, i);
        if (typeIn(all, i) == ',')
            error(all[i].range, "a record pattern declaration binds exactly one pattern to one value");
        else
        {
            if (typeIn(all, i) == ':')
            {
                // The annotation is Luau's to parse; only the '=' that ends it matters here.
                local.colon = all[i++].range;
                int depth = 0;
                for (; !keywordStop(typeIn(all, i)) || depth > 0; ++i)
                {
                    const int t = typeIn(all, i);
                    if (t == T::Eof) break;
                    if (t == '(' || t == '[' || t == '{') ++depth;
                    else if ((t == ')' || t == ']' || t == '}') && depth > 0) --depth;
                    else if (t == '=' && depth == 0) break;
                }
            }
            if (typeIn(all, i) == '=')
            {
                local.equals = all[i].range;
                // Where the value ends, when the token skipper is sure: after a ';', or before a
                // token that cannot continue the expression. Luau's parse verifies it.
                const size_t end = skipExpression(all, i + 1, 0);
                if (end != SIZE_MAX && end > i + 1)
                {
                    if (typeIn(all, end) == ';')
                        local.extract = all[end].range.end;
                    else if (!continuesExpression(typeIn(all, end)) && typeIn(all, end) != ',')
                        local.extract = all[end - 1].range.end;
                }
            }
            else
            {
                local.equals = pointIn(all, i);
                error(local.equals, "expected '=' after record pattern; a record pattern declaration needs a value");
            }
        }
        document.locals.push_back(std::move(local));
    }

    // `function [name {. name} [: name]] [<generics>] (params)`: every parameter that begins
    // with '{' is a record pattern; the annotation after it is Luau's.
    void parameterPatterns(const std::vector<Token>& all, size_t at)
    {
        size_t i = at + 1;
        if (typeIn(all, i) == T::Name)
        {
            ++i;
            while ((typeIn(all, i) == '.' || typeIn(all, i) == ':') && typeIn(all, i + 1) == T::Name)
                i += 2;
        }
        if (typeIn(all, i) == '<')
        {
            int depth = 0;
            for (; typeIn(all, i) != T::Eof; ++i)
            {
                if (typeIn(all, i) == '<') ++depth;
                else if (typeIn(all, i) == '>' && --depth == 0) { ++i; break; }
            }
        }
        if (typeIn(all, i) != '(') return;
        ++i;
        size_t parameter = 0;
        bool start = true;
        int depth = 0;
        std::vector<Range> bound;
        bool patterns = false;
        const size_t first = document.parameters.size();
        for (;;)
        {
            const int t = typeIn(all, i);
            // A parameter list holds names, '...', patterns and types: a statement keyword or
            // an assignment means the list was never closed; Luau reports that.
            if (t == '=' || keywordStop(t) || (depth == 0 && t == ')'))
                break;
            if (depth == 0 && start && t == '{')
            {
                ParameterPattern pattern;
                pattern.function = all[at].range;
                pattern.parameter = parameter;
                pattern.pattern = recordPattern(all, i, 0);
                if (stopped) return;
                patternNames(pattern.pattern, bound);
                if (typeIn(all, i) == ':') pattern.colon = all[i].range;
                document.parameters.push_back(std::move(pattern));
                patterns = true;
                start = false;
                continue;
            }
            if (depth == 0 && start && t == T::Name) bound.push_back(all[i].range);
            start = false;
            if (t == '(' || t == '[' || t == '{' || t == '<') ++depth;
            else if ((t == ')' || t == ']' || t == '}' || t == '>') && depth > 0) --depth;
            else if (t == ',' && depth == 0)
            {
                ++parameter;
                start = true;
            }
            ++i;
        }
        if (patterns) duplicates(bound);
        // Where the body starts, when the skipper is sure: right after ')' or after a return
        // type no token could extend. Luau's parse verifies it.
        if (!patterns || typeIn(all, i) != ')') return;
        size_t body = i + 1;
        if (typeIn(all, body) == ':')
        {
            body = skipType(all, body + 1, 0);
            if (body == SIZE_MAX || continuesType(typeIn(all, body))) return;
        }
        for (size_t k = first; k < document.parameters.size(); ++k)
            document.parameters[k].extract = all[body - 1].range.end;
    }

    // A token skipper for boundary estimates. It mirrors Luau's grammar closely enough to be
    // right almost always; it never has to be right, because Luau's parse of the lowering
    // checks every estimate and a mismatch falls back to locating boundaries with that parse.
    // Each returns the index just past what it skipped, or SIZE_MAX when unsure.
    static constexpr size_t maxSkipDepth = 64;

    static bool binaryOperator(int t)
    {
        return t == '+' || t == '-' || t == '*' || t == '/' || t == T::FloorDiv || t == '%' || t == '^' || t == T::Dot2 ||
            t == T::Equal || t == T::NotEqual || t == '<' || t == T::LessEqual || t == '>' || t == T::GreaterEqual ||
            t == T::ReservedAnd || t == T::ReservedOr;
    }

    // Tokens that, after a complete expression, continue it: Luau would read them as part of it.
    static bool continuesExpression(int t)
    {
        return binaryOperator(t) || t == '.' || t == ':' || t == '(' || t == '[' || t == '{' || t == T::QuotedString ||
            t == T::RawString || t == T::InterpStringBegin || t == T::InterpStringSimple || t == T::DoubleColon;
    }

    // Tokens that, after a complete type, continue it.
    static bool continuesType(int t)
    {
        return t == '?' || t == '|' || t == '&' || t == T::SkinnyArrow || t == '.' || t == '<' || t == '(';
    }

    // From an opening '(', '[', '{' or interpolated string to just past its matching closer.
    size_t skipBalanced(const std::vector<Token>& all, size_t i) const
    {
        std::vector<int> closers;
        for (; i < all.size(); ++i)
        {
            const int t = all[i].type;
            if (t == T::Eof) return SIZE_MAX;
            const int close = t == '(' ? ')' : t == '[' ? ']' : t == '{' ? '}' : t == T::InterpStringBegin ? int(T::InterpStringEnd) : 0;
            if (close)
                closers.push_back(close);
            else if (t == ')' || t == ']' || t == '}' || t == T::InterpStringEnd)
            {
                if (closers.empty() || closers.back() != t) return SIZE_MAX;
                closers.pop_back();
                if (closers.empty()) return i + 1;
            }
        }
        return SIZE_MAX;
    }

    // From '<' to just past its matching '>', brackets inside balanced.
    size_t skipAngles(const std::vector<Token>& all, size_t i) const
    {
        int depth = 0;
        for (; i < all.size(); ++i)
        {
            const int t = all[i].type;
            if (t == T::Eof || t == ';' || keywordStop(t)) return SIZE_MAX;
            if (t == '(' || t == '[' || t == '{')
            {
                i = skipBalanced(all, i);
                if (i == SIZE_MAX) return SIZE_MAX;
                --i;
            }
            else if (t == '<') ++depth;
            else if (t == '>' && --depth == 0) return i + 1;
        }
        return SIZE_MAX;
    }

    size_t skipType(const std::vector<Token>& all, size_t i, size_t depth) const
    {
        if (depth > maxSkipDepth) return SIZE_MAX;
        for (;;)
        {
            if (typeIn(all, i) == '|' || typeIn(all, i) == '&') ++i;
            if (typeIn(all, i) == T::Dot3) ++i;
            const int t = typeIn(all, i);
            if (t == T::Name)
            {
                const bool typeOf = text(all[i].range) == "typeof";
                ++i;
                if (typeOf && typeIn(all, i) == '(')
                    i = skipBalanced(all, i);
                else
                {
                    while (typeIn(all, i) == '.' && typeIn(all, i + 1) == T::Name) i += 2;
                    if (typeIn(all, i) == '<') i = skipAngles(all, i);
                }
            }
            else if (t == '{')
                i = skipBalanced(all, i);
            else if (t == '(' || t == '<')
            {
                if (t == '<' && (i = skipAngles(all, i)) == SIZE_MAX) return SIZE_MAX;
                if (typeIn(all, i) != '(') return SIZE_MAX;
                i = skipBalanced(all, i);
                if (i != SIZE_MAX && typeIn(all, i) == T::SkinnyArrow) i = skipType(all, i + 1, depth + 1);
            }
            else if (t == T::ReservedNil || t == T::ReservedTrue || t == T::ReservedFalse || t == T::QuotedString || t == T::RawString)
                ++i;
            else
                return SIZE_MAX;
            if (i == SIZE_MAX) return SIZE_MAX;
            while (typeIn(all, i) == '?') ++i;
            if (typeIn(all, i) != '|' && typeIn(all, i) != '&') return i;
        }
    }

    size_t skipExpression(const std::vector<Token>& all, size_t i, size_t depth) const
    {
        if (depth > maxSkipDepth) return SIZE_MAX;
        for (;;)
        {
            while (typeIn(all, i) == T::ReservedNot || typeIn(all, i) == '-' || typeIn(all, i) == '#') ++i;
            i = skipSimple(all, i, depth);
            if (i == SIZE_MAX) return SIZE_MAX;
            if (typeIn(all, i) == T::DoubleColon && (i = skipType(all, i + 1, depth + 1)) == SIZE_MAX) return SIZE_MAX;
            if (!binaryOperator(typeIn(all, i))) return i;
            ++i;
        }
    }

    size_t skipSimple(const std::vector<Token>& all, size_t i, size_t depth) const
    {
        const int t = typeIn(all, i);
        switch (t)
        {
        case T::Number: case T::QuotedString: case T::RawString: case T::InterpStringSimple: case T::ReservedNil:
        case T::ReservedTrue: case T::ReservedFalse: case T::Dot3:
            return i + 1;
        case '{': case T::InterpStringBegin:
            return skipBalanced(all, i);
        case T::Attribute: case T::AttributeOpen:
            while (typeIn(all, i) == T::Attribute || typeIn(all, i) == T::AttributeOpen)
            {
                if (typeIn(all, i) == T::Attribute) { ++i; continue; }
                int nested = 1; // `@[` ... `]`
                for (++i; nested && typeIn(all, i) != T::Eof; ++i)
                    nested += typeIn(all, i) == '[' ? 1 : typeIn(all, i) == ']' ? -1 : 0;
            }
            if (typeIn(all, i) != T::ReservedFunction) return SIZE_MAX;
            return skipFunction(all, i + 1, depth + 1);
        case T::ReservedFunction:
            return skipFunction(all, i + 1, depth + 1);
        case T::ReservedIf:
        {
            i = skipExpression(all, i + 1, depth + 1);
            if (i == SIZE_MAX || typeIn(all, i) != T::ReservedThen) return SIZE_MAX;
            i = skipExpression(all, i + 1, depth + 1);
            while (i != SIZE_MAX && typeIn(all, i) == T::ReservedElseif)
            {
                i = skipExpression(all, i + 1, depth + 1);
                if (i == SIZE_MAX || typeIn(all, i) != T::ReservedThen) return SIZE_MAX;
                i = skipExpression(all, i + 1, depth + 1);
            }
            if (i == SIZE_MAX || typeIn(all, i) != T::ReservedElse) return SIZE_MAX;
            return skipExpression(all, i + 1, depth + 1);
        }
        case T::Name: case '(':
        {
            i = t == '(' ? skipBalanced(all, i) : i + 1;
            while (i != SIZE_MAX)
            {
                const int next = typeIn(all, i);
                if (next == '.' && typeIn(all, i + 1) == T::Name) i += 2;
                else if (next == ':' && typeIn(all, i + 1) == T::Name)
                {
                    i += 2;
                    const int argument = typeIn(all, i);
                    if (argument == '(' || argument == '{' || argument == T::InterpStringBegin) i = skipBalanced(all, i);
                    else if (argument == T::QuotedString || argument == T::RawString || argument == T::InterpStringSimple) ++i;
                    else return SIZE_MAX;
                }
                else if (next == '[' || next == '(' || next == '{' || next == T::InterpStringBegin) i = skipBalanced(all, i);
                else if (next == T::QuotedString || next == T::RawString || next == T::InterpStringSimple) ++i;
                else break;
            }
            return i;
        }
        default:
            return SIZE_MAX;
        }
    }

    // A function literal after `function`: signature, then its body to the matching `end`.
    // Block keywords are counted; an `if` opens a block only where it starts a statement.
    size_t skipFunction(const std::vector<Token>& all, size_t i, size_t depth) const
    {
        if (depth > maxSkipDepth) return SIZE_MAX;
        if (typeIn(all, i) == '<' && (i = skipAngles(all, i)) == SIZE_MAX) return SIZE_MAX;
        if (typeIn(all, i) != '(' || (i = skipBalanced(all, i)) == SIZE_MAX) return SIZE_MAX;
        if (typeIn(all, i) == ':' && (i = skipType(all, i + 1, depth + 1)) == SIZE_MAX) return SIZE_MAX;
        for (int blocks = 1; i < all.size(); ++i)
        {
            const int t = all[i].type;
            if (t == T::Eof) return SIZE_MAX;
            if (t == T::ReservedFunction || t == T::ReservedDo || t == T::ReservedRepeat) ++blocks;
            else if (t == T::ReservedIf && !expressionExpected(all[i - 1].type)) ++blocks;
            else if (t == T::ReservedUntil) --blocks;
            else if (t == T::ReservedEnd && --blocks == 0) return i + 1;
        }
        return SIZE_MAX;
    }

    // Whether the token before an `if` leaves an expression to come, making it an if-expression.
    static bool expressionExpected(int previous)
    {
        return binaryOperator(previous) || previous == '=' || previous == '(' || previous == '[' || previous == '{' ||
            previous == ',' || previous == T::ReservedNot || previous == '#' || previous == T::ReservedReturn ||
            previous == T::ReservedIn || (previous >= T::AddAssign && previous <= T::ConcatAssign);
    }

    bool isComprehension(size_t i) const { return type(i) == '[' && type(i + 1) == T::ReservedFor; }
    Reducer reducerOf(const Token& token, T::Type preceding) const
    {
        if (token.type != T::Name || preceding == '.' || preceding == ':') return Reducer::None;
        const auto name = source.substr(token.range.begin, token.range.end - token.range.begin);
        if (name == "sum") return Reducer::Sum;
        if (name == "min") return Reducer::Min;
        if (name == "max") return Reducer::Max;
        if (name == "any") return Reducer::Any;
        if (name == "all") return Reducer::All;
        return Reducer::None;
    }
    // The content of a quoted string token names an element kind, optionally `@stride`.
    bool bufferKindLiteral(Range token) const
    {
        if (token.end - token.begin < 3) return false;
        std::string_view text = source.substr(token.begin + 1, token.end - token.begin - 2);
        static constexpr std::string_view kinds[] = {"u8", "i8", "u16", "i16", "u32", "i32", "f32", "f64"};
        const size_t at = text.find('@');
        const std::string_view kind = text.substr(0, at);
        if (std::find(std::begin(kinds), std::end(kinds), kind) == std::end(kinds)) return false;
        if (at == std::string_view::npos) return true;
        // `kind@stride` or `kind@stride+offset`: digits only, the field inside the stride.
        const std::string_view rest = text.substr(at + 1);
        const size_t plus = rest.find('+');
        const std::string_view stride = rest.substr(0, plus);
        const std::string_view offset = plus == std::string_view::npos ? std::string_view() : rest.substr(plus + 1);
        const auto digits = [](std::string_view digits) {
            if (digits.empty() || digits.size() > 9) return false;
            for (const char c : digits)
                if (!std::isdigit(static_cast<unsigned char>(c))) return false;
            return true;
        };
        if (!digits(stride) || (plus != std::string_view::npos && !digits(offset))) return false;
        const size_t size = kind == "u8" || kind == "i8" ? 1 : kind == "u16" || kind == "i16" ? 2 : kind == "f64" ? 8 : 4;
        const size_t strideBytes = std::stoul(std::string(stride));
        const size_t offsetBytes = offset.empty() ? 0 : std::stoul(std::string(offset));
        return strideBytes >= size && offsetBytes + size <= strideBytes;
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
        // A record pattern of a declaration or parameter inside the prefix (in a function
        // literal) validates as the one name it lowers to.
        const auto mask = [&](Range pattern) {
            if (pattern.begin < r.begin || pattern.end > r.end || pattern.empty()) return;
            for (size_t j = pattern.begin - r.begin; j < pattern.end - r.begin; ++j)
                if (text[j] != '\n' && text[j] != '\r') text[j] = ' ';
            text[pattern.begin - r.begin] = '_';
        };
        for (const LocalPattern& local : document.locals) mask(local.pattern.range);
        for (const ParameterPattern& parameter : document.parameters) mask(parameter.pattern.range);
        // Opaque placeholders ONLY for stock validation, never returned source.
        auto it = std::lower_bound(document.comprehensions.begin(), document.comprehensions.end(), r.begin,
            [](const Comprehension& c, size_t offset) { return c.open.begin < offset; });
        size_t covered = r.begin;
        for (; it != document.comprehensions.end() && it->open.begin < r.end; ++it)
        {
            if (it->range.begin < covered || it->range.end > r.end || it->range.end - it->range.begin < 3) continue;
            // A JSL reducer consumer has no stock-Luau expression spelling. Hide
            // it together with its comprehension ONLY in validation; otherwise
            // `sum <trivia> nil` would reject a valid enclosing clause prefix.
            const size_t originalBegin = !it->sinkPrefix.empty() && it->sinkPrefix.begin >= r.begin ? it->sinkPrefix.begin
                : !it->reducerPrefix.empty() && it->reducerPrefix.begin >= r.begin                      ? it->reducerPrefix.begin
                                                                                                         : it->range.begin;
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
    void recognizeSlice(size_t begin, size_t end, Clause& clause)
    {
        if (end <= begin || type(end - 1) != ']') return;
        size_t open = end - 1;
        int depth = 0;
        for (;;)
        {
            const int token = type(open);
            if (token == ']') ++depth;
            else if (token == '[' && --depth == 0) break;
            if (open == begin) return;
            --open;
        }
        if (open == begin) return;
        size_t colon = 0;
        int nested = 0;
        for (size_t at = open + 1; at + 1 < end; ++at)
        {
            const int token = type(at);
            if (token == '(' || token == '[' || token == '{') ++nested;
            else if (token == ')' || token == ']' || token == '}') --nested;
            else if (token == ':' && nested == 0)
            {
                if (colon) return;
                colon = at;
            }
        }
        if (!colon) return;
        // An optional `, "kind"` after the last bound makes the slice typed.
        size_t comma = 0;
        nested = 0;
        for (size_t at = colon + 1; at + 1 < end; ++at)
        {
            const int token = type(at);
            if (token == '(' || token == '[' || token == '{') ++nested;
            else if (token == ')' || token == ']' || token == '}') --nested;
            else if (token == ',' && nested == 0 && !comma) comma = at;
        }
        const size_t lastEnd = comma ? comma - 1 : end - 2; // Last token of the last bound.
        clause.sliceSource = {tokens[begin].range.begin, tokens[open - 1].range.end};
        const bool missingFirst = colon == open + 1;
        const bool missingLast = colon == lastEnd;
        clause.sliceFirst = missingFirst ? point(colon) : Range{tokens[open + 1].range.begin, tokens[colon - 1].range.end};
        clause.sliceLast = missingLast ? Range{tokens[colon].range.end, tokens[colon].range.end}
                                       : Range{tokens[colon + 1].range.begin, tokens[lastEnd].range.end};
        if (missingFirst) error(clause.sliceFirst, "expected slice first bound before ':'");
        if (missingLast) error(clause.sliceLast, "expected slice last bound after ':'");
        if (comma)
        {
            if (comma + 1 == end - 2 && type(comma + 1) == T::QuotedString && bufferKindLiteral(tokens[comma + 1].range))
                clause.sliceKind = tokens[comma + 1].range;
            else
                error(comma + 1 < end - 1 ? Range{tokens[comma + 1].range.begin, tokens[end - 2].range.end} : point(comma),
                    "slice element kind must be a string literal naming an element kind, e.g. \"f32\" or \"f32@16\"");
        }
    }

    void comprehension(size_t& i, size_t depth)
    {
        if (limit(i, depth)) return;
        const size_t index = document.comprehensions.size();
        document.comprehensions.emplace_back(); // Parent before children.
        Comprehension c;
        c.lengthPrefix = point(i);
        if (i > 0 && type(i - 1) == '#') c.lengthPrefix = tokens[i - 1].range;
        c.reducerPrefix = point(i);
        if (i > 0 && (c.reducer = reducerOf(tokens[i - 1], i > 1 ? type(i - 2) : T::Eof)) != Reducer::None)
            c.reducerPrefix = tokens[i - 1].range;
        c.sinkPrefix = point(i);
        c.sinkDestination = point(i);
        if (i > 0 && type(i - 1) == ')')
        {
            // `into(...)` ends at the token before the opener: find its `(` and the Name before.
            size_t open = i - 1;
            int nested = 0;
            for (;; --open)
            {
                if (type(open) == ')') ++nested;
                else if (type(open) == '(' && --nested == 0) break;
                if (open == 0 || i - open >= maxSinkTokens) { nested = -1; break; }
            }
            if (nested == 0 && open > 0 && type(open - 1) == T::Name && (open < 2 || (type(open - 2) != '.' && type(open - 2) != ':')) &&
                source.substr(tokens[open - 1].range.begin, tokens[open - 1].range.end - tokens[open - 1].range.begin) == "into")
            {
                c.sinkPrefix = {tokens[open - 1].range.begin, tokens[i - 1].range.end};
                c.sinkStatement = open >= 2 && isPostfix(tokens[open - 2], open >= 3 ? tokens[open - 3] : Token{T::Eof, {0, 0}});
                if (open + 1 < i - 1)
                {
                    // Top-level commas split `into(destination[, "kind"[, offset]])`.
                    std::vector<size_t> commas;
                    int nested = 0;
                    for (size_t at = open + 1; at < i - 1; ++at)
                    {
                        const int t = type(at);
                        if (t == '(' || t == '[' || t == '{') ++nested;
                        else if (t == ')' || t == ']' || t == '}') --nested;
                        else if (t == ',' && nested == 0) commas.push_back(at);
                    }
                    const size_t destinationEnd = commas.empty() ? i - 2 : commas[0] - 1;
                    if (destinationEnd < open + 1)
                    {
                        c.sinkDestination = point(open + 1);
                        error(c.sinkDestination, "expected sink destination inside into()");
                    }
                    else
                        c.sinkDestination = {tokens[open + 1].range.begin, tokens[destinationEnd].range.end};
                    if (commas.size() > 2)
                        error(point(commas[2]), "into() takes a destination, an optional buffer kind and an optional offset");
                    if (!commas.empty())
                    {
                        const size_t kindAt = commas[0] + 1;
                        const size_t kindEnd = commas.size() > 1 ? commas[1] - 1 : i - 2;
                        if (kindAt == kindEnd && type(kindAt) == T::QuotedString && bufferKindLiteral(tokens[kindAt].range))
                            c.sinkKind = tokens[kindAt].range;
                        else
                            error(kindAt <= kindEnd ? Range{tokens[kindAt].range.begin, tokens[kindEnd].range.end} : point(kindAt),
                                "sink buffer kind must be a string literal naming an element kind, e.g. \"f32\" or \"f32@16\"");
                        if (commas.size() > 1)
                        {
                            const size_t offsetAt = commas[1] + 1;
                            if (offsetAt <= i - 2) c.sinkOffset = {tokens[offsetAt].range.begin, tokens[i - 2].range.end};
                            else error(point(offsetAt), "expected sink buffer offset after ','");
                        }
                    }
                }
                else
                {
                    c.sinkDestination = point(i - 1);
                    error(c.sinkDestination, "expected sink destination inside into()");
                }

            }
        }
        c.postfix = c.lengthPrefix.empty() && c.reducerPrefix.empty() && c.sinkPrefix.empty() && i > 0 &&
            isPostfix(tokens[i - 1], i > 1 ? tokens[i - 2] : Token{T::Eof, {0, 0}});
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
                // One binding slot: a name or a record pattern.
                const auto slot = [&]() {
                    Pattern pattern;
                    if (type(i) == T::Name) pattern.range = tokens[i++].range;
                    else if (type(i) == '{') pattern = recordPattern(tokens, i, depth);
                    else return false;
                    clause.bindings.push_back(pattern.range);
                    clause.patterns.push_back(std::move(pattern));
                    return true;
                };
                if (slot())
                {
                    while (type(i) == ',')
                    {
                        ++i;
                        if (!slot())
                        {
                            error(point(i), "expected generator binding name after ','");
                            break;
                        }
                    }
                    clause.binding = clause.bindings.front();
                    std::vector<Range> bound;
                    for (const Pattern& pattern : clause.patterns) patternNames(pattern, bound);
                    if (std::any_of(clause.patterns.begin(), clause.patterns.end(), [](const Pattern& p) { return p.record; }))
                        duplicates(bound);
                }
                else error(point(i), "expected generator binding name after 'for'");
                clause.in = point(i);
                // After a fatal limit nothing further is diagnosed: the limit is the one error.
                if (type(i) == T::ReservedIn) clause.in = tokens[i++].range;
                else if (!stopped) error(point(i), "expected 'in' after generator binding");
            }
            const size_t expressionBegin = i;
            clause.expression = expression(i, depth, false, clause.expressionSuffix);
            if (clause.kind == ClauseKind::Generator && clause.expressionSuffix.empty())
            {
                recognizeRange(expressionBegin, i, clause);
                recognizeEnumerate(expressionBegin, i, clause);
                recognizeZip(expressionBegin, i, clause);
                recognizeSlice(expressionBegin, i, clause);
                if (!clause.enumerateArgument.empty() && clause.bindings.size() != 2)
                    error(clause.binding, "JSL enumerate generator requires two bindings");
                else if (!clause.zipArguments.empty() && clause.bindings.size() != clause.zipArguments.size())
                    error(clause.binding, "JSL zip generator binding count must match its arguments");
                else if (clause.enumerateArgument.empty() && clause.zipArguments.empty() && clause.bindings.size() > 1)
                    error(clause.binding, "multiple generator bindings require a recognized multi-value source");
            }
            if (clause.expression.empty() && !stopped)
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
        else if (!stopped) error(c.arrow, "expected '=>' and projection expression");
        c.close = point(i);
        if (!stopped && type(i) == ']') c.close = tokens[i++].range;
        else if (!stopped) error(c.close, "expected ']' to close comprehension");
        c.range = {c.open.begin, c.close.end};
        c.complete = !c.postfix && !c.close.empty() && document.errors.size() == errorsBefore;
        document.comprehensions[index] = std::move(c);
    }
};
}
Document parseSurface(std::string_view source) { return Frontend(source).run(); }
}
