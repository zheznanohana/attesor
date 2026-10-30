#include <CommonCrypto/CommonCryptor.h>
#include "hostcall.h"
uint64_t attesor_native_crypto_calls(void) { return hostcall(56,0,0,0,0,0); }
static CCCryptorStatus crypt(CCOperation op,CCAlgorithm alg,CCOptions options,const void *key,size_t keylen,const void *iv,
    const void *src,size_t size,void *dst,size_t capacity,size_t *written) {
    if(alg!=kCCAlgorithmAES || (op!=kCCEncrypt && op!=kCCDecrypt))return CCCrypt(op,alg,options,key,keylen,iv,src,size,dst,capacity,written);
    uint64_t v[]={op,alg,options,(uintptr_t)key,keylen,(uintptr_t)iv,(uintptr_t)src,size,(uintptr_t)dst,capacity,(uintptr_t)written};
    return (CCCryptorStatus)hostcall(57,(uintptr_t)v,0,0,0,0);
}
struct interpose {const void *replacement,*replacee;};
__attribute__((used)) static const struct interpose hooks[] __attribute__((section("__DATA,__interpose")))={
    {(const void*)crypt,(const void*)CCCrypt},
};
