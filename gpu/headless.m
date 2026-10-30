// No AppKit, layer, view, window or presentation. Native/guest differential workload.
// Build: clang -O2 -fobjc-arc -framework Foundation -framework Metal gpu/headless.m -o ...
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <errno.h>
#include <inttypes.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum { W = 64, H = 32, ROW = W * 4 + 16, PIXELS = W * H * 4 };
static const size_t lengths[] = {1, 7, 8, 4095, 4096, 4097, 16384, 65536};
static uint64_t checksum = UINT64_C(14695981039346656037), checked;

static uint8_t pattern(uint64_t seed, unsigned frame, unsigned item, size_t offset) {
    uint64_t x = seed ^ ((uint64_t)frame << 32) ^ ((uint64_t)item << 24) ^ offset;
    x ^= x >> 30; x *= UINT64_C(0xbf58476d1ce4e5b9);
    x ^= x >> 27; x *= UINT64_C(0x94d049bb133111eb);
    return (uint8_t)(x ^ (x >> 31));
}

static size_t mismatches(const uint8_t *got, const uint8_t *want, size_t n) {
    size_t bad = 0;
    for (size_t i = 0; i < n; i++) bad += got[i] != want[i];
    return bad;
}

static BOOL check(const char *kind, const uint8_t *got, const uint8_t *want, size_t n) {
    size_t bad = mismatches(got, want, n);
    if (bad) { fprintf(stderr, "%s: mismatch=%zu bytes\n", kind, bad); return NO; }
    for (size_t i = 0; i < n; i++) { checksum ^= got[i]; checksum *= UINT64_C(1099511628211); }
    checked += n;
    return YES;
}

// Sensitivity of the byte oracle, runnable even without a Metal device.
static int checkOracle(void) {
    uint8_t want[65536], got[65536];
    for (unsigned k = 0; k < sizeof lengths / sizeof *lengths; k++) {
        size_t n = lengths[k];
        for (size_t i = 0; i < n; i++) want[i] = pattern(7, 3, k, i);
        memcpy(got, want, n);
        if (mismatches(got, want, n)) return 1;
        size_t positions[] = {0, n / 2, n - 1};
        for (unsigned j = 0; j < 3; j++) {
            got[positions[j]] ^= 1;
            if (mismatches(got, want, n) != 1) return 1;
            got[positions[j]] ^= 1;
        }
    }
    puts("PASS byte oracle: 8 lengths, 24 intentional corruptions detected");
    return 0;
}

static BOOL frame(id<MTLDevice> dev, id<MTLCommandQueue> queue,
                  id<MTLTexture> upload, id<MTLTexture> target,
                  unsigned f, uint64_t seed, BOOL oracle) {
    uint8_t want[65536], pixels[PIXELS], padded[ROW * H], got[PIXELS];
    NSMutableArray<id<MTLBuffer>> *outputs = [NSMutableArray array];
    id<MTLCommandBuffer> last = nil;
    for (unsigned k = 0; k < sizeof lengths / sizeof *lengths; k++) {
        size_t n = lengths[k];
        for (size_t i = 0; i < n; i++) want[i] = pattern(seed, f, k, i);
        if (oracle) { if (!check("oracle-buffer", want, want, n)) return NO; continue; }
        id<MTLBuffer> src = [dev newBufferWithBytes:want length:n options:MTLResourceStorageModeShared];
        id<MTLBuffer> dst = [dev newBufferWithLength:n options:MTLResourceStorageModeShared];
        if (!src || !dst) { fputs("buffer allocation failed\n", stderr); return NO; }
        // The input may be reused immediately after newBufferWithBytes returns.
        memset(want, 0xcd, n);
        last = [queue commandBuffer];
        id<MTLBlitCommandEncoder> blit = [last blitCommandEncoder];
        if (!blit) return NO;
        [blit copyFromBuffer:src sourceOffset:0 toBuffer:dst destinationOffset:0 size:n];
        [blit endEncoding]; [last commit];
        [outputs addObject:dst];
    }
    if (!oracle) {
        [last waitUntilCompleted];
        if (last.status != MTLCommandBufferStatusCompleted) {
            fprintf(stderr, "buffer commands failed: %s\n", last.error.description.UTF8String); return NO;
        }
        for (unsigned k = 0; k < outputs.count; k++) {
            for (size_t i = 0; i < lengths[k]; i++) want[i] = pattern(seed, f, k, i);
            if (!check("buffer", outputs[k].contents, want, lengths[k])) return NO;
        }
    }
    memset(padded, 0xaa, sizeof padded);
    for (unsigned y = 0; y < H; y++) for (unsigned x = 0; x < W * 4; x++) {
        uint8_t p = pattern(seed, f, 8, y * W * 4 + x);
        padded[y * ROW + x] = pixels[y * W * 4 + x] = p;
    }
    if (!oracle) {
        // Alternate both public replaceRegion overloads; padded rows catch size/stride errors.
        if (f & 1) [upload replaceRegion:MTLRegionMake2D(0, 0, W, H) mipmapLevel:0 slice:0
                             withBytes:padded bytesPerRow:ROW bytesPerImage:sizeof padded];
        else [upload replaceRegion:MTLRegionMake2D(0, 0, W, H) mipmapLevel:0 withBytes:padded bytesPerRow:ROW];
        memset(padded, 0xcd, sizeof padded);
        [upload getBytes:got bytesPerRow:W * 4 fromRegion:MTLRegionMake2D(0, 0, W, H) mipmapLevel:0];
    }
    if (!check("texture-upload", oracle ? pixels : got, pixels, sizeof pixels)) return NO;
    // Exact 0/1 clear colours avoid GPU-dependent floating point rounding.
    uint8_t r = (f & 1) ? 255 : 0, g = (f & 2) ? 255 : 0, b = (f & 4) ? 255 : 0;
    for (unsigned i = 0; i < PIXELS; i += 4) { pixels[i] = r; pixels[i+1] = g; pixels[i+2] = b; pixels[i+3] = 255; }
    if (!oracle) {
        MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
        pass.colorAttachments[0].texture = target;
        pass.colorAttachments[0].loadAction = MTLLoadActionClear;
        pass.colorAttachments[0].storeAction = MTLStoreActionStore;
        pass.colorAttachments[0].clearColor = MTLClearColorMake(r / 255., g / 255., b / 255., 1);
        last = [queue commandBuffer];
        id<MTLRenderCommandEncoder> render = [last renderCommandEncoderWithDescriptor:pass];
        if (!render) return NO;
        [render endEncoding];
        if (target.storageMode == MTLStorageModeManaged) {
            id<MTLBlitCommandEncoder> blit = [last blitCommandEncoder];
            if (!blit) return NO;
            [blit synchronizeResource:target]; [blit endEncoding];
        }
        [last commit]; [last waitUntilCompleted];
        if (last.status != MTLCommandBufferStatusCompleted) {
            fprintf(stderr, "render commands failed: %s\n", last.error.description.UTF8String); return NO;
        }
        [target getBytes:got bytesPerRow:W * 4 fromRegion:MTLRegionMake2D(0, 0, W, H) mipmapLevel:0];
    }
    return check("offscreen-clear", oracle ? pixels : got, pixels, sizeof pixels);
}

static BOOL number(const char *s, uint64_t *n) {
    char *end; errno = 0;
    if (!s[0] || s[0] == '-') return NO;
    *n = strtoull(s, &end, 0);
    return !errno && !*end;
}

int main(int argc, const char **argv) { @autoreleasepool {
    if (argc == 2 && !strcmp(argv[1], "--check-oracle")) return checkOracle();
    BOOL oracle = argc > 1 && !strcmp(argv[1], "--oracle");
    unsigned base = oracle ? 2 : 1;
    uint64_t frames = 120, seed = 1;
    if (argc > (int)base + 2 || (argc > (int)base && !number(argv[base], &frames)) ||
        (argc > (int)base + 1 && !number(argv[base+1], &seed)) || !frames || frames > 100000) {
        fputs("usage: headless [--oracle] [frames (1..100000)] [seed]\n       headless --check-oracle\n", stderr); return 2;
    }
    id<MTLDevice> dev = oracle ? nil : MTLCreateSystemDefaultDevice();
    if (!oracle && !dev) { fputs("UNAVAILABLE: MTLCreateSystemDefaultDevice returned nil; no GPU measurement\n", stderr); return 77; }
    id<MTLCommandQueue> queue = oracle ? nil : [dev newCommandQueue];
    MTLTextureDescriptor *desc = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:W height:H mipmapped:NO];
    desc.storageMode = dev.hasUnifiedMemory ? MTLStorageModeShared : MTLStorageModeManaged;
    desc.usage = MTLTextureUsageShaderRead | MTLTextureUsageRenderTarget;
    id<MTLTexture> upload = oracle ? nil : [dev newTextureWithDescriptor:desc];
    id<MTLTexture> target = oracle ? nil : [dev newTextureWithDescriptor:desc];
    if (!oracle && (!queue || !upload || !target)) { fputs("resource allocation failed\n", stderr); return 1; }
    uint64_t start = mach_absolute_time();
    for (unsigned f = 0; f < frames; f++) { @autoreleasepool {
        if (!frame(dev, queue, upload, target, f, seed, oracle)) return 1;
    } }
    mach_timebase_info_data_t tb; mach_timebase_info(&tb);
    double elapsed = (double)(mach_absolute_time() - start) * tb.numer / tb.denom / 1e9;
    uint64_t bufferBytes = 0;
    for (unsigned k = 0; k < sizeof lengths / sizeof *lengths; k++) bufferBytes += lengths[k];
    printf("RESULT mode=%s frames=%llu seed=%llu checksum=%016llx checked_bytes=%llu mismatch=0\n",
           oracle ? "oracle" : "gpu", frames, seed, checksum, checked);
    if (!oracle) printf("MEASURE seconds=%.6f frames_per_second=%.3f command_buffers=%llu submitted_upload_bytes=%llu\n",
                        elapsed, frames / elapsed, frames * 9, frames * (bufferBytes + PIXELS));
    return 0;
} }
