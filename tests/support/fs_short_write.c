#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

ssize_t write(int fd, const void *buffer, size_t count) {
    static ssize_t (*real_write)(int, const void *, size_t);
    static atomic_int injected;
    if (!real_write) real_write = dlsym(RTLD_NEXT, "write");
    const char *target = getenv("L3I_DURABILITY_FAULT_PATH");
    char link[64], path[4096];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(link, path, sizeof(path) - 1);
    if (length >= 0) path[length] = '\0';
    if (target && length >= 0 && strcmp(path, target) == 0 && count > 7 && !atomic_exchange(&injected, 1)) {
        return real_write(fd, buffer, 7);
    }
    return real_write(fd, buffer, count);
}
