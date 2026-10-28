// Prints what the CoreAudio HAL sees: device count, default output device name, its running state.
#include <CoreAudio/CoreAudio.h>
#include <stdio.h>
static void name(AudioObjectID d, char *out) {
  CFStringRef s = NULL; UInt32 sz = sizeof s;
  AudioObjectPropertyAddress a = {kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
  out[0] = 0; if (!AudioObjectGetPropertyData(d, &a, 0, NULL, &sz, &s) && s) CFStringGetCString(s, out, 256, kCFStringEncodingUTF8);
}
int main(void) {
  AudioObjectPropertyAddress a = {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
  UInt32 sz = 0; OSStatus e = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, NULL, &sz);
  printf("devices: err=%d count=%u\n", (int)e, sz / 4);
  AudioObjectID ids[64]; if (sz > sizeof ids) sz = sizeof ids; AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, NULL, &sz, ids);
  char n[256]; for (UInt32 i = 0; i < sz / 4; i++) { name(ids[i], n); printf("  dev %u: %s\n", ids[i], n); }
  AudioObjectID out = 0; sz = sizeof out; a.mSelector = kAudioHardwarePropertyDefaultOutputDevice;
  e = AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, NULL, &sz, &out); name(out, n);
  printf("default output: err=%d id=%u name=%s\n", (int)e, out, n);
  return 0;
}
