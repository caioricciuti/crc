// CoreText's synchronous block API, kept in C so Clang owns the block ABI.
// No block or captured pointer escapes this call.
#include <CoreText/CoreText.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// The convenience CTLine constructor can silently simplify sufficiently
// complex paragraphs. The caller bounds source and expanded UTF-16 storage;
// request full native Unicode layout explicitly on the shaping worker.
CTTypesetterRef crc_paragraph_typesetter(CFAttributedStringRef attributed) {
    const void *key = kCTTypesetterOptionAllowUnboundedLayout;
    const void *value = kCFBooleanTrue;
    CFDictionaryRef options = CFDictionaryCreate(NULL, &key, &value, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (!options) return NULL;
    CTTypesetterRef typesetter = CTTypesetterCreateWithAttributedStringAndOptions(attributed, options);
    CFRelease(options);
    return typesetter;
}

static bool line_caret_edges(CTLineRef line, float scale, float *primary,
                             float *secondary, size_t count,
                             bool (*cancelled)(const void *), const void *context) {
    for (size_t i = 0; i < count; ++i) {
        primary[i] = INFINITY;
        secondary[i] = -INFINITY;
    }
    __block size_t visited = 0;
    __block bool aborted = false;
    CTLineEnumerateCaretOffsets(line, ^(double offset, CFIndex index,
                                        bool leading, bool *stop) {
        if ((visited++ & 127) == 0 && cancelled && cancelled(context)) {
            aborted = true;
            *stop = true;
            return;
        }
        // A trailing edge belongs to the boundary after its UTF-16 unit.
        CFIndex boundary = index + (leading ? 0 : 1);
        if (boundary < 0 || (size_t)boundary >= count) return;
        // Match the Rust native-query path, including non-integral scales.
        float x = (float)offset / scale;
        primary[boundary] = fminf(primary[boundary], x);
        secondary[boundary] = fmaxf(secondary[boundary], x);
    });
    return !aborted && !(cancelled && cancelled(context));
}

static void fill_missing_edges(float *primary, float *secondary, size_t count) {
    // CoreText omits the interior of composed sequences. These are not
    // mouse stops, but source/search mappings still index every UTF-16 unit.
    float next = 0;
    for (size_t i = count; i-- > 0;) {
        if (!isfinite(primary[i])) primary[i] = secondary[i] = next;
        next = primary[i];
    }
}

bool crc_line_caret_offsets(CTLineRef line, float scale, float *primary,
                             float *secondary, size_t count,
                             bool (*cancelled)(const void *), const void *context) {
    if (!line_caret_edges(line, scale, primary, secondary, count, cancelled, context)) return false;
    fill_missing_edges(primary, secondary, count);
    return !(cancelled && cancelled(context));
}

// Native windows come from the SAME full-paragraph typesetter. Their glyphs
// must match the original line (font, direction, advances and positions)
// throughout the body and neighboring source clusters before translating
// their caret answers. Expand context up to the original line on mismatch.
// The original whole-line glyphs always remain the display geometry.

typedef struct {
    CFIndex source, next;
    CGGlyph glyph;
    CGPoint position;
    CGSize advance;
    CTFontRef font;
    CTRunStatus status;
    unsigned matched;
} IndexedGlyph;

static bool is_cancelled(bool (*cancelled)(const void *), const void *context) {
    return cancelled && cancelled(context);
}

typedef struct {
    const CGGlyph *glyphs;
    const CGPoint *positions;
    const CGSize *advances;
    const CFIndex *indices;
    void *owned[4];
} RunView;

static void release_view(RunView *view) {
    for (int i = 0; i < 4; ++i) free(view->owned[i]);
}

static bool run_view(CTRunRef run, RunView *view) {
    memset(view, 0, sizeof(*view));
    CFIndex n = CTRunGetGlyphCount(run);
    view->glyphs = CTRunGetGlyphsPtr(run);
    view->positions = CTRunGetPositionsPtr(run);
    view->advances = CTRunGetAdvancesPtr(run);
    view->indices = CTRunGetStringIndicesPtr(run);
#define COPY_IF_NEEDED(field, slot, Type, getter) \
    if (!view->field && n) { \
        view->owned[slot] = malloc((size_t)n * sizeof(Type)); \
        if (!view->owned[slot]) { release_view(view); return false; } \
        getter(run, CFRangeMake(0, n), view->owned[slot]); \
        view->field = view->owned[slot]; \
    }
    COPY_IF_NEEDED(glyphs, 0, CGGlyph, CTRunGetGlyphs)
    COPY_IF_NEEDED(positions, 1, CGPoint, CTRunGetPositions)
    COPY_IF_NEEDED(advances, 2, CGSize, CTRunGetAdvances)
    COPY_IF_NEEDED(indices, 3, CFIndex, CTRunGetStringIndices)
#undef COPY_IF_NEEDED
    return true;
}

static bool window_translation(CTLineRef window, CFRange guard,
                               const CFIndex *heads, IndexedGlyph *glyphs,
                               size_t count, unsigned stamp, double *translation) {
    CFArrayRef runs = CTLineGetGlyphRuns(window);
    bool found = false;
    for (CFIndex r = 0; r < CFArrayGetCount(runs); ++r) {
        CTRunRef run = CFArrayGetValueAtIndex(runs, r);
        CFIndex n = CTRunGetGlyphCount(run);
        CTFontRef font = CFDictionaryGetValue(CTRunGetAttributes(run), kCTFontAttributeName);
        RunView view;
        if (!run_view(run, &view)) return false;
        bool valid = true;
        for (CFIndex j = 0; j < n; ++j) {
            CFIndex index = view.indices[j];
            if (index < guard.location || index >= guard.location + guard.length) continue;
            if (index < 0 || (size_t)index >= count) { valid = false; break; }
            bool matched = false;
            for (CFIndex k = heads[index]; k >= 0; k = glyphs[k].next) {
                IndexedGlyph *original = &glyphs[k];
                double delta = original->position.x - view.positions[j].x;
                if (original->matched != stamp && original->glyph == view.glyphs[j]
                    && CFEqual(original->font, font)
                    && original->status == CTRunGetStatus(run)
                    && fabs(original->advance.width - view.advances[j].width) < 1e-7
                    && fabs(original->advance.height - view.advances[j].height) < 1e-7
                    && fabs(original->position.y - view.positions[j].y) < 1e-7
                    && (!found || fabs(delta - *translation) < 1e-7)) {
                    *translation = delta;
                    original->matched = stamp;
                    matched = found = true;
                    break;
                }
            }
            if (!matched) {
                valid = false; break;
            }
        }
        release_view(&view);
        if (!valid) return false;
    }
    // Check both directions: a window may not omit an original glyph or
    // reuse one original glyph to validate several local glyphs.
    for (CFIndex index = guard.location; index < guard.location + guard.length; ++index)
        for (CFIndex k = heads[index]; k >= 0; k = glyphs[k].next)
            if (glyphs[k].matched != stamp) return false;
    return found;
}

bool crc_line_indexed_carets(CTLineRef line, CFAttributedStringRef attributed,
                             CTTypesetterRef typesetter,
                             float scale, float *left, float *right, float *primary,
                             size_t count, bool (*cancelled)(const void *),
                             const void *context) {
    CFStringRef text = CFAttributedStringGetString(attributed);
    CFIndex length = CFStringGetLength(text);
    if (count != (size_t)length + 1) return false;
    bool has_controls = false;
    for (CFIndex i = 0; i < length; ++i) {
        if ((i & 255) == 0 && is_cancelled(cancelled, context)) return false;
        UniChar unit = CFStringGetCharacterAtIndex(text, i);
        has_controls |= unit == 0x061c || unit == 0x200e || unit == 0x200f
            || (unit >= 0x202a && unit <= 0x202e)
            || (unit >= 0x2066 && unit <= 0x2069);
    }
    // Explicit embedding/isolate controls can change native caret affinity
    // without changing glyph geometry. Keep their full native table on the
    // worker; a glyph-equivalence check alone cannot validate a smaller line.
    if (has_controls) {
        double began = CFAbsoluteTimeGetCurrent();
        if (!line_caret_edges(line, scale, left, right, count, cancelled, context)) return false;
        // Remember only genuinely enumerated unique edges, before filling
        // omitted cluster interiors for source/selection indexing.
        for (size_t i = 0; i < count; ++i) {
            if ((i & 255) == 0 && is_cancelled(cancelled, context)) return false;
            primary[i] = left[i] == right[i] && isfinite(left[i]) ? left[i] : NAN;
        }
        fill_missing_edges(left, right, count);
        double enumerated = CFAbsoluteTimeGetCurrent();
        size_t reused = 0;
        for (CFIndex i = 0; i <= length; ++i) {
            if ((i & 127) == 0 && is_cancelled(cancelled, context)) return false;
            UniChar unit = i < length ? CFStringGetCharacterAtIndex(text, i) : 0;
            UniChar before = i > 0 ? CFStringGetCharacterAtIndex(text, i - 1) : 0;
            // Keep reuse inside adjacent printable ASCII. Even a unique
            // enumerated edge at NUL or other Unicode/control boundaries can
            // differ from native primary affinity in controlled paragraphs.
            // Omitted ligature interiors, ambiguous edges and endpoints query.
            if (before >= 0x20 && before <= 0x7e && unit >= 0x20 && unit <= 0x7e && isfinite(primary[i])) {
                ++reused;
                continue;
            }
            CGFloat other = 0;
            CGFloat value = CTLineGetOffsetForStringIndex(line, i, &other);
            primary[i] = (float)value / scale;
            if ((unit >= 0x202a && unit <= 0x202e) || (unit >= 0x2066 && unit <= 0x2069)) {
                left[i] = (float)fmin(value, other) / scale;
                right[i] = (float)fmax(value, other) / scale;
            }
        }
        if (getenv("CRC_SHAPE_PROFILE")) fprintf(stderr, "control carets: reused %zu enumerate %.3f ms primary %.3f ms\n", reused, (enumerated-began)*1000, (CFAbsoluteTimeGetCurrent()-enumerated)*1000);
        return !is_cancelled(cancelled, context);
    }
    CFArrayRef runs = CTLineGetGlyphRuns(line);
    size_t glyph_count = (size_t)CTLineGetGlyphCount(line), used = 0;
    IndexedGlyph *glyphs = calloc(glyph_count ? glyph_count : 1, sizeof(*glyphs));
    CFIndex *heads = malloc(count * sizeof(*heads));
    if (!glyphs || !heads) {
        free(glyphs); free(heads);
        return false;
    }
    for (size_t i = 0; i < count; ++i) {
        heads[i] = -1;
        left[i] = INFINITY; right[i] = -INFINITY; primary[i] = NAN;
    }
    bool complete = false;
    unsigned stamp = 0;
    size_t fast = 0, reused = 0, windows = 0, full = 0, zeros = 0, largest = 0, attempts = 0;
    double began = CFAbsoluteTimeGetCurrent(), copied = began;
    double creation = 0, queries = 0, enumeration = 0;
    for (CFIndex r = 0; r < CFArrayGetCount(runs); ++r) {
        if (is_cancelled(cancelled, context)) goto done;
        CTRunRef run = CFArrayGetValueAtIndex(runs, r);
        CFIndex n = CTRunGetGlyphCount(run);
        CTRunStatus status = CTRunGetStatus(run);
        CTFontRef font = CFDictionaryGetValue(CTRunGetAttributes(run), kCTFontAttributeName);
        bool simple = status == 0 && (CTFontGetSymbolicTraits(font) & kCTFontMonoSpaceTrait);
        RunView view;
        if (!run_view(run, &view)) goto done;
        bool valid = true;
        for (CFIndex j = 0; j < n; ++j) {
            if ((j & 255) == 0 && is_cancelled(cancelled, context)) { valid = false; break; }
            IndexedGlyph *g = &glyphs[used];
            g->source = view.indices[j]; g->glyph = view.glyphs[j];
            g->position = view.positions[j]; g->advance = view.advances[j];
            g->font = font; g->status = status;
            if (g->source < 0 || (size_t)g->source >= count) { valid = false; break; }
            g->next = heads[g->source]; heads[g->source] = (CFIndex)used++;
        }
        release_view(&view);
        if (!valid) goto done;
        // Adjacent one-glyph-per-unit printable ASCII boundaries within a run
        // have neither cluster interpolation nor a bidi/run affinity choice.
        // Read their already-shaped monospace positions directly.
        if (simple) for (size_t j = used - (size_t)n + 1; j < used; ++j) {
            CFIndex index = glyphs[j].source;
            if (glyphs[j - 1].source + 1 != index) continue;
            UniChar before = CFStringGetCharacterAtIndex(text, index - 1);
            UniChar after = CFStringGetCharacterAtIndex(text, index);
            if (before >= 0x20 && before <= 0x7e && after >= 0x20 && after <= 0x7e) {
                float x = (float)glyphs[j].position.x / scale;
                left[index] = right[index] = primary[index] = x;
                ++fast;
            }
        }
    }
    copied = CFAbsoluteTimeGetCurrent();
    for (CFIndex from = 0; from <= length; from += 256) {
        if (is_cancelled(cancelled, context)) goto done;
        CFIndex to = from + 256 < length + 1 ? from + 256 : length + 1;
        bool prepared = true;
        for (CFIndex i = from; i < to; ++i) prepared = prepared && isfinite(primary[i]);
        if (prepared) continue;
        ++windows;
        CFIndex guard_lo = from > 0 ? from - 1 : 0;
        CFIndex guard_hi = to < length ? to : length;
        if (guard_lo < length) guard_lo = CFStringGetRangeOfComposedCharactersAtIndex(text, guard_lo).location;
        if (guard_hi < length) {
            CFRange g = CFStringGetRangeOfComposedCharactersAtIndex(text, guard_hi);
            guard_hi = g.location + g.length;
        }
        // Include a ligature whose source index precedes the guarded cluster.
        while (guard_lo > 0 && heads[guard_lo] < 0) --guard_lo;
        double stage = CFAbsoluteTimeGetCurrent();
        CTLineRef window = NULL;
        double translation = 0;
        for (CFIndex padding = 64;; padding *= 2) {
            ++attempts;
            CFIndex lo = guard_lo > padding ? guard_lo - padding : 0;
            CFIndex hi = guard_hi + padding < length ? guard_hi + padding : length;
            if (lo < length) lo = CFStringGetRangeOfComposedCharactersAtIndex(text, lo).location;
            if (hi < length) {
                CFRange g = CFStringGetRangeOfComposedCharactersAtIndex(text, hi);
                hi = g.location + g.length;
            }
            if (lo == 0 && hi == length) {
                window = CFRetain(line); translation = 0; ++full; break;
            }
            window = CTTypesetterCreateLine(typesetter, CFRangeMake(lo, hi - lo));
            if (!window) goto done;
            if (window_translation(window, CFRangeMake(guard_lo, guard_hi - guard_lo),
                                   heads, glyphs, count, ++stamp, &translation)) break;
            CFRelease(window);
            if (is_cancelled(cancelled, context)) goto done;
        }
        CFRange accepted = CTLineGetStringRange(window);
        if ((size_t)accepted.length > largest) largest = (size_t)accepted.length;
        creation += CFAbsoluteTimeGetCurrent() - stage;
        stage = CFAbsoluteTimeGetCurrent();
        __block bool aborted = false;
        __block size_t visited = 0;
        CTLineEnumerateCaretOffsets(window, ^(double offset, CFIndex index, bool leading, bool *stop) {
            if ((visited++ & 127) == 0 && is_cancelled(cancelled, context)) { aborted = true; *stop = true; return; }
            CFIndex boundary = index + (leading ? 0 : 1);
            if (boundary < from || boundary >= to || isfinite(primary[boundary])) return;
            float x = (float)(offset + translation) / scale;
            left[boundary] = fminf(left[boundary], x);
            right[boundary] = fmaxf(right[boundary], x);
        });
        enumeration += CFAbsoluteTimeGetCurrent() - stage;
        stage = CFAbsoluteTimeGetCurrent();
        if (!aborted) for (CFIndex i = from; i < to; ++i) {
            if ((i & 127) == 0 && is_cancelled(cancelled, context)) { aborted = true; break; }
            if (isfinite(primary[i])) continue;
            // An enumerated interior boundary with one visual edge has no
            // primary/secondary affinity choice. Reuse that native answer.
            // Cluster interiors omitted by enumeration and bidi boundaries
            // still need a query. Paragraph endpoints also need it: native
            // primary can be zero there even when enumeration gives an edge.
            if (i > 0 && i < length && isfinite(left[i]) && left[i] == right[i]) {
                primary[i] = left[i];
                ++reused;
                continue;
            }
            double value = CTLineGetOffsetForStringIndex(window, i, NULL);
            // Native failure/interior answers of zero are not coordinates.
            // Resolve these rare ambiguous zeros against the original line.
            if (value == 0 && translation != 0) {
                CFRange cluster = CFStringGetRangeOfComposedCharactersAtIndex(text, i < length ? i : length - 1);
                CGFloat begin = CTLineGetOffsetForStringIndex(window, cluster.location, NULL);
                CGFloat end = CTLineGetOffsetForStringIndex(window, cluster.location + cluster.length, NULL);
                // Zero outside the cluster's visual extent is CoreText's
                // failure answer for an interior index, not a translatable x.
                if (begin == 0 || end == 0) {
                    ++zeros; value = CTLineGetOffsetForStringIndex(line, i, NULL);
                }
            } else value += translation;
            primary[i] = (float)value / scale;

        }
        queries += CFAbsoluteTimeGetCurrent() - stage;
        CFRelease(window);
        if (aborted) goto done;
    }
    float next = 0;
    for (size_t i = count; i-- > 0;) {
        if (!isfinite(left[i])) left[i] = right[i] = next;
        next = left[i];
    }
    complete = !is_cancelled(cancelled, context);
    if (getenv("CRC_SHAPE_PROFILE")) fprintf(stderr, "caret index: fast %zu reused %zu windows %zu whole %zu zeros %zu largest %zu attempts %zu copy %.3f ms query %.3f ms (create %.3f enumerate %.3f primary %.3f)\n",fast,reused,windows,full,zeros,largest,attempts,(copied-began)*1000,(CFAbsoluteTimeGetCurrent()-copied)*1000,creation*1000,enumeration*1000,queries*1000);
done:
    free(heads); free(glyphs);
    return complete;
}
