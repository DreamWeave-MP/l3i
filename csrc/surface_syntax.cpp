// L3i surface syntax: eager optimizer-visible list comprehensions.
//
// This deliberately lives outside Luau.  L3i recognizes the non-conflicting surface form
//
//     [for item in source => project]
//     [for item in source if predicate => project]
//     [for x in xs for y in ys => project]
//
// and rewrites it to an immediately-invoked Luau function containing explicit numeric loops.
// Comprehensions always produce dense, non-nil arrays: every accepted projection is evaluated
// exactly once and a nil projection raises an explicit error. Filters use ordinary Luau truthiness.
// The unary length form `#[for ... => ...]` is fused to an allocation-free count traversal while
// preserving projection evaluation and the same non-nil check.
//
// L3i enables LuauCompileIifeInline, allowing stock Luau to erase the expression wrapper and see
// essentially the same loop shape a human would write.
//
// The `[for ... => ...]` spelling is intentionally not Python's literal spelling.  In Luau, `[[`
// begins a long string, which makes directly nested Python-style comprehensions lexically hostile.
// Prefixing the collection expression with `for` gives L3i an unambiguous sentinel and makes
// nesting natural:
//
//     [for row in rows => [for x in row => x * 2]]
//
// This remains a source-front-end prototype.  It is independent of Luau internals so the feature
// does not require a Luau fork.  A later AST/token implementation can preserve the same surface and
// lowering semantics while adding exact source maps and identity-hygienic locals.

#include "surface_syntax.h"

#include <cctype>
#include <cstdlib>
#include <cstring>
#include <string>
#include <string_view>
#include <vector>

namespace
{
struct Scan
{
    std::string_view text;

    static bool identStart(char c)
    {
        return c == '_' || std::isalpha(static_cast<unsigned char>(c)) != 0;
    }

    static bool identContinue(char c)
    {
        return c == '_' || std::isalnum(static_cast<unsigned char>(c)) != 0;
    }

    // Returns the byte after a quoted string/backtick, or text.size() on unterminated input.
    // Backtick interpolation is intentionally opaque in this source-level prototype.
    size_t quoted(size_t at) const
    {
        const char quote = text[at++];
        while (at < text.size())
        {
            if (text[at] == '\\')
            {
                at += at + 1 < text.size() ? 2 : 1;
                continue;
            }
            if (text[at++] == quote)
                return at;
        }
        return text.size();
    }

    // Luau long strings/comments: [=[ ... ]=], with any number of '=' bytes.
    size_t longBracket(size_t at) const
    {
        if (at >= text.size() || text[at] != '[')
            return at;
        size_t cursor = at + 1;
        while (cursor < text.size() && text[cursor] == '=')
            ++cursor;
        if (cursor >= text.size() || text[cursor] != '[')
            return at;

        const size_t equals = cursor - (at + 1);
        ++cursor;
        while (cursor < text.size())
        {
            if (text[cursor] == ']')
            {
                size_t end = cursor + 1;
                size_t seen = 0;
                while (end < text.size() && text[end] == '=' && seen < equals)
                {
                    ++end;
                    ++seen;
                }
                if (seen == equals && end < text.size() && text[end] == ']')
                    return end + 1;
            }
            ++cursor;
        }
        return text.size();
    }

    size_t comment(size_t at) const
    {
        if (at + 1 >= text.size() || text[at] != '-' || text[at + 1] != '-')
            return at;
        const size_t longStart = at + 2;
        const size_t longEnd = longBracket(longStart);
        if (longEnd != longStart)
            return longEnd;
        at += 2;
        while (at < text.size() && text[at] != '\n')
            ++at;
        return at;
    }

    size_t skipLiteralOrComment(size_t at) const
    {
        const char c = text[at];
        if (c == '\'' || c == '"' || c == '`')
            return quoted(at);
        if (c == '-' && at + 1 < text.size() && text[at + 1] == '-')
            return comment(at);
        if (c == '[')
        {
            const size_t end = longBracket(at);
            if (end != at)
                return end;
        }
        return at;
    }
};

std::string_view trim(std::string_view text)
{
    while (!text.empty() && std::isspace(static_cast<unsigned char>(text.front())) != 0)
        text.remove_prefix(1);
    while (!text.empty() && std::isspace(static_cast<unsigned char>(text.back())) != 0)
        text.remove_suffix(1);
    return text;
}

bool identifier(std::string_view text)
{
    text = trim(text);
    if (text.empty() || !Scan::identStart(text.front()))
        return false;
    for (size_t i = 1; i < text.size(); ++i)
        if (!Scan::identContinue(text[i]))
            return false;
    return true;
}

bool keywordAt(std::string_view text, size_t at, std::string_view keyword)
{
    if (at + keyword.size() > text.size() || text.substr(at, keyword.size()) != keyword)
        return false;
    const bool left = at == 0 || !Scan::identContinue(text[at - 1]);
    const bool right = at + keyword.size() == text.size() || !Scan::identContinue(text[at + keyword.size()]);
    return left && right;
}

size_t skipSpaceAndComments(std::string_view text, size_t at)
{
    Scan scan{text};
    for (;;)
    {
        while (at < text.size() && std::isspace(static_cast<unsigned char>(text[at])) != 0)
            ++at;
        if (at + 1 < text.size() && text[at] == '-' && text[at + 1] == '-')
        {
            const size_t end = scan.comment(at);
            if (end != at)
            {
                at = end;
                continue;
            }
        }
        return at;
    }
}

// Finds a keyword at delimiter depth zero.  Keywords inside strings/comments/nested expressions
// are ignored, and identifier boundaries are required on both sides.
size_t topKeyword(std::string_view text, std::string_view keyword, size_t begin = 0)
{
    Scan scan{text};
    int paren = 0;
    int brace = 0;
    int bracket = 0;
    for (size_t i = begin; i < text.size();)
    {
        const size_t skipped = scan.skipLiteralOrComment(i);
        if (skipped != i)
        {
            i = skipped;
            continue;
        }
        switch (text[i])
        {
        case '(':
            ++paren;
            ++i;
            continue;
        case ')':
            --paren;
            ++i;
            continue;
        case '{':
            ++brace;
            ++i;
            continue;
        case '}':
            --brace;
            ++i;
            continue;
        case '[':
            ++bracket;
            ++i;
            continue;
        case ']':
            --bracket;
            ++i;
            continue;
        default:
            break;
        }

        if (paren == 0 && brace == 0 && bracket == 0 && keywordAt(text, i, keyword))
            return i;
        ++i;
    }
    return std::string_view::npos;
}

// Finds `=>` at delimiter depth zero.  The token is L3i-owned surface syntax, not Luau syntax.
size_t topArrow(std::string_view text, size_t begin = 0)
{
    Scan scan{text};
    int paren = 0;
    int brace = 0;
    int bracket = 0;
    for (size_t i = begin; i < text.size();)
    {
        const size_t skipped = scan.skipLiteralOrComment(i);
        if (skipped != i)
        {
            i = skipped;
            continue;
        }
        switch (text[i])
        {
        case '(':
            ++paren;
            ++i;
            continue;
        case ')':
            --paren;
            ++i;
            continue;
        case '{':
            ++brace;
            ++i;
            continue;
        case '}':
            --brace;
            ++i;
            continue;
        case '[':
            ++bracket;
            ++i;
            continue;
        case ']':
            --bracket;
            ++i;
            continue;
        default:
            break;
        }

        if (paren == 0 && brace == 0 && bracket == 0 && text[i] == '=' && i + 1 < text.size() && text[i + 1] == '>')
            return i;
        ++i;
    }
    return std::string_view::npos;
}

size_t matchingSquare(std::string_view text, size_t open)
{
    Scan scan{text};
    int depth = 1;
    for (size_t i = open + 1; i < text.size();)
    {
        const size_t skipped = scan.skipLiteralOrComment(i);
        if (skipped != i)
        {
            i = skipped;
            continue;
        }
        if (text[i] == '[')
            ++depth;
        else if (text[i] == ']' && --depth == 0)
            return i;
        ++i;
    }
    return std::string_view::npos;
}

struct Generator
{
    std::string_view binding;
    std::string_view source;
    std::vector<std::string_view> predicates;
};

struct Parsed
{
    std::vector<Generator> generators;
    std::string_view project;
};

// Parses the clause side of:
//
//     for x in xs if p(x) for y in ys if q(y) => project
//
// Each source/filter expression is delimited only by a top-level `for`, `if`, or the final arrow.
// Therefore a top-level Luau conditional expression in a source/filter should be parenthesized in
// this source-level prototype.  Nested conditional expressions are otherwise fine.
bool parseComprehension(std::string_view content, Parsed& out)
{
    const size_t arrow = topArrow(content);
    if (arrow == std::string_view::npos)
        return false;

    const std::string_view header = trim(content.substr(0, arrow));
    out.project = trim(content.substr(arrow + 2));
    if (header.empty() || out.project.empty())
        return false;

    size_t cursor = skipSpaceAndComments(header, 0);
    if (!keywordAt(header, cursor, "for"))
        return false;

    while (cursor < header.size())
    {
        cursor += 3; // for
        cursor = skipSpaceAndComments(header, cursor);

        const size_t bindingBegin = cursor;
        if (bindingBegin >= header.size() || !Scan::identStart(header[bindingBegin]))
            return false;
        ++cursor;
        while (cursor < header.size() && Scan::identContinue(header[cursor]))
            ++cursor;
        const std::string_view binding = header.substr(bindingBegin, cursor - bindingBegin);
        if (!identifier(binding))
            return false;

        cursor = skipSpaceAndComments(header, cursor);
        if (!keywordAt(header, cursor, "in"))
            return false;
        cursor += 2;
        const size_t sourceBegin = skipSpaceAndComments(header, cursor);
        if (sourceBegin >= header.size())
            return false;

        const size_t nextIf = topKeyword(header, "if", sourceBegin);
        const size_t nextFor = topKeyword(header, "for", sourceBegin);
        size_t sourceEnd = header.size();
        if (nextIf != std::string_view::npos && nextIf < sourceEnd)
            sourceEnd = nextIf;
        if (nextFor != std::string_view::npos && nextFor < sourceEnd)
            sourceEnd = nextFor;

        Generator generator;
        generator.binding = binding;
        generator.source = trim(header.substr(sourceBegin, sourceEnd - sourceBegin));
        if (generator.source.empty())
            return false;
        cursor = sourceEnd;

        while (cursor < header.size())
        {
            cursor = skipSpaceAndComments(header, cursor);
            if (cursor >= header.size())
                break;
            if (keywordAt(header, cursor, "for"))
                break;
            if (!keywordAt(header, cursor, "if"))
                return false;

            cursor += 2;
            const size_t predicateBegin = skipSpaceAndComments(header, cursor);
            if (predicateBegin >= header.size())
                return false;
            const size_t followingIf = topKeyword(header, "if", predicateBegin);
            const size_t followingFor = topKeyword(header, "for", predicateBegin);
            size_t predicateEnd = header.size();
            if (followingIf != std::string_view::npos && followingIf < predicateEnd)
                predicateEnd = followingIf;
            if (followingFor != std::string_view::npos && followingFor < predicateEnd)
                predicateEnd = followingFor;

            const std::string_view predicate = trim(header.substr(predicateBegin, predicateEnd - predicateBegin));
            if (predicate.empty())
                return false;
            generator.predicates.push_back(predicate);
            cursor = predicateEnd;
        }

        out.generators.push_back(std::move(generator));
        cursor = skipSpaceAndComments(header, cursor);
        if (cursor >= header.size())
            break;
        if (!keywordAt(header, cursor, "for"))
            return false;
    }

    return !out.generators.empty();
}

std::string uniqueStem(std::string_view source, size_t offset)
{
    // Source text cannot produce a truly hygienic name.  Pick a stem absent from the entire source;
    // the AST implementation can replace this with identity-based AstLocal nodes later.
    for (size_t salt = 0;; ++salt)
    {
        std::string stem = "__l3i_comp_" + std::to_string(offset);
        if (salt != 0)
            stem += "_" + std::to_string(salt);
        if (source.find(stem) == std::string_view::npos)
            return stem;
    }
}

std::string rewriteRange(std::string_view source, std::string_view wholeSource, size_t baseOffset, bool& changed);

std::string rewriteSubview(std::string_view view, std::string_view wholeSource, bool& changed)
{
    const size_t offset = static_cast<size_t>(view.data() - wholeSource.data());
    std::string result = rewriteRange(view, wholeSource, offset, changed);
    // Clause trimming can remove the newline that terminates a trailing line comment.
    // Restore that lexical boundary before appending generated statements. Scan literals too:
    // a textual rfind("--") would misclassify strings and closed long comments.
    Scan scan{view};
    for (size_t i = 0; i < view.size();)
    {
        const size_t skipped = scan.skipLiteralOrComment(i);
        if (skipped == view.size() && view[i] == '-' && i + 1 < view.size() && view[i + 1] == '-'
            && scan.longBracket(i + 2) == i + 2)
            result += '\n';
        i = skipped == i ? i + 1 : skipped;
    }
    return result;
}

struct LoweredGenerator
{
    std::string binding;
    std::string source;
    std::vector<std::string> predicates;
};

void emitProjection(std::string& result, const std::string& stem, const std::string& project)
{
    const std::string value = stem + "_value";
    result += "local ";
    result += value;
    result += " = ";
    result += project;
    result += " if ";
    result += value;
    result += " == nil then error(\"L3i comprehension projection produced nil; filter nil explicitly\") end ";
}

void emitGeneratorNest(std::string& result, const std::vector<LoweredGenerator>& generators, size_t level, const std::string& stem,
    const std::string& out, const std::string& count, const std::string& project, bool countOnly)
{
    const LoweredGenerator& generator = generators[level];
    const std::string suffix = "_g" + std::to_string(level);
    const std::string src = stem + suffix + "_src";
    const std::string len = stem + suffix + "_len";
    const std::string index = stem + suffix + "_i";

    result += "local ";
    result += src;
    result += " = ";
    result += generator.source;
    result += " local ";
    result += len;
    result += " = #";
    result += src;
    result += " for ";
    result += index;
    result += " = 1, ";
    result += len;
    result += " do local ";
    result += generator.binding;
    result += " = ";
    result += src;
    result += "[";
    result += index;
    result += "] ";

    for (const std::string& predicate : generator.predicates)
    {
        result += "if ";
        result += predicate;
        result += " then ";
    }

    if (level + 1 < generators.size())
    {
        emitGeneratorNest(result, generators, level + 1, stem, out, count, project, countOnly);
    }
    else
    {
        emitProjection(result, stem, project);
        result += count;
        result += " += 1 ";
        if (!countOnly)
        {
            result += out;
            result += "[";
            result += count;
            result += "] = ";
            result += stem;
            result += "_value ";
        }
    }

    for (size_t i = 0; i < generator.predicates.size(); ++i)
        result += "end ";
    result += "end ";
}

std::string lower(const Parsed& parsed, std::string_view wholeSource, size_t offset, bool& nestedChanged, bool countOnly)
{
    bool projectChanged = false;
    const std::string project = rewriteSubview(parsed.project, wholeSource, projectChanged);
    nestedChanged = nestedChanged || projectChanged;

    std::vector<LoweredGenerator> generators;
    generators.reserve(parsed.generators.size());
    for (const Generator& generator : parsed.generators)
    {
        LoweredGenerator lowered;
        lowered.binding.assign(generator.binding.data(), generator.binding.size());

        bool sourceChanged = false;
        lowered.source = rewriteSubview(generator.source, wholeSource, sourceChanged);
        nestedChanged = nestedChanged || sourceChanged;

        lowered.predicates.reserve(generator.predicates.size());
        for (const std::string_view predicate : generator.predicates)
        {
            bool predicateChanged = false;
            lowered.predicates.push_back(rewriteSubview(predicate, wholeSource, predicateChanged));
            nestedChanged = nestedChanged || predicateChanged;
        }
        generators.push_back(std::move(lowered));
    }

    const std::string stem = uniqueStem(wholeSource, offset);
    const std::string out = stem + "_out";
    const std::string count = stem + "_n";

    // One generator with no filters has an exact output length.  Preserve the strongest possible
    // lowering shape and write by source index directly: no cursor increment in the hot loop.
    const bool exactDense = generators.size() == 1 && generators[0].predicates.empty();

    std::string result;
    size_t reserve = project.size() + 320;
    for (const LoweredGenerator& generator : generators)
    {
        reserve += generator.binding.size() + generator.source.size() + 96;
        for (const std::string& predicate : generator.predicates)
            reserve += predicate.size() + 16;
    }
    result.reserve(reserve);
    result += "(function() ";

    if (exactDense)
    {
        const std::string src = stem + "_g0_src";
        const std::string len = stem + "_g0_len";
        const std::string index = stem + "_g0_i";
        result += "local ";
        result += src;
        result += " = ";
        result += generators[0].source;
        result += " local ";
        result += len;
        result += " = #";
        result += src;
        result += " ";
        if (countOnly)
        {
            result += "local ";
            result += count;
            result += " = 0 ";
        }
        else
        {
            result += "local ";
            result += out;
            result += " = table.create(";
            result += len;
            // table.create's omitted fill parameter infers {unknown} with Luau's new solver.
            // Give the fresh allocation the unsealed builder type of an empty literal, so indexed
            // writes infer the checked projection type. typeof's literal is never executed.
            result += ") :: typeof({}) ";
        }
        result += "for ";
        result += index;
        result += " = 1, ";
        result += len;
        result += " do local ";
        result += generators[0].binding;
        result += " = ";
        result += src;
        result += "[";
        result += index;
        result += "] ";
        emitProjection(result, stem, project);
        if (countOnly)
        {
            result += count;
            result += " += 1 ";
        }
        else
        {
            result += out;
            result += "[";
            result += index;
            result += "] = ";
            result += stem;
            result += "_value ";
        }
        result += "end return ";
        result += countOnly ? count : out;
        result += " end)()";
        return result;
    }

    // Filtered and/or nested forms use one dense output cursor.  Keep the common one-generator
    // filtered case just as tight as the handwritten loop: evaluate the source once, preallocate to
    // its exact upper bound, and avoid a second alias/length pair.
    if (generators.size() == 1)
    {
        const LoweredGenerator& generator = generators[0];
        const std::string src = stem + "_g0_src";
        const std::string len = stem + "_g0_len";
        const std::string index = stem + "_g0_i";
        result += "local ";
        result += src;
        result += " = ";
        result += generator.source;
        result += " local ";
        result += len;
        result += " = #";
        result += src;
        result += " local ";
        if (!countOnly)
        {
            result += out;
            result += " = table.create(";
            result += len;
            result += ") :: typeof({}) local ";
        }
        result += count;
        result += " = 0 for ";
        result += index;
        result += " = 1, ";
        result += len;
        result += " do local ";
        result += generator.binding;
        result += " = ";
        result += src;
        result += "[";
        result += index;
        result += "] ";
        for (const std::string& predicate : generator.predicates)
        {
            result += "if ";
            result += predicate;
            result += " then ";
        }
        emitProjection(result, stem, project);
        result += count;
        result += " += 1 ";
        if (!countOnly)
        {
            result += out;
            result += "[";
            result += count;
            result += "] = ";
            result += stem;
            result += "_value ";
        }
        for (size_t i = 0; i < generator.predicates.size(); ++i)
            result += "end ";
        result += "end ";
    }
    else
    {
        // Nested generators can have data-dependent cardinality and inner sources may refer to the
        // outer binding.  Evaluate each source exactly once at its natural nesting level and grow a
        // dense result table through one output cursor.
        if (!countOnly)
        {
            result += "local ";
            result += out;
            result += " = {} ";
        }
        result += "local ";
        result += count;
        result += " = 0 ";
        emitGeneratorNest(result, generators, 0, stem, out, count, project, countOnly);
    }

    result += "return ";
    result += countOnly ? count : out;
    result += " end)()";
    return result;
}

bool startsComprehension(std::string_view source, size_t open)
{
    // `[` has already been confirmed not to begin a long bracket.  Only `[ <trivia> for` belongs
    // to this surface language, which keeps ordinary indexing/table expressions on the cheap path.
    size_t cursor = skipSpaceAndComments(source, open + 1);
    return cursor < source.size() && keywordAt(source, cursor, "for");
}

std::string rewriteRange(std::string_view source, std::string_view wholeSource, size_t baseOffset, bool& changed)
{
    Scan scan{source};
    std::string result;
    result.reserve(source.size());
    size_t copied = 0;

    for (size_t i = 0; i < source.size();)
    {
        const size_t skipped = scan.skipLiteralOrComment(i);
        if (skipped != i)
        {
            i = skipped;
            continue;
        }
        if (source[i] != '[' || scan.longBracket(i) != i)
        {
            ++i;
            continue;
        }

        // `[for` is the only collection-expression sentinel.  Do not even match ordinary index
        // brackets here; continuing the outer scan naturally discovers a nested `[for` while making
        // the common `array[i]` case nearly free.
        if (!startsComprehension(source, i))
        {
            ++i;
            continue;
        }

        const size_t close = matchingSquare(source, i);
        if (close == std::string_view::npos)
        {
            ++i;
            continue;
        }

        const std::string_view content = source.substr(i + 1, close - i - 1);
        Parsed parsed{};
        if (parseComprehension(content, parsed))
        {
            // Fuse the unary length form directly: `#[for ... => ...]` returns the number of
            // accepted projections without allocating a result table. Projection expressions still
            // run exactly once and still trip the non-nil invariant, so observable effects match the
            // materialized comprehension followed by `#`.
            bool countOnly = false;
            size_t replaceBegin = i;
            size_t prefix = i;
            while (prefix > copied && std::isspace(static_cast<unsigned char>(source[prefix - 1])) != 0)
                --prefix;
            if (prefix > copied && source[prefix - 1] == '#')
            {
                countOnly = true;
                replaceBegin = prefix - 1;
            }

            result.append(source.data() + copied, replaceBegin - copied);
            bool nested = false;
            result += lower(parsed, wholeSource, baseOffset + i, nested, countOnly);
            changed = true;
            copied = close + 1;
            i = close + 1;
            continue;
        }

        // Malformed candidate: leave it for stock Luau diagnostics, but keep scanning its interior in
        // case it contains an independently valid nested comprehension.
        ++i;
    }

    if (!changed)
        return std::string(source);
    result.append(source.data() + copied, source.size() - copied);
    return result;
}
} // namespace

extern "C" char* l3i_rewrite_surface_syntax(const char* source, size_t size, size_t* outsize)
{
    if (outsize)
        *outsize = 0;
    if (!source || !outsize)
        return nullptr;

    std::string_view input(source, size);
    // Cheap rejection for the overwhelmingly common path.  The exact `[for` sentinel may contain
    // whitespace/comments, so avoid an expensive regex/search and just require both ingredients.
    if (input.find("=>") == std::string_view::npos || input.find('[') == std::string_view::npos
        || input.find("for") == std::string_view::npos)
        return nullptr;

    bool changed = false;
    std::string output = rewriteRange(input, input, 0, changed);
    if (!changed)
        return nullptr;

    char* memory = static_cast<char*>(std::malloc(output.size()));
    if (!memory)
        return nullptr;
    std::memcpy(memory, output.data(), output.size());
    *outsize = output.size();
    return memory;
}
