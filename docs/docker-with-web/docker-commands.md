# 常用指令与访问地址

## 常用指令

```bash
docker build --build-arg NODE_IMAGE=docker.m.daocloud.io/library/node:22-bookworm --build-arg RUNTIME_IMAGE=docker.m.daocloud.io/library/debian:bookworm-slim --build-arg DEBIAN_MIRROR=http://mirrors.aliyun.com/debian --build-arg DEBIAN_SECURITY_MIRROR=http://mirrors.aliyun.com/debian-security -t waliapi:local .
```

```bash
docker save waliapi:local -o waliapi.tar
```

```bash
docker load -i waliapi.tar
```

```bash
docker compose up -d
```

```bash
docker compose up -d --force-recreate
```

```bash
docker compose logs -f waliapi nginx
```

```bash
docker compose down
```

## 访问地址

- 公网 API: `https://fla1662.cc.cd/health`
- 局域网 API: `https://192.168.123.222:8443/health`
- Web 管理面板: `https://192.168.123.222:8443/`（或直连 `http://192.168.123.222:8777`）

## 备注

- 管理面板随 API 同源开放，无独立 UI 端口；首次登录的临时密码见 `docker logs waliapi` 或容器内 `/data/waliapi.xiaofuge.cn/INITIAL_PASSWORD`。
- 如果 NAS 的 `443` 已被占用，先改宿主端口映射，再同步调整路由器或穿透配置。
