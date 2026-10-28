// Prints supportsFamily: for every MTLGPUFamily value and a few device properties.
#import <Metal/Metal.h>
#include <stdio.h>
int main(void) { @autoreleasepool {
  id<MTLDevice> d = MTLCreateSystemDefaultDevice(); printf("device=%s registryID=%llu lowPower=%d headless=%d unified=%d\n", d.name.UTF8String, d.registryID, d.lowPower, d.headless, d.hasUnifiedMemory);
  long fams[] = {1001,1002,1003,1004,1005,1006,1007,1008,1009,1010, 2001,2002, 3001,3002,3003, 4001,4002, 5001};
  for (unsigned i = 0; i < sizeof fams / sizeof *fams; i++) printf("family %ld=%d\n", fams[i], [d supportsFamily:(MTLGPUFamily)fams[i]]);
  printf("argbuf tier=%lu readWriteTex tier=%lu maxThreadsPerTG=%lu\n", (unsigned long)d.argumentBuffersSupport, (unsigned long)d.readWriteTextureSupport, (unsigned long)d.maxThreadsPerThreadgroup.width);
  return 0;
}}
