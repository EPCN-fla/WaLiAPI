# WaLiAPI Web 管理面板

Docker 部署不再依赖 VNC/noVNC 桌面转发：管理面板以纯 Web 形式内嵌在 waliapi 二进制中，浏览器直接访问即可。页面与桌面端 1:1 一致（复用同一套 React 源码）。

## 架构

```
浏览器
  │ HTTPS
  ▼
nginx (docker-compose, :8443)
  │ /* → waliapi:8777
  ▼
axum (内置 HTTP 服务, :8777)
  ├─ /v1/*              LLM 网关协议
  ├─ /api/kb/* /api/wiki/* /mcp   知识库 / Wiki / MCP
  ├─ /admin/api/*       管理 REST API（需认证）
  │   ├─ /auth/login | logout | check | change-password
  │   ├─ /invoke        与 Tauri invoke 语义 1:1 的命令入口
  │   └─ /events        SSE 事件桥（KB/Wiki 进度事件）
  └─ /*                 rust-embed 内嵌的 Web 静态资源（SPA fallback）
```

- 前端：`web/` 是 pnpm workspace 子包，通过 vite alias 复用 `src/` 全部页面组件，仅把 `@tauri-apps/api/*` 替换为 HTTP fetch / EventSource 实现。
- 后端：`/admin/api/invoke` 按命令名直接分发到现有 commands 函数，桌面端与 Web 端共用同一套业务逻辑。
- 认证：SQLite `admin_users` 表 + argon2id 密码哈希；会话为内存 Bearer token（7 天），同时写 `waliapi_admin_token` Cookie 供 SSE 使用；进程重启会话失效。

## 构建与部署

### 单容器

```bash
docker build -t waliapi:web .
docker run -d -p 8777:8777 -v waliapi-data:/data --name waliapi waliapi:web
```

访问 `http://localhost:8777`。

### docker-compose（HTTPS）

```bash
cd docs/docker
docker compose up -d
```

访问 `https://<host>:8443`（证书放在 `docs/docker/certs/`）。

## 首次登录与改密

1. 首次启动自动生成账号 `admin` + 16 位随机临时密码，打印到容器日志：

   ```
   docker logs waliapi | grep -A3 初始临时密码
   ```

   同时写入 `/data/INITIAL_PASSWORD`（卷已持久化）：

   ```
   docker exec waliapi cat /data/INITIAL_PASSWORD
   ```

2. 浏览器打开面板，用临时密码登录，系统强制跳转修改密码页（新密码至少 8 位）。
3. 改密后进入仪表盘，正常使用全部页面。

## 与桌面版的差异

| 桌面功能 | Web 版行为 |
|---|---|
| 应用更新检查 / 一键更新 | 不暴露（Web 版随镜像升级），无入口 |
| auth.json 导入（文件对话框） | 浏览器文件选择器上传内容导入 |
| auth.json 导出（保存对话框） | 浏览器直接下载 |
| 渠道导入 / 导出 | 同上：浏览器上传 / 下载 |
| 打开配置文件夹 | 容器内应用均不可用，置灰 |
| 系统托盘 / 开机自启 / 关闭到托盘 | 设置页隐藏相关开关 |
| OAuth 登录（打开系统浏览器） | 在服务器侧打开浏览器通常不可用，建议用 auth.json 导入 |

## 本地开发

```bash
# 终端 1：后端（内嵌已构建的 web/dist）
cd web && pnpm build && cd ..
cargo run --manifest-path src-tauri/Cargo.toml --features embed-web

# 终端 2：web dev server（代理 /admin/api、/api、/v1、/mcp 到 8777）
cd web && pnpm dev
```

访问 `http://localhost:1420`。桌面端开发流程（`pnpm tauri dev`）不受影响。

## 故障排查

- **无法登录 / 忘记密码**：`docker exec -it waliapi sqlite3 /data/waliapi.db "DELETE FROM admin_users;"`，重启容器后重新生成临时密码（数据库实际路径为 Tauri 应用数据目录，容器内即 `/data` 下的应用目录）。
- **SSE 进度不更新**：确认经 nginx 访问；`docker/nginx.conf` 已对 `/admin/api/` 关闭缓冲。浏览器直连 8777 时无此问题。
- **静态资源 404 / 白屏**：镜像是用 `--features embed-web` 构建的；本地 `cargo run` 需先 `cd web && pnpm build` 生成 `web/dist`，否则 `/` 返回 404。
- **会话频繁失效**：会话存于内存，进程重启后需重新登录，属预期行为。
