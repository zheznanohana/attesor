// Plays a 440 Hz tone for 1 s through AudioQueue; prints progress.
#include <AudioToolbox/AudioToolbox.h>
#include <math.h>
#include <stdio.h>
#include <unistd.h>
static double phase; static int filled;
static void cb(void *u, AudioQueueRef q, AudioQueueBufferRef b) {
  float *s = b->mAudioData; int n = b->mAudioDataBytesCapacity / sizeof(float);
  for (int i = 0; i < n; i++) { s[i] = 0.2f * sinf(phase); phase += 2 * M_PI * 440 / 44100; }
  b->mAudioDataByteSize = n * sizeof(float); filled++;
  AudioQueueEnqueueBuffer(q, b, 0, NULL);
}
int main(void) {
  AudioStreamBasicDescription f = {44100, kAudioFormatLinearPCM, kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked, 4, 1, 4, 1, 32, 0};
  AudioQueueRef q; OSStatus e = AudioQueueNewOutput(&f, cb, NULL, NULL, NULL, 0, &q);
  printf("AudioQueueNewOutput=%d\n", (int)e); if (e) return 1;
  for (int i = 0; i < 3; i++) { AudioQueueBufferRef b; AudioQueueAllocateBuffer(q, 8820 * 4, &b); cb(NULL, q, b); }
  e = AudioQueueStart(q, NULL); printf("AudioQueueStart=%d\n", (int)e);
  CFRunLoopRunInMode(kCFRunLoopDefaultMode, getenv("SECS") ? atof(getenv("SECS")) : 1.0, false);
  printf("buffers filled=%d (expect >5)\n", filled);
  AudioQueueStop(q, true); AudioQueueDispose(q, true);
  return filled > 5 ? 0 : 2;
}
