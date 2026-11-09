# WebUI 工作区操作 Implementation Plan

**Goal:** 在工作区 + 前增加菜单，提供置顶、清空会话、移除工作区。

**Architecture:** daemon 是工作区展示偏好的唯一持久化 owner，JSON 旁挂只记录 pinned/hidden，不改会话格式或 runtime。GET /projects 带可选标记；PATCH 更新标记；已有前端 DELETE /projects/:hash 改为隐藏工作区的实际 handler，绝不调用删除 catalog 目录的旧 helper。工作区原目录与历史保留。WebUI 所有选择器共用这些事实；隐藏项目在全局会话和搜索中仍可访问，重新添加目录可恢复展示。

**Tech Stack:** Rust/axum、原生 session 删除接口、Preact/TypeScript。

## 实施步骤

1. 新增 daemon project_preferences 模块：验证 bucket、跨进程文件锁串行读改写、同目录临时文件原子替换；坏文件或写失败明确失败。新接口只处理 UI 偏好，不触碰 live runtime。增加缺省、更新、重读、目录不变和非法输入测试。
2. GET /projects 合并偏好；PATCH /projects/:hash 更新 pinned/hidden；DELETE 仅设置 hidden。新增 WebUI API 与排序/筛选 helper，置顶优先、其次当前选中工作区；隐藏当前项目也不能被合成占位行重新插回。
3. WebUI 工作区菜单使用现有 portal/弹窗样式；移除和清空均有语义清楚的确认、busy 与错误反馈。成功后才修改列表。目录选择器不再吞错，重新打开目录恢复隐藏标记。
4. 清空使用完整项目列表、现有逐会话 DELETE 和逐条成功回调；先检查本页运行中会话，后端每项继续执行租约保护。中途失败不假装全成功，可见已删除数并可重试；不递归删除项目目录。对后台/其他客户端占用沿用原 SESSION_IN_USE 语义。
5. 测试持久偏好及失败、隐藏/置顶排序、单项删除失败时的部分结果、API 行为；运行受影响测试、WebUI 类型与构建、compile gate、fmt 和 diff 检查。

## 对照

deepseek-harness 的 WorkspaceRegistry.delete 只移除工作区实体、保留目录及日志，界面确认失败可重试。AtomCode 保留自己的按目录 bucket 会话模型，不引入第二会话 owner 或重建该产品的 registry 架构。

## 风险与限制

清空会话是逐项操作，并非跨会话原子事务；后端忙碌/租约冲突可能在部分成功后发生，必须报告部分成功。运行中新产生的会话不属于确认时读取的快照，不承诺强制全部删除。偏好更新失败不能当成功，文件损坏不能静默覆盖。

## 审查补充

服务端确认成功的 pinned/hidden 更新通过本页事件先写入展示状态，再刷新目录清单。后续 GET 失败保留已确认偏好并显示重试入口，避免已移除的当前目录被合成占位行重新插回；单测和桌面/手机浏览器测试覆盖这一失败顺序。
