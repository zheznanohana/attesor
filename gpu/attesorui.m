// Guest-side UI fixups for app bundles (arm64, injected with DYLD_INSERT_LIBRARIES by attesor).
//
// The running guest registers with LaunchServices as its own arm64-only bundle, so the Dock shows
// that bundle's icon with the "prohibited" overlay. Once the app has finished launching, set the
// Dock icon explicitly from the bundle's own .icns. Links only Foundation: AppKit is looked up at
// runtime, so helper processes without a UI are not made to load it. Compiled without ARC.
#import <Foundation/Foundation.h>
#import <objc/message.h>
#include <stdlib.h>

static void set_dock_icon(void) {
    Class appCls = NSClassFromString(@"NSApplication"), imgCls = NSClassFromString(@"NSImage");
    if (!appCls || !imgCls) return;
    id app = ((id(*)(Class, SEL))objc_msgSend)(appCls, @selector(sharedApplication));
    // only apps with a Dock tile (NSApplicationActivationPolicyRegular == 0)
    if (!app || ((long(*)(id, SEL))objc_msgSend)(app, @selector(activationPolicy)) != 0) return;
    NSBundle *b = [NSBundle mainBundle];
    NSString *name = [b objectForInfoDictionaryKey:@"CFBundleIconFile"];
    NSString *path = name ? [b pathForResource:[name stringByDeletingPathExtension] ofType:@"icns"] : nil;
    id img = path ? ((id(*)(id, SEL, id))objc_msgSend)(((id(*)(Class, SEL))objc_msgSend)(imgCls, @selector(alloc)), @selector(initWithContentsOfFile:), path) : nil;
    if (!img) return;
    ((void(*)(id, SEL, id))objc_msgSend)(app, @selector(setApplicationIconImage:), img);
    [img release];
}

__attribute__((constructor)) static void attesorui_init(void) {
    // dyld has already processed it; children are re-injected by attesor itself
    unsetenv("DYLD_INSERT_LIBRARIES");
    if (getenv("ATTESOR_NO_DOCK_ICON")) return;
    [[NSNotificationCenter defaultCenter] addObserverForName:@"NSApplicationDidFinishLaunchingNotification" object:nil queue:nil
                                                  usingBlock:^(NSNotification *n) { set_dock_icon(); }];
}
