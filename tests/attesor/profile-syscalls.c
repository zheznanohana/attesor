// Deterministic arm64 workload for syscall identities and host/emulation timing.
// Files and profile output are supplied by the test driver inside its output dir.
#include <fcntl.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/event.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

static void *sleeper(void *unused) {
    (void)unused;
    usleep(350000);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    const int iterations = 20000;
    pthread_t thread;
    if (pthread_create(&thread, NULL, sleeper, NULL)) return 3;
    int kq = kqueue();
    if (kq < 0) return 4;
    unsigned long long checksum = 0;
    for (int i = 0; i < iterations; i++) {
        struct stat st;
        if (stat(argv[1], &st)) return 5;
        int fd = open(argv[1], O_RDONLY);
        if (fd < 0) return 6;
        char buf[32];
        if (read(fd, buf, sizeof buf) != sizeof buf || close(fd)) return 7;
        // libc caches getpid on some releases. An explicit SVC tests the BSD
        // entry every iteration; every tenth call tests indirect syscall #0.
        register unsigned long x0 __asm__("x0") = i % 10 == 0 ? 20 : 0;
        register unsigned long x16 __asm__("x16") = i % 10 == 0 ? 0 : 20;
        __asm__ volatile("svc #0x80" : "+r"(x0) : "r"(x16) : "cc", "memory");
        checksum += x0;
        struct timeval tv;
        if (gettimeofday(&tv, NULL)) return 8;
        checksum += mach_absolute_time();
        struct timespec zero = {0, 0};
        struct kevent event;
        if (kevent(kq, NULL, 0, &event, 1, &zero) != 0) return 9;
    }
    if (close(kq) || pthread_join(thread, NULL)) return 10;
    printf("profile-syscalls: PASS iterations=%d checksum=%llu\n", iterations, checksum);
    return 0;
}
