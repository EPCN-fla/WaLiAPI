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
- 局域网 UI: `http://192.168.123.222:6080/vnc.html?autoconnect=1&resize=scale`

## 备注

- UI 默认不对公网开放。
- 如果需要临时打开 UI，把 `docker-compose.yml` 里的 `WALIAPI_ENABLE_UI` 改成 `"1"`，再执行 `docker compose up -d --force-recreate`。
- 如果 NAS 的 `443` 已被占用，先改宿主端口映射，再同步调整路由器或穿透配置。
