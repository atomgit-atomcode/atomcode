# Termux DNS 连接故障

Linux 静态 musl 版本通常从 `/etc/resolv.conf` 获取 DNS 配置。Android Termux 中该路径可能不可读，即使 Termux 自带的 curl 能连接，AtomCode 仍可能出现 `dns error: failed to lookup address information`。

AtomCode 在 Linux 上检测到 `TERMUX_VERSION` 或标准 Termux `PREFIX` 路径，且 `/etc/resolv.conf` 不可读时，登录、CodingPlan 和模型请求会使用 `$PREFIX/etc/resolv.conf` 中配置的 DNS。`PREFIX` 必须是绝对路径；配置必须可读、大小不超过 64 KiB，并包含有效的 `nameserver`。无需额外开启配置项，也不会自动修改文件或设置公共 DNS。

检查环境与连通性：

```sh
ls -l /etc/resolv.conf "$PREFIX/etc/resolv.conf"
curl -I --connect-timeout 10 https://llm-api.atomgit.com
curl -I --connect-timeout 10 https://api.gitcode.com
```

如果备用配置无效或不可读，请修正本地 DNS 配置，使用当前网络允许的解析服务器。解析器会在新的域名查询时重新读取文件。原有代理和 TLS 证书验证规则保持生效；代理负责解析目标域名时，不会强制改为本地 DNS。

普通 Linux、有可读系统配置的环境，以及原生 Android 构建继续使用原有系统解析器。此适配不改变外部子进程（如 Git、curl）的 DNS 行为，也不意味着所有工具都已获得 Termux 原生支持。

旧版本仍可在安装了 proot 且备用配置有效时临时运行：

```sh
proot -b "$PREFIX/etc/resolv.conf:/etc/resolv.conf" atomcode
```
