// CPU-only format/sample codec: no device and no VT session.
#import <Foundation/Foundation.h>
#include <assert.h>
#include "mclvtcommon.h"
int main(void) { @autoreleasepool {
    const uint8_t sps[] = {0x67,0x42,0x00,0x1e,0xe9,0x01,0x40,0x7b,0x20};
    const uint8_t pps[] = {0x68,0xce,0x06,0xe2};
    NSDictionary *format = @{@"codec": @(kCMVideoCodecType_H264), @"nal": @4,
        @"sets": @[[NSData dataWithBytes:sps length:sizeof sps], [NSData dataWithBytes:pps length:sizeof pps]], @"extensions": @{}};
    const uint8_t payload[] = {0,0,0,3,0x65,0x88,0x84};
    CMTime pts = CMTimeMakeWithEpoch(123, 30, 7), dts = CMTimeMake(120, 30);
    NSDictionary *wire = @{@"format": format, @"data": [NSData dataWithBytes:payload length:sizeof payload],
        @"pts": vt_time(pts), @"dts": vt_time(dts), @"duration": vt_time(CMTimeMake(1,30)),
        @"attachments": @[@{(__bridge id)kCMSampleAttachmentKey_NotSync: @YES}]};
    CMSampleBufferRef sb = NULL;
    OSStatus st = vt_unsample(wire, &sb);
    assert(!st && sb);
    NSDictionary *encoded = vt_sample(sb);
    assert(encoded && [encoded[@"data"] isEqual:wire[@"data"]]);
    assert([encoded[@"pts"] isEqual:wire[@"pts"]] && [encoded[@"dts"] isEqual:wire[@"dts"]]);
    assert([encoded[@"format"][@"sets"] isEqual:format[@"sets"]]);
    assert([encoded[@"format"][@"nal"] intValue] == 4);
    assert([encoded[@"attachments"][0][(__bridge id)kCMSampleAttachmentKey_NotSync] boolValue]);
    // Entire payload must survive a real binary-plist serialization, not a CF pointer round trip.
    NSData *plist = [NSPropertyListSerialization dataWithPropertyList:encoded format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
    assert(plist);
    NSDictionary *parsed = [NSPropertyListSerialization propertyListWithData:plist options:0 format:NULL error:NULL];
    CMSampleBufferRef copy = NULL; assert(!vt_unsample(parsed, &copy));
    assert([vt_sample(copy)[@"data"] isEqual:wire[@"data"]]);
    assert(CMTimeCompare(CMSampleBufferGetPresentationTimeStamp(copy), pts) == 0);
    CFRelease(copy); CFRelease(sb);
    CMVideoFormatDescriptionRef fd = NULL;
    assert(vt_unformat(@{@"sets": @[]}, &fd) == kVTParameterErr);
    assert(vt_unformat(@{@"sets": @[@"not data"]}, &fd) == kVTParameterErr);
    assert(CMTIME_IS_INVALID(vt_untime([NSData data])));
    puts("PASS VT wire: H.264 parameter sets, NAL size, bytes, timings/epoch, attachments, plist, invalid values");
} return 0; }
