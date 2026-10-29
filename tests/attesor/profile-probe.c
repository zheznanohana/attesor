// Real guest workload for the sampling profiler; no native thunks required.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/wait.h>
#include <zlib.h>

static volatile unsigned long checksum;
static int compare(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}
__attribute__((noinline)) static void main_work(void) {
    unsigned long x = 1;
    for (unsigned long i = 0; i < 100000000; i++) x = x * 1664525 + 1013904223;
    checksum = x;
}
static void *sleep_worker(void *p) { (void)p; usleep(350000); return NULL; }
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--wait")) {
        puts("PROFILE_READY"); fflush(stdout);
        for (;;) sleep(1);
    }
    if (argc > 1 && !strcmp(argv[1], "--interp-probe")) {
        for (unsigned long i = 0; i < 1000000; i++) checksum += i;
        usleep(200000);
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "--fork")) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, sleep_worker, NULL)) return 3;
        int pid = fork();
        if (pid < 0) return 4;
        usleep(250000);
        if (!pid) _exit(0);
        pthread_join(thread, NULL);
        int status;
        waitpid(pid, &status, 0);
        return WIFEXITED(status) ? WEXITSTATUS(status) : 5;
    }
    if (argc > 1 && !strcmp(argv[1], "--exec-fail")) {
        char *args[] = {"missing", NULL};
        execv("/attesor-profiler-missing-executable", args);
        usleep(200000);
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "--exec")) {
        usleep(100000);
        char *args[] = {argv[0], "--after-exec", NULL};
        execv(argv[0], args);
        return 6;
    }
    const size_t n = 128 * 1024;
    unsigned char *a = malloc(n), *b = malloc(n), *z = malloc(compressBound(n));
    int *values = malloc(16384 * sizeof(int));
    if (!a || !b || !z || !values) return 2;
    for (size_t i = 0; i < n - 1; i++) a[i] = 'a' + ((i * 61 + i / 31) % 26);
    a[n - 1] = 0;
    for (int k = 0; k < 180; k++) {
        memcpy(b, a, n);
        checksum += strlen((char *)b);
        for (int i = 0; i < 16384; i++) values[i] = (i * 71023 + k) & 65535;
        qsort(values, 16384, sizeof(int), compare);
        uLongf out = compressBound(n);
        if (compress(z, &out, a, n) != Z_OK) return 1;
        checksum += out;
    }
    main_work();
    usleep(300000);
    puts("profile probe: PASS");
    free(a); free(b); free(z); free(values);
    return 0;
}
