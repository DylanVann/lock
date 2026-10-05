#import "LockStore.h"

#import <libproc.h>
#import <signal.h>
#import <sys/proc_info.h>
#import <sys/time.h>

NSNotificationName const LockStoreDidChangeNotification = @"LockStoreDidChangeNotification";

uint64_t LockNowMs(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (uint64_t)tv.tv_sec * 1000 + (uint64_t)tv.tv_usec / 1000;
}

NSString *LockFormatDuration(uint64_t ms) {
    uint64_t s = ms / 1000;
    if (s < 60) return [NSString stringWithFormat:@"%llus", s];
    if (s < 3600) return [NSString stringWithFormat:@"%llum%02llus", s / 60, s % 60];
    return [NSString stringWithFormat:@"%lluh%02llum", s / 3600, (s % 3600) / 60];
}

NSString *LockFormatAgo(uint64_t thenMs, uint64_t nowMs) {
    if (nowMs < thenMs + 60 * 1000) return @"just now";
    static NSRelativeDateTimeFormatter *formatter;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        formatter = [NSRelativeDateTimeFormatter new];
        formatter.unitsStyle = NSRelativeDateTimeFormatterUnitsStyleFull;
    });
    return [formatter localizedStringForDate:[NSDate dateWithTimeIntervalSince1970:thenMs / 1000.0]
                              relativeToDate:[NSDate dateWithTimeIntervalSince1970:nowMs / 1000.0]];
}

/// Mirrors `Proc::is_alive` in the Rust library: the PID must exist, not be a zombie,
/// and (when recorded) have the same start time, so a reused PID doesn't count.
static BOOL LockProcessAlive(pid_t pid, uint64_t recordedStart) {
    if (pid <= 0) return NO;
    if (kill(pid, 0) != 0 && errno != EPERM) return NO;
    struct proc_bsdinfo info;
    int size = (int)sizeof(info);
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, size) != size) {
        // A zombie still answers kill(pid, 0), but has no task info left.
        return errno != ESRCH;
    }
    if (recordedStart == 0) return YES;
    uint64_t start = info.pbi_start_tvsec * 1000000ull + info.pbi_start_tvusec;
    return start == recordedStart;
}

static uint64_t U64(id value) {
    return [value isKindOfClass:NSNumber.class] ? [value unsignedLongLongValue] : 0;
}

static NSString *_Nullable Str(id value) {
    return [value isKindOfClass:NSString.class] ? value : nil;
}

#pragma mark - LockTask

@implementation LockTask

+ (nullable instancetype)taskWithJSON:(NSDictionary *)json phase:(LockTaskPhase)phase {
    if (![json isKindOfClass:NSDictionary.class]) return nil;
    NSDictionary *spec = json[@"spec"];
    if (![spec isKindOfClass:NSDictionary.class]) return nil;
    LockTask *task = [LockTask new];
    task.taskID = U64(json[@"id"]);
    task.phase = phase;
    task.exclusive = [Str(spec[@"kind"]) isEqualToString:@"exclusive"];
    task.light = [spec[@"light"] isKindOfClass:NSNumber.class] && [spec[@"light"] boolValue];
    task.parentID = U64(json[@"parent"]);
    task.paused = U64(json[@"paused_at_ms"]) > 0;
    task.name = Str(spec[@"name"]);
    task.agent = Str(spec[@"agent"]);
    task.cwd = Str(spec[@"cwd"]) ?: @"";
    task.repo = Str(spec[@"repo"]);
    task.branch = Str(spec[@"branch"]);
    NSArray *command = spec[@"command"];
    task.command = [command isKindOfClass:NSArray.class] ? command : @[];
    task.timeout = U64(spec[@"timeout_ms"]);
    NSDictionary *owner = json[@"owner"];
    if ([owner isKindOfClass:NSDictionary.class]) {
        task.ownerPID = (pid_t)U64(owner[@"pid"]);
        task.ownerStarted = U64(owner[@"started"]);
    }
    NSDictionary *child = json[@"child"];
    if ([child isKindOfClass:NSDictionary.class]) task.childPID = (pid_t)U64(child[@"pid"]);
    task.enqueuedAt = U64(json[@"enqueued_at_ms"]);
    task.startedAt = U64(json[@"started_at_ms"]);
    task.deadline = U64(json[@"deadline_ms"]);
    task.expected = U64(json[@"expected_ms"]);
    return task;
}

- (NSString *)title {
    return self.name.length ? self.name : self.commandLine;
}

- (NSString *)commandLine {
    return [self.command componentsJoinedByString:@" "];
}

- (NSString *)projectPath {
    return self.repo ?: self.cwd;
}

- (NSString *)location {
    NSString *name = self.projectPath.lastPathComponent;
    if (!name.length) name = self.projectPath;
    return self.branch ? [NSString stringWithFormat:@"%@@%@", name, self.branch] : name;
}

- (pid_t)displayPID {
    return self.childPID ?: self.ownerPID;
}

- (NSString *)outcomeLabel {
    NSString *outcome = self.outcome;
    if ([outcome isEqualToString:@"completed"]) {
        if (!self.exitCode) return @"Killed";
        return self.exitCode.intValue == 0 ? @"Completed"
                                           : [NSString stringWithFormat:@"Exit %d", self.exitCode.intValue];
    }
    if ([outcome isEqualToString:@"timed_out"]) return @"Timed Out";
    if ([outcome isEqualToString:@"cancelled"]) return @"Stopped";
    if ([outcome isEqualToString:@"vanished"]) return @"Vanished";
    if ([outcome isEqualToString:@"abandoned"]) return @"Gave Up";
    return outcome.capitalizedString ?: @"";
}

- (BOOL)succeeded {
    return [self.outcome isEqualToString:@"completed"] && self.exitCode && self.exitCode.intValue == 0;
}

@end

#pragma mark - LockSnapshot

@implementation LockSnapshot

- (nullable LockTask *)exclusiveHolder {
    for (LockTask *task in self.running)
        if (task.exclusive) return task;
    return nil;
}

- (NSInteger)sharedRunning {
    // Light tasks, and tasks running under another running task's lease, take no slot.
    NSMutableSet<NSNumber *> *leaseHolders = [NSMutableSet set];
    for (LockTask *task in self.running)
        if (!task.light) [leaseHolders addObject:@(task.taskID)];
    NSInteger count = 0;
    for (LockTask *task in self.running)
        if (!task.exclusive && !task.light && ![leaseHolders containsObject:@(task.parentID)]) count++;
    return count;
}

- (NSInteger)lightRunning {
    NSInteger count = 0;
    for (LockTask *task in self.running)
        if (task.light) count++;
    return count;
}

- (NSString *)summary {
    NSMutableArray<NSString *> *parts = [NSMutableArray array];
    if (self.exclusiveHolder) {
        // Not the task's name: it's listed right below, under Running.
        [parts addObject:@"Exclusive lock held"];
    } else if (self.sharedRunning > 0) {
        [parts addObject:[NSString stringWithFormat:@"%ld of %ld shared slots busy", (long)self.sharedRunning,
                                                    (long)self.sharedSlots]];
    }
    // Light tasks take no slot, but they're running all the same.
    NSInteger light = self.lightRunning;
    if (light > 0) {
        NSString *tasks = light == 1 ? @"light task" : @"light tasks";
        [parts addObject:parts.count ? [NSString stringWithFormat:@"%ld %@", (long)light, tasks]
                                     : [NSString stringWithFormat:@"%ld %@ running", (long)light, tasks]];
    }
    if (self.waiting.count) [parts addObject:[NSString stringWithFormat:@"%lu waiting", (unsigned long)self.waiting.count]];
    return parts.count ? [parts componentsJoinedByString:@" · "] : @"Idle";
}

@end

#pragma mark - LockStore

@implementation LockStore {
    dispatch_source_t _watcher;
    NSTimer *_tick;
    NSData *_lastData;
}

+ (LockStore *)sharedStore {
    static LockStore *store;
    static dispatch_once_t once;
    dispatch_once(&once, ^{ store = [LockStore new]; });
    return store;
}

+ (NSString *)stateDirectory {
    NSString *dir = NSProcessInfo.processInfo.environment[@"LOCK_DIR"];
    return dir.length ? dir : [NSHomeDirectory() stringByAppendingPathComponent:@".local/state/lock"];
}

/// Same default as the Rust library: max(2, cpus / 3).
static NSInteger DefaultSharedSlots(void) {
    return MAX(2, (NSInteger)NSProcessInfo.processInfo.activeProcessorCount / 3);
}

- (instancetype)init {
    if ((self = [super init])) {
        _snapshot = [LockSnapshot new];
        _snapshot.sharedSlots = DefaultSharedSlots();
        [self reload];
    }
    return self;
}

- (void)startWatching {
    if (_watcher) return;
    NSString *dir = LockStore.stateDirectory;
    [NSFileManager.defaultManager createDirectoryAtPath:dir withIntermediateDirectories:YES attributes:nil error:nil];
    // state.json is replaced by rename, so watch the directory rather than the file.
    int fd = open(dir.fileSystemRepresentation, O_EVTONLY);
    if (fd >= 0) {
        _watcher = dispatch_source_create(DISPATCH_SOURCE_TYPE_VNODE, (uintptr_t)fd, DISPATCH_VNODE_WRITE,
                                          dispatch_get_main_queue());
        __weak LockStore *weakSelf = self;
        dispatch_source_set_event_handler(_watcher, ^{ [weakSelf reload]; });
        dispatch_source_set_cancel_handler(_watcher, ^{ close(fd); });
        dispatch_resume(_watcher);
    }
    _tick = [NSTimer scheduledTimerWithTimeInterval:1.0 target:self selector:@selector(reload) userInfo:nil repeats:YES];
    _tick.tolerance = 0.1;
    // Keep ticking while menus are open or the user is dragging.
    [NSRunLoop.mainRunLoop addTimer:_tick forMode:NSRunLoopCommonModes];
}

- (void)reload {
    NSString *path = [LockStore.stateDirectory stringByAppendingPathComponent:@"state.json"];
    NSError *error = nil;
    NSData *data = [NSData dataWithContentsOfFile:path options:0 error:&error];
    LockSnapshot *snapshot = [LockSnapshot new];
    snapshot.sharedSlots = DefaultSharedSlots();
    snapshot.jobserverTokens = (NSInteger)NSProcessInfo.processInfo.activeProcessorCount;
    snapshot.defaultTimeout = 5000;
    if (!data) {
        // No file yet just means nothing has ever been queued.
        if (!(error.domain == NSCocoaErrorDomain && error.code == NSFileReadNoSuchFileError))
            snapshot.error = error.localizedDescription;
        snapshot.running = snapshot.waiting = snapshot.history = @[];
    } else {
        [self parse:data into:snapshot];
    }
    _snapshot = snapshot;
    [NSNotificationCenter.defaultCenter postNotificationName:LockStoreDidChangeNotification object:self];
}

- (void)parse:(NSData *)data into:(LockSnapshot *)snapshot {
    NSError *error = nil;
    NSDictionary *state = [NSJSONSerialization JSONObjectWithData:data options:0 error:&error];
    NSMutableArray *running = [NSMutableArray array], *waiting = [NSMutableArray array],
                   *history = [NSMutableArray array];
    if (![state isKindOfClass:NSDictionary.class]) {
        snapshot.error = error.localizedDescription ?: @"state.json is not an object";
    } else {
        if (U64(state[@"shared_slots"]) > 0) snapshot.sharedSlots = (NSInteger)U64(state[@"shared_slots"]);
        // Absent in older state files, which means the defaults set in -reload.
        if ([state[@"jobserver_tokens"] isKindOfClass:NSNumber.class])
            snapshot.jobserverTokens = (NSInteger)U64(state[@"jobserver_tokens"]);
        if (U64(state[@"default_timeout_ms"]) > 0) snapshot.defaultTimeout = U64(state[@"default_timeout_ms"]);
        for (NSDictionary *json in state[@"tasks"]) {
            BOOL isRunning = [Str(json[@"state"]) isEqualToString:@"running"];
            LockTask *task = [LockTask taskWithJSON:json
                                              phase:isRunning ? LockTaskPhaseRunning : LockTaskPhaseWaiting];
            // Hide tasks whose `lock` process died; the next `lock` to run will clean them up.
            if (!task || !LockProcessAlive(task.ownerPID, task.ownerStarted)) continue;
            if (isRunning) {
                [running addObject:task];
            } else {
                task.queuePosition = (NSInteger)waiting.count + 1;
                [waiting addObject:task];
            }
        }
        for (NSDictionary *json in state[@"history"]) {
            if (![json isKindOfClass:NSDictionary.class]) continue;
            LockTask *task = [LockTask taskWithJSON:json[@"task"] phase:LockTaskPhaseFinished];
            if (!task) continue;
            task.endedAt = U64(json[@"ended_at_ms"]);
            NSDictionary *outcome = json[@"outcome"];
            if ([outcome isKindOfClass:NSDictionary.class]) {
                task.outcome = Str(outcome[@"type"]);
                id code = outcome[@"exit_code"];
                task.exitCode = [code isKindOfClass:NSNumber.class] ? code : nil;
            }
            [history addObject:task];
        }
    }
    snapshot.running = running;
    snapshot.waiting = waiting;
    snapshot.history = history;
}

#pragma mark Actions

/// The `lock` CLI: `$LOCK_BIN`, the copy bundled inside the app, or a usual install location.
static NSString *_Nullable LockExecutable(void) {
    NSMutableArray *candidates = [NSMutableArray array];
    NSString *env = NSProcessInfo.processInfo.environment[@"LOCK_BIN"];
    if (env.length) [candidates addObject:env];
    // In Contents/Helpers, not next to the app's own executable: on a case-insensitive
    // file system `lock` and `Lock` would be the same file.
    [candidates addObject:[NSBundle.mainBundle.bundlePath stringByAppendingPathComponent:@"Contents/Helpers/lock"]];
    [candidates addObjectsFromArray:@[
        [NSHomeDirectory() stringByAppendingPathComponent:@".cargo/bin/lock"],
        @"/opt/homebrew/bin/lock",
        @"/usr/local/bin/lock",
    ]];
    for (NSString *path in candidates)
        if ([NSFileManager.defaultManager isExecutableFileAtPath:path]) return path;
    return nil;
}

- (void)runLock:(NSArray<NSString *> *)arguments completion:(void (^)(NSString *_Nullable error))completion {
    NSString *exe = LockExecutable();
    if (!exe) {
        completion(@"Couldn't find the `lock` command. Install it with `./install.sh` in the lock repository, "
                   @"or set LOCK_BIN.");
        return;
    }
    NSTask *task = [NSTask new];
    task.executableURL = [NSURL fileURLWithPath:exe];
    task.arguments = arguments;
    NSPipe *stderrPipe = [NSPipe pipe];
    task.standardError = stderrPipe;
    task.standardOutput = NSFileHandle.fileHandleWithNullDevice;
    __weak LockStore *weakSelf = self;
    task.terminationHandler = ^(NSTask *finished) {
        NSData *output = [stderrPipe.fileHandleForReading readDataToEndOfFile];
        NSString *message = nil;
        if (finished.terminationStatus != 0) {
            message = [[NSString alloc] initWithData:output encoding:NSUTF8StringEncoding];
            message = [message stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet];
            if ([message hasPrefix:@"lock: "]) message = [message substringFromIndex:6];
            if (!message.length) message = [NSString stringWithFormat:@"`lock` exited with status %d", finished.terminationStatus];
        }
        dispatch_async(dispatch_get_main_queue(), ^{
            [weakSelf reload];
            completion(message);
        });
    };
    NSError *error = nil;
    if (![task launchAndReturnError:&error]) completion(error.localizedDescription);
}

- (void)cancelTask:(uint64_t)taskID completion:(void (^)(NSString *_Nullable))completion {
    [self runLock:@[ @"cancel", [NSString stringWithFormat:@"%llu", taskID] ] completion:completion];
}

- (void)setSharedSlots:(NSInteger)slots completion:(void (^)(NSString *_Nullable))completion {
    [self runLock:@[ @"slots", [NSString stringWithFormat:@"%ld", (long)slots] ] completion:completion];
}

- (void)setJobserverTokens:(NSInteger)tokens completion:(void (^)(NSString *_Nullable))completion {
    NSString *value = tokens > 0 ? [NSString stringWithFormat:@"%ld", (long)tokens] : @"off";
    [self runLock:@[ @"jobs", value ] completion:completion];
}

- (void)setDefaultTimeout:(NSString *)duration completion:(void (^)(NSString *_Nullable))completion {
    [self runLock:@[ @"default-timeout", duration ] completion:completion];
}

@end
