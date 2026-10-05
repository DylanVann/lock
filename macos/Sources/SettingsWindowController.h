#import <Cocoa/Cocoa.h>

NS_ASSUME_NONNULL_BEGIN

/// Settings for the machine-wide queue: shared slots, the jobserver, and the default timeout.
/// Changes go through the `lock` CLI, like everything else the app changes.
@interface SettingsWindowController : NSWindowController <NSWindowDelegate>
@end

NS_ASSUME_NONNULL_END
