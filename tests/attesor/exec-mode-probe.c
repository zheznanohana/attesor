// Common ARM/native leaf and middleman. Prints a deterministic transcript of
// argv, mode, cwd, stdin and inherited IPC FD; parents return the leaf's status.
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
#ifdef OWNED_STUB
__attribute__((used)) static const char owned[] = "ATTESOR_STUB_V2";
int main(void) { fputs("WRONG: native GUI stub was executed\n", stderr); return 103; }
#else
// The ARM implementation deliberately has the same text: it must not make a
// genuine x86+ARM universal executable look owned.
#ifdef __aarch64__
__attribute__((used)) static const char guest_marker[] = "ATTESOR_STUB_V2";
#endif
static void hex(const char *name, const char *s) {
    printf("%s=", name); for (; *s; s++) printf("%02x", (unsigned char)*s); puts("");
}
static void *concurrent_spawn(void *p) {
    char **argv = p; pid_t pid; int status;
    if (posix_spawn(&pid, argv[0], NULL, NULL, argv, environ) || waitpid(pid, &status, 0) != pid)
        return (void *)1;
    return WIFEXITED(status) && WEXITSTATUS(status) == 17 ? NULL : (void *)1;
}
int main(int argc, char **argv) {
    if (argc > 4 && (!strcmp(argv[1], "spawn0") || !strcmp(argv[1], "exec0"))) {
        char **child = argv + 3;
        if (!strcmp(argv[1], "exec0")) { execve(argv[2], child, environ); return 120; }
        pid_t pid; int status;
        if (posix_spawn(&pid, argv[2], NULL, NULL, child, environ)) return 121;
        if (waitpid(pid, &status, 0) != pid) return 122;
        return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
    }
    if (argc > 2 && !strcmp(argv[1], "threads")) {
        pthread_t threads[8];
        for (int i = 0; i < 8; i++) if (pthread_create(&threads[i], NULL, concurrent_spawn, argv + 2)) return 125;
        for (int i = 0; i < 8; i++) { void *status; if (pthread_join(threads[i], &status) || status) return 126; }
        return 17;
    }
    if (argc > 2 && (!strcmp(argv[1], "spawn") || !strcmp(argv[1], "spawnp") || !strcmp(argv[1], "exec"))) {
        if (!strcmp(argv[1], "exec")) { execve(argv[2], argv + 2, environ); perror("execve"); return 120; }
        pid_t pid; int status;
        int rc = !strcmp(argv[1], "spawnp") ? posix_spawnp(&pid, argv[2], NULL, NULL, argv + 2, environ) :
                                            posix_spawn(&pid, argv[2], NULL, NULL, argv + 2, environ);
        if (rc) { fprintf(stderr, "spawn error %d\n", rc); return 121; }
        if (waitpid(pid, &status, 0) != pid) return 122;
        return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
    }
#ifdef __aarch64__
    puts("arch=arm64");
#else
    puts("arch=x86_64");
#endif
    if (getenv("TEST_ARGV0")) hex("argv0", argv[0]);
    for (int i = 1; i < argc; i++) hex("arg", argv[i]);
    hex("mode", getenv("ELECTRON_RUN_AS_NODE") ? getenv("ELECTRON_RUN_AS_NODE") : "<unset>");
    hex("channel", getenv("NODE_CHANNEL_FD") ? getenv("NODE_CHANNEL_FD") : "<unset>");
    char cwd[4096], in[128]; if (!getcwd(cwd, sizeof cwd)) return 123;
    if (getenv("TEST_SKIP_STDIN")) strcpy(in, "skipped\n");
    else if (!fgets(in, sizeof in, stdin)) return 123;
    hex("cwd", cwd); hex("stdin", in);
    const char *fd = getenv("TEST_IPC_FD");
    if (!fd || write(atoi(fd), "IPC-preserved", 13) != 13) return 124;
    return 17;
}
#endif
