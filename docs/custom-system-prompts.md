# 自定义 coding persona

主 CLI/TUI 支持替换和追加 coding persona：

```sh
atomcode --system-prompt '你是一位代码导师，先解释设计再实现。'
atomcode --system-prompt-file ./persona.md --append-system-prompt-file ./constraints.md
atomcode -p '分析这个项目' --append-system-prompt '回答使用中文。'
```

`--system-prompt` 与 `--system-prompt-file` 互斥；两个 append 参数同样互斥。文件按 UTF-8 在启动时读取一次，保留原文，不展开模板。文件不可读、不是 UTF-8 或显式参数为空白时直接报错；文件参数不支持 `-` 读取 stdin。

“替换”只针对内置 coding persona。工具说明、项目指令、运行时能力说明继续由各自组件提供；权限、审批和 plan/build 门控不会被提示词修改。根参数只用于主 coding 入口，子命令使用自身接口，例如 `atomcode review --system-prompt ...`。

也可以使用已有配置字段，CLI、TUI、WebUI/daemon、ACP 共用该解析结果：

```toml
# 旧式 provider 配置
[providers.local]
type = "openai"
model = "my-model"
system_prompt = "你是一位代码导师。"

# 使用新式模型配置时，在对应的 [models.<选择ID>] 中设置：
# system_prompt = "你是一位代码导师。"
```

选择顺序：CLI 显式替换 > 当前模型/provider 解析得到的 `system_prompt` > 内置 persona。追加内容接在选中的 persona 后面。新式模型字段与旧式 provider 字段沿用现有模型解析规则，不额外合并成两份提示词。

切换模型时，配置中的 persona 随当前模型更新；没有配置的模型恢复内置 persona。本次启动指定的 CLI 替换和追加保持不变，包括 reload、undo、会话切换和 runtime 重建；TUI 新建后台 runtime 同样携带它们。

CLI 提示词不会持久化到会话日志。下次进程启动恢复会话时，使用当次参数及配置；需要长期使用时写入 config.toml，或每次传入同一个文件。当前不提供 persona 预设库、运行中 persona 切换命令或 SystemPromptHook。

## 查看实际生效内容

TUI 和 WebUI 输入 `/context persona`，可查看当前挂载 persona 的来源、是否追加指令及原文。`/context prompt` 在 TUI 中仍显示完整系统提示词；普通 `/context` 仍显示上下文预算。

WebUI 只查询该视图对应的已绑定运行时，不会为了查看提示词切换绑定。浏览离线历史或其他会话时会提示不可查询。自定义宿主的追加状态无法确定时显示“未知”；若宿主使用私有片段 ID，无法可靠识别 persona 时会提示未提供查看信息，不猜测原文。
