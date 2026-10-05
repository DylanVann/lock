#import <Cocoa/Cocoa.h>

NS_ASSUME_NONNULL_BEGIN

/// The main window: running, queued and recent tasks in one grouped list.
@interface QueueWindowController : NSWindowController <NSWindowDelegate>
- (void)selectTaskWithID:(uint64_t)taskID;
@end

NS_ASSUME_NONNULL_END
