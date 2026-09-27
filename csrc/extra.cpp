// dream-binder additions to the Luau C API. Luau leaves these to the embedder; they are the
// only pieces of the binder's C surface that are not stock Luau, and each is a thin reach into
// VM internals the public headers do not expose.

#include "lua.h"
#include "lapi.h"
#include "lgc.h"
#include "lobject.h"
#include "lstate.h"

#include "Luau/Common.h"

#include <string.h>

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
