// Wrap nodejs-mobile's `libnode.so` as a plugin.
//
// On this device a program cannot be executed from the app's data directory, so
// `node` cannot be shipped as a binary. What *can* be loaded is a shared library,
// and nodejs-mobile is built to be embedded — its `libnode.so` exports
// `node::Start(argc, argv)`, the same entry an embedder calls instead of `main`.
//
// So the shape is identical to the Lua and CPython plugins: `pluginhost` dlopens
// this library, calls `plugin_main`, and node takes over. `argv[0]` is the
// package name, which is fine — node only uses it for `process.argv[0]`.
//
// Honest limits on this device, worth knowing before using it:
//   * `process.execPath` is `pluginhost`, so `child_process.fork`/`spawn` cannot
//     re-execute node itself (the same class of problem `sys.executable` had).
//   * native addons (`.node`) need symbols from libnode; they resolve only when
//     libnode is preloaded, and most prebuilt ones are glibc-linked anyway.
//   * the interpreter is in-process, so a crash takes the agent with it.
#include <cstddef>

namespace node {
int Start(int argc, char **argv);
}

extern "C" int plugin_main(int argc, char **argv) {
    // node expects argv[0] to exist and be NUL-terminated; pluginhost guarantees
    // both (it always passes the package name first).
    return node::Start(argc, argv);
}
