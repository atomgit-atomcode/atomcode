# AtomGit 仓库查询的范围与反馈

保留 /user/repos 的当前账号列表语义，不添加未经核实的公开搜索接口。空列表说明查询范围并引导直接 view。limit=0 拒绝，避免把客户端截断误报为账号空列表。

view 接受 owner+repo 或 gitcode.com / atomgit.com 普通代码仓库 URL（含 issues 等常见页面），两者互斥。URL 只提取标识，网络请求仍发往已配置的 API；不访问传入的 host。多级模型路径和其他 host 给出明确支持范围，不推断其 issue 能力。其他写操作仍要求显式 owner+repo。

错误由现有共享 AtomGit client 负责：401 保留重新登录引导，403 提示账号权限/token scope，404 提示不存在、不可访问或 API 不支持等可能性，不猜测替代路径。工具输出由终端/WebUI 共用，沿用模型工具的英文协议；站点说明中英文同步。

不改变 runtime、持久化、凭据管理或审批风险等级。验证覆盖个人列表为空后仍可直接 view、URL 定位、多级/歧义参数拒绝及 403/404 区分；运行 AtomGit 相关测试、格式检查与跨 crate 编译门禁。
