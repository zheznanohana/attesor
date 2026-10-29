// Host side (x86-64) exec redirection, injected with DYLD_INSERT_LIBRARIES into native programs that an
// Attesor guest starts (and, through them, into their native children).
//
// A native program such as /bin/sh cannot exec an arm64-only binary: the kernel answers "Bad CPU type in
// executable", and Attesor only sees the guest's own execs. Package managers, shell launchers and script
// runners are exactly such middlemen (Electron "node" wrappers, pnpm, bundled Python). Interpose execve,
// posix_spawn and posix_spawnp: an arm64-only target, or a #! script whose interpreter is arm64-only, is
// started as `attesor <program> args…`; everything else runs natively with this library passed on.
// If starting attesor fails, the original exec/spawn runs instead; ATTESOR_EXEC_OFF=1 disables the redirect.
#include <errno.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <libkern/OSByteOrder.h>

#include "attesor_exec_arch.h"

extern char **environ;

// 1: ARM-only or Attesor-managed universal Mach-O. 2: script with such an interpreter. 0: genuine native.
static int classify(const char *path, char *interp, char *iarg) {
    if (attesor_needs_translation(path)) return 1;
    char line[PATH_MAX + 64];
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 0;
    ssize_t n = read(fd, line, sizeof line - 1);
    close(fd);
    if (n < 3 || line[0] != '#' || line[1] != '!') return 0;
    line[n] = 0;
    char *p = line + 2, *e = strchr(p, '\n');
    if (e) *e = 0;
    while (*p == ' ' || *p == '\t') p++;
    char *sp = strpbrk(p, " \t");
    iarg[0] = 0;
    if (sp) {
        *sp++ = 0;
        while (*sp == ' ' || *sp == '\t') sp++;
        char *end = sp + strlen(sp);
        while (end > sp && (end[-1] == ' ' || end[-1] == '\t' || end[-1] == '\r')) *--end = 0;
        strlcpy(iarg, sp, PATH_MAX);
    }
    strlcpy(interp, p, PATH_MAX);
    return attesor_needs_translation(interp) ? 2 : 0;
}

static const char *self_path(void) {
    static _Thread_local char path[PATH_MAX];
    if (!path[0]) {
        Dl_info info;
        if (dladdr((void *)attesor_needs_translation, &info) && info.dli_fname) strlcpy(path, info.dli_fname, sizeof path);
    }
    return path;
}

static const char *attesor_bin(void) {
    static _Thread_local char path[PATH_MAX];
    if (!path[0]) {
        const char *e = getenv("ATTESOR_BIN"), *h = getenv("HOME");
        if (e && *e) strlcpy(path, e, sizeof path);
        else snprintf(path, sizeof path, "%s/bin/attesor", h ? h : "");
    }
    return path;
}

// Copy of envp that keeps this library in DYLD_INSERT_LIBRARIES and ATTESOR_BIN set (callers often
// build a fresh environment for their children). Leaks: only used right before exec/spawn.
static char **with_self(char *const envp[]) {
    const char *self = self_path();
    if (!envp) envp = environ;
    size_t n = 0;
    int has_ins = 0, has_bin = 0;
    for (; envp && envp[n]; n++) {
        if (!strncmp(envp[n], "DYLD_INSERT_LIBRARIES=", 22) && strstr(envp[n], self)) has_ins = 1;
        if (!strncmp(envp[n], "ATTESOR_BIN=", 12)) has_bin = 1;
    }
    char **out = calloc(n + 3, sizeof *out);
    size_t j = 0;
    for (size_t i = 0; i < n; i++) {
        if (!has_ins && !strncmp(envp[i], "DYLD_INSERT_LIBRARIES=", 22)) {
            asprintf(&out[j++], "%s:%s", envp[i], self);
            has_ins = 1;
        } else {
            out[j++] = envp[i];
        }
    }
    if (!has_ins && self[0]) asprintf(&out[j++], "DYLD_INSERT_LIBRARIES=%s", self);
    if (!has_bin) asprintf(&out[j++], "ATTESOR_BIN=%s", attesor_bin());
    return out;
}

static char **attesor_argv(const char *path, char *const argv[], int kind, const char *interp, const char *iarg) {
    size_t n = 0;
    while (argv && argv[n]) n++;
    char **out = calloc(n + 5, sizeof *out);
    size_t j = 0;
    out[j++] = (char *)attesor_bin();
    if (kind == 2) {
        out[j++] = (char *)interp;
        if (iarg[0]) out[j++] = (char *)iarg;
    }
    out[j++] = (char *)path;
    for (size_t i = 1; i < n; i++) out[j++] = argv[i];
    return out;
}

static int my_execve(const char *path, char *const argv[], char *const envp[]) {
    char interp[PATH_MAX], iarg[PATH_MAX];
    char **env = with_self(envp);
    int kind = classify(path, interp, iarg);
    if (kind && !getenv("ATTESOR_EXEC_OFF")) {
        execve(attesor_bin(), attesor_argv(path, argv, kind, interp, iarg), env);
        // attesor missing or broken: fall through to the plain exec (and its usual error)
    }
    return execve(path, argv, env);
}

static int my_posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa, const posix_spawnattr_t *attr,
                          char *const argv[], char *const envp[]) {
    char interp[PATH_MAX], iarg[PATH_MAX];
    char **env = with_self(envp);
    int kind = classify(path, interp, iarg);
    if (kind && !getenv("ATTESOR_EXEC_OFF")) {
        char **av = attesor_argv(path, argv, kind, interp, iarg);
        int r = posix_spawn(pid, attesor_bin(), fa, attr, av, env);
        free(av);
        if (r == 0) return 0;
        // attesor missing or broken: fall back to the plain spawn
    }
    return posix_spawn(pid, path, fa, attr, argv, env);
}

static int my_posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *fa, const posix_spawnattr_t *attr,
                           char *const argv[], char *const envp[]) {
    if (strchr(file, '/')) return my_posix_spawn(pid, file, fa, attr, argv, envp);
    // same search as posix_spawnp: the caller's PATH, first executable match
    const char *path = getenv("PATH");
    char buf[PATH_MAX], interp[PATH_MAX], iarg[PATH_MAX];
    for (const char *p = path ? path : "/usr/bin:/bin"; p && *p;) {
        const char *c = strchr(p, ':');
        size_t len = c ? (size_t)(c - p) : strlen(p);
        snprintf(buf, sizeof buf, "%.*s/%s", (int)len, len ? p : ".", file);
        if (access(buf, X_OK) == 0) {
            if (classify(buf, interp, iarg) && !getenv("ATTESOR_EXEC_OFF")) return my_posix_spawn(pid, buf, fa, attr, argv, envp);
            break;
        }
        p = c ? c + 1 : NULL;
    }
    return posix_spawnp(pid, file, fa, attr, argv, with_self(envp));
}

struct interpose { const void *replacement, *replacee; };
__attribute__((used)) static const struct interpose interposers[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)my_execve, (const void *)execve},
    {(const void *)my_posix_spawn, (const void *)posix_spawn},
    {(const void *)my_posix_spawnp, (const void *)posix_spawnp},
};
