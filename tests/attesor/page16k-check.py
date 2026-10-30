#!/usr/bin/env python3
"""Real libc page discovery must agree with the opt-in VM syscall policy."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = r'''
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/mman.h>
#include <mach/mach.h>
int main(int argc, char **argv) {
    unsigned long expected = strtoul(argv[1], 0, 10);
    vm_size_t host = 0;
    if (host_page_size(mach_host_self(), &host) || getpagesize() != expected ||
        sysconf(_SC_PAGESIZE) != expected || vm_page_size != expected ||
        vm_kernel_page_size != expected || host != expected) return 2;
    char *p = mmap(0, expected * 2, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED || mprotect(p + expected, expected, PROT_READ)) return 3;
    p[expected - 1] = 42;
    if (p[expected - 1] != 42 || munmap(p, expected * 2)) return 4;
    printf("PASS pagesize=%lu libc/commpage/VM agree; adjacent page preserved\n", expected);
    return 0;
}
'''


def main():
    base = {k: v for k, v in os.environ.items() if not k.startswith(('ATTESOR_', 'DYLD_'))}
    base.update(TMPDIR='/private/tmp', ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0')
    with tempfile.TemporaryDirectory(prefix='attesor-page16k-', dir='/private/tmp') as directory:
        root = Path(directory)
        source, guest = root/'pages.c', root/'pages'
        source.write_text(SOURCE)
        subprocess.run(['clang', '-O2', '-arch', 'arm64', source, '-o', guest], env=base, check=True)
        for label, on, off, size in [('default', '', '', 4096), ('on', 'page16k', '', 16384),
                                     ('off-wins', 'page16k', 'page16k', 4096),
                                     ('exact', 'xpage16k, page16k', '', 4096)]:
            for mode in ['--interp', '--no-aot']:
                p = subprocess.run([ROOT/'target/release/attesor', mode, '--no-aot', guest, str(size)],
                    env=dict(base, ATTESOR_JIT_ON=on, ATTESOR_JIT_OFF=off, ATTESOR_CACHE=str(root/'cache')),
                    text=True, capture_output=True, timeout=90)
                assert p.returncode == 0 and f'PASS pagesize={size}' in p.stdout, (label, mode, p.stdout, p.stderr)
                print(label, mode, p.stdout.strip(), flush=True)


if __name__ == '__main__':
    main()
