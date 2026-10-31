/*
 * Host-side test entry point for `plugin.c`.
 *
 * On device, `pluginhost` loads the library and calls `plugin_main` with the
 * inherited stdio. That binary only exists in the APK, so this tiny wrapper
 * lets the same source speak the protocol on a build machine:
 *
 *   gcc -o /tmp/mcp-filesystem plugin.c host_main.c
 *   printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize"}' | /tmp/mcp-filesystem
 *
 * It is not shipped and not part of the package.
 */

int plugin_main(int argc, char **argv);

int main(int argc, char **argv) {
    return plugin_main(argc, argv);
}