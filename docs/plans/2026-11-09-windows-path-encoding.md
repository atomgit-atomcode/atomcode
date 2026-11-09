# Windows 中文路径编码诊断与回归

## 事实与范围

本次事故尚未定位到原始命令。UTF-8 按 CP936 误解码能复现“增肌 → 澧炶倢”，但不证明串码发生在 AtomCode 的进程启动、输出解码或某个生成的脚本中。仓库无 `_atomcode_archive` 内置归档流程。现有 PowerShell 工具走 UTF-16LE/Base64，cmd 直接 raw_arg，文件系统走原生路径。

状态 owner 不变：Shell 实现构造命令，共享 spawn_local_command 持有子进程；路径与输出不能经新转换层。不存在 runtime、持久化格式或审批变化。

## 实施

1. 在共享 Windows shell spawn 处补 debug 诊断：实际 shell、cwd、参数数量/是否非 ASCII、是否 EncodedCommand、ACP/OEM/控制台 code page。完整命令、环境变量值、stdout/stderr 不进入新增日志。默认 info 级别不新增输出，不改变系统或子进程 code page。
2. 跨平台验证 UTF-8、CP936、UTF-16 中文路径的输出解码，UTF-8 分块和 PowerShell 编码命令传输；明确已损坏但合法的 Unicode 文本不能自动猜测修复。
3. Windows 实机测试使用临时目录：原生文件系统、强制 cmd（不被 Git Bash 自动替换）、实际选择 shell、PowerShell 5.1/可用的 7，包含空格及中文路径。断言准确落盘的目录与文件，并枚举临时根目录排除乱码同级树；失效 cwd 不被 shell 自动创建。
4. 提供 Windows 本地复现脚本，用 UTF-16LE 编码 PowerShell 作为外层控制，分开比较原生调用、cmd 直接参数、UTF-8 批处理和无 BOM/BOM PowerShell 文件。仅写系统临时目录，保留诊断报告供比对，不修改真实工作区。这里的故意错误解码是负向对照，不能当作事故已复现。
5. 运行受影响 capabilities 回归、fmt 与编译门。macOS 无法运行 Windows API/cmd；把 Windows 专属用例加入仓库并接入 windows-latest 独立 CI job（仅该 crate 的 lib 测试，不改既有编译门），但如无实机执行条件明确标为未执行，不能声称根因已确认或缺陷已修复。
