/*
 * Host-side entry point for the example MCP server.
 *
 * The server itself is a `pluginhost` plugin: it exports `plugin_main` and
 * speaks MCP over the stdio it inherits. On the device `pluginhost` calls that
 * entry point; on the build machine there is no `pluginhost`, so this shim
 * provides `main` and forwards to `plugin_main`.
 *
 * It exists only for `scripts/mcp-example/selftest.sh` — it is not part of the
 * Android package.
 */
int plugin_main(int argc, char **argv);

int main(int argc, char **argv) {
    return plugin_main(argc, argv);
}