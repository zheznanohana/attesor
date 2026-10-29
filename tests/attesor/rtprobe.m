// Metal coverage probe: feature queries, a ray-traced triangle (primitive acceleration structure +
// intersector in a compute kernel) and a MetalFX spatial upscale. Output must match the x86-64 build.
#import <Metal/Metal.h>
#import <MetalFX/MetalFX.h>
#include <stdio.h>

static const char *src =
"#include <metal_stdlib>\n"
"using namespace metal; using namespace raytracing;\n"
"kernel void trace(primitive_acceleration_structure as [[buffer(0)]], device uint *out [[buffer(1)]], uint2 id [[thread_position_in_grid]]) {\n"
"  ray r; r.origin = float3((id.x + 0.5) / 16.0, (id.y + 0.5) / 16.0, -1); r.direction = float3(0, 0, 1); r.min_distance = 0; r.max_distance = 10;\n"
"  intersector<triangle_data> i; auto h = i.intersect(r, as);\n"
"  out[id.y * 16 + id.x] = h.type == intersection_type::triangle ? 1 + h.primitive_id : 0; }\n";

int main(void) { @autoreleasepool {
  id<MTLDevice> d = MTLCreateSystemDefaultDevice();
  printf("raytracing=%d fromRender=%d functionPointers=%d dynamicLibraries=%d 32bitFloatFilter=%d bcTextures=%d\n",
         d.supportsRaytracing, d.supportsRaytracingFromRender, d.supportsFunctionPointers, d.supportsDynamicLibraries,
         d.supports32BitFloatFiltering, d.supportsBCTextureCompression);
  printf("metalfx spatial=%d temporal=%d\n", [MTLFXSpatialScalerDescriptor supportsDevice:d], [MTLFXTemporalScalerDescriptor supportsDevice:d]);
  id<MTLCommandQueue> q = [d newCommandQueue];
  if (d.supportsRaytracing) {
    float v[] = {0.1, 0.1, 0, 0.9, 0.1, 0, 0.1, 0.9, 0};
    id<MTLBuffer> vb = [d newBufferWithBytes:v length:sizeof v options:MTLResourceStorageModeShared];
    MTLAccelerationStructureTriangleGeometryDescriptor *g = [MTLAccelerationStructureTriangleGeometryDescriptor descriptor];
    g.vertexBuffer = vb; g.vertexStride = 12; g.triangleCount = 1;
    MTLPrimitiveAccelerationStructureDescriptor *pd = [MTLPrimitiveAccelerationStructureDescriptor descriptor];
    pd.geometryDescriptors = @[g];
    MTLAccelerationStructureSizes sz = [d accelerationStructureSizesWithDescriptor:pd];
    id<MTLAccelerationStructure> as = [d newAccelerationStructureWithSize:sz.accelerationStructureSize];
    id<MTLBuffer> scratch = [d newBufferWithLength:sz.buildScratchBufferSize options:MTLResourceStorageModePrivate];
    id<MTLCommandBuffer> cb = [q commandBuffer];
    id<MTLAccelerationStructureCommandEncoder> ae = [cb accelerationStructureCommandEncoder];
    [ae buildAccelerationStructure:as descriptor:pd scratchBuffer:scratch scratchBufferOffset:0];
    [ae endEncoding];
    NSError *err = nil;
    id<MTLLibrary> lib = [d newLibraryWithSource:@(src) options:nil error:&err];
    if (!lib) { printf("compile failed: %s\n", err.localizedDescription.UTF8String); return 1; }
    id<MTLComputePipelineState> ps = [d newComputePipelineStateWithFunction:[lib newFunctionWithName:@"trace"] error:&err];
    if (!ps) { printf("pipeline failed: %s\n", err.localizedDescription.UTF8String); return 1; }
    id<MTLBuffer> out = [d newBufferWithLength:256 * 4 options:MTLResourceStorageModeShared];
    id<MTLComputeCommandEncoder> ce = [cb computeCommandEncoder];
    [ce setComputePipelineState:ps];
    [ce setAccelerationStructure:as atBufferIndex:0];
    [ce setBuffer:out offset:0 atIndex:1];
    [ce useResource:as usage:MTLResourceUsageRead];
    [ce dispatchThreads:MTLSizeMake(16, 16, 1) threadsPerThreadgroup:MTLSizeMake(8, 8, 1)];
    [ce endEncoding];
    [cb commit]; [cb waitUntilCompleted];
    if (cb.error) printf("rt command buffer error: %s\n", cb.error.localizedDescription.UTF8String);
    uint32_t *o = out.contents; int hits = 0;
    for (int i = 0; i < 256; i++) hits += o[i] != 0;
    printf("rt hits=%d of 256 (corner %u, far corner %u)\n", hits, o[16 + 1], o[255]);
  }
  if ([MTLFXSpatialScalerDescriptor supportsDevice:d]) {
    MTLFXSpatialScalerDescriptor *sd = [MTLFXSpatialScalerDescriptor new];
    sd.inputWidth = 64; sd.inputHeight = 64; sd.outputWidth = 128; sd.outputHeight = 128;
    sd.colorTextureFormat = MTLPixelFormatRGBA8Unorm; sd.outputTextureFormat = MTLPixelFormatRGBA8Unorm;
    sd.colorProcessingMode = MTLFXSpatialScalerColorProcessingModePerceptual;
    id<MTLFXSpatialScaler> sc = [sd newSpatialScalerWithDevice:d];
    MTLTextureDescriptor *td = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:64 height:64 mipmapped:NO];
    td.usage = sc.colorTextureUsage; td.storageMode = MTLStorageModeManaged;
    id<MTLTexture> in = [d newTextureWithDescriptor:td];
    uint32_t px[64 * 64]; for (int i = 0; i < 64 * 64; i++) px[i] = 0xff000000 | ((i % 64) * 4) | (((i / 64) * 4) << 8);
    [in replaceRegion:MTLRegionMake2D(0, 0, 64, 64) mipmapLevel:0 withBytes:px bytesPerRow:256];
    MTLTextureDescriptor *od = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:128 height:128 mipmapped:NO];
    od.usage = sc.outputTextureUsage; od.storageMode = MTLStorageModePrivate;
    id<MTLTexture> ot = [d newTextureWithDescriptor:od];
    sc.colorTexture = in; sc.outputTexture = ot;
    id<MTLBuffer> rb = [d newBufferWithLength:128 * 128 * 4 options:MTLResourceStorageModeShared];
    id<MTLCommandBuffer> cb = [q commandBuffer];
    [sc encodeToCommandBuffer:cb];
    id<MTLBlitCommandEncoder> be = [cb blitCommandEncoder];
    [be copyFromTexture:ot sourceSlice:0 sourceLevel:0 sourceOrigin:MTLOriginMake(0, 0, 0) sourceSize:MTLSizeMake(128, 128, 1) toBuffer:rb destinationOffset:0 destinationBytesPerRow:512 destinationBytesPerImage:512 * 128];
    [be endEncoding];
    [cb commit]; [cb waitUntilCompleted];
    if (cb.error) printf("metalfx command buffer error: %s\n", cb.error.localizedDescription.UTF8String);
    uint32_t *r = rb.contents;
    printf("metalfx out[64,64]=%08x out[127,127]=%08x\n", r[64 * 128 + 64], r[127 * 128 + 127]);
  }
  return 0;
}}
