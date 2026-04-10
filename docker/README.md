# AtomCode Daemon Docker 镜像

用于部署 AtomCode Daemon 后台服务的 Docker 镜像。

## 构建镜像

首先运行 release 脚本生成 Linux 二进制文件：

```bash
./scripts/release.sh
```

然后构建 Docker 镜像：

```bash
docker build -t atomcode-daemon:v4.2.0 -f docker/Dockerfile-Daemon .
```

## 运行容器

### 基本运行

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  atomcode-daemon:v4.2.0
```

### 挂载配置文件

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  -v /path/to/config.toml:/root/.atomcode/config.toml \
  atomcode-daemon:v4.2.0
```

### 挂载项目目录

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  -v /path/to/config.toml:/root/.atomcode/config.toml \
  -v /path/to/project:/workspace \
  atomcode-daemon:v4.2.0
```

### 传递环境变量

```bash
docker run -d --name atomcode-daemon \
  -p 13456:13456 \
  -e ANTHROPIC_API_KEY=your-api-key \
  -v $(pwd)/config.toml:/root/.atomcode/config.toml \
  atomcode-daemon:v4.2.0
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
