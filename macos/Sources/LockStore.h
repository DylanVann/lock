#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

typedef NS_ENUM(NSInteger, LockTaskPhase) {
    LockTaskPhaseRunning,
    LockTaskPhaseWaiting,
    LockTaskPhaseFinished,
};

/// One entry from ~/.local/state/lock/state.json: a running or waiting task, or a finished one from history.
@interface LockTask : NSObject
@property (nonatomic) uint64_t taskID;
@property (nonatomic) LockTaskPhase phase;
@property (nonatomic) BOOL exclusive;
/// A long-lived, mostly idle task: takes no shared slot, paused while an exclusive task runs.
@property (nonatomic) BOOL light;
/// The task whose lease this one runs under (a `lock` inside a locked command); 0 when none.
@property (nonatomic) uint64_t parentID;
/// Light tasks only: paused for an exclusive task.
@property (nonatomic) BOOL paused;
@property (nonatomic, copy, nullable) NSString *name;
@property (nonatomic, copy, nullable) NSString *agent;
@property (nonatomic, copy) NSString *cwd;
@property (nonatomic, copy, nullable) NSString *repo;
@property (nonatomic, copy, nullable) NSString *branch;
@property (nonatomic, copy) NSArray<NSString *> *command;
@property (nonatomic) pid_t ownerPID;
@property (nonatomic) uint64_t ownerStarted; // 0 when unknown
@property (nonatomic) pid_t childPID;        // 0 before the command starts
/// Milliseconds since the Unix epoch; 0 when not applicable.
@property (nonatomic) uint64_t enqueuedAt, startedAt, deadline, endedAt;
@property (nonatomic) uint64_t timeout;      // ms, 0 = none
/// Median of this task's recent successful runs, in ms; 0 when there's no history.
@property (nonatomic) uint64_t expected;
/// 1-based position among waiting tasks.
@property (nonatomic) NSInteger queuePosition;
/// Finished tasks only: "completed", "timed_out", "cancelled", "vanished", "abandoned".
@property (nonatomic, copy, nullable) NSString *outcome;
@property (nonatomic, strong, nullable) NSNumber *exitCode;

/// The name if given, otherwise the command line.
@property (nonatomic, readonly) NSString *title;
@property (nonatomic, readonly) NSString *commandLine;
/// "repo@branch", or the cwd's last component.
@property (nonatomic, readonly) NSString *location;
@property (nonatomic, readonly) NSString *projectPath;
@property (nonatomic, readonly) pid_t displayPID;
@property (nonatomic, readonly) NSString *outcomeLabel;
@property (nonatomic, readonly) BOOL succeeded;
@end

@interface LockSnapshot : NSObject
@property (nonatomic, copy) NSArray<LockTask *> *running;
@property (nonatomic, copy) NSArray<LockTask *> *waiting;
@property (nonatomic, copy) NSArray<LockTask *> *history;
@property (nonatomic) NSInteger sharedSlots;
/// Size of the jobserver shared by running shared tasks; 0 when it's off.
@property (nonatomic) NSInteger jobserverTokens;
/// Run timeout, in ms, for commands started without `-t`.
@property (nonatomic) uint64_t defaultTimeout;
@property (nonatomic, copy, nullable) NSString *error;
@property (nonatomic, readonly) LockTask *_Nullable exclusiveHolder;
@property (nonatomic, readonly) NSInteger sharedRunning;
/// Running light tasks (dev servers and the like), which take no slot.
@property (nonatomic, readonly) NSInteger lightRunning;
/// "Idle", "2 of 3 shared slots busy · 1 waiting", ...
@property (nonatomic, readonly) NSString *summary;
@end

extern NSNotificationName const LockStoreDidChangeNotification;

@interface LockStore : NSObject
@property (class, readonly) LockStore *sharedStore;
@property (nonatomic, readonly) LockSnapshot *snapshot;
/// `$LOCK_DIR` or ~/.local/state/lock.
@property (class, readonly) NSString *stateDirectory;

/// Start watching the state directory, plus a 1s tick so elapsed times advance.
- (void)startWatching;
- (void)reload;

/// Changes go through the `lock` CLI so there's a single writer implementation.
- (void)cancelTask:(uint64_t)taskID completion:(void (^)(NSString *_Nullable error))completion;
- (void)setSharedSlots:(NSInteger)slots completion:(void (^)(NSString *_Nullable error))completion;
/// 0 turns the jobserver off.
- (void)setJobserverTokens:(NSInteger)tokens completion:(void (^)(NSString *_Nullable error))completion;
/// Takes what `lock default-timeout` does: "5s", "90", "2m".
- (void)setDefaultTimeout:(NSString *)duration completion:(void (^)(NSString *_Nullable error))completion;
@end

/// Current time in ms since the Unix epoch.
uint64_t LockNowMs(void);
/// "42s", "3m05s", "1h02m".
NSString *LockFormatDuration(uint64_t ms);
/// "just now" under a minute, then the system's relative phrasing: "5 minutes ago", "2 hours ago".
NSString *LockFormatAgo(uint64_t thenMs, uint64_t nowMs);

NS_ASSUME_NONNULL_END
