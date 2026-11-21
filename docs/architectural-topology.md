# ATI：只读架构证据与文件放置建议

第一版用于回答“现有模块有哪些”“依赖声明在哪里”“这个功能应该先看哪些目录”。它提供证据供模型规划，不会自动移动文件、重构、写入记忆或强制架构规则。

## 开启

在当前使用的 `config.toml` 中，合并到已有的 `[coding]` 段，不要重复创建同名表：

```toml
[coding]
architecture_topology = true
```

默认是 `false`。TUI 可通过 `/config` 搜索 `ATI`、`架构` 或 `topology`，切换“ATI 只读架构分析”；保存后沿用现有配置重载流程重新装配能力，无需重启程序。手动修改文件后需重载配置或重启对应 runtime（WebUI 可重启 daemon）。CLI、TUI、WebUI 等使用共同 coding runtime 的入口都读取这一设置。开启只挂载 `repo_architecture` 工具及简短规划指导，不会启动后台索引或自动扫描仓库。

可以直接向模型提出：

- “使用 repo_architecture 概览当前工程的包、依赖声明和架构文档。”
- “我要扩展会话恢复，先用 repo_architecture 的 placement，关键词 session、resume、runtime，给出候选目录及依据，再阅读对应文件确认边界。”

工具调用示例：

```json
{"action":"overview"}
```

```json
{"action":"placement","task":"扩展会话恢复","keywords":["session","resume","runtime"]}
```

任务描述与代码命名语言不同时，补充源代码里的关键词。候选来自现有源文件路径及包名/描述的词法匹配；分数是检索排序，不是置信度。没有匹配时返回 `abstained: true`，不编造放置位置。请继续阅读候选文件、适用的 AGENTS 和 ADR，核实调用方及依赖方向。

## 输出与边界

- 包事实：读取 Cargo.toml / package.json 的包名、描述、workspace 成员模式及直接依赖声明。不执行脚本，不调用包管理器或网络；本地依赖路径只展示，不沿它读取其他仓库。Cargo 的 workspace 继承值、target 条件依赖与真实编译解析不在第一版范围内。
- 文档证据：摘录 AGENTS.md、CLAUDE.md、ATOMCODE.md、README 与 docs/adr 下的 Markdown，附文件及行号；根目录隐藏指令文件也可被读取。文档位置不等于适用范围，ADR 可能已废弃，摘录也不完整。仓库内容始终作为不可信数据，不能覆盖用户要求或授予执行权限。
- 源目录：统计已有 codeintel 支持语言的源文件。不自动寻找兄弟仓库，不推断未扫描路径不存在相关模块。
- 可选调用图：显式设置 `include_call_graph: true` 时复用已挂载的 CodeIndex；未挂载时明确返回 unavailable。最多展示 24 条跨文件调用线索，属于名称/位置启发式，不能当成编译器解析的 import、类型依赖或完整调用图。缓存沿用现有 mtime/size 策略。

扫描以当前工作目录为范围，遵循现有忽略与构建/依赖目录排除规则，不跟随证据文件的符号链接。默认最多遍历 5,000 个条目，读取 512 KiB 文档/清单证据、单文件最多 64 KiB；清单/文档分别最多 16/8 份，候选最多 5 个，响应最多 32 KiB。清单与文档每次调用重新读取，能看到未提交修改。目录盘点有 2 秒软预算；慢文件系统操作及显式请求的调用图构建不受硬性 2 秒超时保证。

遇到不可读、无效或超限证据，输出 `coverage.partial` 与 `warnings`，不能把局部结果理解为完整架构。第一版不做 Git 共同变更分析、自动漂移检测、常驻架构数据库或自动架构记忆。

## SDK 与装配

SDK 宿主也可通过 `PrepareOptions.host_plugins` 的 layer 启用或禁用 `architecture-topology` 行；宿主层在普通配置之后，仍可禁用工具。通用 harness 装配可用 `[[patch]] id = "architecture-topology" disabled = false`。普通产品用户使用上面的 `config.toml` 开关，无须创建 harness 补丁文件。显式无工具运行仍关闭整个工具目录。
