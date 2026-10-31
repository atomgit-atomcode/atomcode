/*
 * Lua plugin shim: give `require` somewhere to look.
 *
 * Upstream Lua is built with `LUA_ROOT=/usr/local/`, so `package.path` and
 * `package.cpath` point at directories that do not exist on Android — the only
 * usable entry was `./?.lua`, which means a C module could only be loaded from
 * the current working directory. Measured on device: `require("socket")` and
 * `require("sqlite")` both failed even when the `.so` was present in the
 * product tree.
 *
 * This shim points both search paths at the product directory before handing
 * control to Lua:
 *
 *   <product>/lua/?.lua            pure-Lua modules
 *   <product>/lua/?/init.lua       package-style modules
 *   <product>/lua/?.so             C modules
 *
 * The trailing `;;` is Lua's own "also keep the built-in default" marker, so
 * `./?.lua` and whatever LUA_ROOT was compiled in still work. Both variables are
 * only set when the caller has not set them, so an explicit LUA_PATH/LUA_CPATH
 * always wins.
 *
 * Build: cc -shared -fPIC -DPLUGIN_ENTRY=lua_standalone_main plugin.c lua/*.o …
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#ifndef PLUGIN_ENTRY
#error "define PLUGIN_ENTRY to the renamed lua main symbol"
#endif

extern int PLUGIN_ENTRY(int argc, char **argv);

/* Long enough for the product path plus the patterns around it. */
#define PATH_BUFFER 4096

static void set_default_path(const char *variable, const char *value) {
    if (getenv(variable) != NULL) {
        return;
    }
    setenv(variable, value, 1);
}

int plugin_main(int argc, char **argv) {
    const char *home = getenv("HOME");
    if (home != NULL && *home != '\0') {
        /* A C module is only loadable if its `.so` exists; make the directory so
         * a person can simply drop one in. Failure is fine — the path still
         * works if it appears later. */
        char directory[PATH_BUFFER];
        int written = snprintf(directory, sizeof(directory), "%s/lua", home);
        if (written > 0 && (size_t)written < sizeof(directory)) {
            mkdir(directory, 0777);
        }

        char lua_path[PATH_BUFFER];
        written = snprintf(lua_path, sizeof(lua_path),
                           "%s/lua/?.lua;%s/lua/?/init.lua;;", home, home);
        if (written > 0 && (size_t)written < sizeof(lua_path)) {
            set_default_path("LUA_PATH", lua_path);
        }

        char lua_cpath[PATH_BUFFER];
        /* Two places, in order: the tree the person owns (survives a package
         * update) and the package's own `lib/lua` (so a bundle can carry the
         * modules it needs without touching anything outside itself). */
        const char *plugin_dir = getenv("PLUGIN_DIR");
        if (plugin_dir != NULL && *plugin_dir != '\0') {
            written = snprintf(lua_cpath, sizeof(lua_cpath),
                               "%s/lua/?.so;%s/lib/lua/?.so;;", home, plugin_dir);
        } else {
            written = snprintf(lua_cpath, sizeof(lua_cpath), "%s/lua/?.so;;", home);
        }
        if (written > 0 && (size_t)written < sizeof(lua_cpath)) {
            set_default_path("LUA_CPATH", lua_cpath);
        }
    }
    return PLUGIN_ENTRY(argc, argv);
}