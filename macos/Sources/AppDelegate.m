#import <Cocoa/Cocoa.h>

#import "LockStore.h"
#import "QueueWindowController.h"
#import "SettingsWindowController.h"

@interface AppDelegate : NSObject <NSApplicationDelegate, NSMenuDelegate>
@end

/// Frames of the window's spinner (NSProgressIndicator, small): eight round-capped spokes,
/// the leading one darkest and the rest fading behind it. Menu items only take still images,
/// and the real control animates in a layer that can't be captured frame by frame, so this
/// redraws it at the control's measured geometry. Template images, so they follow the
/// menu's text color, including when the item is highlighted.
static NSArray<NSImage *> *SpinnerFrames(void) {
    static NSArray<NSImage *> *frames;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        NSMutableArray *images = [NSMutableArray array];
        for (NSInteger lead = 0; lead < 8; lead++) {
            NSImage *image = [NSImage imageWithSize:NSMakeSize(16, 16)
                                            flipped:NO
                                     drawingHandler:^BOOL(NSRect rect) {
                                         for (NSInteger spoke = 0; spoke < 8; spoke++) {
                                             // Clockwise from the top; age 0 is the leading spoke.
                                             NSInteger age = (lead - spoke + 8) % 8;
                                             CGFloat angle = M_PI_2 - spoke * M_PI_4;
                                             NSBezierPath *path = [NSBezierPath bezierPath];
                                             path.lineWidth = 2;
                                             path.lineCapStyle = NSLineCapStyleRound;
                                             [path moveToPoint:NSMakePoint(8 + 4.25 * cos(angle), 8 + 4.25 * sin(angle))];
                                             [path lineToPoint:NSMakePoint(8 + 7.0 * cos(angle), 8 + 7.0 * sin(angle))];
                                             [[NSColor.blackColor colorWithAlphaComponent:0.6 - age * 0.065] setStroke];
                                             [path stroke];
                                         }
                                         return YES;
                                     }];
            image.template = YES;
            [images addObject:image];
        }
        frames = images;
    });
    return frames;
}

@implementation AppDelegate {
    QueueWindowController *_windowController;
    SettingsWindowController *_settingsController;
    NSStatusItem *_statusItem;
    /// While the menu bar extra's menu is open, it's kept current: times tick and running
    /// tasks' spinners turn.
    BOOL _menuOpen;
    /// Identifies what the open menu lists; when this changes, the menu is rebuilt.
    NSString *_menuKey;
    NSMutableDictionary<NSNumber *, NSMenuItem *> *_taskItems;
    NSMutableArray<NSMenuItem *> *_spinningItems;
    NSTimer *_spinTimer;
    NSUInteger _spinFrame;
}

- (void)applicationWillFinishLaunching:(NSNotification *)note {
    NSApp.mainMenu = [self buildMainMenu];
}

- (void)applicationDidFinishLaunching:(NSNotification *)note {
    [LockStore.sharedStore startWatching];
    _windowController = [QueueWindowController new];
    [_windowController showWindow:nil];

    _statusItem = [NSStatusBar.systemStatusBar statusItemWithLength:NSVariableStatusItemLength];
    _statusItem.autosaveName = @"LockStatusItem";
    _statusItem.button.imagePosition = NSImageLeading;
    NSMenu *menu = [NSMenu new];
    menu.delegate = self;
    // Wide enough for a typical task's detail line, so the menu doesn't change width as times
    // tick and tasks come and go. Longer lines still widen it.
    menu.minimumWidth = 400;
    _statusItem.menu = menu;
    _taskItems = [NSMutableDictionary dictionary];
    _spinningItems = [NSMutableArray array];
    [NSNotificationCenter.defaultCenter addObserver:self
                                           selector:@selector(storeDidChange)
                                               name:LockStoreDidChangeNotification
                                             object:nil];
    [self updateStatusItem];
    [NSApp activate];
}

- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
    return NO; // Stays available from the menu bar.
}

- (BOOL)applicationShouldHandleReopen:(NSApplication *)sender hasVisibleWindows:(BOOL)visible {
    if (!visible) [self showMainWindow:nil];
    return YES;
}

- (BOOL)applicationSupportsSecureRestorableState:(NSApplication *)app {
    return YES;
}

- (IBAction)showSettings:(id)sender {
    if (!_settingsController) _settingsController = [SettingsWindowController new];
    [_settingsController showWindow:sender];
    [NSApp activate];
}

- (IBAction)showMainWindow:(id)sender {
    [_windowController showWindow:sender];
    [NSApp activate];
}

#pragma mark Menu bar extra

- (void)storeDidChange {
    [self updateStatusItem];
    // The store reloads every second, even while a menu is being tracked.
    if (_menuOpen) [self refreshMenu:_statusItem.menu];
}

- (void)updateStatusItem {
    LockSnapshot *snapshot = LockStore.sharedStore.snapshot;
    BOOL busy = snapshot.running.count > 0;
    NSString *symbol = snapshot.exclusiveHolder ? @"cpu.fill" : @"cpu";
    NSImage *image = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:@"Lock"];
    _statusItem.button.image = image;
    NSString *title = @"";
    if (busy || snapshot.waiting.count) {
        title = [NSString stringWithFormat:@"%lu", (unsigned long)snapshot.running.count];
        if (snapshot.waiting.count) title = [title stringByAppendingFormat:@"+%lu", (unsigned long)snapshot.waiting.count];
    }
    _statusItem.button.title = title;
    _statusItem.button.font = [NSFont monospacedDigitSystemFontOfSize:NSFont.systemFontSize weight:NSFontWeightRegular];
    _statusItem.button.toolTip = [NSString stringWithFormat:@"Lock — %@", snapshot.summary];
}

- (void)menuNeedsUpdate:(NSMenu *)menu {
    [self buildMenu:menu];
}

- (void)menuWillOpen:(NSMenu *)menu {
    _menuOpen = YES;
    // About the system spinner's speed.
    _spinTimer = [NSTimer timerWithTimeInterval:1.0 / 10
                                         target:self
                                       selector:@selector(spin)
                                       userInfo:nil
                                        repeats:YES];
    // Common modes include the one the run loop is in while a menu is open.
    [NSRunLoop.mainRunLoop addTimer:_spinTimer forMode:NSRunLoopCommonModes];
}

- (void)menuDidClose:(NSMenu *)menu {
    _menuOpen = NO;
    [_spinTimer invalidate];
    _spinTimer = nil;
}

/// What the menu lists: when this changes, the menu is rebuilt rather than updated.
- (NSString *)menuKeyFor:(LockSnapshot *)snapshot {
    NSMutableString *key = [NSMutableString string];
    for (LockTask *task in snapshot.running) [key appendFormat:@"r%llu,", task.taskID];
    for (LockTask *task in snapshot.waiting) [key appendFormat:@"w%llu,", task.taskID];
    return key;
}

- (void)buildMenu:(NSMenu *)menu {
    [menu removeAllItems];
    [_taskItems removeAllObjects];
    [_spinningItems removeAllObjects];
    LockSnapshot *snapshot = LockStore.sharedStore.snapshot;
    _menuKey = [self menuKeyFor:snapshot];
    // The tasks themselves say what's going on; only an empty queue needs a line of its own.
    if (!snapshot.running.count && !snapshot.waiting.count) {
        [menu addItemWithTitle:@"Nothing running" action:nil keyEquivalent:@""].enabled = NO;
    }
    [self addSection:@"Running" tasks:snapshot.running to:menu];
    [self addSection:@"Waiting" tasks:snapshot.waiting to:menu];
    [menu addItem:NSMenuItem.separatorItem];
    [menu addItemWithTitle:@"Open Lock" action:@selector(showMainWindow:) keyEquivalent:@""].target = self;
    [menu addItemWithTitle:@"Settings…" action:@selector(showSettings:) keyEquivalent:@","].target = self;
    [menu addItem:NSMenuItem.separatorItem];
    [menu addItemWithTitle:@"Quit Lock" action:@selector(terminate:) keyEquivalent:@"q"];
    [self refreshMenu:menu];
}

/// Bring the open menu up to date: in place when the same tasks are listed, otherwise rebuilt.
- (void)refreshMenu:(NSMenu *)menu {
    LockSnapshot *snapshot = LockStore.sharedStore.snapshot;
    if (![[self menuKeyFor:snapshot] isEqualToString:_menuKey]) {
        [self buildMenu:menu];
        return;
    }
    uint64_t now = LockNowMs();
    for (NSArray<LockTask *> *tasks in @[ snapshot.running, snapshot.waiting ]) {
        for (LockTask *task in tasks) {
            NSMenuItem *item = _taskItems[@(task.taskID)];
            NSString *detail = [self detailFor:task now:now];
            if (@available(macOS 14.4, *)) {
                item.subtitle = detail;
            } else {
                item.toolTip = detail;
            }
        }
    }
}

/// Times first, then where it's from, as in the window: "1m02s of ~3m00s · lock@main · codex",
/// "#2 · waiting 45s · lock@main · codex".
- (NSString *)detailFor:(LockTask *)task now:(uint64_t)now {
    NSMutableArray<NSString *> *parts = [NSMutableArray array];
    if (task.phase == LockTaskPhaseRunning) {
        uint64_t elapsed = now > task.startedAt ? now - task.startedAt : 0;
        if (task.expected && elapsed < task.expected) {
            [parts addObject:[NSString stringWithFormat:@"%@ of ~%@", LockFormatDuration(elapsed),
                                                        LockFormatDuration(task.expected)]];
        } else if (task.expected) {
            [parts addObject:[NSString stringWithFormat:@"%@, usually ~%@", LockFormatDuration(elapsed),
                                                        LockFormatDuration(task.expected)]];
        } else {
            [parts addObject:LockFormatDuration(elapsed)];
        }
    } else {
        [parts addObject:[NSString stringWithFormat:@"#%ld", (long)task.queuePosition]];
        [parts addObject:[NSString stringWithFormat:@"waiting %@",
                                                    LockFormatDuration(now > task.enqueuedAt ? now - task.enqueuedAt : 0)]];
    }
    [parts addObject:task.location];
    if (task.agent) [parts addObject:task.agent];
    if (task.exclusive) [parts addObject:@"exclusive"];
    if (task.light) [parts addObject:@"light"];
    return [parts componentsJoinedByString:@" · "];
}

- (void)addSection:(NSString *)title tasks:(NSArray<LockTask *> *)tasks to:(NSMenu *)menu {
    if (!tasks.count) return;
    if (menu.numberOfItems > 0) [menu addItem:NSMenuItem.separatorItem];
    [menu addItem:[NSMenuItem sectionHeaderWithTitle:title]];
    for (LockTask *task in tasks) {
        NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:task.title action:@selector(showTask:) keyEquivalent:@""];
        item.target = self;
        item.tag = (NSInteger)task.taskID;
        if (task.phase == LockTaskPhaseRunning) {
            item.image = SpinnerFrames()[_spinFrame];
            [_spinningItems addObject:item];
        } else {
            item.image = [NSImage imageWithSystemSymbolName:@"clock" accessibilityDescription:@"Waiting"];
        }
        // macOS 27 hides menu item images unless asked; the spinner is the point here.
        if (@available(macOS 27.0, *)) item.preferredImageVisibility = NSMenuItemImageVisibilityVisible;
        _taskItems[@(task.taskID)] = item;
        [menu addItem:item];
    }
}

/// Advance the running tasks' spinners.
- (void)spin {
    _spinFrame = (_spinFrame + 1) % SpinnerFrames().count;
    for (NSMenuItem *item in _spinningItems) item.image = SpinnerFrames()[_spinFrame];
}

- (void)showTask:(NSMenuItem *)sender {
    [self showMainWindow:sender];
    [_windowController selectTaskWithID:(uint64_t)sender.tag];
}

#pragma mark Main menu

- (NSMenu *)buildMainMenu {
    NSMenu *main = [NSMenu new];
    NSString *app = @"Lock";

    NSMenu *appMenu = [self submenu:app in:main];
    [appMenu addItemWithTitle:[@"About " stringByAppendingString:app]
                       action:@selector(orderFrontStandardAboutPanel:)
                keyEquivalent:@""];
    [appMenu addItem:NSMenuItem.separatorItem];
    [appMenu addItemWithTitle:@"Settings…" action:@selector(showSettings:) keyEquivalent:@","].target = self;
    [appMenu addItem:NSMenuItem.separatorItem];
    NSMenu *services = [NSMenu new];
    [appMenu addItemWithTitle:@"Services" action:nil keyEquivalent:@""].submenu = services;
    NSApp.servicesMenu = services;
    [appMenu addItem:NSMenuItem.separatorItem];
    [appMenu addItemWithTitle:[@"Hide " stringByAppendingString:app] action:@selector(hide:) keyEquivalent:@"h"];
    [appMenu addItemWithTitle:@"Hide Others" action:@selector(hideOtherApplications:) keyEquivalent:@"h"]
        .keyEquivalentModifierMask = NSEventModifierFlagCommand | NSEventModifierFlagOption;
    [appMenu addItemWithTitle:@"Show All" action:@selector(unhideAllApplications:) keyEquivalent:@""];
    [appMenu addItem:NSMenuItem.separatorItem];
    [appMenu addItemWithTitle:[@"Quit " stringByAppendingString:app] action:@selector(terminate:) keyEquivalent:@"q"];

    NSMenu *file = [self submenu:@"File" in:main];
    [file addItemWithTitle:@"Close Window" action:@selector(performClose:) keyEquivalent:@"w"];

    NSMenu *edit = [self submenu:@"Edit" in:main];
    [edit addItemWithTitle:@"Copy" action:@selector(copy:) keyEquivalent:@"c"];
    [edit addItemWithTitle:@"Select All" action:@selector(selectAll:) keyEquivalent:@"a"];

    NSMenu *task = [self submenu:@"Task" in:main];
    [task addItemWithTitle:@"Stop" action:@selector(stopTask:) keyEquivalent:@"."];
    [task addItem:NSMenuItem.separatorItem];
    [task addItemWithTitle:@"Show in Finder" action:@selector(revealInFinder:) keyEquivalent:@"r"]
        .keyEquivalentModifierMask = NSEventModifierFlagCommand | NSEventModifierFlagShift;
    [task addItemWithTitle:@"Copy Command" action:@selector(copy:) keyEquivalent:@""];
    [task addItemWithTitle:@"Copy PID" action:@selector(copyPID:) keyEquivalent:@""];

    NSMenu *view = [self submenu:@"View" in:main];
    [view addItemWithTitle:@"Enter Full Screen" action:@selector(toggleFullScreen:) keyEquivalent:@"f"]
        .keyEquivalentModifierMask = NSEventModifierFlagCommand | NSEventModifierFlagControl;

    NSMenu *window = [self submenu:@"Window" in:main];
    [window addItemWithTitle:@"Minimize" action:@selector(performMiniaturize:) keyEquivalent:@"m"];
    [window addItemWithTitle:@"Zoom" action:@selector(performZoom:) keyEquivalent:@""];
    [window addItem:NSMenuItem.separatorItem];
    [window addItemWithTitle:app action:@selector(showMainWindow:) keyEquivalent:@"0"].target = self;
    [window addItem:NSMenuItem.separatorItem];
    [window addItemWithTitle:@"Bring All to Front" action:@selector(arrangeInFront:) keyEquivalent:@""];
    NSApp.windowsMenu = window;

    NSMenu *help = [self submenu:@"Help" in:main];
    NSApp.helpMenu = help;
    return main;
}

- (NSMenu *)submenu:(NSString *)title in:(NSMenu *)parent {
    NSMenuItem *item = [parent addItemWithTitle:title action:nil keyEquivalent:@""];
    NSMenu *menu = [[NSMenu alloc] initWithTitle:title];
    item.submenu = menu;
    return menu;
}

@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        NSApplication *app = NSApplication.sharedApplication;
        AppDelegate *delegate = [AppDelegate new];
        app.delegate = delegate;
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        [app run];
    }
    return 0;
}
