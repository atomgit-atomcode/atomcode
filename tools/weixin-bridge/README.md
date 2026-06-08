# weixin-bridge (PoC)

个人微信 ⇄ atomcode 桥。底层走腾讯官方 iLink Bot 协议，经 atomcode daemon `/chat` 对接 agent。

## 准备
1. `cd tools/weixin-bridge && npm install`
2. `cp config.example.json config.json`，填 `workingDir`（agent 操作目录），`allowlist` 先留空（里程碑0），里程碑1起填你的微信 `from_user_id`。

## 里程碑0：验证通路（不接 atomcode）
```
npm run smoke
```
终端出二维码 → 微信扫码 → 给 bot 发消息 → 应收到「echo: …」。
日志会打印每条入站消息的 `from_user_id`，把它填进 config.json 的 `allowlist`。

## 里程碑1/2：接 atomcode
1. 另开终端启动 daemon：`atomcode daemon`（默认 13456）。
2. `npm start`
3. 微信发「你好」→ 收到 atomcode 回复（里程碑1）。
4. 发会触发 bash 的任务 → 收到审批提示 → 回 `y` → 看到结果（里程碑2）。

## 测试
```
npm test
```

## 已知约束（PoC）
- 审批进行中（收到「回复 y 同意 / n 拒绝」后），你的**下一条消息会被当成审批答复**（y→同意，其它→拒绝），不会开启新对话。请先回完 y/n 再发新请求。
- HTTP 非 2xx 会作为错误暴露：iLink 报错（如 token 失效 401）会在日志打印 `HTTP 4xx/5xx`；daemon `/chat` 报错会回一条「出错了：…」到微信。

## 故障
- token 失效 / 想重新登录：删除 `~/.atomcode/weixin/bot.json` 后重跑，会重新扫码。
- 收不到消息：确认 `allowlist` 含你的 `from_user_id`（或先留空放行所有人）。
- 日志反复打印 `getUpdates 失败 … HTTP 401`：token 已失效，按上一条重新登录。
