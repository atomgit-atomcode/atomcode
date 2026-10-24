# WebUI 对话节点导航轨

基线 release/v5.2.5 @ ae4433c75。已有 transport、provider、MCP、team 等并行改动，保留且不纳入。使用既有 WebUI 样式与 frontend-design 的精简导航原则；参考本地 DeepSeek Harness TurnNavigator 的独立滚动、固定节点间距、虚拟窗口、可访问标签与 reduced-motion 处理，不引入其 React/虚拟化依赖。

仅修改 WebUI：消息状态为当前对话唯一 owner。以实际渲染的用户消息索引作为锚点，摘要来自用户文本和助手可见文本，不读取 reasoning 或工具输出。不通过用户文本猜 turn_id；重复提问、steer、快照替换均按当前索引重新推导。

右侧绝对定位窄轨默认收起，不改变主内容列宽；展开后以浮层显示摘要。轨内固定高度虚拟窗口，键盘与触屏可用。定位先释放浏览器恢复与自动跟随，再滚动并短暂高亮；用户回到底部恢复既有跟随。

现有 transcript API 没有可与消息 DTO 直接关联的稳定 turn_id；撤回/压缩回合也可能不在当前 DOM。导航明确区分当前对话与完整历史：当前节点定位原消息，完整历史节点以真实 turn_id 打开并定位只读记录，保留 undone 标记。不虚构已压缩消息的 DOM 锚点，不把历史记录注入运行中对话。

完整历史按需拉取，请求随会话切换中止；当前流结束后更新。移动端默认只露出入口，展开作为受限高度浮层。暂不加入 token 用量、关键字过滤或快捷 rewind。

验证：回合推导、重复输入、保留历史来源、流式增长、撤回/快照重建、虚拟窗口及锚点映射单测；WebUI typecheck/全量测试/构建，浏览器验证桌面与手机宽度、定位和流式跟随。

## 实现与验证结果

- 新增 TurnNavRail 和纯数据 turnNavigation；轨内可见窗口外最多保留一个获得焦点的节点，避免滚动令键盘焦点丢失。摘要只处理有界文本，长工具/助手输出不参与整段格式化。
- 当前消息复用搜索已有的 DOM ref。完整历史按需获取；选择记录复用轨内已经获取的 TranscriptTurn，仅挂载该轮，显式点「显示全部历史回合」才回到原有全历史列表。保留回合/撤回标记与现有来源一致；pending steer 使用既有 confirmed 队列判定。
- 平滑滚动中保留显式跳转/回底部意图，中间帧不能错误开启或关闭自动跟随；手动滚动优先。历史跳转以项目/会话作用域隔离，旧请求及待执行定位在切换/用户手势后取消。快照和 rewind 即使回合数量未变，也会在非流式时刷新历史；流式中不逐 token 拉取历史。
- typecheck、全部 314 项 Node 测试、生产构建通过。构建保留现有大 bundle 提示；没有新增前端依赖或后端改动。
- 本地 Chromium 无头浏览器以 80 回合、撤回记录、保留历史、第二会话和受控 SSE 夹具验证：默认收起、窗口化 DOM、当前消息跳转和高亮、撤回记录只读定位、复用记录不重复拉取、流式阅读保持、回底部后继续跟随、390px 手机宽度无横向溢出/自动收起、会话切换清除旧锚点。无页面异常。内置浏览器连接不可用，使用本地 Puppeteer 做同等页面检查，未访问真实用户会话。
- 截图：/tmp/atomcode-turn-nav-desktop.png、/tmp/atomcode-turn-nav-mobile-open.png、/tmp/atomcode-turn-nav-mobile.png。临时验证脚本：/tmp/atomcode-turn-nav-browser.mjs。
- cargo fmt --all、cargo fmt --all -- --check、git diff --check 通过。本次仅 WebUI，未修改 Rust、公共协议或持久化格式；实施阶段未重跑 Rust 门，提交前已补齐 workspace 编译门（见下）。


## 提交前审计

审计基线 release/v5.2.5 @ c97c78656042d2233f8174e2995f170e0d4d95f0。检查当前/归档记录边界、节点窗口与键盘焦点、摘要转义与长度限制、请求取消、来源标记、跨会话 refs 及滚动跟随。补充项目+会话作用域变化时清除导航状态；连续历史跳转只对已经更新到当前选中 turn_id 的记录执行定位。补充缺少 ResizeObserver 时的 window resize 降级，保持已有旧浏览器兼容策略。

全部 314 项单测、typecheck、生产构建通过；浏览器补充连续快速历史跳转、无 ResizeObserver 降级、流式暂停/恢复跟随、手机收起及会话切换验证通过。cargo fmt --all / --check、git diff --check 和 bash gates/compile.sh（workspace/all-targets、feature 编译、两项负向自检）通过。未发现剩余阻塞项；主时间线仍未虚拟化，Windows 真机/中继链路未在本次实测。

Cargo.lock 的三个禁止包均为 0；工作区并行的调度等代码与相关 lock 变化不属于本任务，不纳入提交。仅提交导航轨、消息定位接入、翻译/样式、单测及本文档。
