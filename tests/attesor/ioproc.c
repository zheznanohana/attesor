// Runs an IOProc on the default output for SECS seconds writing a 440 Hz tone; reports frames/s.
#include <CoreAudio/CoreAudio.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
static volatile long frames, calls; static double ph;
static OSStatus proc(AudioObjectID d, const AudioTimeStamp *now, const AudioBufferList *in, const AudioTimeStamp *it,
                     AudioBufferList *out, const AudioTimeStamp *ot, void *u) {
  for (UInt32 b = 0; b < out->mNumberBuffers; b++) {
    float *s = out->mBuffers[b].mData; UInt32 ch = out->mBuffers[b].mNumberChannels;
    UInt32 n = out->mBuffers[b].mDataByteSize / 4 / ch;
    for (UInt32 i = 0; i < n; i++) { float v = 0.2f * sinf(ph); ph += 2 * M_PI * 440 / 48000; for (UInt32 c = 0; c < ch; c++) s[i * ch + c] = v; }
    if (b == 0) frames += n;
  }
  calls++; return 0;
}
int main(void) {
  AudioObjectPropertyAddress a = {kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
  AudioObjectID dev; UInt32 sz = sizeof dev; AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, NULL, &sz, &dev);
  Float64 sr = 0; sz = sizeof sr; a.mSelector = kAudioDevicePropertyNominalSampleRate; AudioObjectGetPropertyData(dev, &a, 0, NULL, &sz, &sr);
  AudioDeviceIOProcID id; OSStatus e = AudioDeviceCreateIOProcID(dev, proc, NULL, &id); printf("create=%d sr=%.0f\n", (int)e, sr);
  e = AudioDeviceStart(dev, id); printf("start=%d\n", (int)e);
  double secs = getenv("SECS") ? atof(getenv("SECS")) : 2; usleep(secs * 1e6);
  UInt32 running = 0; sz = sizeof running; a.mSelector = kAudioDevicePropertyDeviceIsRunningSomewhere; AudioObjectGetPropertyData(dev, &a, 0, NULL, &sz, &running);
  AudioDeviceStop(dev, id);
  printf("calls=%ld frames/s=%.0f runningSomewhere=%u\n", calls, frames / secs, running);
  return 0;
}
