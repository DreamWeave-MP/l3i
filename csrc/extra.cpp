// l3i additions to the Luau C API. Luau leaves these to the embedder; they are the
// only pieces of the binder's C surface that are not stock Luau, and each is a thin reach into
// VM internals the public headers do not expose.

#include "lua.h"
#include "lapi.h"
#include "lgc.h"
#include "lobject.h"
#include "lstate.h"

#include "Luau/Common.h"

#include <stdio.h>
#include <string.h>

namespace {

// Luau's assertions (enabled by build.rs) call this before trapping; without a handler a failed
// assertion is a bare SIGILL with no message.
int reportAssertion(const char* expression, const char* file, int line, const char* function)
{
    fprintf(stderr, "l3i: Luau assertion failed: %s (%s:%d, %s)\n", expression, file, line, function);
    fflush(stderr);
    return 1;
}

struct InstallAssertionHandler
{
    InstallAssertionHandler()
    {
        Luau::assertHandler() = reportAssertion;
    }
} installAssertionHandler;

} // namespace

extern "C" {

// The identity of a table's or userdata's metatable without pushing it: exact-identity checks
// for untagged userdata and read-only views.
const void* lua_getmetatablepointer(lua_State* L, int objindex)
{
    const TValue* obj = luaA_toobject(L, objindex);
    if (obj == nullptr)
        return nullptr;
    switch (ttype(obj))
    {
    case LUA_TTABLE:
        return hvalue(obj)->metatable;
    case LUA_TUSERDATA:
        return uvalue(obj)->metatable;
    default:
        return nullptr;
    }
}

// Luau's heap dump (luaC_dump) with a memory-category naming callback; `file` is a FILE*.
void lua_gcdump(lua_State* L, void* file, const char* (*categoryName)(lua_State* L, uint8_t memcat))
{
    luaC_dump(L, file, categoryName);
}

// Sets a boolean fast flag by name; returns 1 when the flag exists in this build.
int luau_setfflag(const char* name, int value)
{
    for (Luau::FValue<bool>* flag = Luau::FValue<bool>::list; flag; flag = flag->next)
    {
        if (strcmp(flag->name, name) == 0)
        {
            flag->value = value != 0;
            return 1;
        }
    }
    return 0;
}

// Reads a boolean fast flag by name; returns -1 when the flag does not exist.
int luau_getfflag(const char* name)
{
    for (Luau::FValue<bool>* flag = Luau::FValue<bool>::list; flag; flag = flag->next)
        if (strcmp(flag->name, name) == 0)
            return flag->value ? 1 : 0;
    return -1;
}

// Sets an integer fast flag by name; returns 1 when it exists.
int luau_setfint(const char* name, int value)
{
    for (Luau::FValue<int>* flag = Luau::FValue<int>::list; flag; flag = flag->next)
    {
        if (strcmp(flag->name, name) == 0)
        {
            flag->value = value;
            return 1;
        }
    }
    return 0;
}

// Reads an integer fast flag by name into `out`; returns 1 when it exists.
int luau_getfint(const char* name, int* out)
{
    for (Luau::FValue<int>* flag = Luau::FValue<int>::list; flag; flag = flag->next)
    {
        if (strcmp(flag->name, name) == 0)
        {
            *out = flag->value;
            return 1;
        }
    }
    return 0;
}

// Enumerates the boolean fast flags this build registered: calls `visit` with each name and
// current value. Flags live in object files the linker kept, so the list is the truth for this
// binary, not for the headers.
void luau_visitfflags(void* context, void (*visit)(void* context, const char* name, int value))
{
    for (Luau::FValue<bool>* flag = Luau::FValue<bool>::list; flag; flag = flag->next)
        visit(context, flag->name, flag->value ? 1 : 0);
}

} // extern "C"

extern "C" {

// One call for the binder's scalar readers: the slot's type tag, plus the payload when it is a
// number or an integer. Replaces a lua_type + lua_tonumberx/lua_tointeger64 pair, and never
// coerces strings (lua_tonumberx would). Returns LUA_TNONE for a nonexistent slot.
int l3i_read_scalar(lua_State* L, int idx, double* number, int64_t* integer)
{
    const TValue* o = luaA_toobject(L, idx);
    if (o == nullptr)
        return LUA_TNONE;
    if (ttisnumber(o))
    {
        *number = nvalue(o);
        return LUA_TNUMBER;
    }
    if (ttisinteger(o))
    {
        *integer = lvalue(o);
        return LUA_TINTEGER;
    }
    return ttype(o);
}

// One call for tagged userdata checks: the payload pointer and the tag, or null for anything
// that is not a full userdata.
void* l3i_touserdata_tag(lua_State* L, int idx, int* tag)
{
    const TValue* o = luaA_toobject(L, idx);
    if (o == nullptr || !ttisuserdata(o))
        return nullptr;
    Udata* u = uvalue(o);
    *tag = u->tag;
    return u->data;
}

} // extern "C"

extern "C" {

// Everything a bound function's entry needs in one call: the argument count, the thread's data
// slot (the binder's per-thread record), and upvalue 1 (the closure context userdata payload,
// or null). Replaces lua_gettop + lua_getthreaddata + lua_touserdata(lua_upvalueindex(1)).
void* l3i_native_enter(lua_State* L, int* top, void** threaddata, const TValue** base)
{
    *top = int(L->top - L->base);
    *threaddata = L->userdata;
    *base = L->base;
    // Upvalue 1 straight from the running C closure: the pseudo-index route (pseudo2addr) was
    // a twentieth of a bound call.
    Closure* cl = curr_func(L);
    const TValue* uv = &cl->c.upvals[0];
    if (!ttisuserdata(uv))
        return nullptr;
    return uvalue(uv)->data;
}

// The binder reads argument slots straight from Luau's TValue layout (src/convert/raw.rs mirrors
// it). These pin the layout of this exact Luau build; a bump that moves a byte fails here.
static_assert(sizeof(TValue) == 16, "l3i mirrors a 16-byte TValue");
static_assert(offsetof(TValue, value) == 0, "l3i mirrors the value union at offset 0");
static_assert(offsetof(TValue, extra) == 8, "l3i mirrors extra at offset 8");
static_assert(offsetof(TValue, tt) == 12, "l3i mirrors the tag at offset 12");
static_assert(LUA_VECTOR_SIZE == 3, "l3i mirrors three-component vectors");
static_assert(offsetof(Udata, tag) == 3, "l3i mirrors the userdata tag at offset 3");
static_assert(offsetof(Udata, data) == 16, "l3i mirrors the userdata payload at offset 16");
static_assert(offsetof(TString, len) == 20, "l3i mirrors the string length at offset 20");
static_assert(offsetof(TString, data) == 24, "l3i mirrors the string bytes at offset 24");
static_assert(offsetof(Buffer, len) == 4, "l3i mirrors the buffer length at offset 4");
static_assert(offsetof(Buffer, data) == 8, "l3i mirrors the buffer bytes at offset 8");

// The running call's first argument slot (`L->base`): the first argument of a C function, the
// receiver of a direct userdata callback. Valid until something grows the stack.
const TValue* l3i_call_base(lua_State* L)
{
    return L->base;
}

// The slot at `idx` for the layout self-test, or null.
const TValue* l3i_stack_slot(lua_State* L, int idx)
{
    return luaA_toobject(L, idx);
}

// One call for the generated and planned dispatchers: the argument count, the thread's data
// slot (the binder's per-thread record), and the first argument slot.
int l3i_direct_enter(lua_State* L, void** threaddata, const TValue** base)
{
    *threaddata = L->userdata;
    *base = L->base;
    return lua_gettop(L);
}

// One call for vector reads: copies the components and returns 1, or returns 0 for any other
// type without touching `out`.
int l3i_read_vector(lua_State* L, int idx, float* out)
{
    const TValue* o = luaA_toobject(L, idx);
    if (o == nullptr || !ttisvector(o))
        return 0;
    const float* v = vvalue(o);
    out[0] = v[0];
    out[1] = v[1];
    out[2] = v[2];
    return 1;
}

} // extern "C"
