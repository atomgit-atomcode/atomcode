/*
 * Host-side self test for the text plugin's buffer growth.
 *
 * The plugin itself is a `pluginhost` plugin: it renders text to a PNG with
 * FreeType and is dlopened into the host process on the device. FreeType and
 * a font file are not available on a build machine, and they are not what
 * needs testing — the buffer is pure C, and it is where the memory-safety bug
 * lived.
 *
 * That bug: `text_push` wrote the result of `realloc` back into the struct
 * without checking it. A failed allocation left the field NULL and the very
 * next line indexed it — a NULL dereference inside the agent process, because
 * `pluginhost` loads this code into it.
 *
 * The allocation is injected through `TEXT_BUFFER_REALLOC`, so the failure
 * paths are reachable deterministically instead of by exhausting memory:
 *
 *   1. the first allocation of the first growth fails;
 *   2. the second allocation of the first growth fails (the first one already
 *      moved, so the struct must stay consistent with it);
 *   3. a failure during a later growth, after the buffer already holds text.
 *
 * `scripts/text-plugin/selftest.sh` compiles this against the same
 * `text_buffer.h` the plugin uses — the real code, not a copy of it.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- the injectable allocator ---- */

static int fail_at = -1; /* which allocation fails; -1 = none */
static int calls = 0;

static void *test_realloc(void *previous, size_t size) {
    calls++;
    if (fail_at >= 0 && calls == fail_at) {
        return NULL;
    }
    return realloc(previous, size);
}

/* Defined BEFORE the header, so the header's growth path uses this allocator
 * rather than the real one. The plugin's own build never defines it and keeps
 * the real `realloc`. */
#define TEXT_BUFFER_REALLOC(previous, size) test_realloc((previous), (size))

#include "text_buffer.h"

static int failures = 0;

static void check(int condition, const char *what) {
    if (condition) {
        printf("  ok   %s\n", what);
    } else {
        printf("  FAIL %s\n", what);
        failures++;
    }
}

int main(void) {
    printf("== the buffer grows and keeps what was pushed ==\n");
    {
        Text text = {0};
        for (unsigned int cp = 'a'; cp < 'a' + 300; cp++) {
            text_push(&text, cp, (int)(cp - 'a'));
        }
        check(text.count == 300, "300 code points were kept");
        check(text.capacity >= 300, "the capacity grew to hold them");
        check(text.codepoints[0] == 'a' && text.codepoints[299] == 'a' + 299,
              "the first and last code points survived the growth");
        check(text.advances[0] == 0 && text.advances[299] == 299,
              "the advances survived the growth");
        free(text.codepoints);
        free(text.advances);
    }

    printf("== a failed first allocation drops the code point ==\n");
    {
        fail_at = 1;
        calls = 0;
        Text text = {0};
        text_push(&text, 'x', 1);
        check(text.count == 0, "nothing was written into a buffer that never existed");
        check(text.codepoints == NULL, "the pointer stayed NULL instead of being written through");
        check(text.capacity == 0, "the capacity was not advanced past a failed growth");
        free(text.codepoints);
        free(text.advances);
    }

    printf("== a failed second allocation keeps the struct consistent ==\n");
    {
        fail_at = 2;
        calls = 0;
        Text text = {0};
        text_push(&text, 'x', 1);
        check(text.count == 0, "the code point was dropped rather than written");
        check(text.codepoints != NULL,
              "the first allocation was kept, so the struct matches the buffer");
        check(text.advances == NULL, "the field that failed stayed NULL");
        check(text.capacity == 0, "the capacity was not advanced past a failed growth");
        /* The next push must not reallocate over a stale pointer: the capacity
         * is still 0, so it grows again — and now it succeeds. */
        fail_at = -1;
        text_push(&text, 'y', 2);
        check(text.count == 1 && text.codepoints[0] == 'y',
              "a later push still works after a failed growth");
        free(text.codepoints);
        free(text.advances);
    }

    printf("== a failed growth does not lose the text already stored ==\n");
    {
        Text text = {0};
        for (unsigned int cp = 'a'; cp < 'a' + 256; cp++) {
            text_push(&text, cp, 1);
        }
        int before = text.count;
        calls = 0;
        fail_at = calls + 1; /* the next growth fails */
        text_push(&text, 'z', 9);
        fail_at = -1;
        check(text.count == before, "the failed growth did not advance the count");
        check(text.codepoints[0] == 'a' && text.codepoints[before - 1] == (unsigned int)('a' + before - 1),
              "the code points already stored are intact");
        free(text.codepoints);
        free(text.advances);
    }

    if (failures) {
        printf("%d check(s) failed\n", failures);
        return 1;
    }
    printf("ok\n");
    return 0;
}
