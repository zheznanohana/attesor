// Metal devices' registryIDs vs IOKit GPU entries (PCI display devices and IOAccelerators) with their entry IDs.
#import <Metal/Metal.h>
#import <IOKit/IOKitLib.h>
#include <stdio.h>
static void list(const char *cls) {
  io_iterator_t it; if (IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching(cls), &it)) { printf("%s: match failed\n", cls); return; }
  io_object_t o; int n = 0;
  while ((o = IOIteratorNext(it))) {
    uint64_t id = 0; IORegistryEntryGetRegistryEntryID(o, &id); io_name_t nm = "", cn = ""; IORegistryEntryGetName(o, nm); IOObjectGetClass(o, cn);
    CFTypeRef cc = IORegistryEntryCreateCFProperty(o, CFSTR("class-code"), NULL, 0); uint32_t code = 0;
    if (cc && CFGetTypeID(cc) == CFDataGetTypeID() && CFDataGetLength(cc) >= 4) CFDataGetBytes(cc, CFRangeMake(0, 4), (UInt8 *)&code);
    if (strcmp(cls, "IOPCIDevice") || (code >> 16) == 3) printf("  %s id=%#llx name=%s class=%s pci-class=%#x\n", cls, id, nm, cn, code), n++;
    IOObjectRelease(o);
  }
  printf("%s: %d shown\n", cls, n);
}
int main(void) { @autoreleasepool {
  for (id<MTLDevice> d in MTLCopyAllDevices()) printf("metal %s registryID=%#llx\n", d.name.UTF8String, d.registryID);
  list("IOPCIDevice"); list("IOAccelerator"); list("IOGPU");
  return 0;
}}
