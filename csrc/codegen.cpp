// l3i native code generation shim.
//
// Luau's code generator is a C++ API (Luau/CodeGen.h): shared code contexts, compilation with
// CompilationOptions, the host IR hooks that lower vector/userdata operations, and the IrBuilder
// those hooks drive. luacodegen.h exposes only create/compile-with-defaults, so this file gives
// every one of those C++ entry points a C ABI. Host hooks written in Rust receive an opaque
// IrBuilder handle and build IR through the db_ir_* functions below.
//
// Rules: nothing here throws across the C boundary (Luau's code generator does not throw; the
// Rust side aborts on panic), and IrOp travels as a 32-bit value (it is a 4+28 bit struct).

#include <Luau/CodeGen.h>
#include <Luau/IrBuilder.h>
#include <Luau/IrData.h>

#include <lua.h>

#include <array>
#include <cstdint>
#include <cstring>
#include <memory>

namespace CG = Luau::CodeGen;

static_assert(sizeof(CG::IrOp) == sizeof(uint32_t), "IrOp must travel as a 32-bit value");

extern "C" {

struct db_ir_builder; // opaque: Luau::CodeGen::IrBuilder

typedef uint8_t (*db_vector_type_fn)(void* context, const char* member, size_t length);
typedef bool (*db_vector_access_fn)(
    void* context, db_ir_builder* build, const char* member, size_t length, int result_reg, int source_reg, int pcpos);
typedef bool (*db_vector_namecall_fn)(void* context, db_ir_builder* build, const char* member, size_t length,
    int arg_res_reg, int source_reg, int params, int results, int pcpos);
typedef uint8_t (*db_userdata_type_fn)(void* context, uint8_t type, const char* member, size_t length);
typedef uint8_t (*db_userdata_metamethod_type_fn)(void* context, uint8_t lhs, uint8_t rhs, int method);
typedef bool (*db_userdata_access_fn)(void* context, db_ir_builder* build, uint8_t type, const char* member,
    size_t length, int result_reg, int source_reg, int pcpos);
typedef bool (*db_userdata_metamethod_fn)(void* context, db_ir_builder* build, uint8_t lhs, uint8_t rhs,
    int result_reg, uint32_t lhs_op, uint32_t rhs_op, int method, int pcpos);
typedef bool (*db_userdata_namecall_fn)(void* context, db_ir_builder* build, uint8_t type, const char* member,
    size_t length, int arg_res_reg, int source_reg, int params, int results, int pcpos);
typedef uint8_t (*db_remapper_fn)(void* context, const char* name, size_t length);

struct db_ir_hooks
{
    void* context;
    db_vector_type_fn vector_access_type;
    db_vector_type_fn vector_namecall_type;
    db_vector_access_fn vector_access;
    db_vector_namecall_fn vector_namecall;
    db_userdata_type_fn userdata_access_type;
    db_userdata_metamethod_type_fn userdata_metamethod_type;
    db_userdata_type_fn userdata_namecall_type;
    db_userdata_access_fn userdata_access;
    db_userdata_metamethod_fn userdata_metamethod;
    db_userdata_namecall_fn userdata_namecall;
};

struct db_compilation_options
{
    unsigned flags;
    bool record_counters;
    bool nop_padding;
    const char* const* userdata_types; // null-terminated, or null
    const db_ir_hooks* hooks; // or null
};

struct db_compilation_stats
{
    size_t bytecode_size_bytes;
    size_t native_code_size_bytes;
    size_t native_data_size_bytes;
    size_t native_metadata_size_bytes;
    uint32_t functions_total;
    uint32_t functions_compiled;
    uint32_t functions_bound;
};

} // extern "C"

namespace
{
    // HostIrHooks are context-free function pointers; compilation is synchronous, so the hooks
    // of the compilation in progress live here for its duration.
    thread_local const db_ir_hooks* g_hooks = nullptr;

    db_ir_builder* wrap(CG::IrBuilder& build)
    {
        return reinterpret_cast<db_ir_builder*>(&build);
    }

    CG::IrBuilder& unwrap(db_ir_builder* build)
    {
        return *reinterpret_cast<CG::IrBuilder*>(build);
    }

    uint32_t pack(CG::IrOp op)
    {
        uint32_t raw = 0;
        std::memcpy(&raw, &op, sizeof(raw));
        return raw;
    }

    CG::IrOp unpack(uint32_t raw)
    {
        CG::IrOp op;
        std::memcpy(&op, &raw, sizeof(op));
        return op;
    }

    uint8_t vectorAccessType(const char* member, size_t length)
    {
        return g_hooks->vector_access_type(g_hooks->context, member, length);
    }
    uint8_t vectorNamecallType(const char* member, size_t length)
    {
        return g_hooks->vector_namecall_type(g_hooks->context, member, length);
    }
    bool vectorAccess(CG::IrBuilder& build, const char* member, size_t length, int resultReg, int sourceReg, int pcpos)
    {
        return g_hooks->vector_access(g_hooks->context, wrap(build), member, length, resultReg, sourceReg, pcpos);
    }
    bool vectorNamecall(CG::IrBuilder& build, const char* member, size_t length, int argResReg, int sourceReg,
        int params, int results, int pcpos)
    {
        return g_hooks->vector_namecall(
            g_hooks->context, wrap(build), member, length, argResReg, sourceReg, params, results, pcpos);
    }
    uint8_t userdataAccessType(uint8_t type, const char* member, size_t length)
    {
        return g_hooks->userdata_access_type(g_hooks->context, type, member, length);
    }
    uint8_t userdataMetamethodType(uint8_t lhs, uint8_t rhs, CG::HostMetamethod method)
    {
        return g_hooks->userdata_metamethod_type(g_hooks->context, lhs, rhs, static_cast<int>(method));
    }
    uint8_t userdataNamecallType(uint8_t type, const char* member, size_t length)
    {
        return g_hooks->userdata_namecall_type(g_hooks->context, type, member, length);
    }
    bool userdataAccess(CG::IrBuilder& build, uint8_t type, const char* member, size_t length, int resultReg,
        int sourceReg, int pcpos)
    {
        return g_hooks->userdata_access(
            g_hooks->context, wrap(build), type, member, length, resultReg, sourceReg, pcpos);
    }
    bool userdataMetamethod(CG::IrBuilder& build, uint8_t lhs, uint8_t rhs, int resultReg, CG::IrOp lhsOp,
        CG::IrOp rhsOp, CG::HostMetamethod method, int pcpos)
    {
        return g_hooks->userdata_metamethod(g_hooks->context, wrap(build), lhs, rhs, resultReg, pack(lhsOp),
            pack(rhsOp), static_cast<int>(method), pcpos);
    }
    bool userdataNamecall(CG::IrBuilder& build, uint8_t type, const char* member, size_t length, int argResReg,
        int sourceReg, int params, int results, int pcpos)
    {
        return g_hooks->userdata_namecall(
            g_hooks->context, wrap(build), type, member, length, argResReg, sourceReg, params, results, pcpos);
    }

    CG::CompilationOptions translate(const db_compilation_options& options)
    {
        CG::CompilationOptions result;
        result.flags = options.flags;
        result.recordCounters = options.record_counters;
        result.nopPadding = options.nop_padding;
        result.userdataTypes = options.userdata_types;
        if (const db_ir_hooks* hooks = options.hooks)
        {
            // Only hooks the host implemented are exposed to Luau, so the generator never pays
            // for a round trip that would only answer "no".
            if (hooks->vector_access_type)
                result.hooks.vectorAccessBytecodeType = vectorAccessType;
            if (hooks->vector_namecall_type)
                result.hooks.vectorNamecallBytecodeType = vectorNamecallType;
            if (hooks->vector_access)
                result.hooks.vectorAccess = vectorAccess;
            if (hooks->vector_namecall)
                result.hooks.vectorNamecall = vectorNamecall;
            if (hooks->userdata_access_type)
                result.hooks.userdataAccessBytecodeType = userdataAccessType;
            if (hooks->userdata_metamethod_type)
                result.hooks.userdataMetamethodBytecodeType = userdataMetamethodType;
            if (hooks->userdata_namecall_type)
                result.hooks.userdataNamecallBytecodeType = userdataNamecallType;
            if (hooks->userdata_access)
                result.hooks.userdataAccess = userdataAccess;
            if (hooks->userdata_metamethod)
                result.hooks.userdataMetamethod = userdataMetamethod;
            if (hooks->userdata_namecall)
                result.hooks.userdataNamecall = userdataNamecall;
        }
        return result;
    }

    struct ActiveHooks
    {
        const db_ir_hooks* previous;
        explicit ActiveHooks(const db_ir_hooks* hooks)
            : previous(g_hooks)
        {
            g_hooks = hooks;
        }
        ~ActiveHooks()
        {
            g_hooks = previous;
        }
    };
}

extern "C" {

int db_codegen_supported(void)
{
    return CG::isSupported() ? 1 : 0;
}

void* db_codegen_create_shared_context(size_t block_size, size_t max_total_size)
{
    CG::UniqueSharedCodeGenContext context = block_size == 0 && max_total_size == 0
        ? CG::createSharedCodeGenContext()
        : CG::createSharedCodeGenContext(block_size, max_total_size, nullptr, nullptr);
    return context.release();
}

void db_codegen_destroy_shared_context(void* context)
{
    if (context != nullptr)
        CG::destroySharedCodeGenContext(static_cast<const CG::SharedCodeGenContext*>(context));
}

void db_codegen_create(lua_State* L, void* context)
{
    if (context != nullptr)
        CG::create(L, static_cast<CG::SharedCodeGenContext*>(context));
    else
        CG::create(L);
}

int db_codegen_is_native_execution_enabled(lua_State* L)
{
    return CG::isNativeExecutionEnabled(L) ? 1 : 0;
}

void db_codegen_set_native_execution_enabled(lua_State* L, int enabled)
{
    CG::setNativeExecutionEnabled(L, enabled != 0);
}

void db_codegen_disable_native_execution_for_function(lua_State* L, int level)
{
    CG::disableNativeExecutionForFunction(L, level);
}

void db_codegen_set_userdata_remapper(lua_State* L, void* context, db_remapper_fn remapper)
{
    CG::setUserdataRemapper(L, context, reinterpret_cast<CG::UserdataRemapperCallback*>(remapper));
}

int db_codegen_compile(lua_State* L, int idx, const uint8_t* module_id, const db_compilation_options* options,
    db_compilation_stats* out_stats)
{
    ActiveHooks active(options ? options->hooks : nullptr);
    const CG::CompilationOptions translated = options ? translate(*options) : CG::CompilationOptions{};
    CG::CompilationStats stats;
    CG::CompilationResult result;
    if (module_id != nullptr)
    {
        CG::ModuleId id;
        std::memcpy(id.data(), module_id, id.size());
        result = CG::compile(id, L, idx, translated, &stats);
    }
    else
    {
        result = CG::compile(L, idx, translated, &stats);
    }
    if (out_stats != nullptr)
    {
        out_stats->bytecode_size_bytes = stats.bytecodeSizeBytes;
        out_stats->native_code_size_bytes = stats.nativeCodeSizeBytes;
        out_stats->native_data_size_bytes = stats.nativeDataSizeBytes;
        out_stats->native_metadata_size_bytes = stats.nativeMetadataSizeBytes;
        out_stats->functions_total = stats.functionsTotal;
        out_stats->functions_compiled = stats.functionsCompiled;
        out_stats->functions_bound = stats.functionsBound;
    }
    return static_cast<int>(result.result);
}

// ---- IrBuilder surface -------------------------------------------------------------------------

uint32_t db_ir_inst(db_ir_builder* build, uint8_t cmd, const uint32_t* ops, size_t count)
{
    CG::IrOps operands;
    for (size_t i = 0; i < count && i < 8; ++i)
        operands.push_back(unpack(ops[i]));
    return pack(unwrap(build).inst(static_cast<CG::IrCmd>(cmd), operands));
}

uint32_t db_ir_undef(db_ir_builder* build)
{
    return pack(unwrap(build).undef());
}
uint32_t db_ir_const_int(db_ir_builder* build, int value)
{
    return pack(unwrap(build).constInt(value));
}
uint32_t db_ir_const_int64(db_ir_builder* build, int64_t value)
{
    return pack(unwrap(build).constInt64(value));
}
uint32_t db_ir_const_uint(db_ir_builder* build, unsigned value)
{
    return pack(unwrap(build).constUint(value));
}
uint32_t db_ir_const_import(db_ir_builder* build, unsigned value)
{
    return pack(unwrap(build).constImport(value));
}
uint32_t db_ir_const_double(db_ir_builder* build, double value)
{
    return pack(unwrap(build).constDouble(value));
}
uint32_t db_ir_const_tag(db_ir_builder* build, uint8_t value)
{
    return pack(unwrap(build).constTag(value));
}
uint32_t db_ir_cond(db_ir_builder* build, uint8_t condition)
{
    return pack(unwrap(build).cond(static_cast<CG::IrCondition>(condition)));
}
uint32_t db_ir_block(db_ir_builder* build, uint8_t kind)
{
    return pack(unwrap(build).block(static_cast<CG::IrBlockKind>(kind)));
}
uint32_t db_ir_block_at_inst(db_ir_builder* build, uint32_t index)
{
    return pack(unwrap(build).blockAtInst(index));
}
uint32_t db_ir_fallback_block(db_ir_builder* build, uint32_t pcpos)
{
    return pack(unwrap(build).fallbackBlock(pcpos));
}
void db_ir_begin_block(db_ir_builder* build, uint32_t block)
{
    unwrap(build).beginBlock(unpack(block));
}
void db_ir_load_and_check_tag(db_ir_builder* build, uint32_t location, uint8_t tag, uint32_t fallback)
{
    unwrap(build).loadAndCheckTag(unpack(location), tag, unpack(fallback));
}
uint32_t db_ir_vm_reg(db_ir_builder* build, uint8_t index)
{
    return pack(unwrap(build).vmReg(index));
}
uint32_t db_ir_vm_const(db_ir_builder* build, uint32_t index)
{
    return pack(unwrap(build).vmConst(index));
}
uint32_t db_ir_vm_upvalue(db_ir_builder* build, uint8_t index)
{
    return pack(unwrap(build).vmUpvalue(index));
}
uint32_t db_ir_vm_exit(db_ir_builder* build, uint32_t pcpos)
{
    return pack(unwrap(build).vmExit(pcpos));
}
int db_ir_in_terminated_block(db_ir_builder* build)
{
    return unwrap(build).inTerminatedBlock ? 1 : 0;
}

} // extern "C"

// ---- Assembly and IR dumps, perf log -------------------------------------------------------

extern "C" {

struct db_assembly_options
{
    int target; // 0 host, 1 A64, 2 A64 without extensions, 3 X64 Windows, 4 X64 System V
    bool include_assembly;
    bool include_ir;
    bool include_outlined_code;
    bool include_ir_types;
    bool include_reg_spills;
    const db_compilation_options* compilation; // or null for defaults
};

typedef void (*db_text_sink)(void* ctx, const char* data, size_t length);
typedef void (*db_perf_log_fn)(void* ctx, uintptr_t address, unsigned size, const char* symbol);

// Writes the assembly (and/or IR) Luau would generate for the closure at `idx` through `sink`.
// Returns 0 on success, 1 when the function could not be lowered, 2 on an exception.
int db_codegen_get_assembly(lua_State* L, int idx, const db_assembly_options* options, db_text_sink sink, void* ctx)
{
    try
    {
        CG::AssemblyOptions assembly;
        switch (options->target)
        {
        case 1:
            assembly.target = CG::AssemblyOptions::A64;
            break;
        case 2:
            assembly.target = CG::AssemblyOptions::A64_NoFeatures;
            break;
        case 3:
            assembly.target = CG::AssemblyOptions::X64_Windows;
            break;
        case 4:
            assembly.target = CG::AssemblyOptions::X64_SystemV;
            break;
        default:
            assembly.target = CG::AssemblyOptions::Host;
            break;
        }
        assembly.includeAssembly = options->include_assembly;
        assembly.includeIr = options->include_ir;
        assembly.includeOutlinedCode = options->include_outlined_code;
        assembly.includeIrTypes = options->include_ir_types;
        assembly.includeRegSpills = options->include_reg_spills;
        ActiveHooks active(options->compilation ? options->compilation->hooks : nullptr);
        if (options->compilation)
            assembly.compilationOptions = translate(*options->compilation);
        CG::LoweringStats stats;
        std::string text = CG::getAssembly(L, idx, assembly, &stats);
        sink(ctx, text.data(), text.size());
        return text.empty() ? 1 : 0;
    }
    catch (...)
    {
        return 2;
    }
}

// Installs (or clears, with a null function) the process-wide perf log that receives every
// natively compiled function's address, size, and symbol.
void db_codegen_set_perf_log(void* ctx, db_perf_log_fn log)
{
    CG::setPerfLog(ctx, reinterpret_cast<CG::PerfLogFn>(log));
}

} // extern "C"
