/*
 * `clang` as the agent sees it: a launcher that points the real driver at the data
 * package, then execs it.
 *
 * Why this exists: clang derives its resource directory (builtin headers, per
 * target builtins) and its default sysroot from its own location. On Android the
 * executable must live in the APK's `nativeLibraryDir`, while the machine-readable
 * half of the toolchain is a market data package under the app's files directory.
 * The two are never siblings, so the driver has to be told.
 *
 * Why a binary and not a shell script: a script in the data directory cannot be
 * executed either (the same 126 that forced the APK carrier in the first place).
 *
 * argv[0] is passed through, because that is how clang decides whether it is the C
 * or the C++ driver — `clang++` is a link name, not a different program.
 *
 * Build: see scripts/build-android-clang-provider.sh (NDK clang, -DCLANG_RESOURCE).
 */
#include <libgen.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifndef CLANG_RESOURCE
#define CLANG_RESOURCE "lib/clang/21"
#endif

#define MAX_PATH 4096

int main(int argc, char **argv) {
    char exe[MAX_PATH];
    ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (n <= 0) {
        perror("clang: cannot resolve my own path");
        return 127;
    }
    exe[n] = '\0';

    /* dirname() may modify its argument; exe is ours to clobber after the copy. */
    char driver[MAX_PATH];
    snprintf(driver, sizeof driver, "%s/libclang-real.so", dirname(exe));
    if (access(driver, X_OK) != 0) {
        fprintf(stderr, "clang: the driver is missing next to this launcher: %s\n", driver);
        return 127;
    }

    char resource[MAX_PATH];
    char sysroot[MAX_PATH];
    char libdir[MAX_PATH];
    const char *injected[3];
    int injected_n = 0;

    /* `-L` belongs on a LINK only: passed to a compile, clang warns
     * `argument unused during compilation`, and a `-Werror` caller (Go's cgo is
     * one) turns that warning into a failure — measured. So it is added when the
     * command line is not compile-only. */
    int compile_only = 0;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-c") == 0 || strcmp(argv[i], "-S") == 0 ||
            strcmp(argv[i], "-E") == 0 || strncmp(argv[i], "-M", 2) == 0) {
            compile_only = 1;
            break;
        }
    }

    const char *root = getenv("ATOMCODE_CLANG_DATA");
    if (root != NULL && root[0] != '\0') {
        snprintf(resource, sizeof resource, "-resource-dir=%s/%s", root, CLANG_RESOURCE);
        snprintf(sysroot, sizeof sysroot, "--sysroot=%s", root);
        /* `libunwind.a` lives in the package's `lib/`, which is where Termux's
         * clang expects it in its own $PREFIX. Without it a C++/cgo LINK fails
         * with "cannot find libunwind.a" even though compilation works. */
        snprintf(libdir, sizeof libdir, "-L%s/lib", root);
        injected[injected_n++] = resource;
        injected[injected_n++] = sysroot;
        if (!compile_only) {
            injected[injected_n++] = libdir;
        }
    } else {
        /* Without the data package there are no headers; say so instead of failing
         * later with "stddef.h file not found", which reads like a broken compiler. */
        fprintf(stderr,
                "clang: ATOMCODE_CLANG_DATA is not set — install the clang-sysroot package "
                "so the headers and resource directory are available\n");
    }

    char **args = calloc((size_t)argc + (size_t)injected_n + 1, sizeof *args);
    if (args == NULL) {
        perror("clang: out of memory");
        return 127;
    }
    int at = 0;
    /* argv[0] keeps the *link* name: `clang++` must stay `clang++`. */
    args[at++] = argv[0];
    for (int i = 0; i < injected_n; i++) {
        args[at++] = (char *)injected[i];
    }
    for (int i = 1; i < argc; i++) {
        args[at++] = argv[i];
    }
    args[at] = NULL;

    execv(driver, args);
    perror("clang: cannot start the driver");
    return 127;
}