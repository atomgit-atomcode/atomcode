/* QuickJS 的 `qjs` CLI，作为插件运行。
 *
 * 为什么不写自己的驱动：`qjs.c` 已经处理了 `-e`、脚本参数、模块加载、`std`/`os` 模块，
 * 自己重写只会得到半个壳。做法是把它编译时的 `main` 改名（`-Dmain=quickjs_main`），
 * 这里只做转发——与 Lua、CPython、node 的插件形态一致。
 *
 * 用法：
 *   qjs -e 'console.log(1 + 2)'
 *   qjs script.js arg1 arg2
 *
 * 注意：REPL（无参数直接运行）依赖 readline，Android 上没有，所以不带参数时会走
 * qjs 自己的非交互路径；脚本与 `-e` 不受影响。
 */
extern int quickjs_main(int argc, char **argv);

int plugin_main(int argc, char **argv) {
    return quickjs_main(argc, argv);
}
