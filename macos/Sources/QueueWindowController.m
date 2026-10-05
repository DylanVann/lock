#import "QueueWindowController.h"

#import "LockStore.h"

static NSUserInterfaceItemIdentifier const TaskCell = @"task";
static NSUserInterfaceItemIdentifier const HeaderCell = @"header";
static NSUserInterfaceItemIdentifier const PlaceholderCell = @"placeholder";

/// Leading space before the text column: icon inset + icon + gap. Placeholders line up with titles.
static const CGFloat TextInset = 4 + 20 + 10;

#pragma mark - Rows

typedef NS_ENUM(NSInteger, QueueRowKind) {
    QueueRowKindHeader,
    QueueRowKindPlaceholder,
    QueueRowKindTask,
};

@interface QueueRow : NSObject
@property (nonatomic) QueueRowKind kind;
@property (nonatomic, copy) NSString *text;
@property (nonatomic, copy, nullable) NSString *detail;
@property (nonatomic, strong, nullable) LockTask *task;
/// Identifies the row across refreshes; when the list of keys is unchanged, rows update in place.
@property (nonatomic, readonly) NSString *key;
@end

@implementation QueueRow
+ (instancetype)header:(NSString *)text detail:(nullable NSString *)detail {
    QueueRow *row = [QueueRow new];
    row.kind = QueueRowKindHeader;
    row.text = text;
    row.detail = detail;
    return row;
}
+ (instancetype)placeholder:(NSString *)text {
    QueueRow *row = [QueueRow new];
    row.kind = QueueRowKindPlaceholder;
    row.text = text;
    return row;
}
+ (instancetype)task:(LockTask *)task {
    QueueRow *row = [QueueRow new];
    row.kind = QueueRowKindTask;
    row.text = task.title;
    row.task = task;
    return row;
}
- (NSString *)key {
    switch (self.kind) {
    case QueueRowKindHeader: return [@"h:" stringByAppendingString:self.text];
    case QueueRowKindPlaceholder: return [@"p:" stringByAppendingString:self.text];
    case QueueRowKindTask:
        return [NSString stringWithFormat:@"t:%llu:%ld", self.task.taskID, (long)self.task.phase];
    }
}
@end

#pragma mark - Small views

static NSImage *Symbol(NSString *name, CGFloat pointSize, NSFontWeight weight) {
    NSImage *image = [NSImage imageWithSystemSymbolName:name accessibilityDescription:nil];
    return [image imageWithSymbolConfiguration:[NSImageSymbolConfiguration configurationWithPointSize:pointSize
                                                                                               weight:weight]];
}

static NSTextField *Label(NSFont *font) {
    NSTextField *label = [NSTextField labelWithString:@""];
    label.font = font;
    label.lineBreakMode = NSLineBreakByTruncatingTail;
    label.translatesAutoresizingMaskIntoConstraints = NO;
    [label setContentCompressionResistancePriority:NSLayoutPriorityDefaultLow
                                    forOrientation:NSLayoutConstraintOrientationHorizontal];
    return label;
}

/// A small gray capsule label, e.g. "Exclusive".
@interface TagView : NSView
@property (nonatomic, strong) NSTextField *label;
@property (nonatomic) BOOL emphasized;
@end

@implementation TagView
- (instancetype)initWithFrame:(NSRect)frame {
    if ((self = [super initWithFrame:frame])) {
        self.translatesAutoresizingMaskIntoConstraints = NO;
        _label = [NSTextField labelWithString:@""];
        _label.font = [NSFont systemFontOfSize:10 weight:NSFontWeightMedium];
        _label.textColor = NSColor.secondaryLabelColor;
        _label.translatesAutoresizingMaskIntoConstraints = NO;
        [self addSubview:_label];
        [NSLayoutConstraint activateConstraints:@[
            [_label.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:6],
            [_label.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-6],
            [_label.topAnchor constraintEqualToAnchor:self.topAnchor constant:1],
            [_label.bottomAnchor constraintEqualToAnchor:self.bottomAnchor constant:-1],
        ]];
        [self setContentCompressionResistancePriority:NSLayoutPriorityRequired
                                       forOrientation:NSLayoutConstraintOrientationHorizontal];
    }
    return self;
}
- (void)setEmphasized:(BOOL)emphasized {
    _emphasized = emphasized;
    self.label.textColor = emphasized ? NSColor.alternateSelectedControlTextColor : NSColor.secondaryLabelColor;
    self.needsDisplay = YES;
}
- (void)drawRect:(NSRect)dirty {
    NSRect bounds = self.bounds;
    NSColor *fill = self.emphasized ? [NSColor.whiteColor colorWithAlphaComponent:0.25] : NSColor.quaternaryLabelColor;
    [fill setFill];
    CGFloat radius = bounds.size.height / 2;
    [[NSBezierPath bezierPathWithRoundedRect:bounds xRadius:radius yRadius:radius] fill];
}
@end

/// "times out in 40s" during the last fifth of the timeout. Mirrors `Task::timeout_warning`
/// in the Rust library.
static NSString *_Nullable TimeoutWarning(LockTask *task, uint64_t now) {
    if (!task.deadline) return nil;
    uint64_t left = task.deadline > now ? task.deadline - now : 0;
    if (left == 0) return @"past its timeout";
    if (left <= task.timeout / 5)
        return [NSString stringWithFormat:@"times out in %@", LockFormatDuration(left)];
    return nil;
}

#pragma mark - Text cells

/// Section headers ("Running  2 of 3 slots") and placeholders ("Nothing running"). The
/// labels deliberately aren't the cell's `textField`: AppKit restyles that outlet (font,
/// color) when it reuses rows, especially group rows, which made headers change look after
/// scrolling away and back. Here only `configure...` sets their style.
@interface TextCellView : NSTableCellView
@property (nonatomic, strong) NSTextField *title;
@property (nonatomic, strong) NSTextField *detail;
@end

@implementation TextCellView
- (instancetype)initWithIdentifier:(NSUserInterfaceItemIdentifier)identifier inset:(CGFloat)inset offset:(CGFloat)offset {
    if ((self = [super initWithFrame:NSZeroRect])) {
        self.identifier = identifier;
        _title = Label([NSFont systemFontOfSize:NSFont.systemFontSize]);
        _detail = Label([NSFont systemFontOfSize:NSFont.systemFontSize]);
        NSStackView *row = [NSStackView stackViewWithViews:@[ _title, _detail ]];
        row.spacing = 6;
        row.alignment = NSLayoutAttributeFirstBaseline;
        row.translatesAutoresizingMaskIntoConstraints = NO;
        [self addSubview:row];
        [NSLayoutConstraint activateConstraints:@[
            [row.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:inset],
            [row.trailingAnchor constraintLessThanOrEqualToAnchor:self.trailingAnchor constant:-4],
            [row.centerYAnchor constraintEqualToAnchor:self.centerYAnchor constant:offset],
        ]];
    }
    return self;
}

- (void)configureHeader:(NSString *)title detail:(nullable NSString *)detail {
    self.title.stringValue = title;
    self.title.font = [NSFont systemFontOfSize:NSFont.systemFontSize weight:NSFontWeightBold];
    self.title.textColor = NSColor.labelColor;
    self.detail.stringValue = detail ?: @"";
    self.detail.hidden = detail == nil;
    self.detail.font = [NSFont systemFontOfSize:NSFont.systemFontSize];
    self.detail.textColor = NSColor.tertiaryLabelColor;
}

- (void)configurePlaceholder:(NSString *)text {
    self.title.stringValue = text;
    self.title.font = [NSFont systemFontOfSize:NSFont.systemFontSize];
    self.title.textColor = NSColor.tertiaryLabelColor;
    self.detail.hidden = YES;
}
@end

#pragma mark - Task cell

/// Two-line row: status icon, title (+ tag), optional progress bar, detail line, and an action button.
@interface TaskCellView : NSTableCellView
@property (nonatomic, strong) NSImageView *icon;
@property (nonatomic, strong) NSProgressIndicator *spinner;
@property (nonatomic, strong) NSTextField *title;
@property (nonatomic, strong) TagView *kindTag;
@property (nonatomic, strong) NSProgressIndicator *bar;
@property (nonatomic, strong) NSTextField *detail;
@property (nonatomic, strong) NSButton *action;
@property (nonatomic, strong, nullable) NSColor *iconColor;
- (void)configureWithTask:(LockTask *)task now:(uint64_t)now;
@end

@implementation TaskCellView

- (instancetype)initWithTarget:(id)target action:(SEL)action {
    if ((self = [super initWithFrame:NSZeroRect])) {
        self.identifier = TaskCell;

        _icon = [NSImageView new];
        _icon.translatesAutoresizingMaskIntoConstraints = NO;
        _spinner = [NSProgressIndicator new];
        _spinner.style = NSProgressIndicatorStyleSpinning;
        _spinner.controlSize = NSControlSizeSmall;
        _spinner.indeterminate = YES;
        _spinner.displayedWhenStopped = NO;
        _spinner.translatesAutoresizingMaskIntoConstraints = NO;

        _title = Label([NSFont systemFontOfSize:NSFont.systemFontSize weight:NSFontWeightMedium]);
        _kindTag = [TagView new];
        _kindTag.label.stringValue = @"Exclusive";
        NSStackView *titleRow = [NSStackView stackViewWithViews:@[ _title, _kindTag ]];
        titleRow.spacing = 6;
        titleRow.alignment = NSLayoutAttributeCenterY;

        _bar = [NSProgressIndicator new];
        _bar.style = NSProgressIndicatorStyleBar;
        _bar.controlSize = NSControlSizeSmall;
        _bar.minValue = 0;
        _bar.maxValue = 1;
        _bar.translatesAutoresizingMaskIntoConstraints = NO;
        _detail = Label([NSFont monospacedDigitSystemFontOfSize:NSFont.smallSystemFontSize weight:NSFontWeightRegular]);

        NSStackView *text = [NSStackView stackViewWithViews:@[ titleRow, _bar, _detail ]];
        text.orientation = NSUserInterfaceLayoutOrientationVertical;
        text.alignment = NSLayoutAttributeLeading;
        text.spacing = 4;
        text.translatesAutoresizingMaskIntoConstraints = NO;

        _action = [NSButton buttonWithImage:Symbol(@"xmark.circle.fill", 15, NSFontWeightRegular)
                                     target:target
                                     action:action];
        _action.bordered = NO;
        _action.translatesAutoresizingMaskIntoConstraints = NO;
        _action.contentTintColor = NSColor.tertiaryLabelColor;

        for (NSView *view in @[ _icon, _spinner, text, _action ]) [self addSubview:view];
        [NSLayoutConstraint activateConstraints:@[
            [_icon.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:4],
            [_icon.widthAnchor constraintEqualToConstant:20],
            [_icon.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
            [_spinner.centerXAnchor constraintEqualToAnchor:_icon.centerXAnchor],
            [_spinner.centerYAnchor constraintEqualToAnchor:_icon.centerYAnchor],
            [text.leadingAnchor constraintEqualToAnchor:_icon.trailingAnchor constant:10],
            [text.trailingAnchor constraintEqualToAnchor:_action.leadingAnchor constant:-12],
            [text.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
            [_bar.widthAnchor constraintEqualToAnchor:text.widthAnchor],
            [titleRow.trailingAnchor constraintLessThanOrEqualToAnchor:text.trailingAnchor],
            [_detail.trailingAnchor constraintLessThanOrEqualToAnchor:text.trailingAnchor],
            [_action.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-6],
            [_action.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
            [_action.widthAnchor constraintEqualToConstant:20],
        ]];
    }
    return self;
}

- (void)configureWithTask:(LockTask *)task now:(uint64_t)now {
    self.title.stringValue = task.title;
    self.kindTag.hidden = !task.exclusive && !task.light;
    self.kindTag.label.stringValue = task.exclusive ? @"Exclusive" : task.paused ? @"Light · paused" : @"Light";
    self.toolTip = [NSString stringWithFormat:@"%@\n%@\nPID %d", task.commandLine, task.cwd, task.displayPID];

    NSMutableArray<NSString *> *parts = [NSMutableArray array];
    BOOL running = task.phase == LockTaskPhaseRunning;
    // Progress is judged against how long this task usually takes, not the timeout, which is
    // only an upper bound. No history means no bar; running over the usual time goes indeterminate.
    self.bar.hidden = !(running && task.expected);
    if (running) {
        [self.spinner startAnimation:nil];
        uint64_t elapsed = now > task.startedAt ? now - task.startedAt : 0;
        if (task.expected && elapsed < task.expected) {
            [self setBarIndeterminate:NO];
            self.bar.doubleValue = (double)elapsed / (double)task.expected;
            [parts addObject:[NSString stringWithFormat:@"%@ of ~%@", LockFormatDuration(elapsed),
                                                        LockFormatDuration(task.expected)]];
        } else if (task.expected) {
            [self setBarIndeterminate:YES];
            [parts addObject:[NSString stringWithFormat:@"%@, usually ~%@", LockFormatDuration(elapsed),
                                                        LockFormatDuration(task.expected)]];
        } else {
            [self setBarIndeterminate:NO];
            [parts addObject:LockFormatDuration(elapsed)];
        }
        uint64_t waited = task.startedAt > task.enqueuedAt ? task.startedAt - task.enqueuedAt : 0;
        if (waited >= 1000) [parts addObject:[NSString stringWithFormat:@"waited %@", LockFormatDuration(waited)]];
        NSString *warning = TimeoutWarning(task, now);
        if (warning) [parts addObject:warning];
        [self setIcon:nil color:nil];
        self.action.image = Symbol(@"xmark.circle.fill", 15, NSFontWeightRegular);
        self.action.toolTip = @"Stop";
    } else {
        [self.spinner stopAnimation:nil];
        [self setBarIndeterminate:NO];
        if (task.phase == LockTaskPhaseWaiting) {
            uint64_t waiting = now > task.enqueuedAt ? now - task.enqueuedAt : 0;
            [parts addObject:[NSString stringWithFormat:@"#%ld in queue", (long)task.queuePosition]];
            [parts addObject:[NSString stringWithFormat:@"waiting %@", LockFormatDuration(waiting)]];
            if (task.expected) [parts addObject:[NSString stringWithFormat:@"usually ~%@", LockFormatDuration(task.expected)]];
            if (task.timeout) [parts addObject:[NSString stringWithFormat:@"limit %@", LockFormatDuration(task.timeout)]];
            [self setIcon:@"clock" color:nil];
            self.action.image = Symbol(@"xmark.circle.fill", 15, NSFontWeightRegular);
            self.action.toolTip = @"Remove from Queue";
        } else {
            [parts addObject:[self finishedSummary:task]];
            [parts addObject:LockFormatAgo(task.endedAt, now)];
            [self setFinishedIcon:task];
            self.action.image = Symbol(@"magnifyingglass.circle.fill", 15, NSFontWeightRegular);
            self.action.toolTip = @"Show in Finder";
        }
    }
    [parts addObject:task.location];
    if (task.agent) [parts addObject:task.agent];
    self.detail.stringValue = [parts componentsJoinedByString:@"  ·  "];
    [self applyColors];
}

- (void)setBarIndeterminate:(BOOL)indeterminate {
    if (self.bar.indeterminate == indeterminate) return;
    self.bar.indeterminate = indeterminate;
    if (indeterminate) {
        [self.bar startAnimation:nil];
    } else {
        [self.bar stopAnimation:nil];
    }
}

/// "Completed in 22s", "Exit 3 after 5s", "Gave up after waiting 1m02s", ...
- (NSString *)finishedSummary:(LockTask *)task {
    uint64_t ran = task.startedAt && task.endedAt > task.startedAt ? task.endedAt - task.startedAt : 0;
    if (!task.startedAt) {
        uint64_t waited = task.endedAt > task.enqueuedAt ? task.endedAt - task.enqueuedAt : 0;
        NSString *verb = [task.outcome isEqualToString:@"cancelled"] ? @"Removed" : @"Gave up";
        return [NSString stringWithFormat:@"%@ after waiting %@", verb, LockFormatDuration(waited)];
    }
    if (task.succeeded) return [NSString stringWithFormat:@"Completed in %@", LockFormatDuration(ran)];
    return [NSString stringWithFormat:@"%@ after %@", task.outcomeLabel, LockFormatDuration(ran)];
}

- (void)setFinishedIcon:(LockTask *)task {
    NSString *outcome = task.outcome;
    if (task.succeeded) {
        [self setIcon:@"checkmark.circle.fill" color:NSColor.systemGreenColor];
    } else if ([outcome isEqualToString:@"timed_out"]) {
        [self setIcon:@"exclamationmark.triangle.fill" color:NSColor.systemOrangeColor];
    } else if ([outcome isEqualToString:@"cancelled"] || [outcome isEqualToString:@"abandoned"]) {
        [self setIcon:@"minus.circle.fill" color:nil];
    } else {
        [self setIcon:@"xmark.circle.fill" color:NSColor.systemRedColor];
    }
}

- (void)setIcon:(nullable NSString *)symbol color:(nullable NSColor *)color {
    self.icon.image = symbol ? Symbol(symbol, 15, NSFontWeightRegular) : nil;
    self.iconColor = color;
}

- (void)setBackgroundStyle:(NSBackgroundStyle)style {
    [super setBackgroundStyle:style];
    [self applyColors];
}

/// Selected rows in a key window get white text and icons on the accent color.
- (void)applyColors {
    BOOL emphasized = self.backgroundStyle == NSBackgroundStyleEmphasized;
    NSColor *onAccent = NSColor.alternateSelectedControlTextColor;
    self.title.textColor = emphasized ? onAccent : NSColor.labelColor;
    self.detail.textColor = emphasized ? [onAccent colorWithAlphaComponent:0.8] : NSColor.secondaryLabelColor;
    self.icon.contentTintColor = emphasized ? onAccent : (self.iconColor ?: NSColor.secondaryLabelColor);
    self.action.contentTintColor = emphasized ? [onAccent colorWithAlphaComponent:0.8] : NSColor.tertiaryLabelColor;
    self.kindTag.emphasized = emphasized;
}

@end

#pragma mark - Window controller

@interface QueueWindowController () <NSTableViewDataSource, NSTableViewDelegate, NSMenuDelegate,
                                     NSMenuItemValidation>
@end

@implementation QueueWindowController {
    NSTableView *_table;
    NSScrollView *_scroll;
    NSView *_emptyState;
    NSArray<QueueRow *> *_rows;
    NSArray<NSString *> *_rowKeys;
    NSMenu *_rowMenu;
    /// The task a context menu was opened on. Captured because rows update every second.
    LockTask *_menuTask;
}

- (instancetype)init {
    NSWindow *window = [[NSWindow alloc]
        initWithContentRect:NSMakeRect(0, 0, 640, 600)
                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskMiniaturizable |
                            NSWindowStyleMaskResizable | NSWindowStyleMaskFullSizeContentView
                    backing:NSBackingStoreBuffered
                      defer:YES];
    if ((self = [super initWithWindow:window])) {
        window.title = @"Lock";
        window.delegate = self;
        window.minSize = NSMakeSize(440, 300);
        window.tabbingMode = NSWindowTabbingModeDisallowed;
        [window center];
        window.frameAutosaveName = @"QueueWindow";

        _rows = @[];
        _rowKeys = @[];
        [self buildContent];
        [NSNotificationCenter.defaultCenter addObserver:self
                                               selector:@selector(storeDidChange:)
                                                   name:LockStoreDidChangeNotification
                                                 object:nil];
        [self storeDidChange:nil];
    }
    return self;
}

- (void)dealloc {
    [NSNotificationCenter.defaultCenter removeObserver:self];
}

#pragma mark Building

- (void)buildContent {
    _table = [NSTableView new];
    _table.style = NSTableViewStyleInset;
    // Our cells set their own fonts; the default style would have AppKit resize cell text.
    _table.rowSizeStyle = NSTableViewRowSizeStyleCustom;
    _table.headerView = nil;
    _table.allowsMultipleSelection = NO;
    _table.floatsGroupRows = NO;
    _table.intercellSpacing = NSMakeSize(0, 2);
    _table.dataSource = self;
    _table.delegate = self;
    _table.target = self;
    _table.doubleAction = @selector(tableDoubleClicked:);
    NSTableColumn *column = [[NSTableColumn alloc] initWithIdentifier:@"main"];
    column.resizingMask = NSTableColumnAutoresizingMask;
    [_table addTableColumn:column];
    _table.columnAutoresizingStyle = NSTableViewUniformColumnAutoresizingStyle;

    _rowMenu = [NSMenu new];
    _rowMenu.delegate = self;
    _table.menu = _rowMenu;

    _scroll = [NSScrollView new];
    _scroll.documentView = _table;
    _scroll.hasVerticalScroller = YES;
    _scroll.autohidesScrollers = YES;
    _scroll.translatesAutoresizingMaskIntoConstraints = NO;

    _emptyState = [self buildEmptyState];

    NSView *content = [NSView new];
    [content addSubview:_scroll];
    [content addSubview:_emptyState];
    [NSLayoutConstraint activateConstraints:@[
        [_scroll.leadingAnchor constraintEqualToAnchor:content.leadingAnchor],
        [_scroll.trailingAnchor constraintEqualToAnchor:content.trailingAnchor],
        [_scroll.topAnchor constraintEqualToAnchor:content.topAnchor],
        [_scroll.bottomAnchor constraintEqualToAnchor:content.bottomAnchor],
        [_emptyState.centerXAnchor constraintEqualToAnchor:content.centerXAnchor],
        [_emptyState.centerYAnchor constraintEqualToAnchor:content.safeAreaLayoutGuide.centerYAnchor],
        [_emptyState.widthAnchor constraintLessThanOrEqualToAnchor:content.widthAnchor constant:-40],
    ]];
    self.window.contentView = content;
}

/// Shown when there's nothing running, queued or in recent history.
- (NSView *)buildEmptyState {
    NSImageView *image = [NSImageView imageViewWithImage:Symbol(@"cpu", 40, NSFontWeightLight)];
    image.contentTintColor = NSColor.tertiaryLabelColor;
    NSTextField *title = [NSTextField labelWithString:@"No Tasks"];
    title.font = [NSFont systemFontOfSize:17 weight:NSFontWeightSemibold];
    title.textColor = NSColor.secondaryLabelColor;
    NSTextField *body = [NSTextField wrappingLabelWithString:@"Commands started with lock will appear here."];
    body.textColor = NSColor.tertiaryLabelColor;
    body.alignment = NSTextAlignmentCenter;
    NSStackView *stack = [NSStackView stackViewWithViews:@[ image, title, body ]];
    stack.orientation = NSUserInterfaceLayoutOrientationVertical;
    stack.spacing = 6;
    [stack setCustomSpacing:12 afterView:image];
    stack.translatesAutoresizingMaskIntoConstraints = NO;
    return stack;
}

#pragma mark Data

- (void)storeDidChange:(nullable NSNotification *)note {
    LockSnapshot *snapshot = LockStore.sharedStore.snapshot;
    // The sections say what's running; the subtitle is only for a queue that can't be read.
    self.window.subtitle = snapshot.error ? [NSString stringWithFormat:@"Error: %@", snapshot.error] : @"";

    BOOL empty = !snapshot.running.count && !snapshot.waiting.count && !snapshot.history.count;
    _emptyState.hidden = !empty;
    _scroll.hidden = empty;

    NSMutableArray<QueueRow *> *rows = [NSMutableArray array];
    NSString *capacity = snapshot.exclusiveHolder
                             ? @"Exclusive"
                             : [NSString stringWithFormat:@"%ld of %ld slots", (long)snapshot.sharedRunning,
                                                          (long)snapshot.sharedSlots];
    [rows addObject:[QueueRow header:@"Running" detail:capacity]];
    if (!snapshot.running.count) [rows addObject:[QueueRow placeholder:@"Nothing running"]];
    for (LockTask *task in snapshot.running) [rows addObject:[QueueRow task:task]];

    NSString *waiting = snapshot.waiting.count ? [NSString stringWithFormat:@"%lu", (unsigned long)snapshot.waiting.count] : nil;
    [rows addObject:[QueueRow header:@"Queue" detail:waiting]];
    if (!snapshot.waiting.count) [rows addObject:[QueueRow placeholder:@"No one is waiting"]];
    for (LockTask *task in snapshot.waiting) [rows addObject:[QueueRow task:task]];

    if (snapshot.history.count) {
        [rows addObject:[QueueRow header:@"Recent" detail:nil]];
        for (LockTask *task in snapshot.history) [rows addObject:[QueueRow task:task]];
    }

    NSArray<NSString *> *keys = [rows valueForKey:@"key"];
    _rows = rows;
    if ([keys isEqualToArray:_rowKeys]) {
        // Same rows as before: refresh the visible ones in place so spinners and selection are undisturbed.
        NSRange visible = [_table rowsInRect:_table.visibleRect];
        for (NSUInteger i = visible.location; i < NSMaxRange(visible); i++) {
            NSView *view = [_table viewAtColumn:0 row:(NSInteger)i makeIfNecessary:NO];
            if (view) [self configureView:view row:_rows[i]];
        }
        return;
    }
    // Keep the selection on the same task, even as it moves between sections.
    LockTask *selected = self.selectedTask;
    _rowKeys = keys;
    [_table reloadData];
    if (selected) {
        [self selectTaskWithID:selected.taskID];
    } else {
        [_table deselectAll:nil];
    }
}

- (nullable LockTask *)selectedTask {
    NSInteger row = _table.selectedRow;
    return row >= 0 && row < (NSInteger)_rows.count ? _rows[row].task : nil;
}

/// For menu actions: the right-clicked task if a context menu is open, otherwise the selection.
- (nullable LockTask *)targetTask {
    return _menuTask ?: self.selectedTask;
}

- (void)selectTaskWithID:(uint64_t)taskID {
    // Prefer the live entry over a history entry with the same ID.
    NSInteger match = -1;
    for (NSInteger i = 0; i < (NSInteger)_rows.count; i++) {
        LockTask *task = _rows[i].task;
        if (!task || task.taskID != taskID) continue;
        match = i;
        if (task.phase != LockTaskPhaseFinished) break;
    }
    if (match >= 0) {
        [_table selectRowIndexes:[NSIndexSet indexSetWithIndex:(NSUInteger)match] byExtendingSelection:NO];
        [_table scrollRowToVisible:match];
    } else {
        [_table deselectAll:nil];
    }
}

#pragma mark Table

- (NSInteger)numberOfRowsInTableView:(NSTableView *)tableView {
    return (NSInteger)_rows.count;
}

- (BOOL)tableView:(NSTableView *)tableView isGroupRow:(NSInteger)row {
    return _rows[row].kind == QueueRowKindHeader;
}

- (BOOL)tableView:(NSTableView *)tableView shouldSelectRow:(NSInteger)row {
    return _rows[row].kind == QueueRowKindTask;
}

- (CGFloat)tableView:(NSTableView *)tableView heightOfRow:(NSInteger)row {
    QueueRow *item = _rows[row];
    switch (item.kind) {
    case QueueRowKindHeader: return 28;
    case QueueRowKindPlaceholder: return 30;
    case QueueRowKindTask: return item.task.phase == LockTaskPhaseRunning && item.task.expected ? 58 : 46;
    }
}

- (nullable NSView *)tableView:(NSTableView *)tableView viewForTableColumn:(nullable NSTableColumn *)column row:(NSInteger)index {
    QueueRow *row = _rows[index];
    NSView *view;
    switch (row.kind) {
    case QueueRowKindHeader:
        // Headers sit a little low in their row, closer to the rows they introduce.
        view = [tableView makeViewWithIdentifier:HeaderCell owner:self]
                   ?: [[TextCellView alloc] initWithIdentifier:HeaderCell inset:4 offset:3];
        break;
    case QueueRowKindPlaceholder:
        view = [tableView makeViewWithIdentifier:PlaceholderCell owner:self]
                   ?: [[TextCellView alloc] initWithIdentifier:PlaceholderCell inset:TextInset offset:0];
        break;
    case QueueRowKindTask:
        view = [tableView makeViewWithIdentifier:TaskCell owner:self]
                   ?: [[TaskCellView alloc] initWithTarget:self action:@selector(rowButtonClicked:)];
        break;
    }
    [self configureView:view row:row];
    return view;
}

- (void)configureView:(NSView *)view row:(QueueRow *)row {
    switch (row.kind) {
    case QueueRowKindHeader:
        [(TextCellView *)view configureHeader:row.text detail:row.detail];
        break;
    case QueueRowKindPlaceholder:
        [(TextCellView *)view configurePlaceholder:row.text];
        break;
    case QueueRowKindTask:
        [(TaskCellView *)view configureWithTask:row.task now:LockNowMs()];
        break;
    }
}

#pragma mark Menus

- (void)menuNeedsUpdate:(NSMenu *)menu {
    [menu removeAllItems];
    NSInteger clicked = _table.clickedRow;
    _menuTask = clicked >= 0 && clicked < (NSInteger)_rows.count ? _rows[clicked].task : nil;
    if (!_menuTask) return;
    if (_menuTask.phase != LockTaskPhaseFinished) {
        NSString *stop = _menuTask.phase == LockTaskPhaseWaiting ? @"Remove from Queue" : @"Stop";
        [menu addItemWithTitle:stop action:@selector(stopTask:) keyEquivalent:@""].target = self;
        [menu addItem:NSMenuItem.separatorItem];
    }
    [menu addItemWithTitle:@"Show in Finder" action:@selector(revealInFinder:) keyEquivalent:@""].target = self;
    [menu addItemWithTitle:@"Copy Command" action:@selector(copy:) keyEquivalent:@""].target = self;
    [menu addItemWithTitle:@"Copy PID" action:@selector(copyPID:) keyEquivalent:@""].target = self;
}

- (void)menuDidClose:(NSMenu *)menu {
    // Actions are dispatched after the menu closes, so clear the captured task on the next turn.
    dispatch_async(dispatch_get_main_queue(), ^{ self->_menuTask = nil; });
}

- (BOOL)validateMenuItem:(NSMenuItem *)item {
    LockTask *task = self.targetTask;
    SEL action = item.action;
    if (action == @selector(stopTask:)) return task && task.phase != LockTaskPhaseFinished;
    if (action == @selector(revealInFinder:) || action == @selector(copy:) || action == @selector(copyPID:))
        return task != nil;
    return YES;
}

#pragma mark Actions

- (void)rowButtonClicked:(NSButton *)sender {
    NSInteger row = [_table rowForView:sender];
    if (row < 0 || row >= (NSInteger)_rows.count) return;
    LockTask *task = _rows[row].task;
    if (!task) return;
    if (task.phase == LockTaskPhaseFinished) {
        [self revealTask:task];
    } else {
        [self confirmStop:task];
    }
}

- (IBAction)stopTask:(id)sender {
    LockTask *task = self.targetTask;
    if (task && task.phase != LockTaskPhaseFinished) [self confirmStop:task];
}

- (void)confirmStop:(LockTask *)task {
    BOOL waiting = task.phase == LockTaskPhaseWaiting;
    NSAlert *alert = [NSAlert new];
    alert.messageText = waiting ? [NSString stringWithFormat:@"Remove “%@” from the queue?", task.title]
                                : [NSString stringWithFormat:@"Stop “%@”?", task.title];
    NSString *who = task.agent ? [NSString stringWithFormat:@" started by %@", task.agent] : @"";
    alert.informativeText =
        waiting ? [NSString stringWithFormat:@"The command%@ won’t run.", who]
                : [NSString stringWithFormat:@"The command%@ in %@ will be terminated and its lock released.", who,
                                             task.location];
    NSButton *confirm = [alert addButtonWithTitle:waiting ? @"Remove" : @"Stop"];
    confirm.hasDestructiveAction = YES;
    [alert addButtonWithTitle:@"Cancel"];
    uint64_t taskID = task.taskID;
    [alert beginSheetModalForWindow:self.window
                  completionHandler:^(NSModalResponse response) {
                      if (response != NSAlertFirstButtonReturn) return;
                      [LockStore.sharedStore cancelTask:taskID
                                             completion:^(NSString *error) {
                                                 if (error) [self showError:error title:@"Couldn’t stop the task"];
                                             }];
                  }];
}

- (IBAction)revealInFinder:(id)sender {
    LockTask *task = self.targetTask;
    if (task) [self revealTask:task];
}

- (void)revealTask:(LockTask *)task {
    [NSWorkspace.sharedWorkspace activateFileViewerSelectingURLs:@[ [NSURL fileURLWithPath:task.projectPath] ]];
}

- (IBAction)copy:(id)sender {
    LockTask *task = self.targetTask;
    if (!task) return;
    [NSPasteboard.generalPasteboard clearContents];
    [NSPasteboard.generalPasteboard setString:task.commandLine forType:NSPasteboardTypeString];
}

- (IBAction)copyPID:(id)sender {
    LockTask *task = self.targetTask;
    if (!task) return;
    [NSPasteboard.generalPasteboard clearContents];
    [NSPasteboard.generalPasteboard setString:[NSString stringWithFormat:@"%d", task.displayPID]
                                      forType:NSPasteboardTypeString];
}

- (void)tableDoubleClicked:(id)sender {
    NSInteger row = _table.clickedRow;
    if (row >= 0 && _rows[row].task) [self revealTask:_rows[row].task];
}

- (void)showError:(NSString *)message title:(NSString *)title {
    NSAlert *alert = [NSAlert new];
    alert.messageText = title;
    alert.informativeText = message;
    [alert beginSheetModalForWindow:self.window completionHandler:nil];
}

@end
