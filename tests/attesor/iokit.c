#include <IOKit/IOKitLib.h>
#include <IOKit/ps/IOPowerSources.h>
#include <IOKit/hid/IOHIDManager.h>
#include <stdio.h>
int main(void) {
  io_service_t pe = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching("IOPlatformExpertDevice"));
  printf("IOPlatformExpertDevice=%u\n", pe);
  if (pe) { CFTypeRef s = IORegistryEntryCreateCFProperty(pe, CFSTR("IOPlatformSerialNumber"), NULL, 0);
    char buf[64] = "?"; if (s) CFStringGetCString(s, buf, 64, kCFStringEncodingUTF8); printf("serial=%s\n", buf); }
  CFTypeRef ps = IOPSCopyPowerSourcesInfo(); printf("powersources=%p\n", ps);
  IOHIDManagerRef m = IOHIDManagerCreate(NULL, 0); IOHIDManagerSetDeviceMatching(m, NULL);
  IOReturn r = IOHIDManagerOpen(m, 0); CFSetRef d = IOHIDManagerCopyDevices(m);
  printf("hid open=%x devices=%ld\n", r, d ? CFSetGetCount(d) : -1L);
  return 0;
}
