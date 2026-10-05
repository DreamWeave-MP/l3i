// l3i analysis shim: a C ABI over Luau's Analysis library (type checker, linter,
// autocomplete) and the parser. The host supplies sources and per-module configuration through
// callbacks; results come back through callbacks too, so no C++ types cross the boundary.
//
// Nothing throws across the C boundary: every entry point catches Luau's exceptions and reports
// them as internal-error diagnostics.

#include <Luau/Ast.h>
#include <Luau/AstJsonEncoder.h>
#include <Luau/Autocomplete.h>
#include <Luau/BuiltinDefinitions.h>
#include <Luau/Config.h>
#include <Luau/Error.h>
#include <Luau/Frontend.h>
#include <Luau/Linter.h>
#include <Luau/Parser.h>
#include <Luau/ToString.h>
#include <Luau/TypeArena.h>

#include "surface_syntax.h"
#include "source_locations.h"

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <cstring>
#include <exception>
#include <memory>
#include <optional>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <utility>

extern "C" {

typedef void (*db_sink)(void* ctx, const char* data, size_t length);

// Diagnostic kinds.
enum
{
    DB_DIAG_TYPE_ERROR = 0,
    DB_DIAG_LINT_WARNING = 1,
    DB_DIAG_LINT_ERROR = 2,
    DB_DIAG_PARSE_ERROR = 3,
    DB_DIAG_INTERNAL_ERROR = 4,
};

typedef void (*db_diagnostic_fn)(void* ctx, int kind, int code, const char* name, size_t name_length, const char* module,
    size_t module_length, const char* text, size_t text_length, unsigned begin_line, unsigned begin_column,
    unsigned end_line, unsigned end_column);

// Per-module configuration the host answers with. `mode`: 0 nocheck, 1 nonstrict, 2 strict.
struct db_module_config
{
    int mode;
    uint64_t enabled_lint_mask; // 0 keeps Luau's defaults
    int lint_errors;
    int type_errors;
};

struct db_source_provider
{
    void* ctx;
    // Writes the module's source through `sink` and its kind to *type (1 module, 2 script);
    // returns 0 when the module does not exist.
    int (*read_source)(void* ctx, const char* name, size_t name_length, db_sink sink, void* sink_ctx, int* type);
    // Resolves a `require(<string>)` argument seen in `requirer` to a module name (through
    // `sink`); returns 0 when it cannot. May be null.
    int (*resolve_module)(void* ctx, const char* requirer, size_t requirer_length, const char* required, size_t required_length, db_sink sink,
        void* sink_ctx);
    // Fills the module's configuration; may be null for Luau's defaults.
    void (*module_config)(void* ctx, const char* name, size_t name_length, db_module_config* out);
    // Writes a display name for the module; may be null.
    int (*human_name)(void* ctx, const char* name, size_t name_length, db_sink sink, void* sink_ctx);
};

// One definitions file (`.d.luau`: `declare class`, `declare name: T`, `export type`) loaded
// into the global scope before it is frozen.
struct db_definition
{
    const char* name;
    size_t name_length;
    const char* source;
    size_t source_length;
};

struct db_analysis_options
{
    int solver_mode; // 0 old, 1 new
    int register_builtins;
    int retain_full_type_graphs;
    const db_definition* definitions;
    size_t definitions_count;
    // Receives a definitions file's parse and type errors; creation then fails.
    db_diagnostic_fn diagnostic;
    void* diagnostic_ctx;
};

typedef void (*db_completion_fn)(
    void* ctx, const char* name, size_t name_length, int kind, int deprecated, const char* type_text, size_t type_length);

struct db_analysis;

} // extern "C"

namespace
{
    void appendToString(void* ctx, const char* data, size_t length)
    {
        static_cast<std::string*>(ctx)->append(data, length);
    }

    void discardSource(void*, const char*, size_t)
    {
    }

    struct SourceSnapshot
    {
        std::string original;
        L3i::Surface::LoweredSource lowered;
    };

    Luau::Location location(L3i::Surface::Span span)
    {
        return {Luau::Position(span.begin.line, span.begin.column), Luau::Position(span.end.line, span.end.column)};
    }

    size_t offset(std::string_view source, Luau::Position position)
    {
        size_t at = 0;
        for (unsigned line = 0; line < position.line && at < source.size(); ++line)
        {
            const size_t next = source.find('\n', at);
            at = next == std::string_view::npos ? source.size() : next + 1;
        }
        const size_t newline = source.find('\n', at);
        const size_t end = newline == std::string_view::npos ? source.size() : newline;
        return at + std::min(size_t(position.column), end - at);
    }

    bool overlaps(L3i::Surface::Range range, L3i::Surface::Range other)
    {
        // A point at the end of an expression belongs to it too: parser errors
        // for a copied trailing '.' must not be mistaken for scaffolding errors.
        return range.empty() ? other.begin <= range.begin && range.begin <= other.end
                             : range.begin < other.end && other.begin < range.end;
    }

    bool synthetic(const SourceSnapshot& snapshot, const Luau::Location& loc)
    {
        const auto& lowered = snapshot.lowered;
        const L3i::Surface::Range range{offset(lowered.source, loc.begin), offset(lowered.source, loc.end)};
        // Innermost site wins; a nested generated call is not a copied expression
        // merely because its parent expression contains it.
        const L3i::Surface::ComprehensionSite* inner = nullptr;
        for (const auto& site : lowered.sites)
            if (site.call.begin <= range.begin && range.end <= site.call.end &&
                (!inner || site.call.end - site.call.begin < inner->call.end - inner->call.begin))
                inner = &site;
        if (!inner)
            return false;
        const auto& node = lowered.document.comprehensions.at(inner->comprehension);
        const auto copiedExpression = [&](L3i::Surface::Range site)
        {
            // A trailing '.' or operator is diagnosed at its generated ')' fence.
            // It is still a copied-expression error, even when another structural
            // hole shares the same original EOF insertion point.
            return overlaps(range, site) ||
                (range.begin == site.end && range.end <= site.end + 1 && site.end < lowered.source.size() && lowered.source[site.end] == ')');
        };
        if (!node.projection.empty() && copiedExpression(inner->projection))
            return false;
        for (size_t i = 0; i < inner->clauses.size(); ++i)
        {
            if (!node.clauses[i].expression.empty() && copiedExpression(inner->clauses[i].expression))
                return false;
            if (!node.clauses[i].binding.empty() && overlaps(range, inner->clauses[i].binding))
                return false;
        }
        return true;
    }

    bool recoveryNoise(const SourceSnapshot& snapshot, const Luau::Location& loc)
    {
        if (!synthetic(snapshot, loc))
            return false;
        const auto span = snapshot.lowered.map.originalSpan({{loc.begin.line, loc.begin.column}, {loc.end.line, loc.end.column}});
        const size_t begin = snapshot.lowered.map.originalOffset(span.begin);
        const size_t end = snapshot.lowered.map.originalOffset(span.end);
        if (begin != end)
            return false;
        for (const auto& node : snapshot.lowered.document.comprehensions)
        {
            if (node.projection.empty() && begin == node.projection.begin)
                return true;
            for (const auto& clause : node.clauses)
                if ((clause.expression.empty() && begin == clause.expression.begin) ||
                    (clause.binding.empty() && begin == clause.binding.begin))
                    return true;
        }
        return false;
    }

    struct GeneratedBindings : Luau::AstVisitor
    {
        const SourceSnapshot& snapshot;
        std::unordered_set<std::string> names;
        explicit GeneratedBindings(const SourceSnapshot& snapshot) : snapshot(snapshot) {}
        void add(Luau::AstLocal* local)
        {
            if (synthetic(snapshot, local->location) || snapshot.lowered.map.generatedName(local->name.value))
                names.insert(local->name.value);
        }
        bool visit(Luau::AstStatLocal* stat) override
        {
            for (auto* local : stat->vars)
                add(local);
            return true;
        }
        bool visit(Luau::AstStatFor* stat) override
        {
            add(stat->var);
            return true;
        }
    };

    struct HostFileResolver : Luau::FileResolver
    {
        db_source_provider provider;
        // Updated only by the frontend's readSource, so cached ASTs keep the map
        // from their source snapshot even after markDirty or an existence probe.
        std::unordered_map<Luau::ModuleName, SourceSnapshot> snapshots;

        bool sourceExists(const Luau::ModuleName& name) const
        {
            int type = 0;
            return provider.read_source(provider.ctx, name.data(), name.size(), discardSource, nullptr, &type) != 0;
        }

        const SourceSnapshot* snapshot(const Luau::ModuleName& name) const
        {
            auto it = snapshots.find(name);
            return it == snapshots.end() ? nullptr : &it->second;
        }

        const L3i::Surface::SourceMap* sourceMap(const Luau::ModuleName& name) const
        {
            const auto* value = snapshot(name);
            return value ? &value->lowered.map : nullptr;
        }

        Luau::Location originalLocation(const Luau::ModuleName& name, const Luau::Location& location) const
        {
            const auto* map = sourceMap(name);
            if (map == nullptr || map->empty())
                return location;
            const auto span = map->originalSpan({{location.begin.line, location.begin.column}, {location.end.line, location.end.column}});
            return Luau::Location{Luau::Position(span.begin.line, span.begin.column), Luau::Position(span.end.line, span.end.column)};
        }

        std::string parseMessage(const Luau::ModuleName& name, const Luau::SyntaxError& error, const Luau::Location& location) const
        {
            const auto* map = sourceMap(name);
            return map ? L3i::Surface::originalParseMessage(error.message, {location.begin.line, location.begin.column}, *map) : error.message;
        }

        std::string lintMessage(const Luau::ModuleName& name, const Luau::LintWarning& warning) const
        {
            const auto* map = sourceMap(name);
            if (!map || map->empty())
                return warning.text;
            switch (warning.code)
            {
            case Luau::LintWarning::Code_GlobalUsedAsLocal:
            case Luau::LintWarning::Code_LocalShadow:
            case Luau::LintWarning::Code_ImplicitReturn:
            case Luau::LintWarning::Code_TableLiteral:
            case Luau::LintWarning::Code_UninitializedLocal:
            case Luau::LintWarning::Code_DuplicateFunction:
            case Luau::LintWarning::Code_DuplicateCondition:
            case Luau::LintWarning::Code_DuplicateLocal:
            {
                // These templates append a reference after the (possibly quoted) user name.
                size_t at = std::string::npos;
                for (std::string_view phrase : {"at line ", "on line ", "at column ", "on column "})
                {
                    const size_t found = warning.text.rfind(phrase);
                    if (found != std::string::npos && (at == std::string::npos || found > at))
                        at = found;
                }
                return map->referenceText(warning.text, at, {warning.location.begin.line, warning.location.begin.column});
            }
            default:
                return warning.text;
            }
        }

        std::optional<Luau::SourceCode> readSource(const Luau::ModuleName& name) override
        {
            std::string source;
            int type = 0;
            if (!provider.read_source(provider.ctx, name.data(), name.size(), appendToString, &source, &type))
            {
                // A failed frontend reread has no new AST. Do not report an old
                // document's structural errors for a dependency that disappeared.
                snapshots.erase(name);
                return std::nullopt;
            }
            Luau::SourceCode::Type kind = type == 2 ? Luau::SourceCode::Script : Luau::SourceCode::Module;
            auto lowered = L3i::Surface::lower(source, true);
            // Luau owns one shared SourceModule cache for both solvers/check modes.
            // Retain the exact original/lowered pair until that AST is reread.
            auto it = snapshots.insert_or_assign(name, SourceSnapshot{std::move(source), std::move(lowered)}).first;
            return Luau::SourceCode{it->second.lowered.source, kind};
        }

        std::optional<Luau::ModuleInfo> resolveModule(const Luau::ModuleInfo* context, Luau::AstExpr* node, const Luau::TypeCheckLimits&) override
        {
            if (provider.resolve_module == nullptr || context == nullptr)
                return std::nullopt;
            Luau::AstExprConstantString* expr = node->as<Luau::AstExprConstantString>();
            if (expr == nullptr)
                return std::nullopt;
            std::string resolved;
            if (!provider.resolve_module(provider.ctx, context->name.data(), context->name.size(), expr->value.data, expr->value.size,
                    appendToString, &resolved))
                return std::nullopt;
            return Luau::ModuleInfo{std::move(resolved)};
        }

        std::string getHumanReadableModuleName(const Luau::ModuleName& name) const override
        {
            if (provider.human_name == nullptr)
                return name;
            std::string human;
            if (!provider.human_name(provider.ctx, name.data(), name.size(), appendToString, &human))
                return name;
            return human;
        }
    };

    struct HostConfigResolver : Luau::ConfigResolver
    {
        db_source_provider provider;
        mutable Luau::Config config;

        const Luau::Config& getConfig(const Luau::ModuleName& name, const Luau::TypeCheckLimits&) const override
        {
            config = Luau::Config{};
            if (provider.module_config == nullptr)
                return config;
            db_module_config answer{1, 0, 0, 1};
            provider.module_config(provider.ctx, name.data(), name.size(), &answer);
            switch (answer.mode)
            {
            case 0:
                config.mode = Luau::Mode::NoCheck;
                break;
            case 2:
                config.mode = Luau::Mode::Strict;
                break;
            default:
                config.mode = Luau::Mode::Nonstrict;
                break;
            }
            if (answer.enabled_lint_mask != 0)
                config.enabledLint.warningMask = answer.enabled_lint_mask;
            config.lintErrors = answer.lint_errors != 0;
            config.typeErrors = answer.type_errors != 0;
            return config;
        }
    };

    void emit(db_diagnostic_fn diagnostic, void* ctx, int kind, int code, const char* name, const std::string& module, const std::string& text,
        const Luau::Location& location)
    {
        diagnostic(ctx, kind, code, name, name ? strlen(name) : 0, module.data(), module.size(), text.data(), text.size(), location.begin.line,
            location.begin.column, location.end.line, location.end.column);
    }

    int emitStructural(const SourceSnapshot& snapshot, const std::string& module, db_diagnostic_fn diagnostic, void* ctx)
    {
        for (const auto& error : snapshot.lowered.document.errors)
            emit(diagnostic, ctx, DB_DIAG_PARSE_ERROR, 0, nullptr, module, error.message,
                location(snapshot.lowered.map.originalRange(error.range.begin, error.range.end)));
        return int(snapshot.lowered.document.errors.size());
    }

    void reachableModules(const Luau::Frontend& frontend, const std::string& name, std::unordered_set<std::string>& seen,
        std::vector<std::string>& modules)
    {
        if (!seen.insert(name).second)
            return;
        modules.push_back(name);
        auto it = frontend.sourceNodes.find(name);
        if (it == frontend.sourceNodes.end())
            return;
        std::vector<std::string> dependencies;
        for (const auto& dependency : it->second->requireSet)
            dependencies.push_back(dependency);
        std::sort(dependencies.begin(), dependencies.end());
        for (const auto& dependency : dependencies)
            reachableModules(frontend, dependency, seen, modules);
    }

    bool trivia(const SourceSnapshot& snapshot, size_t begin, size_t end)
    {
        if (begin > end || end > snapshot.original.size())
            return false;
        for (size_t at = begin; at < end;)
        {
            if (std::isspace(static_cast<unsigned char>(snapshot.original[at])))
            {
                ++at;
                continue;
            }
            const auto& comments = snapshot.lowered.document.comments;
            auto it = std::find_if(comments.begin(), comments.end(), [at](auto range) { return range.begin == at; });
            if (it == comments.end() || it->end > end)
                return false;
            at = it->end;
        }
        return true;
    }

    bool completeExpression(const SourceSnapshot& snapshot, L3i::Surface::Range range)
    {
        if (range.empty())
            return false;
        std::string text = "return " + snapshot.original.substr(range.begin, range.end - range.begin);
        // This is validation, not recognition. Nested surface expressions use the
        // same seam; stock Luau decides whether the resulting expression is complete.
        auto lowered = L3i::Surface::lower(text, true);
        if (!lowered.document.errors.empty())
            return false;
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        return Luau::Parser::parse(lowered.source.data(), lowered.source.size(), names, allocator).errors.empty();
    }

    struct CompletionPoint
    {
        L3i::Surface::Position generated;
        const char* keyword = nullptr;
    };

    CompletionPoint completionPoint(const SourceSnapshot& snapshot, unsigned line, unsigned column)
    {
        const auto& lowered = snapshot.lowered;
        CompletionPoint result{lowered.map.generatedPosition({line, column}), nullptr};
        const size_t at = lowered.map.empty() ? offset(snapshot.original, Luau::Position(line, column))
                                             : lowered.map.originalOffset({line, column});
        // Prefer the innermost surface node at the cursor.
        const L3i::Surface::ComprehensionSite* inner = nullptr;
        for (const auto& site : lowered.sites)
        {
            const auto& node = lowered.document.comprehensions.at(site.comprehension);
            if (node.open.end <= at && at <= node.close.begin &&
                (!inner || node.open.begin > lowered.document.comprehensions[inner->comprehension].open.begin))
                inner = &site;
        }
        if (!inner)
            return result;
        const auto& node = lowered.document.comprehensions[inner->comprehension];
        bool expressionHole = false;
        for (size_t i = 0; i < node.clauses.size(); ++i)
        {
            const auto& clause = node.clauses[i];
            const size_t after = clause.kind == L3i::Surface::ClauseKind::Filter ? clause.keyword.end
                : !clause.in.empty() ? clause.in.end : !clause.binding.empty() ? clause.binding.end : clause.keyword.end;
            if (clause.expression.empty() && at <= clause.expression.begin && trivia(snapshot, after, at))
            {
                result.generated = lowered.map.generatedPoint(inner->clauses[i].expression.begin);
                expressionHole = true;
            }
            if (clause.kind == L3i::Surface::ClauseKind::Generator && !clause.binding.empty() && clause.in.empty() &&
                at <= clause.in.begin && trivia(snapshot, clause.binding.end, at))
                result.keyword = "in";
        }
        // Without an arrow, the cursor can still belong to a copied source or
        // filter (notably 'record.'). Do not move it to the projection hole.
        if (!expressionHole && !node.arrow.empty() && node.projection.empty() && at <= node.projection.begin &&
            trivia(snapshot, node.arrow.end, at))
            result.generated = lowered.map.generatedPoint(inner->projection.begin);
        if (!result.keyword && node.arrow.empty() && !node.clauses.empty())
        {
            const auto& last = node.clauses.back();
            if (at <= node.arrow.begin && trivia(snapshot, last.expression.end, at) && completeExpression(snapshot, last.expression))
            {
                result.keyword = "=>";
                // Whitespace after a complete expression is a separator slot,
                // while the token end itself retains stock expression completion.
                if (at > last.expression.end && node.projection.empty())
                    result.generated = lowered.map.generatedPoint(inner->projection.begin);
            }
        }
        if (!result.keyword && node.close.empty() && !node.arrow.empty() &&
            at <= node.close.begin && trivia(snapshot, node.projection.end, at) && completeExpression(snapshot, node.projection))
            result.keyword = "]";
        return result;
    }

    void emitInternal(db_diagnostic_fn diagnostic, void* ctx, const std::string& module, const std::string& text)
    {
        emit(diagnostic, ctx, DB_DIAG_INTERNAL_ERROR, 0, nullptr, module, text, Luau::Location{});
    }
}

struct db_analysis
{
    HostFileResolver files;
    HostConfigResolver configs;
    Luau::FrontendOptions options;
    std::unique_ptr<Luau::Frontend> frontend;
};

extern "C" {

db_analysis* db_analysis_create(const db_source_provider* provider, const db_analysis_options* options)
{
    try
    {
        std::unique_ptr<db_analysis> analysis(new db_analysis());
        analysis->files.provider = *provider;
        analysis->configs.provider = *provider;
        analysis->options.retainFullTypeGraphs = options->retain_full_type_graphs != 0;
        analysis->options.runLintChecks = true;
        Luau::SolverMode mode = options->solver_mode == 0 ? Luau::SolverMode::Old : Luau::SolverMode::New;
        analysis->frontend = std::make_unique<Luau::Frontend>(mode, &analysis->files, &analysis->configs, analysis->options);
        if (options->register_builtins)
        {
            Luau::registerBuiltinGlobals(*analysis->frontend, analysis->frontend->globals);
            Luau::registerBuiltinGlobals(*analysis->frontend, analysis->frontend->globalsForAutocomplete, true);
        }
        for (size_t i = 0; i < options->definitions_count; ++i)
        {
            const db_definition& definition = options->definitions[i];
            std::string name(definition.name, definition.name_length);
            std::string_view source(definition.source, definition.source_length);
            Luau::LoadDefinitionFileResult result =
                analysis->frontend->loadDefinitionFile(analysis->frontend->globals, analysis->frontend->globals.globalScope, source, name, false, false);
            Luau::LoadDefinitionFileResult forAutocomplete = analysis->frontend->loadDefinitionFile(
                analysis->frontend->globalsForAutocomplete, analysis->frontend->globalsForAutocomplete.globalScope, source, name, false, true);
            if (result.success && forAutocomplete.success)
                continue;
            if (options->diagnostic != nullptr)
            {
                for (const Luau::ParseError& error : result.parseResult.errors)
                    emit(options->diagnostic, options->diagnostic_ctx, DB_DIAG_PARSE_ERROR, 0, nullptr, name, error.getMessage(), error.getLocation());
                if (result.module)
                    for (const Luau::TypeError& error : result.module->errors)
                        emit(options->diagnostic, options->diagnostic_ctx, DB_DIAG_TYPE_ERROR, error.code(), nullptr, name,
                            Luau::toString(error, Luau::TypeErrorToStringOptions{analysis->frontend->fileResolver}), error.location);
                if (result.parseResult.errors.empty() && (!result.module || result.module->errors.empty()))
                    emitInternal(options->diagnostic, options->diagnostic_ctx, name, "definitions file rejected");
            }
            return nullptr;
        }
        if (options->register_builtins || options->definitions_count != 0)
        {
            Luau::freeze(analysis->frontend->globals.globalTypes);
            Luau::freeze(analysis->frontend->globalsForAutocomplete.globalTypes);
        }
        return analysis.release();
    }
    catch (...)
    {
        return nullptr;
    }
}

void db_analysis_destroy(db_analysis* analysis)
{
    delete analysis;
}

// Type checks (and lints) `name`, reporting every diagnostic. Returns the number of
// structural, parse, type and lint errors, or -1 on an internal failure (reported too).
int db_analysis_check(db_analysis* analysis, const char* name, size_t name_length, int lint, db_diagnostic_fn diagnostic, void* ctx)
{
    std::string module(name, name_length);
    try
    {
        // Luau asserts on a module its resolver cannot read; report it instead.
        // Probe the provider directly: a clean frontend may still hold an older AST.
        if (!analysis->files.sourceExists(module))
        {
            emit(diagnostic, ctx, DB_DIAG_INTERNAL_ERROR, 0, nullptr, module, "module not found", Luau::Location{});
            return -1;
        }
        Luau::FrontendOptions options = analysis->options;
        options.runLintChecks = lint != 0;
        Luau::CheckResult result = analysis->frontend->check(module, options);
        int errors = 0;
        std::unordered_set<std::string> seen;
        std::vector<std::string> modules;
        reachableModules(*analysis->frontend, module, seen, modules);
        for (const auto& name : modules)
            if (const auto* snapshot = analysis->files.snapshot(name))
                errors += emitStructural(*snapshot, name, diagnostic, ctx);
        for (const Luau::TypeError& error : result.errors)
        {
            const auto* snapshot = analysis->files.snapshot(error.moduleName);
            if (snapshot && (recoveryNoise(*snapshot, error.location) ||
                (Luau::get_if<Luau::UnknownSymbol>(&error.data) && synthetic(*snapshot, error.location))))
                continue;
            ++errors;
            std::string text;
            if (const Luau::SyntaxError* syntax = Luau::get_if<Luau::SyntaxError>(&error.data))
            {
                text = analysis->files.parseMessage(error.moduleName, *syntax, error.location);
                emit(diagnostic, ctx, DB_DIAG_PARSE_ERROR, error.code(), nullptr, error.moduleName, text,
                    analysis->files.originalLocation(error.moduleName, error.location));
                continue;
            }
            Luau::TypeError display = error;
            if (auto* duplicate = Luau::get_if<Luau::DuplicateTypeDefinition>(&display.data); duplicate && duplicate->previousLocation)
                duplicate->previousLocation = analysis->files.originalLocation(error.moduleName, *duplicate->previousLocation);
            text = Luau::toString(display, Luau::TypeErrorToStringOptions{analysis->frontend->fileResolver});
            emit(diagnostic, ctx, DB_DIAG_TYPE_ERROR, error.code(), nullptr, error.moduleName, text,
                analysis->files.originalLocation(error.moduleName, error.location));
        }
        const auto* snapshot = analysis->files.snapshot(module);
        auto emitLint = [&](const Luau::LintWarning& warning, int kind) {
            if (snapshot && synthetic(*snapshot, warning.location))
                return;
            emit(diagnostic, ctx, kind, warning.code, Luau::LintWarning::getName(warning.code), module,
                analysis->files.lintMessage(module, warning), analysis->files.originalLocation(module, warning.location));
            if (kind == DB_DIAG_LINT_ERROR)
                ++errors;
        };
        for (const Luau::LintWarning& warning : result.lintResult.errors)
            emitLint(warning, DB_DIAG_LINT_ERROR);
        for (const Luau::LintWarning& warning : result.lintResult.warnings)
            emitLint(warning, DB_DIAG_LINT_WARNING);
        return errors;
    }
    catch (const std::exception& error)
    {
        emitInternal(diagnostic, ctx, module, error.what());
        return -1;
    }
    catch (...)
    {
        emitInternal(diagnostic, ctx, module, "unknown analysis failure");
        return -1;
    }
}

void db_analysis_mark_dirty(db_analysis* analysis, const char* name, size_t name_length)
{
    // Keep snapshots while the frontend still owns the corresponding cached ASTs.
    analysis->frontend->markDirty(std::string(name, name_length));
}

void db_analysis_clear(db_analysis* analysis)
{
    analysis->frontend->clear();
    analysis->files.snapshots.clear();
}

// Autocompletes `name` at (line, column) (0-based), reporting entries. Returns the context kind
// (Luau::AutocompleteContext) or -1 on failure.
int db_analysis_autocomplete(
    db_analysis* analysis, const char* name, size_t name_length, unsigned line, unsigned column, db_completion_fn completion, void* ctx)
{
    std::string module(name, name_length);
    try
    {
        if (!analysis->files.sourceExists(module))
            return -1;
        Luau::FrontendOptions options = analysis->options;
        options.forAutocomplete = true;
        options.retainFullTypeGraphs = true; // autocomplete reads per-term types
        options.runLintChecks = false;
        analysis->frontend->check(module, options);
        const auto* snapshot = analysis->files.snapshot(module);
        const auto* sourceModule = analysis->frontend->getSourceModule(module);
        if (!snapshot || !sourceModule || !sourceModule->root)
            return -1;
        const auto point = completionPoint(*snapshot, line, column);
        Luau::AutocompleteResult result = Luau::autocomplete(*analysis->frontend, module, Luau::Position(point.generated.line, point.generated.column),
            [](std::string, std::optional<const Luau::ExternType*>, std::optional<std::string>) { return std::nullopt; });
        GeneratedBindings generated(*snapshot);
        sourceModule->root->visit(&generated);
        for (const auto& [entryName, entry] : result.entryMap)
        {
            if (generated.names.count(entryName))
                continue;
            std::string type = entry.type ? Luau::toString(*entry.type) : std::string();
            completion(ctx, entryName.data(), entryName.size(), int(entry.kind), entry.deprecated ? 1 : 0, type.data(), type.size());
        }
        // Supplement stock expression/scope completion, never replace property,
        // type or string completion with surface punctuation suggestions.
        if (point.keyword && (result.context == Luau::AutocompleteContext::Unknown ||
            result.context == Luau::AutocompleteContext::Expression || result.context == Luau::AutocompleteContext::Statement ||
            result.context == Luau::AutocompleteContext::Keyword))
        {
            if (!result.entryMap.count(point.keyword))
                completion(ctx, point.keyword, strlen(point.keyword), int(Luau::AutocompleteEntryKind::Keyword), 0, "", 0);
            if (result.entryMap.empty())
                return int(Luau::AutocompleteContext::Keyword);
        }
        return int(result.context);
    }
    catch (...)
    {
        return -1;
    }
}

// Parses `source` standalone, reporting parse errors; writes the AST as JSON through `json`
// when given. Returns the error count, or -1 on an internal failure.
int db_parse(const char* source, size_t length, db_diagnostic_fn diagnostic, void* ctx, db_sink json, void* json_ctx)
{
    try
    {
        SourceSnapshot snapshot{std::string(source, length), L3i::Surface::lower(std::string_view(source, length), true)};
        auto& lowered = snapshot.lowered;
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::ParseOptions options;
        options.captureComments = true;
        Luau::ParseResult result = Luau::Parser::parse(lowered.source.data(), lowered.source.size(), names, allocator, options);
        int errors = emitStructural(snapshot, std::string(), diagnostic, ctx);
        for (const Luau::ParseError& error : result.errors)
        {
            if (recoveryNoise(snapshot, error.getLocation()))
                continue;
            const auto loc = error.getLocation();
            const std::string text = L3i::Surface::originalParseMessage(error.getMessage(), {loc.begin.line, loc.begin.column}, lowered.map);
            emit(diagnostic, ctx, DB_DIAG_PARSE_ERROR, 0, nullptr, std::string(), text,
                location(lowered.map.originalSpan({{loc.begin.line, loc.begin.column}, {loc.end.line, loc.end.column}})));
            ++errors;
        }
        L3i::Surface::remapLocations(result, lowered.map);
        if (json != nullptr && result.root != nullptr)
        {
            std::string text = Luau::toJson(result.root, result.commentLocations);
            json(json_ctx, text.data(), text.size());
        }
        return errors;
    }
    catch (const std::exception& error)
    {
        emitInternal(diagnostic, ctx, std::string(), error.what());
        return -1;
    }
    catch (...)
    {
        emitInternal(diagnostic, ctx, std::string(), "unknown parser failure");
        return -1;
    }
}

} // extern "C"
