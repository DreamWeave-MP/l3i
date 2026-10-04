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

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <memory>
#include <optional>
#include <string>

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

    std::string rewriteSurfaceSyntax(std::string source)
    {
        size_t length = 0;
        char* rewritten = l3i_rewrite_surface_syntax(source.data(), source.size(), &length);
        if (rewritten == nullptr)
            return source;
        std::string result(rewritten, length);
        std::free(rewritten);
        return result;
    }

    struct HostFileResolver : Luau::FileResolver
    {
        db_source_provider provider;

        std::optional<Luau::SourceCode> readSource(const Luau::ModuleName& name) override
        {
            std::string source;
            int type = 0;
            if (!provider.read_source(provider.ctx, name.data(), name.size(), appendToString, &source, &type))
                return std::nullopt;
            Luau::SourceCode::Type kind = type == 2 ? Luau::SourceCode::Script : Luau::SourceCode::Module;
            return Luau::SourceCode{rewriteSurfaceSyntax(std::move(source)), kind};
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

// Type checks (and lints) `name`, reporting every diagnostic. Returns the number of type errors
// plus lint errors, or -1 on an internal failure (reported as a diagnostic too).
int db_analysis_check(db_analysis* analysis, const char* name, size_t name_length, int lint, db_diagnostic_fn diagnostic, void* ctx)
{
    std::string module(name, name_length);
    try
    {
        // Luau asserts on a module its resolver cannot read; report it instead.
        if (!analysis->files.readSource(module))
        {
            emit(diagnostic, ctx, DB_DIAG_INTERNAL_ERROR, 0, nullptr, module, "module not found", Luau::Location{});
            return -1;
        }
        Luau::FrontendOptions options = analysis->options;
        options.runLintChecks = lint != 0;
        Luau::CheckResult result = analysis->frontend->check(module, options);
        for (const Luau::TypeError& error : result.errors)
        {
            std::string text;
            if (const Luau::SyntaxError* syntax = Luau::get_if<Luau::SyntaxError>(&error.data))
            {
                text = syntax->message;
                emit(diagnostic, ctx, DB_DIAG_PARSE_ERROR, error.code(), nullptr, error.moduleName, text, error.location);
                continue;
            }
            text = Luau::toString(error, Luau::TypeErrorToStringOptions{analysis->frontend->fileResolver});
            emit(diagnostic, ctx, DB_DIAG_TYPE_ERROR, error.code(), nullptr, error.moduleName, text, error.location);
        }
        for (const Luau::LintWarning& warning : result.lintResult.errors)
            emit(diagnostic, ctx, DB_DIAG_LINT_ERROR, warning.code, Luau::LintWarning::getName(warning.code), module, warning.text, warning.location);
        for (const Luau::LintWarning& warning : result.lintResult.warnings)
            emit(diagnostic, ctx, DB_DIAG_LINT_WARNING, warning.code, Luau::LintWarning::getName(warning.code), module, warning.text, warning.location);
        return int(result.errors.size() + result.lintResult.errors.size());
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
    analysis->frontend->markDirty(std::string(name, name_length));
}

void db_analysis_clear(db_analysis* analysis)
{
    analysis->frontend->clear();
}

// Autocompletes `name` at (line, column) (0-based), reporting entries. Returns the context kind
// (Luau::AutocompleteContext) or -1 on failure.
int db_analysis_autocomplete(
    db_analysis* analysis, const char* name, size_t name_length, unsigned line, unsigned column, db_completion_fn completion, void* ctx)
{
    std::string module(name, name_length);
    try
    {
        Luau::FrontendOptions options = analysis->options;
        options.forAutocomplete = true;
        options.retainFullTypeGraphs = true; // autocomplete reads per-term types
        options.runLintChecks = false;
        analysis->frontend->check(module, options);
        Luau::AutocompleteResult result =
            Luau::autocomplete(*analysis->frontend, module, Luau::Position(line, column), [](std::string, std::optional<const Luau::ExternType*>,
                                                                                            std::optional<std::string>) { return std::nullopt; });
        for (const auto& [entryName, entry] : result.entryMap)
        {
            std::string type = entry.type ? Luau::toString(*entry.type) : std::string();
            completion(ctx, entryName.data(), entryName.size(), int(entry.kind), entry.deprecated ? 1 : 0, type.data(), type.size());
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
        std::string rewritten = rewriteSurfaceSyntax(std::string(source, length));
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::ParseOptions options;
        options.captureComments = true;
        Luau::ParseResult result = Luau::Parser::parse(rewritten.data(), rewritten.size(), names, allocator, options);
        for (const Luau::ParseError& error : result.errors)
            emit(diagnostic, ctx, DB_DIAG_PARSE_ERROR, 0, nullptr, std::string(), error.getMessage(), error.getLocation());
        if (json != nullptr && result.root != nullptr)
        {
            std::string text = Luau::toJson(result.root, result.commentLocations);
            json(json_ctx, text.data(), text.size());
        }
        return int(result.errors.size());
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
