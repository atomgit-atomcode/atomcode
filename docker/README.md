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

## NAS 部署(群晖 / 威联通等)

使用仓库自带的 `docker-compose.yml` 可在 NAS 上实现一键常驻部署,配合 WebUI 通过手机随时修改代码、发起调试任务。

### 准备工作

1. 构建 Daemon 镜像。本机为 x64 架构时直接构建:

   ```bash
   ./scripts/release.sh
   docker build -t atomcode-daemon:local -f docker/Dockerfile-Daemon .
   ```

   > **多架构(amd64 + arm64)镜像**:使用 `docker/build-multiarch.sh` 一键构建并推送同时支持 x64 与 ARM64 的镜像(群晖/威联通 ARM 机型同样适用,详见下方「ARM64 架构说明」)。

2. 在 NAS 上创建一个部署目录(例如 `docker/atomcode/`),将 `docker/docker-compose.yml` 与 `docker/config-example.toml` 复制进去,并把 `config-example.toml` 重命名为 `config.toml`、填入你的 Provider 与 API Key:

   ```toml
   default_provider = "openrouter"

   [providers.openrouter]
   type = "openai"
   api_key = "your-api-key"
   model = "xxx"
   base_url = "https://openrouter.ai/api/v1"
   context_window = 16000
   ```

3. 创建项目目录 `projects/`(将挂载为容器内 `/workspace`)。

### 启动

```bash
cd docker/atomcode
docker compose up -d
```

- 使用 `restart: unless-stopped`,NAS 重启后容器会自动拉起,崩溃也会自愈。
- 端口映射为 `13456:13456`,如与本机其他服务冲突可修改左侧宿主机端口(例如 `23456:13456`)。

### 群晖 Container Manager 操作步骤

1. 打开「Container Manager」→「项目」→「新增」。
2. 项目名称填写 `atomcode`,路径选择包含 `docker-compose.yml` 的部署目录。
3. 来源选择「使用 docker-compose.yml」,点击「下一步」后 Container Manager 会自动拉取/构建并启动。
4. 在「容器」页确认 `atomcode-daemon` 状态为「运行中」,启动策略为「如果异常则自动重启」。

### 威联通 Container Station 操作步骤

1. 打开「Container Station」→「应用程序」→「创建」→「创建应用程序」。
2. 将 `docker-compose.yml` 内容粘贴到编辑器中,点击「验证」通过后「创建」。
3. 首次使用需先在「映像」页构建/导入 `atomcode-daemon:local` 镜像。

### 验证与手机访问

```bash
# 在 NAS 本机或局域网内验证
curl http://<NAS-IP>:13456/
```

- 局域网内手机访问:打开浏览器输入 `http://<NAS-IP>:13456` 即可进入 WebUI(需配合 WebUI 远程访问面板)。
- **注意**:Daemon 默认绑定 `127.0.0.1` 且无内置鉴权,仅适合内网信任环境。如需公网访问,务必使用反向代理 + HTTPS + token 鉴权(如 Caddy / Nginx + 自签证书),或内网穿透方案(如 Tailscale / 蒲公英),切勿直接暴露端口到公网。

### ARM64 架构说明

`Dockerfile-Daemon` 已支持多架构:通过 `docker buildx` 的 `TARGETARCH` 自动选择对应产物(amd64 → `linux-x64` 二进制,arm64 → `linux-arm64` 二进制),一次构建即可产出同时支持 x86 与 ARM64 的镜像。

- **一键构建并推送多架构镜像**(适用于群晖/威联通 ARM 机型、树莓派等):

  ```bash
  docker/build-multiarch.sh                      # 默认镜像名 atomcode-daemon:v<版本>,构建并推送
  docker/build-multiarch.sh myrepo/atomcode:v1   # 指定镜像名
  BUILD_ONLY=1 docker/build-multiarch.sh         # 仅本地构建,不推送
  ```

- 前置条件:安装 musl 交叉编译工具链(`brew install FiloSottile/musl-cross/musl-cross`),脚本会自动调用 `scripts/release.sh` 产出 x64 + arm64 两种 `atomcode-daemon` 产物后交给 buildx。
- 官方多架构镜像与自动化构建正在推进中,详见 issue #1421 / #1431。
