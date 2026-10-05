#import "SettingsWindowController.h"

#import "LockStore.h"

static NSTextField *Help(NSString *text) {
    NSTextField *label = [NSTextField wrappingLabelWithString:text];
    label.font = [NSFont systemFontOfSize:NSFont.smallSystemFontSize];
    label.textColor = NSColor.secondaryLabelColor;
    label.preferredMaxLayoutWidth = 300;
    return label;
}

static NSTextField *Value(void) {
    NSTextField *label = [NSTextField labelWithString:@""];
    label.font = [NSFont monospacedDigitSystemFontOfSize:NSFont.systemFontSize weight:NSFontWeightRegular];
    label.alignment = NSTextAlignmentRight;
    [label.widthAnchor constraintGreaterThanOrEqualToConstant:24].active = YES;
    return label;
}

@implementation SettingsWindowController {
    NSTextField *_slotsValue;
    NSStepper *_slotsStepper;
    NSButton *_jobsEnabled;
    NSTextField *_jobsValue;
    NSStepper *_jobsStepper;
    NSTextField *_timeoutField;
    /// Controls with a change on its way through `lock`, which store updates mustn't overwrite.
    NSMutableSet<NSString *> *_pending;
}

- (instancetype)init {
    NSWindow *window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 480, 200)
                                                   styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
                                                     backing:NSBackingStoreBuffered
                                                       defer:YES];
    if ((self = [super initWithWindow:window])) {
        window.title = @"Settings";
        window.delegate = self;
        window.frameAutosaveName = @"SettingsWindow";
        _pending = [NSMutableSet set];
        [self buildContent];
        [window center];
        [NSNotificationCenter.defaultCenter addObserver:self
                                               selector:@selector(storeDidChange:)
                                                   name:LockStoreDidChangeNotification
                                                 object:nil];
        [self storeDidChange:nil];
    }
    return self;
}

- (void)showWindow:(nullable id)sender {
    [super showWindow:sender];
    // AppKit would focus the timeout field and select its text. Nothing needs focus until clicked.
    [self.window makeFirstResponder:nil];
}

- (void)dealloc {
    [NSNotificationCenter.defaultCenter removeObserver:self];
}

- (void)buildContent {
    NSInteger cpus = (NSInteger)NSProcessInfo.processInfo.activeProcessorCount;

    _slotsValue = Value();
    _slotsStepper = [NSStepper new];
    _slotsStepper.minValue = 1;
    _slotsStepper.maxValue = MAX(1, cpus * 2);
    _slotsStepper.target = self;
    _slotsStepper.action = @selector(slotsChanged:);

    _jobsEnabled = [NSButton checkboxWithTitle:@"Share build jobs between tasks" target:self action:@selector(jobsToggled:)];
    _jobsValue = Value();
    _jobsStepper = [NSStepper new];
    _jobsStepper.minValue = 1;
    _jobsStepper.maxValue = MAX(1, cpus * 4);
    _jobsStepper.integerValue = cpus;
    _jobsStepper.target = self;
    _jobsStepper.action = @selector(jobsChanged:);

    _timeoutField = [NSTextField textFieldWithString:@""];
    _timeoutField.placeholderString = @"5s";
    _timeoutField.target = self;
    _timeoutField.action = @selector(timeoutChanged:);
    [_timeoutField.widthAnchor constraintEqualToConstant:80].active = YES;

    NSStackView *slots = [NSStackView stackViewWithViews:@[ _slotsValue, _slotsStepper ]];
    NSStackView *jobs = [NSStackView stackViewWithViews:@[ _jobsValue, _jobsStepper, [NSTextField labelWithString:@"jobs"] ]];
    NSGridView *grid = [NSGridView gridViewWithViews:@[
        @[ [NSTextField labelWithString:@"Shared slots:"], slots ],
        @[ NSGridCell.emptyContentView, Help(@"How many shared tasks, such as builds and test runs, may run at once.") ],
        @[ [NSTextField labelWithString:@"Jobserver:"], _jobsEnabled ],
        @[ NSGridCell.emptyContentView, jobs ],
        @[ NSGridCell.emptyContentView,
           Help(@"Ninja, make and cargo take one of these for each job they start, so builds running at the "
                @"same time split the machine between them. Takes effect for the next task that starts.") ],
        @[ [NSTextField labelWithString:@"Default timeout:"], _timeoutField ],
        @[ NSGridCell.emptyContentView,
           Help(@"How long a command run without -t may take. Keep it short: it's meant for quick commands, "
                @"so anything longer states its own estimate.") ],
    ]];
    grid.columnSpacing = 8;
    grid.rowSpacing = 6;
    [grid columnAtIndex:0].xPlacement = NSGridCellPlacementTrailing;
    grid.rowAlignment = NSGridRowAlignmentFirstBaseline;
    // A little more room between settings than between a setting and its help text.
    [grid rowAtIndex:2].topPadding = 12;
    [grid rowAtIndex:5].topPadding = 12;
    grid.translatesAutoresizingMaskIntoConstraints = NO;

    NSView *content = [NSView new];
    [content addSubview:grid];
    [NSLayoutConstraint activateConstraints:@[
        [grid.leadingAnchor constraintEqualToAnchor:content.leadingAnchor constant:24],
        [grid.trailingAnchor constraintEqualToAnchor:content.trailingAnchor constant:-24],
        [grid.topAnchor constraintEqualToAnchor:content.topAnchor constant:20],
        [grid.bottomAnchor constraintEqualToAnchor:content.bottomAnchor constant:-24],
    ]];
    self.window.contentView = content;
}

#pragma mark Data

- (void)storeDidChange:(nullable NSNotification *)note {
    LockSnapshot *snapshot = LockStore.sharedStore.snapshot;
    if (![_pending containsObject:@"slots"]) {
        _slotsStepper.integerValue = snapshot.sharedSlots;
        _slotsValue.integerValue = snapshot.sharedSlots;
    }
    if (![_pending containsObject:@"jobs"]) {
        BOOL on = snapshot.jobserverTokens > 0;
        _jobsEnabled.state = on ? NSControlStateValueOn : NSControlStateValueOff;
        // While it's off, keep showing the last size, ready to turn back on.
        if (on) _jobsStepper.integerValue = snapshot.jobserverTokens;
        _jobsValue.integerValue = _jobsStepper.integerValue;
        _jobsStepper.enabled = on;
        _jobsValue.textColor = on ? NSColor.labelColor : NSColor.disabledControlTextColor;
    }
    // Don't overwrite what someone is typing.
    if (![_pending containsObject:@"timeout"] && !_timeoutField.currentEditor) {
        _timeoutField.stringValue = LockFormatDuration(snapshot.defaultTimeout);
    }
}

#pragma mark Actions

- (void)slotsChanged:(NSStepper *)sender {
    _slotsValue.integerValue = sender.integerValue;
    [self apply:@"slots"
         change:^(void (^done)(NSString *)) { [LockStore.sharedStore setSharedSlots:sender.integerValue completion:done]; }];
}

- (void)jobsToggled:(NSButton *)sender {
    BOOL on = sender.state == NSControlStateValueOn;
    _jobsStepper.enabled = on;
    _jobsValue.textColor = on ? NSColor.labelColor : NSColor.disabledControlTextColor;
    NSInteger tokens = on ? _jobsStepper.integerValue : 0;
    [self apply:@"jobs"
         change:^(void (^done)(NSString *)) { [LockStore.sharedStore setJobserverTokens:tokens completion:done]; }];
}

- (void)jobsChanged:(NSStepper *)sender {
    _jobsValue.integerValue = sender.integerValue;
    [self apply:@"jobs"
         change:^(void (^done)(NSString *)) { [LockStore.sharedStore setJobserverTokens:sender.integerValue completion:done]; }];
}

- (void)timeoutChanged:(NSTextField *)sender {
    NSString *text = [sender.stringValue stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceCharacterSet];
    if (!text.length || [text isEqualToString:LockFormatDuration(LockStore.sharedStore.snapshot.defaultTimeout)]) return;
    [self apply:@"timeout"
         change:^(void (^done)(NSString *)) { [LockStore.sharedStore setDefaultTimeout:text completion:done]; }];
}

/// Run a change, holding off store updates for that control until it lands. On failure,
/// show why and put the control back to the stored value.
- (void)apply:(NSString *)control change:(void (^)(void (^done)(NSString *_Nullable error)))change {
    [_pending addObject:control];
    change(^(NSString *error) {
        [self->_pending removeObject:control];
        [self storeDidChange:nil];
        if (!error) return;
        NSAlert *alert = [NSAlert new];
        alert.messageText = @"Couldn’t change the setting";
        alert.informativeText = error;
        [alert beginSheetModalForWindow:self.window completionHandler:nil];
    });
}

@end
