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

## 5. 输出、退出码、调优

- **stdout** —— findings(人类可读报告;`--json` 则输出 `Finding[]` 数组)。按优先级(P0→P3)、
  再按置信度排序。
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
--stream-timeout <秒>             单事件存活上限(默认 180)
--json                            findings 以 JSON 输出
```
