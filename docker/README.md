# AtomCode Docker 镜像

本目录包含两种 Docker 镜像：

- **Dockerfile-Daemon** - 用于部署 AtomCode Daemon 后台服务
- **Dockerfile-TUI** - 用于在 macOS/Windows 上体验 Linux 版本的 AtomCode TUI

---

## AtomCode TUI 镜像

用于在 macOS 或 Windows 上体验 Linux 版本的 AtomCode 终端界面。

### 构建镜像

```bash
# 1. 先编译 Linux 版本（需要 musl 交叉编译工具）
brew install FiloSottile/musl-cross/musl-cross
./scripts/release.sh

# 2. 构建 Docker 镜像
docker build -t atomcode -f docker/Dockerfile-TUI .
```

### 运行容器

```bash
# 基本运行
docker run --rm -it atomcode

# 挂载配置和项目目录
docker run --rm -it \
  -v ~/.atomcode:/root/.atomcode \
  -v $(pwd):/workspace \
  atomcode

# 指定工作目录
docker run --rm -it \
  -v ~/.atomcode:/root/.atomcode \
  -v /path/to/project:/workspace \
  atomcode

# 传递环境变量（API Key）
docker run --rm -it \
  -e ANTHROPIC_API_KEY=your-api-key \
  -v ~/.atomcode:/root/.atomcode \
  atomcode
```

> **注意**: TUI 模式需要 `-it` 参数来启用交互式终端。

---

## AtomCode Daemon 镜像

## 构建镜像

首先运行 release 脚本生成 Linux 二进制文件：

```bash
./scripts/release.sh
```

然后构建 Docker 镜像：

```bash
docker build -t atomcode-daemon:v5.0.3 -f docker/Dockerfile-Daemon .
```

### 多架构构建（amd64 + arm64）

`Dockerfile-Daemon` 支持多架构：通过 buildx 的 `TARGETARCH` 自动选择对应产物（amd64 → `linux-x64` 二进制，arm64 → `linux-arm64` 二进制），一次构建即可产出同时支持 x86 与 ARM64（群晖/威联通 ARM 机型、树莓派等）的镜像。

一键构建并推送多架构镜像：

```bash
docker/build-multiarch.sh                      # 默认镜像名 atomcode-daemon:v<版本>，构建并推送
docker/build-multiarch.sh myrepo/atomcode:v1   # 指定镜像名
BUILD_ONLY=1 docker/build-multiarch.sh         # 构建并加载当前主机架构，不推送
```

脚本会自动调用 `scripts/release.sh`（`ATOMCODE_INCLUDE_DAEMON=1`）交叉编译 x64 + arm64 两种 daemon 产物后交给 buildx。前置条件：安装 musl 交叉编译工具链（`brew install FiloSottile/musl-cross/musl-cross`）。

### 推送到华为云 SWR

华为云 SWR 基础版不支持 OCI 规范的镜像格式。如果你使用的是较新版本的 Docker（BuildKit），需要添加 `--provenance=false` 参数：

```bash
# 标记镜像
docker tag atomcode-daemon:v5.0.3 swr.cn-north-4.myhuaweicloud.com/gitcode-be/atomcode-daemon:v5.0.3

# 使用 buildx 构建并推送（推荐）
docker buildx build --provenance=false --platform linux/amd64 -t swr.cn-north-4.myhuaweicloud.com/gitcode-be/atomcode-daemon:v5.0.3 --push -f docker/Dockerfile-Daemon .

# 或者先构建再推送
docker build --provenance=false -t swr.cn-north-4.myhuaweicloud.com/gitcode-be/atomcode-daemon:v5.0.3 -f docker/Dockerfile-Daemon .
docker push swr.cn-north-4.myhuaweicloud.com/gitcode-be/atomcode-daemon:v5.0.3
```

> **注意**: 如果不添加 `--provenance=false`，推送时会报错: `Invalid image, fail to parse 'manifest.json'`

## 运行容器

### 基本运行

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  atomcode-daemon:v5.0.3
```

### 挂载配置文件

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  -v /path/to/config.toml:/root/.atomcode/config.toml \
  atomcode-daemon:v5.0.3
```

### 挂载项目目录

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  -v /path/to/config.toml:/root/.atomcode/config.toml \
  -v /path/to/project:/workspace \
  atomcode-daemon:v5.0.3
```

### 传递环境变量

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  -e ANTHROPIC_API_KEY=your-api-key \
  -v $(pwd)/config.toml:/root/.atomcode/config.toml \
  atomcode-daemon:v5.0.3
```

## 验证服务

```bash
# 测试 API
curl http://localhost:13456/

# 查看日志
docker logs atomcode-daemon
```

## 常用命令

```bash
docker start atomcode-daemon     # 启动
docker stop atomcode-daemon      # 停止
docker restart atomcode-daemon   # 重启
docker rm -f atomcode-daemon     # 删除
docker logs -f atomcode-daemon   # 查看日志
```

---

## NAS 一键部署（群晖 / 威联通等）

项目提供 `docker-compose.yml`，适合在 NAS / 服务器上常驻运行 daemon：崩溃自动重启（`restart: unless-stopped`）、健康检查、运行数据与项目目录持久化，配合手机 GitCode App（`/app`）、WebUI 或 daemon HTTP API 随时远程调试。

### 快速开始

```bash
# 1. 准备配置、数据与项目目录（daemon 以 root 运行）
mkdir -p docker/data docker/projects
cp docker/config-example.toml docker/config.toml   # 填入 provider / api_key

# 2. 首次使用需先编译 Linux 二进制（生成 dist/ 目录）再构建镜像
./scripts/release.sh

# 3. 构建并后台启动
docker compose -f docker/docker-compose.yml up -d

# 4. 验证
curl http://localhost:13456/health
```

> **安全提示**：daemon 暴露聊天、文件编辑、工具执行等敏感端点，镜像默认无鉴权。
> compose 端口默认仅绑定 `127.0.0.1`（本机访问）。如需从局域网/NAS 访问，用
> `BIND_ADDR` 显式放开，并务必先部署反向代理 + TLS + token 鉴权：
>
> ```bash
> BIND_ADDR=0.0.0.0 docker compose -f docker/docker-compose.yml up -d
> ```

`data/` 会持久化会话、日志、认证、插件等完整运行状态；`config.toml` 会以只读方式覆盖挂载到容器，`projects/` 会挂载为 `/workspace`。

### 群晖 Container Manager

1. 打开「Container Manager」→「项目」→「新增」。
2. 项目名称填写 `atomcode`，路径选择完整 AtomCode 仓库中的 `docker/` 目录；默认 compose 使用 `..` 作为镜像构建上下文，因此不能只复制单个 compose 文件。
3. 来源选择「使用 docker-compose.yml」，确认后启动项目。
4. 在「容器」页确认 `atomcode-daemon` 状态为运行中。

### 威联通 Container Station

1. 打开「Container Station」→「应用程序」→「创建应用程序」。
2. 选择完整仓库 `docker/` 目录中的 `docker-compose.yml`，验证通过后创建。
3. 若使用本地镜像，先在「映像」页构建或导入对应架构的 daemon 镜像。

### 手机访问

默认端口只绑定宿主机 `127.0.0.1`。仅在可信局域网中需要手机直连时，使用：

```bash
BIND_ADDR=0.0.0.0 docker compose -f docker/docker-compose.yml up -d
curl http://<NAS-IP>:13456/health
```

随后可通过 `http://<NAS-IP>:13456` 访问。公网访问必须使用反向代理、HTTPS 与 token 鉴权，或使用 Tailscale 等可信内网方案，切勿直接暴露 daemon 端口。

### 常用 compose 命令

```bash
docker compose -f docker/docker-compose.yml logs -f   # 查看日志
docker compose -f docker/docker-compose.yml restart   # 重启
docker compose -f docker/docker-compose.yml down      # 停止并移除容器
```

### 使用预构建镜像

`docker-compose.yml` 默认从完整仓库本地构建。如果 NAS 上只有独立部署目录，必须先导入本地镜像或使用镜像仓库，并把 `build` 段替换为 `image` 段（见文件内注释）。

ARM64 与多架构构建方式见上方「多架构构建」章节。官方多架构镜像与自动化构建仍在推进中，详见 issue #1421 / #1431。
