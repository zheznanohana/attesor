#import <Foundation/Foundation.h>
int main(void) { @autoreleasepool {
  NSString *home = NSHomeDirectory(); printf("home=%s\n", home.UTF8String);
  NSString *f = [home stringByAppendingPathComponent:@"Documents/attesor.txt"];
  [[NSFileManager defaultManager] createDirectoryAtPath:[f stringByDeletingLastPathComponent] withIntermediateDirectories:YES attributes:nil error:nil];
  BOOL ok = [@"hi" writeToFile:f atomically:YES encoding:NSUTF8StringEncoding error:nil];
  printf("container write=%d\n", ok); return ok ? 0 : 1;
}}
