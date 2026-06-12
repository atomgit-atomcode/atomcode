# atomcode-clix — `atomcodex` 代码评审 CLI

一个独立的、单一能力的命令行工具:**代码评审**。它驱动
[`atomcode-review`](../atomcode-review) agent(kernel + capabilities)对一段 git diff 进行
评审,并输出结构化发现(findings)。与 `atomcode-cli` / `atomcode-core` 完全解耦。

二进制名:**`atomcodex`**。通过 `cargo run -p atomcode-clix -- review …` 运行,或安装后直接
`atomcodex review …`。

```
atomcodex review [diff 来源] [provider] [system prompt] [输出] [调优]
```

---

## 1. Provider 凭据

解析优先级:**命令行 flag > 环境变量(`ATOMCODE_*`)> `~/.atomcode/config.toml`**。

```bash
# A) 零配置 —— 用 config.toml 的 default_provider
atomcodex review

# B) 指定 config.toml 里的某个 [providers.<name>]
atomcodex review --provider openrouter

# C) 直接传入(任意 OpenAI 兼容端点)
atomcodex review \
  --api-key sk-... --base-url https://api.deepseek.com/v1 --model deepseek-chat

# 或用环境变量
ATOMCODE_API_KEY=sk-... ATOMCODE_BASE_URL=https://api.deepseek.com/v1 \
  ATOMCODE_MODEL=deepseek-chat atomcodex review
```

`config.toml` 结构(clix 读取的子集):

```toml
default_provider = "openrouter"

[providers.openrouter]
api_key = "$OPENROUTER_API_KEY"   # $VAR / ${VAR} / ${VAR:-default} 会从环境变量展开
model = "stepfun/step-3.7-flash"
base_url = "https://openrouter.ai/api/v1"
context_window = 128000
```

`[providers.x]` 若写的是**字面 api_key**,则零环境变量即可用;若是 `$VAR` 引用,则需对应环境变量已
设置。`--config <path>` 可覆盖配置文件路径。

> **AtomGit / gitcode 签名网关**(`llm-api.atomgit.com`、`api-ai.gitcode.com` 等)需要 AtomCode
> 的闭源请求签名,`atomcodex` **无法对接** —— 会提前给出可操作的报错。请换用普通 key 的 provider。

---

## 2. 选择评审哪段 diff

| 命令 | 评审内容 |
|---|---|
| `atomcodex review` | 未提交的改动(`git diff HEAD`) |
| `atomcodex review --staged` | 暂存区改动(`git diff --staged`) |
| `atomcodex review --base origin/main` | 分支改动(`origin/main...HEAD`) |
| `atomcodex review --pr 123` | GitHub PR 的 diff(`gh pr diff 123`,需 `gh`) |
| `atomcodex review --diff-file pr.diff` | 来自文件的 diff |
| `… --diff-file -` | 来自 **stdin** 的 diff(任意 forge / CI) |
| `--repo <dir>` | 对另一个仓库根目录运行(默认 `.`) |

**未跟踪的新文件**不会出现在 `git diff HEAD` 里。要纳入它们:

```bash
git add -N path/to/new_files      # intent-to-add:此后这些文件会出现在 diff 中
atomcodex review
```

---

## 3. 结合全仓代码评审 PR(推荐流程)

有意义的评审需要**工作区先 checkout 到 PR 的代码状态**,这样 agent 的 read/grep/codeintel 工具
读到的代码才和 diff 对得上。**先切分支,再评审**:

**GitHub:**
```bash
gh pr checkout 123            # 工作区现在是 PR 的 head
atomcodex review --base main  # diff = main...HEAD;agent 结合 PR 代码上下文评审
```

**gitcode**(MR ref 为 `refs/merge-requests/<N>/head`,对应 gitcode "克隆/下载 → 拉取 PR 分支代码"):
```bash
# 步骤一:更新远程
git fetch origin
# 步骤二:拉取 PR 分支代码(SSH;HTTPS 把 URL 换成 https 形式即可)
git fetch git@gitcode.com:<owner>/<repo>.git +refs/merge-requests/<N>/head:pr_<N>
# 步骤三:切换到 PR 源分支
git checkout pr_<N>
# 然后评审(此时工作区即 PR 代码)
atomcodex review --base main
```
例如评审 246 号 PR:`git fetch git@gitcode.com:atomgit_atomcode/atomcode.git +refs/merge-requests/246/head:pr_246 && git checkout pr_246 && atomcodex review --base main`。

> 仅用 `--pr 123`(或 `--diff-file -`)只取**diff**,**不会改动工作区** —— 磁盘上的代码可能与 diff
> 不一致。要做结合上下文的评审,务必先 checkout 对应分支。

---

## 4. 自定义 system prompt(全量覆盖)

**完全替换**内置的 reviewer 提示词:

```bash
atomcodex review --system-prompt "你是严格的安全审查员。……"
atomcodex review --system-prompt-file ./reviewer.md
cat reviewer.md | atomcodex review --system-prompt-file -
```

> 全量覆盖会**丢弃内置的工具清单 + `report_finding` 用法说明**。你的自定义提示词里必须告诉模型
> 有哪些工具(`read_file`/`grep`/`ast_grep`/codeintel/`web_search`),以及"逐条用
> `report_finding` 上报问题",否则 findings 会是空的。

---

## 4.1 追加 system prompt(推荐:保留内置 + 叠加)

大多数定制不需要全量覆盖。`--append-system-prompt[-file]` 在内置 reviewer 提示词
(或 `--system-prompt` 覆盖后的提示词)**之后追加**一段,**内置工具说明与 `report_finding`
协议原样保留** —— 适合塞领域规则、忽略清单、仓库风格指南、PR 元信息等。

```bash
atomcodex review --append-system-prompt "本仓库忽略 vendor/ 下的改动;命名遵循 snake_case。"
atomcodex review --append-system-prompt-file ./team-rules.md
cat team-rules.md | atomcodex review --append-system-prompt-file -
```

> 与 `--system-prompt`(全量覆盖)的区别:覆盖会丢掉内置说明,追加不会。日常定制优先用追加。
> 二者可叠加:`--system-prompt` 换 persona 后,`--append-system-prompt` 再补充。

---

## 4.5 自定义 task(chat / explain / summary)

默认 task 写死为"评审下面这段 diff"。`--task` **替换**它,跑任意单轮任务并**跳过 diff 计算**——
调用方把模型需要的一切(用户问题、目标代码、任何 diff 上下文)都放进 task 文本里。配合
`--system-prompt` 设 persona、`--json` 从 `text` 字段取自由文本答案:

```bash
# explain:解释某段代码
atomcodex review --repo . \
  --task '一句话解释这段代码做什么：func Sum(...) {...}' \
  --system-prompt '你是简洁的代码讲解员，直接作答，不要调用 report_finding。' --json

# chat:带 diff 上下文回答用户问题
cat task.txt | atomcodex review --repo . --task-file - \
  --system-prompt-file ./chat_persona.md --json
```

- `--task` 与 `--base/--staged/--pr/--diff-file` 互斥(自定义 task 模式不算 diff)。
- review agent 壳不变:`report_finding` 工具仍挂着但 persona 让它"直接答、别报 finding"即可绕开;
  `read_file`/`grep`/codeintel 等只读工具正好给 chat/explain 读上下文用。
- `--json` 下 findings 通常为空,答案在 `text`;空 findings + 无错误 → 退 `0`。

---

## 4.6 内置语言规则(按改动文件自动匹配)

每次评审会根据**本次改动的文件类型**,自动在 system prompt 后追加一段"针对性审查重点"——
改了 `.go` 注入 Go 规则、改了 `.sql` 注入 SQL 规则,混合改动各自只作用于对应文件、互不污染。
无需配置,默认生效。

目前内置覆盖 **40+ 种语言/文件类型**:Go / Rust / TS·JS / Python / Java / Kotlin / C / C++ /
C# / Swift / Objective-C / Dart / Scala / Ruby / PHP / Groovy / Lua / Perl / R / Elixir /
Erlang / Haskell / Clojure / Solidity / ArkTS / SQL / Shell,以及 Dockerfile / Terraform /
HTML / CSS / XML / YAML / JSON / TOML / Protobuf / GraphQL / Makefile / CMake / properties /
Maven·Gradle / MyBatis mapper 等。

```bash
# 热调优:用 <dir>/<name>.md 覆盖任意内置规则,无需重新编译(缺的名字回退内置)
atomcodex review --rules-dir ./my-rules     # 例如放一个 go.md 覆盖内置 Go 规则

# 完全关闭规则注入(需要干净 prompt 的 A/B 实验等)
atomcodex review --no-rules
```

> 规则名即文件名:`go.md` / `sql.md` / `csharp.md`……与
> [`atomcode-review/rules/`](../atomcode-review/rules) 内的内置文件一一对应。

---

## 5. 输出、退出码、调优

- **stdout** —— 人类可读报告;`--json` 则输出一个结构化对象(便于嵌入者一次取齐):
  `{ "findings": [...], "text": "<agent 最终自由文本>", "usage": {"prompt":N,"completion":N,"cached":N} | null }`。
  `findings` 按优先级(P0→P3)、再按置信度排序;`usage` 在 provider 未上报时为 `null`。
- **stderr** —— 实时执行轨迹(每次工具调用 + 结果),收尾给出工具用量画像 + token 统计。(让 stdout
  在 `--json` 时保持纯净。)

```
Reviewing 120 changed line(s) with deepseek-chat …
  → read_file src/auth.rs
    ✓ read_file (4096 chars)
  → grep verify_token
    ✓ grep (812 chars)
  → report_finding [P0] fix: token expiry not checked
    ✓ report_finding (140 chars)
— trace — 12 tool call(s): read_file×6, grep×3, find_references×2, report_finding×1
— tokens — prompt 21044 / completion 180 / cached 18432
```

**退出码**:干净跑完 → `0`;出错但已收集到 findings → `0` + 警告;出错且 0 findings(认证/连接/卡死)
→ 非零(便于 CI 检测)。

**调优**:`--stream-timeout <秒>`(默认 180)—— 慢 provider / 大上下文时调高,避免存活守卫提前失败。

---

## 完整参数参考

以 `atomcodex review --help` 为准:

```
--base <ref>            相对 base...HEAD 评审
--staged                评审暂存区改动
--pr <N>                评审 GitHub PR(需 gh)
--diff-file <path|->    从文件或 stdin 读取 diff
--repo <dir>            仓库根目录(默认 .)
--provider <name>       config.toml 的 provider 条目(覆盖 default_provider)
--config <path>         配置文件(默认 ~/.atomcode/config.toml)
--model / --api-key / --base-url   provider 覆盖项
--system-prompt <text>            全量覆盖 persona
--system-prompt-file <path|->     从文件/stdin 全量覆盖 persona
--append-system-prompt <text>     在 persona 之后追加一段(保留内置说明;日常定制首选)
--append-system-prompt-file <path|->  从文件/stdin 追加
--rules-dir <dir>                 覆盖内置语言规则目录(<dir>/<name>.md;缺的回退内置)
--no-rules                        关闭内置语言规则注入
--task <text>                     自定义 task(替换 diff 评审,跳过 diff;chat/explain/summary 用)
--task-file <path|->              从文件/stdin 读取自定义 task
--stream-timeout <秒>             单事件存活上限(默认 180)
--json                            findings 以 JSON 输出
```
