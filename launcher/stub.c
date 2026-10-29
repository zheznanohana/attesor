// x86-64 slice added next to the untouched ARM slice(s). Finder/LaunchServices
// can launch the bundle, and this entry delegates to Attesor's shared runner.
#include <errno.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

__attribute__((used)) static const char marker[] = "ATTESOR_STUB_V2";

int main(int argc, char **argv) {
    char exe[PATH_MAX], real[PATH_MAX], runner[PATH_MAX];
    uint32_t n = sizeof exe;
    if (_NSGetExecutablePath(exe, &n) != 0 || !realpath(exe, real)) return 126;
    const char *home = getenv("HOME");
    if (!home || !*home) {
        struct passwd *pw = getpwuid(getuid());
        home = pw ? pw->pw_dir : "";
    }
    int written = snprintf(runner, sizeof runner, "%s/attesor-launcher/run-app", home);
    if (written < 0 || written >= sizeof runner) return 126;
    char **av = calloc((size_t)argc + 2, sizeof *av);
    if (!av) return 126;
    av[0] = runner;
    av[1] = real;
    for (int i = 1; i < argc; i++) av[i + 1] = argv[i];
    execv(runner, av);
    int error = errno;
    fprintf(stderr, "Attesor runner %s: %s\n", runner, strerror(error));
    free(av);
    const char *no_alert = getenv("ATTESOR_NO_ALERT");
    if (!no_alert || strcmp(no_alert, "1") != 0) {
        // Paths are data arguments, never interpolated AppleScript source.
        pid_t child = fork();
        if (child == 0) {
            execl("/usr/bin/osascript", "osascript", "-e",
                  "on run argv\n display alert \"This app needs Attesor\" message "
                  "((item 1 of argv) & \" requires Attesor. Reinstall Attesor, or restore the app with attesor-apps. Runner: \" & (item 2 of argv))\nend run",
                  "--", real, runner, (char *)NULL);
            _exit(127);
        }
        if (child > 0) while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
    }
    // An alert succeeding must not hide the original launch failure.
    return 127;
}
