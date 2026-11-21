/*
 * The text buffer: the code points of one string and their horizontal advances.
 *
 * Extracted from `plugin.c` so the growth logic can be exercised on a build
 * machine. The rendering path needs FreeType and a font file, which a build
 * machine has no use for — but the buffer is pure C, and it is where the
 * memory-safety bug lived: `text_push` wrote an unchecked `realloc` result
 * back into the struct and then indexed it, so an allocation failure became
 * a NULL dereference inside the host process (this plugin is dlopened into
 * it by `pluginhost`).
 *
 * The allocator is overridable (`TEXT_BUFFER_REALLOC`) so a test can make one
 * allocation fail and prove the growth path survives it. The default is the
 * real `realloc`; the production build never overrides it.
 */
#ifndef ATOMCODE_TEXT_PLUGIN_TEXT_BUFFER_H
#define ATOMCODE_TEXT_PLUGIN_TEXT_BUFFER_H

#include <stddef.h>
#include <stdlib.h>

#ifndef TEXT_BUFFER_REALLOC
#define TEXT_BUFFER_REALLOC(previous, size) realloc((previous), (size))
#endif

typedef struct {
    unsigned int *codepoints;
    int *advances; /* 每个码点的水平步进（像素） */
    int count;
    int capacity;
} Text;

static inline void text_push(Text *text, unsigned int cp, int advance) {
    if (text->count == text->capacity) {
        int grown = text->capacity ? text->capacity * 2 : 256;
        /* Both reallocs are checked: writing an unchecked result back into the
         * struct loses the old pointer (a leak) and leaves the field NULL, and
         * the very next line indexes it — a NULL dereference inside the host
         * process, because this plugin is dlopened into it. A failed growth
         * drops the code point instead of taking the process down. */
        unsigned int *codepoints = TEXT_BUFFER_REALLOC(
            text->codepoints, (size_t)grown * sizeof(*text->codepoints));
        if (!codepoints) return;
        int *advances =
            TEXT_BUFFER_REALLOC(text->advances, (size_t)grown * sizeof(*text->advances));
        if (!advances) {
            /* The first one already moved: keep the struct consistent with it
             * so the next push does not reallocate over a stale pointer. */
            text->codepoints = codepoints;
            return;
        }
        text->codepoints = codepoints;
        text->advances = advances;
        text->capacity = grown;
    }
    text->codepoints[text->count] = cp;
    text->advances[text->count] = advance;
    text->count++;
}

#endif
