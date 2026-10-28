// Window with a CAMetalLayer, clears 60 frames with changing colours, then exits.
#import <AppKit/AppKit.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
int main(void) { @autoreleasepool {
  [NSApplication sharedApplication]; [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
  id<MTLDevice> dev = MTLCreateSystemDefaultDevice(); printf("device=%s\n", dev.name.UTF8String);
  NSWindow *w = [[NSWindow alloc] initWithContentRect:NSMakeRect(100,100,320,240) styleMask:NSWindowStyleMaskTitled backing:NSBackingStoreBuffered defer:NO];
  CAMetalLayer *l = [CAMetalLayer layer]; l.device = dev; l.pixelFormat = MTLPixelFormatBGRA8Unorm;
  w.contentView.wantsLayer = YES; w.contentView.layer = l; l.drawableSize = CGSizeMake(320,240);
  [w makeKeyAndOrderFront:nil];
  id<MTLCommandQueue> q = [dev newCommandQueue]; __block int done = 0;
  for (int i = 0; i < 60; i++) {
    id<CAMetalDrawable> d = [l nextDrawable]; if (!d) { printf("nextDrawable nil at %d\n", i); return 2; }
    MTLRenderPassDescriptor *p = [MTLRenderPassDescriptor renderPassDescriptor];
    p.colorAttachments[0].texture = d.texture; p.colorAttachments[0].loadAction = MTLLoadActionClear;
    p.colorAttachments[0].clearColor = MTLClearColorMake(i/60.0, 0.3, 1-i/60.0, 1);
    id<MTLCommandBuffer> cb = [q commandBuffer]; [[cb renderCommandEncoderWithDescriptor:p] endEncoding];
    [cb addCompletedHandler:^(id<MTLCommandBuffer> b){ __atomic_add_fetch(&done,1,__ATOMIC_RELAXED); }];
    [cb presentDrawable:d]; [cb commit];
    [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.016]];
  }
  [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.3]];
  printf("frames completed=%d/60\n", done); return done == 60 ? 0 : 3;
}}
