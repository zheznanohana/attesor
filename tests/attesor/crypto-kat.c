// Independent known-answer tests for guest crypto library dispatch. Build for
// x86_64 and arm64, run directly / under attesor; both must print crypto-kat: PASS.
#include <CommonCrypto/CommonCrypto.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int equal(const char *name, const void *got, const void *want, size_t n) {
    if (!memcmp(got, want, n)) return 1;
    fprintf(stderr, "%s mismatch\n", name);
    return 0;
}
int main(void) {
    // FIPS 197 AES-128 example; exercise library encrypt and decrypt dispatch.
    const uint8_t key[16] = {0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15};
    const uint8_t plain[16] = {0x00,0x11,0x22,0x33,0x44,0x55,0x66,0x77,
                              0x88,0x99,0xaa,0xbb,0xcc,0xdd,0xee,0xff};
    const uint8_t cipher[16] = {0x69,0xc4,0xe0,0xd8,0x6a,0x7b,0x04,0x30,
                               0xd8,0xcd,0xb7,0x80,0x70,0xb4,0xc5,0x5a};
    uint8_t out[32], back[32], digest[32]; size_t n = 0, m = 0;
    if (CCCrypt(kCCEncrypt, kCCAlgorithmAES, kCCOptionECBMode, key, sizeof key,
                NULL, plain, sizeof plain, out, sizeof out, &n) != kCCSuccess || n != 16)
        return 1;
    if (!equal("AES encrypt", out, cipher, 16)) return 1;
    if (CCCrypt(kCCDecrypt, kCCAlgorithmAES, kCCOptionECBMode, key, sizeof key,
                NULL, out, n, back, sizeof back, &m) != kCCSuccess || m != 16)
        return 1;
    if (!equal("AES decrypt", back, plain, 16)) return 1;
    // FIPS 180-4 SHA-256 test vector, including multiblock/streaming dispatch.
    static const uint8_t abc_hash[32] = {
        0xba,0x78,0x16,0xbf,0x8f,0x01,0xcf,0xea,0x41,0x41,0x40,0xde,0x5d,0xae,0x22,0x23,
        0xb0,0x03,0x61,0xa3,0x96,0x17,0x7a,0x9c,0xb4,0x10,0xff,0x61,0xf2,0x00,0x15,0xad};
    CC_SHA256("abc", 3, digest);
    if (!equal("SHA256 abc", digest, abc_hash, 32)) return 1;
    static const uint8_t million_a_hash[32] = {
        0xcd,0xc7,0x6e,0x5c,0x99,0x14,0xfb,0x92,0x81,0xa1,0xc7,0xe2,0x84,0xd7,0x3e,0x67,
        0xf1,0x80,0x9a,0x48,0xa4,0x97,0x20,0x0e,0x04,0x6d,0x39,0xcc,0xc7,0x11,0x2c,0xd0};
    char a[1000]; memset(a, 'a', sizeof a);
    CC_SHA256_CTX ctx;
    CC_SHA256_Init(&ctx);
    for (int i = 0; i < 1000; i++) CC_SHA256_Update(&ctx, a, sizeof a);
    CC_SHA256_Final(digest, &ctx);
    if (!equal("SHA256 million a", digest, million_a_hash, 32)) return 1;
    puts("crypto-kat: PASS");
    return 0;
}
