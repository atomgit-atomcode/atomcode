/*
 * `__clear_cache` for aarch64 Android.
 *
 * libffi's arm64 closure code calls `__clear_cache` after writing executable
 * trampolines, but bionic does not export that symbol — linking anything against
 * a static libffi.a fails with `undefined symbol: __clear_cache`.
 *
 * `__builtin___clear_cache` is not a fix: on aarch64 the compiler lowers it back
 * to a call to `__clear_cache`, which is how this ends up unresolved in the
 * first place. The cache maintenance has to be expressed directly.
 *
 * Compiled with the NDK clang and appended to libffi.a, so any link that pulls
 * ffi's objects also resolves this:
 *
 *   cc -c -fPIC -O2 clear_cache.c -o clear_cache.o
 *   llvm-ar rcs libffi.a clear_cache.o
 */
void __clear_cache(void *begin, void *end) {
    char *cursor = (char *)begin;
    char *last = (char *)end;

    /* Clean data cache to the point of unification, so the instruction fetch
     * sees what we just wrote. 64 bytes covers the largest cache line on the
     * cores this runs on; overlapping by line is harmless. */
    for (; cursor < last; cursor += 64) {
        __asm__ __volatile__("dc cvau, %0" ::"r"(cursor) : "memory");
    }
    __asm__ __volatile__("dsb ish" ::: "memory");

    /* Invalidate the instruction cache for the same range. */
    for (cursor = (char *)begin; cursor < last; cursor += 64) {
        __asm__ __volatile__("ic ivau, %0" ::"r"(cursor) : "memory");
    }
    __asm__ __volatile__("dsb ish" ::: "memory");
    __asm__ __volatile__("isb" ::: "memory");
}
