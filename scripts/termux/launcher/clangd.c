/*
 * `clangd` as the agent sees it: a launcher that points the language server at the
 * data package, then execs it.
 *
 * Same problem as `clang` (see clang.c): an executable has to live in the APK's
 * `nativeLibraryDir`, while the machine-readable half of the toolchain — headers
 * and clang's resource directory — is a market data package under the app's files
 * directory. clangd derives both from its own location, so they have to be told.
 *
 * clangd has no `-resource-dir` flag of its own; it forwards `--extra-arg=…` to
 * the compiler invocation it builds for each translation unit, which is where the
 * flags belong. There is deliberately no `-L`: a language server never links.
 *
 * Why a binary and not a shell script: a script cannot be executed from either the
 * data directory or the APK (the same 126 that forced the APK carrier).
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
        perror("clangd: cannot resolve my own path");
        return 127;
    }
    exe[n] = '\0';

    /* dirname() may modify its argument; exe is ours to clobber after the copy. */
    char server[MAX_PATH];
    snprintf(server, sizeof server, "%s/libclangd-real.so", dirname(exe));
    if (access(server, X_OK) != 0) {
        fprintf(stderr, "clangd: the server is missing next to this launcher: %s\n", server);
        return 127;
    }

    char resource[MAX_PATH];
    char sysroot[MAX_PATH];
    const char *injected[2];
    int injected_n = 0;

    const char *root = getenv("ATOMCODE_CLANG_DATA");
    if (root != NULL && root[0] != '\0') {
        snprintf(resource, sizeof resource,
                 "--extra-arg=-resource-dir=%s/%s", root, CLANG_RESOURCE);
        snprintf(sysroot, sizeof sysroot, "--extra-arg=--sysroot=%s", root);
        injected[injected_n++] = resource;
        injected[injected_n++] = sysroot;
    } else {
        /* Without the data package every file is parsed with no standard headers;
         * say why, instead of reporting a thousand "stddef.h file not found". */
        fprintf(stderr,
                "clangd: ATOMCODE_CLANG_DATA is not set — install the clang-sysroot package "
                "so the headers and resource directory are available\n");
    }

    char **args = calloc((size_t)argc + (size_t)injected_n + 1, sizeof *args);
    if (args == NULL) {
        perror("clangd: out of memory");
        return 127;
    }
    int at = 0;
    /* argv[0] keeps the link name, the same way clang++ stays clang++. */
    args[at++] = argv[0];
    for (int i = 0; i < injected_n; i++) {
        args[at++] = (char *)injected[i];
    }
    for (int i = 1; i < argc; i++) {
        args[at++] = argv[i];
    }
    args[at] = NULL;

    execv(server, args);
    perror("clangd: cannot start the language server");
    return 127;
}