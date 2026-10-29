// Translation selection shared by the native exec shim and its independent tests.
// A universal app's genuine x86 slice is native. An Attesor-owned x86 launcher
// is only a GUI entry point: programmatic execution must select the ARM slice.
#ifndef ATTESOR_EXEC_ARCH_H
#define ATTESOR_EXEC_ARCH_H
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <libkern/OSByteOrder.h>

static int attesor_read_at(int fd, uint64_t off, void *out, size_t n) {
    unsigned char *p = out;
    while (n) {
        ssize_t got = pread(fd, p, n, (off_t)off);
        if (got < 0 && errno == EINTR) continue;
        if (got <= 0) return 0;
        p += got; off += (uint64_t)got; n -= (size_t)got;
    }
    return 1;
}

static int attesor_marker(int fd, uint64_t off, uint64_t len) {
    static const char prefix[] = "ATTESOR_STUB_V";
    unsigned char b[4096]; size_t matched = 0; int digits = 0;
    while (len) {
        size_t n = len < sizeof b ? (size_t)len : sizeof b;
        if (!attesor_read_at(fd, off, b, n)) return 0;
        for (size_t i = 0; i < n; i++) {
            unsigned char c = b[i];
            if (matched < sizeof prefix - 1 && c == (unsigned char)prefix[matched]) matched++;
            else if (matched == sizeof prefix - 1 && c >= '0' && c <= '9') digits = 1;
            else if (matched == sizeof prefix - 1 && digits && c == 0) return 1;
            else { matched = c == (unsigned char)prefix[0]; digits = 0; }
        }
        off += n; len -= n;
    }
    return 0;
}

static int attesor_owned_x86(int fd, uint64_t base, uint64_t size) {
    unsigned char h[32], cmd[72], sect[80];
    if (size < sizeof h || !attesor_read_at(fd, base, h, sizeof h) ||
        OSReadLittleInt32(h, 0) != 0xfeedfacf || OSReadLittleInt32(h, 4) != 0x01000007) return 0;
    uint32_t ncmds = OSReadLittleInt32(h, 16), bytes = OSReadLittleInt32(h, 20);
    if (ncmds > 4096 || bytes > size - sizeof h) return 0;
    uint64_t at = sizeof h, end = at + bytes; int found = 0;
    for (uint32_t i = 0; i < ncmds; i++) {
        if (at > end || end - at < 8 || !attesor_read_at(fd, base + at, cmd, 8)) return 0;
        uint32_t kind = OSReadLittleInt32(cmd, 0), n = OSReadLittleInt32(cmd, 4);
        if (n < 8 || n > end - at) return 0;
        if (kind == 0x19) {
            if (n < sizeof cmd || !attesor_read_at(fd, base + at, cmd, sizeof cmd)) return 0;
            uint32_t count = OSReadLittleInt32(cmd, 64);
            if (count > (n - sizeof cmd) / sizeof sect) return 0;
            for (uint32_t j = 0; j < count; j++) {
                if (!attesor_read_at(fd, base + at + sizeof cmd + j * sizeof sect, sect, sizeof sect)) return 0;
                if (!memcmp(sect + 16, "__TEXT\0", 7) &&
                    (!memcmp(sect, "__const\0", 8) || !memcmp(sect, "__cstring\0", 10))) {
                    uint64_t len = OSReadLittleInt64(sect, 40), off = OSReadLittleInt32(sect, 48);
                    if (off < end || off > size || len > size - off) return 0;
                    found |= attesor_marker(fd, base + off, len);
                }
            }
        }
        at += n;
    }
    return found && at == end;
}

static int attesor_needs_translation(const char *path) {
    unsigned char h[32], entry[32]; struct stat st;
    int fd = open(path, O_RDONLY | O_CLOEXEC), result = 0;
    if (fd < 0) return 0;
    if (fstat(fd, &st) || st.st_size < 32 || !attesor_read_at(fd, 0, h, 32)) goto done;
    uint64_t size = (uint64_t)st.st_size;
    uint32_t magic = OSReadBigInt32(h, 0);
    int little = magic == 0xbebafeca || magic == 0xbfbafeca;
    int wide = magic == 0xcafebabf || magic == 0xbfbafeca;
    if (magic == 0xcafebabe || magic == 0xcafebabf || little) {
        uint32_t count = little ? OSReadLittleInt32(h, 4) : OSReadBigInt32(h, 4);
        uint64_t step = wide ? 32 : 20, table = 8 + step * count;
        if (!count || count > 64 || table > size) goto done;
        int arm = 0, x86 = 0, owned = 0;
        for (uint32_t i = 0; i < count; i++) {
            if (!attesor_read_at(fd, 8 + i * step, entry, (size_t)step)) goto done;
            uint32_t cpu = little ? OSReadLittleInt32(entry, 0) : OSReadBigInt32(entry, 0);
            uint64_t off = wide ? (little ? OSReadLittleInt64(entry, 8) : OSReadBigInt64(entry, 8)) :
                                  (little ? OSReadLittleInt32(entry, 8) : OSReadBigInt32(entry, 8));
            uint64_t len = wide ? (little ? OSReadLittleInt64(entry, 16) : OSReadBigInt64(entry, 16)) :
                                  (little ? OSReadLittleInt32(entry, 12) : OSReadBigInt32(entry, 12));
            if (off < table || off > size || len < 32 || len > size - off) goto done;
            if (cpu == 0x0100000c) arm = 1;
            if (cpu == 0x01000007) {
                if (x86) goto done;
                x86 = 1; owned = attesor_owned_x86(fd, off, len);
            }
        }
        result = arm && (!x86 || owned);
    } else {
        result = OSReadLittleInt32(h, 0) == 0xfeedfacf && OSReadLittleInt32(h, 4) == 0x0100000c;
    }
done:
    close(fd); return result;
}
#endif
