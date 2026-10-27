# Hugging Face Model Oracle

一个独立的 Hugging Face 模型生态洞察服务：

- 抓取 Hugging Face 公开模型列表，聚合 DeepSeek、Qwen、GLM、Llama、Mistral、Gemma、Phi、Yi、InternLM 等模型家族。
- 展示模型数、总 likes、总下载量、平均热度、最近更新和增长预测。
- 点击家族查看对应的基础模型、微调模型、量化模型、adapter/merged 等衍生模型。
- 使用 SQLite 保存模型原始记录、采集游标、页数、错误和历史报告；每页数据与下一页游标在同一事务内提交，进程中断后重启会从上次游标继续。
- `GET /api/jobs` 可查看模型采集的 running/paused/complete/failed 状态、页数、已落库记录数和错误。

## 启动

需要 Node.js >= 22.22.0（使用内置 SQLite）。

```bash
cd hf-insights
npm ci
npm start
# 浏览器打开 http://127.0.0.1:8787
```

首次刷新沿 Hugging Face 返回的 Link 游标扫描模型目录，直到响应不再提供下一页。并非按名称检索 Top N，也不以“当前页不足一页”判断结束。可用环境变量调整规模和节奏：

```bash
HF_INSIGHTS_PORT=8787
HF_INSIGHTS_HOST=127.0.0.1
HF_INSIGHTS_REFRESH_HOURS=6
HF_INSIGHTS_PAGE_SIZE=100
HF_INSIGHTS_REQUEST_PAGE_SIZE=1000  # 真实 HF 支持的请求页长；游标续采可加速
HF_INSIGHTS_COMPACT=1               # 可选：保留 id/downloads/likes/tags，省略文件列表等详情
HF_INSIGHTS_MAX_MODELS=0             # 0 = 扫描到目录末尾；>0 = 按整页预算暂停
HF_INSIGHTS_REQUEST_DELAY_MS=700
HF_INSIGHTS_API_URL=https://huggingface.co/api/models
HF_INSIGHTS_PROXY=http://127.0.0.1:7890  # 可选；也会读取 HTTPS_PROXY/HTTP_PROXY
HF_INSIGHTS_DB=./data/hf-insights.sqlite
HF_TOKEN=hf_...                # 可选，提高限额；不会写入数据库
```

每次刷新都会返回并保存 `model_count`、`pages`、`complete`、`stop_reason` 和 `duration_ms`。页面展示的是实际覆盖量，而不是把某个 Top N 样本包装成全站统计。全量扫描耗时取决于当时 Hub 的模型数量、网络、HF 限流和请求间隔：粗略可按 `页数 × (接口响应时间 + 请求间隔)` 估算；如果达到 HF 限流或网络失败，服务保留上次完整快照并标记为降级。

## 数据边界与自动刷新

默认每 6 小时触发一次采集；已有采集未结束时跳过重叠触发。服务必须持续运行，停止进程时不会后台执行。`refresh.interval_ms=21600000`、`next_at`、`running`、`automatic_refreshes` 可在 `/api/insights` 查看。`HF_INSIGHTS_REFRESH_SECONDS` 仅用于缩短验证周期，正式运行不要设置。

已使用真实 HF 有限查询验证：10 秒周期产生两个独立的 `complete` 采集任务，自动成功刷新次数为 1。这个测试验证了同一调度路径，不等于已经等待过实际六小时。

无上限扫描完成且 `complete=true` 表示已遍历本次 API 可见的 Models 目录，不包括不可见私有模型，也不是跨页原子快照。首轮扫描中的数量只是已覆盖量。配置预算时按整页暂停，最多可能超过预算一页；提高预算或设为 0 可继续游标，不丢弃页尾模型。

家族识别依据模型 ID、模型卡片及 tags 中的 base_model 声明；未知模型保留为 Other 并置底。声明不等于独立验证的继承关系。Downloads 是 HF 报告的滚动下载指标，不是累计下载或独立用户数。线性外推只是探索性指标，尚未回测；不能把抓取范围扩大造成的增长解释为生态真实增长。

运行 `npm test` 执行隔离回归测试（测试使用内存库，不写入采集库）。当前本地无上限实例使用 `data/models-live.sqlite`；早期 `data/hf-insights.sqlite` 含开发测试记录，不应用作正式统计基线。
