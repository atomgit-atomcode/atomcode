# 公开签名 stub 的崩溃修复

基线：release/v5.2.3，2d8ef266f。tracked 工作区修改前为空。

用户 Linux 自编译 94ff58a2c 的 panic.log 确认调用了公开 atomcode-codingplan-crypto 的 sign_v1；其 ALGORITHM_VERSION 为 0，入口刻意 panic。feature 开启不等于官方实现已覆盖，但 auth 的 signer_available 原先只按 feature 返回 true。

## 边界与失败语义

签名能力由 auth/gateway_crypto 持有。签名可用必须同时满足 feature 开启和非零算法版本。RealSigner::sign 在调用底层实现前再次检查版本，stub 返回既有 SignError::Unavailable；provider_factory 继续使用既有 SourceBuildGatewayUnsupported 错误，不新增未签名网关 fallback。

不改公开或私有签名实现、不修改签名算法和返回类型、不改变真实签名覆盖的行为、不改变配置或会话格式。覆盖所有经同一 auth 签名入口的 driver。

## 验证

普通源码构建、从公开 Git archive 创建的 stub 检出且开启 feature、本机真实签名覆盖且开启 feature，分别运行 auth lib 测试。测试不使用真实凭据或官方网关。公开 stub 环境直接签名返回 Unavailable，能力检测为 false；真实实现算法版本非零时能力检测为 true。
