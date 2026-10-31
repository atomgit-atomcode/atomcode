/*
 * Generic plugin shim: forward `plugin_main` to a program's own entry point.
 *
 * A package is a shared library that `pluginhost` dlopens (Android forbids
 * executing a downloaded binary), so a program written as `int main(...)` needs
 * a `plugin_main` of the same shape. The build compiles the program's own
 * source with `-Dmain=<PLUGIN_ENTRY>`, which renames its entry point, and this
 * file hands the arguments straight through — no behaviour change, no fork.
 *
 * Build:
 *   cc -shared -fPIC -DPLUGIN_ENTRY=lua_standalone_main \
 *      -o plugin.so plugin_main.c lua.o lapi.o …
 */
#ifndef PLUGIN_ENTRY
#error "define PLUGIN_ENTRY to the renamed main symbol"
#endif

extern int PLUGIN_ENTRY(int argc, char **argv);

int plugin_main(int argc, char **argv) {
    return PLUGIN_ENTRY(argc, argv);
}
