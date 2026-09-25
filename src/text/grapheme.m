// NSString's documented backing-store primitives let Foundation read only
// the rope context needed for a composed sequence. No borrowed pointer escapes.
#import <Foundation/Foundation.h>

typedef void (*ReadUnits)(const void *, NSUInteger, NSUInteger, unichar *);
@interface CrcRopeString : NSString {
    const void *_context;
    NSUInteger _count;
    ReadUnits _read;
}
- (instancetype)initWithContext:(const void *)context count:(NSUInteger)count read:(ReadUnits)read;
@end

@implementation CrcRopeString
- (instancetype)initWithContext:(const void *)context count:(NSUInteger)count read:(ReadUnits)read {
    self = [super init];
    if (self) { _context = context; _count = count; _read = read; }
    return self;
}
- (NSUInteger)length { return _count; }
- (unichar)characterAtIndex:(NSUInteger)index {
    unichar unit;
    [self getCharacters:&unit range:NSMakeRange(index, 1)];
    return unit;
}
- (void)getCharacters:(unichar *)buffer range:(NSRange)range {
    if (range.location > _count || range.length > _count - range.location) {
        [NSException raise:NSRangeException format:@"Rope string range outside source"];
    }
    _read(_context, range.location, range.length, buffer);
}
@end

void crc_rope_grapheme(const void *context, NSUInteger count, NSUInteger index,
                        ReadUnits read, NSUInteger *start, NSUInteger *end) {
    @autoreleasepool {
        CrcRopeString *string = [[CrcRopeString alloc] initWithContext:context count:count read:read];
        NSRange range = [string rangeOfComposedCharacterSequenceAtIndex:index];
        *start = range.location;
        *end = NSMaxRange(range);
        [string release];
    }
}
