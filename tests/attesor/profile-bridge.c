// Exercise the real bridge protocol without opening a device or warming Metal.
// Deliberately missing handles test timed early returns and a selector waiting
// for an asynchronous handle, on two concurrent guest/bridge thread pairs.
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "../../gpu/mclproto.h"

static unsigned char request[4096];
static size_t request_len;
static void *call(uint64_t op, const void *input, uint64_t len, uint64_t *outlen) {
    register uint64_t x0 __asm__("x0") = op;
    register uint64_t x1 __asm__("x1") = (uint64_t)input;
    register uint64_t x2 __asm__("x2") = len;
    register uint64_t x16 __asm__("x16") = 0x4d43;
    __asm__ volatile("svc #0x80" : "+r"(x0), "+r"(x1) : "r"(x2), "r"(x16) : "memory", "cc");
    *outlen = x1;
    return (void *)x0;
}
static void *worker(void *unused) {
    (void)unused;
    for (int i = 0; i < 3; i++) {
        struct { MclMsgHdr hdr; uint64_t words[4]; } batch = {
            { MCL_MAGIC, 32, 0, 0 },
            { 32ull | (uint64_t)MCL_R_SYNC << 32, 1, MCL_GUEST_HANDLE_BASE, 0 }
        };
        uint64_t n;
        MclReply *reply = call(MCL_OP_BATCH, &batch, sizeof batch, &n);
        if (!reply || n < sizeof *reply || reply->tag != MCL_T_EXC) return (void *)1;
        void *rpc = call(MCL_OP_RPC, request, request_len, &n);
        if (!rpc || n == 0) return (void *)2;
        call(MCL_OP_FREE, rpc, 0, &n);
    }
    return NULL;
}
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    if (!strcmp(argv[1], "--legacy")) {
        uint64_t n;
        for (int i = 0; i < 7; i++) call(MCL_OP_FREE, NULL, 0, &n);
        puts("profile-bridge legacy: PASS");
        return 0;
    }
    FILE *file = fopen(argv[1], "rb");
    if (!file) return 3;
    request_len = fread(request, 1, sizeof request, file);
    fclose(file);
    struct { uint32_t id; char name[12]; } def = {1, "description"};
    uint64_t n;
    call(MCL_OP_DEFSEL, &def, sizeof def, &n);
    pthread_t a, b;
    if (pthread_create(&a, NULL, worker, NULL) || pthread_create(&b, NULL, worker, NULL)) return 4;
    void *ra, *rb;
    if (pthread_join(a, &ra) || pthread_join(b, &rb) || ra || rb) return 5;
    puts("profile-bridge: PASS");
    return 0;
}
