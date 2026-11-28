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

## 时好时坏：Termux 默认配置是公共 DNS

Termux 的 `resolv-conf` 包默认把 `$PREFIX/etc/resolv.conf` 写成：

```
nameserver 8.8.8.8
nameserver 8.8.4.4
```

AtomCode 不会自己选择公共 DNS，但使用上面的备用配置时，实际就是在用这两个服务器。部分网络（常见于家庭宽带）访问它们不稳定，表现为同一台设备换个网络就好、或者跑一段时间后反复出现 `dns error`。Termux 自带的 curl 等程序走 Android 系统解析器，使用当前网络下发的 DNS，所以不受影响——「curl 能连、AtomCode 不能」多半是这个原因。

把它改成当前网络的 DNS 即可，通常是路由器地址（Android「WLAN → 当前网络详情」中可看到网关或 DNS，例如 `192.168.1.1`），或运营商提供的 DNS：

```sh
cp "$PREFIX/etc/resolv.conf" "$PREFIX/etc/resolv.conf.bak"
printf 'nameserver 192.168.1.1\n' > "$PREFIX/etc/resolv.conf"   # 换成你网络的 DNS 地址
```

修改立即对新的查询生效，无需重启 AtomCode。经常切换网络时，需要按当前网络调整；该文件是 Termux 的配置文件，`pkg upgrade` 不会静默覆盖你的修改。

普通 Linux 与有可读系统配置的环境继续使用原有系统解析器。目前没有发布链接 Android 系统 C 库的原生构建，Termux 中运行的是 Linux 静态版本，因此无法直接使用 Android 系统的 DNS。此适配不改变外部子进程（如 Git、curl）的 DNS 行为，也不意味着所有工具都已获得 Termux 原生支持。

旧版本仍可在安装了 proot 且备用配置有效时临时运行：

```sh
proot -b "$PREFIX/etc/resolv.conf:/etc/resolv.conf" atomcode
```
