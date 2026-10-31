/*
 * The `plugin_main` entry point of the CPython runtime package.
 *
 * `pluginhost` (see crates/atomcode-android/src/bin/pluginhost.rs) dlopens this
 * library and calls `plugin_main(argc, argv)`. We hand those bytes straight to
 * CPython, so the caller sees normal `python` behaviour: options, `-c`,
 * `-m` and script paths all work.
 *
 * The only thing to arrange first is where CPython finds its standard library.
 * The package ships it next to this library, and `pluginhost` puts the package
 * directory in PLUGIN_DIR, so PYTHONHOME points at a tree that looks like a
 * normal install (`lib/python3.12/…`).
 */

#include <Python.h>

#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <string.h>
#include <limits.h>
#include <unistd.h>
#include <dirent.h>
#include <dlfcn.h>

/* Long enough for a product path plus one child name. */
#define PATH_BUFFER 4096

/* Hand the real launcher path to `sitecustomize.py`.
 *
 * An embedded CPython reports an empty `sys.executable`, and everything that
 * re-executes the interpreter dies with `Permission denied: ''`: ensurepip's own
 * bootstrap, `multiprocessing`'s spawn, `subprocess.run([sys.executable, …])`.
 * `Py_SetProgramName` cannot fix it here because `Py_BytesMain` builds its own
 * `PyConfig` and ignores the deprecated global; doing it properly means
 * reimplementing `Py_BytesMain` on `Py_InitializeFromConfig`.
 *
 * So the cheap, reliable route: resolve argv[0] to an absolute path here, put it
 * in the environment, and let the shipped `sitecustomize.py` assign
 * `sys.executable`. Measured before: a path invocation gave '', a bare name
 * found on PATH happened to work — this makes both work.
 */
/* `<product>/bin` from `PLUGIN_DIR` (`<product>/packages/<name>`). */
static int product_dir(char *out, size_t size) {
    const char *dir = getenv("PLUGIN_DIR");
    if (dir == NULL || *dir == '\0') {
        return 0;
    }
    char buffer[PATH_MAX + 1];
    if (snprintf(buffer, sizeof(buffer), "%s", dir) <= 0) {
        return 0;
    }
    size_t length = strlen(buffer);
    while (length > 0 && buffer[length - 1] == '/') {
        buffer[--length] = '\0';
    }
    for (int level = 0; level < 2; level++) {
        char *slash = strrchr(buffer, '/');
        if (slash == NULL) {
            return 0;
        }
        *slash = '\0';
    }
    return snprintf(out, size, "%s", buffer) > 0;
}

static void export_launcher_path(const char *argv0) {
    char resolved[PATH_MAX + 1];
    if (argv0 == NULL || *argv0 == '\0') {
        return;
    }
    if (strchr(argv0, '/') != NULL) {
        /* A path the caller typed. Keep it as given when it works: `realpath`
         * would follow the symlinks into the APK's native library directory,
         * and that resolved target is a shared object, not something `execve`
         * can start — while the symlink itself starts fine (measured). */
        if (realpath(argv0, resolved) != NULL) {
            setenv("ATOMCODE_PYTHON_EXE", argv0, 1);
            return;
        }
        setenv("ATOMCODE_PYTHON_EXE", argv0, 1);
        return;
    }

    /* `plugin_main` receives the *package name* as argv[0], never a path (see
     * pluginhost: "argv[0] is the package name"), so neither `realpath` nor a
     * bare `access` can find it. `PLUGIN_DIR` is the one absolute location we
     * are given, and `<product>/bin/<name>` is where the launcher actually is. */
    char product[PATH_MAX + 1];
    if (product_dir(product, sizeof(product))) {
        char candidate[PATH_MAX + 1];
        int written = snprintf(candidate, sizeof(candidate), "%s/bin/%s", product, argv0);
        if (written > 0 && (size_t)written < sizeof(candidate) &&
            access(candidate, X_OK) == 0) {
            setenv("ATOMCODE_PYTHON_EXE", candidate, 1);
            return;
        }
    }

    const char *path = getenv("PATH");
    if (path == NULL) {
        return;
    }
    const char *cursor = path;
    while (*cursor != '\0') {
        const char *end = strchr(cursor, ':');
        size_t length = end != NULL ? (size_t)(end - cursor) : strlen(cursor);
        char candidate[PATH_MAX + 1];
        int written = snprintf(candidate, sizeof(candidate), "%.*s/%s", (int)length, cursor, argv0);
        if (written > 0 && (size_t)written < sizeof(candidate) && access(candidate, X_OK) == 0) {
            setenv("ATOMCODE_PYTHON_EXE", candidate, 1);
            return;
        }
        if (end == NULL) {
            break;
        }
        cursor = end + 1;
    }
}


/* Load a Python payload's companion libraries into the global group.
 *
 * Wheels built elsewhere (Chaquopy, Termux) link their shared libraries by plain
 * soname — `libopenblas.so`, `libc++_shared.so` — and bionic only resolves a
 * DT_NEEDED against a library that is *already loaded* under that soname: a
 * directory is not searched for an app-private path. Payload packages ship those
 * libraries inside the payload in `.libs` (the layout wheels use), the daemon
 * links that directory into `python-packages`, and this preloads everything in
 * it with RTLD_GLOBAL before any extension asks.
 *
 * Loading them here instead of shipping them with the runtime is deliberate: the
 * dependency belongs to the package that links it, so a stale or re-versioned
 * runtime cannot silently break numpy, and the runtime does not carry a BLAS it
 * never uses. Missing directory is normal (a device without such a package).
 */
static void load_package_libs(const char *home) {
    char directory[PATH_BUFFER];
    int written = snprintf(directory, sizeof(directory), "%s/python-packages/.libs", home);
    if (written <= 0 || (size_t)written >= sizeof(directory)) {
        return;
    }
    DIR *handle = opendir(directory);
    if (handle == NULL) {
        return;
    }
    struct dirent *entry;
    while ((entry = readdir(handle)) != NULL) {
        const char *name = entry->d_name;
        size_t length = strlen(name);
        if (name[0] == '.' || length <= 3 || strcmp(name + length - 3, ".so") != 0) {
            continue;
        }
        char path[PATH_BUFFER];
        written = snprintf(path, sizeof(path), "%s/%s", directory, name);
        if (written > 0 && (size_t)written < sizeof(path)) {
            dlopen(path, RTLD_NOW | RTLD_GLOBAL);
        }
    }
    closedir(handle);
}

int plugin_main(int argc, char **argv) {
    const char *dir = getenv("PLUGIN_DIR");
    if (dir != NULL && *dir != '\0') {
        setenv("PYTHONHOME", dir, 1);
    }
    /* Packages must survive a runtime update.
     *
     * Installing a new runtime replaces the whole package directory, so anything
     * pip dropped inside it (site-packages, and pip itself) is destroyed. Put the
     * install target OUTSIDE the package instead — the product tree, which the
     * Android host exposes as HOME — and add it to the import path here. Then
     * `pip install --target $HOME/python-packages …` keeps working across
     * runtime updates. */
    const char *home = getenv("HOME");
    if (home != NULL && *home != '\0') {
        char persistent[4096];
        int written = snprintf(persistent, sizeof(persistent), "%s/python-packages", home);
        if (written > 0 && (size_t)written < sizeof(persistent)) {
            mkdir(persistent, 0777);
            const char *existing = getenv("PYTHONPATH");
            if (existing != NULL && *existing != '\0') {
                char combined[8192];
                int size = snprintf(combined, sizeof(combined), "%s:%s", persistent, existing);
                if (size > 0 && (size_t)size < sizeof(combined)) {
                    setenv("PYTHONPATH", combined, 1);
                }
            } else {
                setenv("PYTHONPATH", persistent, 1);
            }
        }
    }
    /* Keep bytecode out of the package: it is read-only in spirit and the
     * device writes nothing back into it, so caching only wastes space. */
    setenv("PYTHONDONTWRITEBYTECODE", "1", 1);
    /* Unbuffered by default would make piped output arrive one byte at a time;
     * line buffering matches what a person expects from a REPL-less run. */
    setenv("PYTHONUNBUFFERED", "1", 1);

    /* pip needs a reachable index to be useful at all.
     *
     * Measured on this device: `files.pythonhosted.org` gives no response in
     * 12 s and `pypi.org/simple/` is a 47 MB page, so the default index makes
     * every install time out — while a mainland mirror answers and installs
     * fine. Set a mirror ONLY when the caller has not configured one, so an
     * explicit `PIP_INDEX_URL` or `pip config` always wins. */
    if (getenv("PIP_INDEX_URL") == NULL && getenv("PIP_CONFIG_FILE") == NULL) {
        setenv("PIP_INDEX_URL", "https://mirrors.aliyun.com/pypi/simple/", 1);
        /* Slow mobile links need a longer read timeout than pip's 15 s default. */
        setenv("PIP_TIMEOUT", "120", 1);
        /* The version check reaches the same unreachable host on every run. */
        setenv("PIP_DISABLE_PIP_VERSION_CHECK", "1", 1);
    }

    export_launcher_path(argc > 0 ? argv[0] : NULL);
    if (home != NULL && home[0] != 0) {
        load_package_libs(home);
    }
    return Py_BytesMain(argc, argv);
}
