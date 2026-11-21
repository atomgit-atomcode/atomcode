# Windows 粘贴 Unicode 符号丢失

## 已确认的代码问题与证据边界

Windows 控制台会把部分文本输入表达为 Alt 松开事件，并在事件中携带已提交的 Unicode 字符。修复前，vendored crossterm 把该 BMP 字符标为 `Release`，而 TUI 正常过滤按键释放事件，导致字符被静默丢弃。修复将这种特定事件转为一次 `Press`；普通按键释放仍不作为输入，避免重复字符。代理对仍由原有 UTF-16 合并逻辑处理。

这证明了一条实际可丢字的代码路径，不代表已经取得报告机器的原始事件。仅凭符号无法编码为 GBK，不能断定剪贴板文字已在 Windows 转码时消失：输入后端使用的是 `ReadConsoleInputW`，粘贴还会经过模拟按键转换。

5.2.6 已有自动切换 UTF-8 的代码。启动时打印的 936 警告发生在 TUI 初始化之前，不等于粘贴时的代码页。修复后的 TUI 在接受输入前核对 `SetConsoleCP` / `SetConsoleOutputCP` 的实际结果；任一仍不是 65001，就明确报错，避免静默进入已知有风险的控制台配置。控制台代码页由附着的进程共享，子进程也可能改变它；原有绘制时的修复仍保留。它不改变系统 ANSI 代码页或脚本文件解码规则。

## Windows 实机复验

在 VS Code 集成终端和 Windows Terminal 中分别运行包含修复的构建：

1. 在启动前执行 `chcp 936`，再运行 AtomCode。
2. 粘贴以下完整样例，检查正文、双空格、换行和符号；发送后核对会话中的用户原文：

```text
【验证】✅一致 px 53:16 |  推断:Int64  |  悬停:Int64
❌❓⚠️⏭😀
```

3. 切换到 `chcp 65001` 后重复；确认普通键入没有字符重复、回车没有重复提交。
4. 另试手工 Alt+数字输入，确认字符只插入一次。

若仍丢失，需抓取 Windows 输入事件的 `virtual_key_code`、`key_down`、`u_char` 和修饰键，确认字符是否到达原始事件层。不要通过匹配剪贴板全文自动替换输入，也不要猜测丢失字符数量；远程终端、剪贴板变化和部分粘贴都可能使这种修复改写用户原意。

## 临时配置

可在 VS Code `settings.json` 中增加独立终端配置（保留已有的其他配置）：

```json
{
  "terminal.integrated.profiles.windows": {
    "PowerShell UTF-8": {
      "path": "${env:WINDIR}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
      "args": ["-NoLogo", "-NoExit", "-Command", "chcp 65001 > $null"]
    }
  },
  "terminal.integrated.defaultProfile.windows": "PowerShell UTF-8"
}
```

该配置降低旧代码页带来的风险，但不能替代 Alt 字符提交事件的解析修复。

## 自动回归

Windows CI 分别运行 vendored crossterm 的 `windows_alt_unicode` 原始记录测试，以及 TUI 的 `windows_paste` / `console_utf8_setup` 测试。前者验证 BMP、变体选择符、代理对和普通释放事件的区别；后者验证完整输入保真及代码页失败反馈。macOS 的输入回归和 Windows 交叉编译不能替代上述实机粘贴验证。
