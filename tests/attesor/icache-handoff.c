// Cooperative cross-thread code publication. The executor parks before the
// writer changes a separate target page, then acquire+ISB precedes re-entry.
// This is full synchronization, not unsynchronized concurrent MOV replacement.
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#ifdef __aarch64__

typedef struct {
    _Atomic uint32_t phase;
    _Atomic uint32_t parked;
    _Atomic uint64_t calls;
} Shared;

#define TEMPLATE(KIND, CALL) \
".p2align 4\n" \
".globl _reader_" KIND "_begin\n_reader_" KIND "_begin:\n" \
"stp x19,x20,[sp,#-64]!\n stp x21,x22,[sp,#16]\n stp x23,x24,[sp,#32]\n stp x29,x30,[sp,#48]\n" \
"mov x29,sp\n mov x19,x1\n adr x20,2f\n mov x22,x0\n add x21,x22,#8\n mov x23,#0\n" \
"1: ldar w24,[x22]\n cmp w24,#1\n b.eq 3f\n isb\n" \
".globl _reader_" KIND "_call\n_reader_" KIND "_call:\n" CALL "\n" \
".globl _reader_" KIND "_after\n_reader_" KIND "_after:\n" \
"2: add x23,x23,#1\n stlr x23,[x21]\n cmp w24,#2\n b.hs 4f\n b 1b\n" \
"3: add x9,x22,#4\n mov w10,#1\n stlr w10,[x9]\n b 1b\n" \
"4: ldp x21,x22,[sp,#16]\n ldp x23,x24,[sp,#32]\n ldp x29,x30,[sp,#48]\n ldp x19,x20,[sp],#64\n ret\n" \
".globl _reader_" KIND "_end\n_reader_" KIND "_end:\n"
__asm__(TEMPLATE("indirect", "br x19") TEMPLATE("direct", "nop"));
extern const char reader_indirect_begin[], reader_indirect_end[], reader_indirect_after[];
extern const char reader_direct_begin[], reader_direct_end[], reader_direct_call[], reader_direct_after[];

static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t);
    return t.tv_sec+t.tv_nsec*1e-9;
}
static int wait_u32(_Atomic uint32_t *p,uint32_t want) {
    double deadline=now()+5;
    while (atomic_load_explicit(p,memory_order_acquire)!=want) {
        if (now()>deadline) return 0;
        usleep(100);
    }
    return 1;
}
static uint32_t branch(void *at,void *to) {
    intptr_t d=(char*)to-(char*)at;
    if ((d&3) || d<-(1ll<<27) || d>=(1ll<<27)) abort();
    return 0x14000000u|((uint32_t)(d>>2)&0x03ffffffu);
}
static void publish(void *at) {
    __asm__ volatile("dc cvau,%0\n dsb ish\n ic ivau,%0\n dsb ish"::"r"(at):"memory");
}
typedef struct { Shared *shared; void *target; int (*reader)(Shared*,void*); int result; } Run;
static void *execute(void *p) {
    Run *r=p; r->result=r->reader(r->shared,r->target); return NULL;
}
// Ordinary indirect branches, with no instruction synchronization in the hot
// loop. The coherence fix does not add an epoch load to this path.
__asm__(
".p2align 4\n.globl _ibtc_no_isb_bench\n_ibtc_no_isb_bench:\n"
"stp x19,x20,[sp,#-32]!\n stp x21,x22,[sp,#16]\n"
"mov x21,x0\n mov x22,#0\n adr x19,3f\n adr x20,2f\n"
"1: br x19\n"
"2: add x22,x22,x0\n subs x21,x21,#1\n b.ne 1b\n"
"mov x0,x22\n ldp x21,x22,[sp,#16]\n ldp x19,x20,[sp],#32\n ret\n"
"3: mov x0,#1\n br x20\n");
extern uint64_t ibtc_no_isb_bench(uint64_t);
#else
static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t);
    return t.tv_sec+t.tv_nsec*1e-9;
}
static uint64_t ibtc_no_isb_bench(uint64_t n) {
    // Independent semantic reference, not a native instruction-latency claim.
    uint64_t sum=0; volatile uint64_t one=1;
    for(uint64_t i=0;i<n;i++) sum+=one;
    return sum;
}
#endif

int main(int argc,char **argv) {
    if (argc>1 && !strcmp(argv[1],"--bench")) {
        uint64_t n=argc>2?strtoull(argv[2],NULL,10):5000000;
        if (!n) return 2;
        double t=now(); uint64_t sum=ibtc_no_isb_bench(n); double ms=(now()-t)*1000;
        if (sum!=n) return 1;
        printf("ibtc-ordinary %.3f ms\nchecksum %016llx\n",ms,(unsigned long long)sum);
        return 0;
    }
#ifndef __aarch64__
    fputs("SMC execution probe requires arm64; x86 supports --bench reference\n",stderr);
    return 2;
#else
    int indirect=argc<2 || strcmp(argv[1],"direct");
    int rounds=argc>2?atoi(argv[2]):4;
    if (rounds<1 || rounds>100) return 2;
    for (int round=0;round<rounds;round++) {
        uint8_t *mem=mmap(NULL,32768,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANON,-1,0);
        if (mem==MAP_FAILED) return 2;
        const char *begin=indirect?reader_indirect_begin:reader_direct_begin;
        const char *end=indirect?reader_indirect_end:reader_direct_end;
        const char *after=indirect?reader_indirect_after:reader_direct_after;
        memcpy(mem,begin,(size_t)(end-begin));
        uint32_t *target=(uint32_t*)(mem+16384);
        target[0]=0x52800020; // MOV W0,#1
        target[1]=indirect?0xd61f0280u:branch(target+1,mem+(after-begin)); // BR X20 / B after
        if (!indirect) {
            uint32_t *call=(uint32_t*)(mem+(reader_direct_call-begin));
            *call=branch(call,target);
        }
        // Initial fresh code is published before the reader exists.
        for (size_t off=0;off<192;off+=64) publish(mem+off);
        publish(target);
        Shared shared={0};
        Run run={.shared=&shared,.target=target,.reader=(void*)mem,.result=-1};
        pthread_t thread;
        if (pthread_create(&thread,NULL,execute,&run)) return 2;
        double deadline=now()+5;
        while (atomic_load_explicit(&shared.calls,memory_order_acquire)<10000 && now()<deadline) usleep(100);
        int ok=atomic_load_explicit(&shared.calls,memory_order_acquire)>=10000;
        atomic_store_explicit(&shared.phase,1,memory_order_release); // request parking
        ok=ok && wait_u32(&shared.parked,1);
        // Give the pure parked loop time to become JIT-compiled/chained. No
        // executor touches the target between its release ack and phase 2.
        usleep(50000);
        if (ok) {
            atomic_store_explicit((_Atomic uint32_t*)target,0x52800040u,memory_order_relaxed); // MOV W0,#2
            publish(target);
        }
        atomic_store_explicit(&shared.phase,ok?2:3,memory_order_release);
        pthread_join(thread,NULL);
        if (!ok || run.result!=2) {
            fprintf(stderr,"FAIL %s round=%d result=%d expected=2 warm=%llu parked=%u\n",
                    indirect?"indirect":"direct",round,run.result,
                    (unsigned long long)atomic_load(&shared.calls),atomic_load(&shared.parked));
            return 1;
        }
        munmap(mem,32768);
    }
    printf("icache-handoff %s PASS rounds=%d\n",indirect?"indirect":"direct",rounds);
    return 0;
#endif
}
