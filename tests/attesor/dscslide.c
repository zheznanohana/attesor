#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>

int main(void) {
    CFStringRef s = CFStringCreateWithFormat(NULL, NULL, CFSTR("slide-%d"), 42);
    CFArrayRef a = CFArrayCreate(NULL, NULL, 0, &kCFTypeArrayCallBacks);
    char b[64];
    if (!s || !a || !CFStringGetCString(s, b, sizeof(b), kCFStringEncodingUTF8))
        return 1;
    printf("%s length=%ld number=%ld\n", b, CFStringGetLength(s), CFArrayGetCount(a));
    CFRelease(a);
    CFRelease(s);
    return 0;
}
