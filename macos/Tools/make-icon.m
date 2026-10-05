// Renders the app icon into an .iconset directory: an indigo squircle with the SF Symbol "cpu".
// Usage: make-icon <output.iconset>
#import <Cocoa/Cocoa.h>

static NSBitmapImageRep *Render(CGFloat pixels) {
    NSBitmapImageRep *rep = [[NSBitmapImageRep alloc] initWithBitmapDataPlanes:NULL
                                                                    pixelsWide:(NSInteger)pixels
                                                                    pixelsHigh:(NSInteger)pixels
                                                                 bitsPerSample:8
                                                               samplesPerPixel:4
                                                                      hasAlpha:YES
                                                                      isPlanar:NO
                                                                colorSpaceName:NSCalibratedRGBColorSpace
                                                                   bytesPerRow:0
                                                                  bitsPerPixel:0];
    [NSGraphicsContext saveGraphicsState];
    NSGraphicsContext.currentContext = [NSGraphicsContext graphicsContextWithBitmapImageRep:rep];
    CGFloat scale = pixels / 1024.0;
    NSAffineTransform *transform = [NSAffineTransform transform];
    [transform scaleBy:scale];
    [transform concat];

    // Apple's macOS icon grid: an 824pt rounded square centred in 1024, with a soft shadow.
    NSRect body = NSMakeRect(100, 100, 824, 824);
    NSBezierPath *shape = [NSBezierPath bezierPathWithRoundedRect:body xRadius:185 yRadius:185];
    NSShadow *shadow = [NSShadow new];
    shadow.shadowColor = [NSColor colorWithWhite:0 alpha:0.3];
    shadow.shadowOffset = NSMakeSize(0, -10);
    shadow.shadowBlurRadius = 20;
    [NSGraphicsContext saveGraphicsState];
    [shadow set];
    [NSColor.blackColor setFill];
    [shape fill];
    [NSGraphicsContext restoreGraphicsState];

    NSGradient *gradient = [[NSGradient alloc]
        initWithStartingColor:[NSColor colorWithSRGBRed:0.36 green:0.45 blue:0.98 alpha:1]
                  endingColor:[NSColor colorWithSRGBRed:0.42 green:0.20 blue:0.78 alpha:1]];
    [gradient drawInBezierPath:shape angle:-90];

    NSImageSymbolConfiguration *config =
        [[NSImageSymbolConfiguration configurationWithPointSize:430 weight:NSFontWeightMedium]
            configurationByApplyingConfiguration:[NSImageSymbolConfiguration
                                                     configurationWithHierarchicalColor:NSColor.whiteColor]];
    NSImage *symbol = [[NSImage imageWithSystemSymbolName:@"cpu" accessibilityDescription:nil]
        imageWithSymbolConfiguration:config];
    NSSize size = symbol.size;
    [symbol drawInRect:NSMakeRect(512 - size.width / 2, 512 - size.height / 2, size.width, size.height)];

    [NSGraphicsContext restoreGraphicsState];
    return rep;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 2) {
            fprintf(stderr, "usage: make-icon <output.iconset>\n");
            return 1;
        }
        NSString *dir = @(argv[1]);
        [NSFileManager.defaultManager createDirectoryAtPath:dir withIntermediateDirectories:YES attributes:nil error:nil];
        for (NSNumber *points in @[ @16, @32, @128, @256, @512 ]) {
            for (int scale = 1; scale <= 2; scale++) {
                NSString *name = scale == 1 ? [NSString stringWithFormat:@"icon_%@x%@.png", points, points]
                                            : [NSString stringWithFormat:@"icon_%@x%@@2x.png", points, points];
                NSData *png = [Render(points.doubleValue * scale) representationUsingType:NSBitmapImageFileTypePNG
                                                                              properties:@{}];
                [png writeToFile:[dir stringByAppendingPathComponent:name] atomically:YES];
            }
        }
    }
    return 0;
}
