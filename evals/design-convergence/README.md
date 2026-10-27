# Design convergence evaluation

This is a paired **prompt-level** experiment, not a full AtomCode agent/session
reproduction. It uses the rendered production persona, six synthetic multi-turn
cases and a static-key OpenAI-compatible endpoint. No tools, skills, memory,
project instructions, session persistence, actual compaction trigger, or gateway
signing are exercised. The reported user's endpoint and session are unavailable.

Requires Python 3.11+, an existing config selection with a static API key, and the
Rust `dump_persona` example. Secrets stay in the normal config store; artifacts
never copy credentials, endpoint URLs or reasoning content. Results are ignored.

Capture the baseline **before editing** persona or summarization instructions:

```sh
mkdir -p evals/design-convergence/results
cargo run -q -p atomcode-coding --example dump_persona -- deepseek-v4.1-flash-1017 > evals/design-convergence/results/baseline-persona.txt
python3.12 evals/design-convergence/eval.py snapshot --label baseline --persona evals/design-convergence/results/baseline-persona.txt
```

After the edit, repeat with `candidate` replacing `baseline`. Snapshots refuse
overwrite. The extractor supports the JSON-compatible Rust string escapes used
by these constants, including literal newlines and Rust line continuations; it
fails if a required constant is missing. Persona and summary instructions are
fingerprinted. Use the same model spelling when dumping both personas.

```sh
python3.12 evals/design-convergence/eval.py run --output evals/design-convergence/results/ordinary --phase ordinary
python3.12 evals/design-convergence/eval.py run --output evals/design-convergence/results/summary --phase summary
```

Default: three repetitions per arm, six cases, six user turns per trial,
concurrency four, configured reasoning effort, output cap 8192, request timeout
300 seconds. `--selection` selects another account; it does not change config.
Run one case/repetition first as a transport and truncation preflight. Each
invocation creates a new result directory and randomizes arm/trial order.

The summary phase performs a real separate LLM summary after the first three
turns, replaces that prefix with a framed checkpoint, then continues the last
three turns. Summary input uses the production system/template constants, but
this does not test production history selection, retained messages, previous
anchors, image handling or compaction retries. Ordinary and summary phases are
separate experiments. Differences may reflect both persona and summary edits;
this is not a factorial causal attribution.

Each anonymous `trial-NNN.json` carries the rubric, all visible turns, reported
models, usage and elapsed request times. The private manifest maps packets to
arms and fingerprints. Give reviewers packets **without** that manifest; do not
use an arm-aware model judge as a substitute for checking field semantics.
Progress/partial artifacts preserve completed-call usage on interruption. A
failed call can consume unreported usage; recorded usage is not a billing total.

Score each complete transcript, not only its final reply:

- Critical failure: merged independent identifiers; TubeSeq changed to device
  identity; required events/semantics lost; valid new evidence ignored; explicitly
  changed scope not followed. A later correction does not erase earlier errors.
- Convergence: count questions with no material unanswered choice, unjustified
  redesigns, and reopened resolved alternatives. Do not count justified changes
  for new protocol facts as regressions.
- Cost: after correctness, compare successful API requests, summed prompt/cache/
  completion usage, elapsed call time and response size. Summary calls count too.
- Infrastructure failures, empty outputs and truncations are **not** behavioral
  passes. Report their counts separately. Identical model aliases do not prove
  an immutable backend; report the actual response model IDs and endpoint limits.

There is no automatic semantic pass/fail rule based merely on finding field
names. Write reviews separately, disclose reviewer identity and whether blinded,
and treat a small three-repeat sample as directional evidence, not a guarantee.

Offline runner tests:

```sh
python3.12 -m unittest discover -s evals/design-convergence -p 'test_*.py' -v
```

Optional **model-assisted** blind screening (not human review):

```sh
python3.12 evals/design-convergence/review.py evals/design-convergence/results/ordinary
```

The reviewer process never reads the arm manifest. It uses the same configured
model unless overridden; this is not an independent model-quality oracle. Every
flag must cite an exact quote from the claimed answer/summary; malformed ratings
and fabricated quotes are review failures, not passes. Human checking remains
necessary, especially for disputed schema semantics. Reviewer requests/usage are
stored separately from candidate execution cost.

Execution uses configured effort (in this run: max). The optional reviewer uses
high effort and a 16384 output cap by default, separately recorded in review
artifacts. It is a screening aid, not the candidate under test. `--output` can
retain a separate review attempt without overwriting a failed earlier run.

The turn schedule is fixed. Successful ordinary trials always have six requests;
summary trials have seven. This experiment can assess semantic drift, unnecessary
questions and redesign, but cannot establish fewer real-world discussion rounds
or lower end-to-end task latency. Prompt token sums include overlapping history
and cache hits; they are not context-window sizes or billing estimates. Concurrent
phases can influence latency/cache; report costs as descriptive, not causal wins.
The baseline is this checkout before the patch, not the reporter's dfa998f4 build.

To aggregate execution and preliminary model ratings after reviewing completes:

```sh
python3.12 evals/design-convergence/summarize.py evals/design-convergence/results/ordinary
```

## Review protocol v2 (2026-10-26)

See [ADJUDICATION.md](ADJUDICATION.md) for the assistant's flag-by-flag follow-up.
Historical v1 ratings are unchanged. Future reviews require exact user-requirement
quotes and their turn numbers in addition to assistant evidence. The validator
rejects requirements introduced later than the evidence and quotes originating
only in assistant output; these gates do not decide semantic truth. Future
artifacts include a protocol version and reviewer-prompt hash. Aggregation refuses
mixed protocols/prompts; keep re-reviews in a separate output directory/report.
No new live trials or paid judging were run for this adjudication.

## 分离干预

见 [ABLATION.md](ABLATION.md)。`--cases ablation-cases.json` 与
`--arms baseline persona-only` / `--arms baseline summary-only` 可分别比较。
单项快照通过不可变 baseline/candidate 快照按字段组合生成，身份与哈希写入各运行 manifest；不要覆盖旧结果。新案例文件哈希也写入 manifest。
本轮无稳定增益，生产提示词候选已撤回，试验资料保留。

## 真实执行链测试

见 [NATIVE-TODO.md](NATIVE-TODO.md)。`native_eval.py` 使用当前本地 CLI、隔离产品目录和技能副本，逐轮 `--continue`；它会真实调用配置模型，保留可见消息与工具记录，丢弃 reasoning.delta。与 `eval.py` 的受控工具结果回放不同。两者都不能冒充原用户会话复现。
