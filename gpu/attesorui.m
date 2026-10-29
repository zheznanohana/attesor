// Guest-side UI fixups for app bundles (arm64, injected with DYLD_INSERT_LIBRARIES by attesor).
//
// Sets the Dock icon of a running app: the badged icon attesor-apps made for it (the app's icon with an
// "arm" badge), else the bundle's own .icns (an arm64-only bundle that was not made universal by
// attesor-apps would otherwise show the "prohibited" overlay). Set
// the moment the Dock tile comes into existence: +[NSApplication sharedApplication] is swizzled as
// soon as AppKit is loaded, and the icon is set again when launching finishes (in case the app or
// AppKit resets it). Links only Foundation: AppKit is looked up at runtime, so helper processes
// without a UI are not made to load it. Compiled without ARC.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#import <objc/message.h>
#include <mach-o/dyld.h>
#include <pwd.h>
#include <stdatomic.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>

static id gIcon;   // NSImage, loaded once

static id bundle_icon(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        Class imgCls = NSClassFromString(@"NSImage");
        NSBundle *b = [NSBundle mainBundle];
        // attesor-apps renders the app's icon with an "arm" badge for the Dock tile of the running app
        // (the real home: a sandboxed guest's HOME is its container)
        NSString *path = nil;
        struct passwd *pw = getpwuid(getuid());
        if (pw && b.bundleIdentifier) {
            NSString *badged = [NSString stringWithFormat:@"%s/Library/Application Support/Attesor/icons/%@.icns", pw->pw_dir, b.bundleIdentifier];
            if ([[NSFileManager defaultManager] fileExistsAtPath:badged]) path = badged;
        }
        NSString *name = [b objectForInfoDictionaryKey:@"CFBundleIconFile"];
        if (!path && name) path = [b pathForResource:[name stringByDeletingPathExtension] ofType:@"icns"];
        if (imgCls && path)
            gIcon = ((id(*)(id, SEL, id))objc_msgSend)(((id(*)(Class, SEL))objc_msgSend)(imgCls, @selector(alloc)), @selector(initWithContentsOfFile:), path);
    });
    return gIcon;
}

static void set_dock_icon(id app, const char *when) {
    if (getenv("ATTESOR_UI_TRACE")) fprintf(stderr, "[attesorui] %s: app=%p policy=%ld\n", when, app, app ? ((long(*)(id, SEL))objc_msgSend)(app, @selector(activationPolicy)) : -1L);
    // only apps with a Dock tile (NSApplicationActivationPolicyRegular == 0)
    if (!app || ((long(*)(id, SEL))objc_msgSend)(app, @selector(activationPolicy)) != 0) return;
    id img = bundle_icon();
    if (img) ((void(*)(id, SEL, id))objc_msgSend)(app, @selector(setApplicationIconImage:), img);
    if (getenv("ATTESOR_UI_TRACE")) fprintf(stderr, "[attesorui] dock icon set (%s)%s\n", when, img ? "" : ": no icon");
}

static IMP gOrigShared;
static id swz_sharedApplication(id self, SEL _cmd) {
    static _Atomic int done;
    id app = ((id(*)(id, SEL))gOrigShared)(self, _cmd);
    if (!atomic_exchange(&done, 1)) set_dock_icon(app, "sharedApplication");
    return app;
}

static void hook_appkit(void) {
    static _Atomic int hooked;
    if (atomic_load(&hooked)) return;   // runs for every image load: stay out of the runtime once done
    Class c = objc_getClass("NSApplication");
    if (!c || atomic_exchange(&hooked, 1)) return;
    Method m = class_getClassMethod(c, @selector(sharedApplication));
    if (m) gOrigShared = method_setImplementation(m, (IMP)swz_sharedApplication);
    if (getenv("ATTESOR_UI_TRACE")) fprintf(stderr, "[attesorui] hooked +[NSApplication sharedApplication] (%p)\n", (void *)gOrigShared);
}

static void on_image(const struct mach_header *mh, intptr_t slide) { hook_appkit(); }

__attribute__((constructor)) static void attesorui_init(void) {
    // dyld has already processed it; children are re-injected by attesor itself
    unsetenv("DYLD_INSERT_LIBRARIES");
    if (getenv("ATTESOR_UI_TRACE")) fprintf(stderr, "[attesorui] loaded\n");
    if (getenv("ATTESOR_NO_DOCK_ICON")) return;
    // called for every image already loaded and for later ones: hooks as soon as AppKit is present
    _dyld_register_func_for_add_image(on_image);
    for (NSString *n in @[@"NSApplicationWillFinishLaunchingNotification", @"NSApplicationDidFinishLaunchingNotification"])
        [[NSNotificationCenter defaultCenter] addObserverForName:n object:nil queue:nil usingBlock:^(NSNotification *note) {
            set_dock_icon(note.object, n.UTF8String + 13);
        }];
}
