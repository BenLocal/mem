# mem：npm 发布 MCP、Docker 部署服务端、GitHub Actions 发布流水线

日期：2026-03-22  
状态：Accepted

## 1. 目标

1. 将 **`integrations/mem-mcp`** 发布到 **npm**，便于 `npx` / 全局安装，无需克隆仓库即可跑 MCP。  
2. 将 **mem HTTP 服务**（Rust 二进制）做成 **Docker 镜像**，可在服务器或本地用容器运行，数据通过 volume 持久化 DuckDB。  
3. 在 **GitHub Actions** 中增加**发布向**工作流（与现有 PR CI 区分）：在打 **`v*.*.*` tag** 时构建并推送镜像，并发布 npm 包。

**部署模型（已定）**：Docker **仅承载 mem**；MCP 仍在客户端运行，通过 `MEM_BASE_URL` 指向容器内或负载均衡后的 mem。不把「远程 stdio MCP」作为本期目标。

## 2. 非目标

- 本期不把 Rust crate `mem` 发布到 **crates.io**（可后续单独立项）。  
- 本期不把 MCP 改为 **HTTP/SSE 服务端** 供公网直连（若需要，另开 spec）。  
- 不在镜像内内置生产级 **TLS 终止**（由反向代理或 K8s Ingress 处理）。  
- 不解决多副本同时 **写同一 DuckDB 文件** 的并发模型（保持单写者实例）。

## 3. npm 包（mem-mcp）

### 3.1 包名与可见性

- 推荐 **scoped** 包名：`@<org-or-user>/mem-mcp`（具体字符串在实现时与仓库 owner 对齐）。  
- 发布前移除 `package.json` 的 `"private": true`。  
- 使用 `files`（或 `.npmignore`）限制发布内容：**`dist/`**、`README.md`、`package.json`、可选 `LICENSE`；避免把未构建的 `src` 作为运行依赖暴露（若希望发布源码供调试，可显式列入并文档说明）。

### 3.2 构建与入口

- 增加 **`prepack`**（或 `prepare`）：`npm run build`，保证 `npm publish` 生成的 tarball 内含 **`dist/index.js`**。  
- 增加 **`bin`** 字段：例如 `"mem-mcp": "dist/index.js"`，文件首行需兼容 Node 的 shebang（`#!/usr/bin/env node`）——在实现计划中明确由构建脚本或 `index.ts` 顶部写入。  
- **engines**：维持 `node >= 20`。  
- **版本号**：与 git tag / 变更日志策略在实现计划中约定（建议与 Docker 镜像 tag 共用 `v0.x.y`）。

### 3.3 文档

- `integrations/mem-mcp/README.md`：安装方式、`mcp.json` 示例（`npx @scope/mem-mcp` 与 `command`/`args`）、环境变量、与 Docker 中 mem 的 URL 示例。

## 4. Docker（mem 服务端）

### 4.1 镜像内容

- **多阶段构建**：Stage 1 使用官方 `rust:…-bookworm`（或 `rust:slim` + 必要依赖）`cargo build --release`；Stage 2 使用 **`debian:bookworm-slim`**（或与 builder 相同 libc 的 slim 镜像）复制二进制。  
- **DuckDB bundled**：需验证 release 链接选项（通常静态或带齐动态库）；若 slim 镜像缺库，在实现阶段用 `ldd` 与一次 `docker run` 冒烟修齐。  
- **入口**：默认执行 `mem`（或 `COPY` 后的路径），监听 **`BIND_ADDR`**（默认保持与现有一致，如 `0.0.0.0:3000` 便于容器外访问）。

### 4.2 数据与配置

- **环境变量**：沿用现有 `MEM_DB_PATH`、`BIND_ADDR`、embedding 相关变量；文档说明在 Docker 中 **`MEM_DB_PATH` 必须指向挂载卷**（例如 `/data/mem.duckdb`）。  
- **健康检查**：`HEALTHCHECK` 使用 `curl -f http://127.0.0.1:3000/health`（若 slim 无 curl，则安装 `curl` 或换 `wget`）。  
- **暴露端口**：`EXPOSE 3000`（若 `BIND_ADDR` 可改端口，文档同步）。

### 4.3 文件位置

- 建议：`Dockerfile` 放在仓库根目录，**context** 为仓库根，以便 `COPY` 整个 Rust 项目。  
- 可选：`docker-compose.yml` 示例（`volumes` + `ports`）放在 `deploy/` 或根目录，实现计划再定。

## 5. GitHub Actions

### 5.1 与现有 CI 的关系

- **保留** `.github/workflows/ci.yml`：`push`/`pull_request` 跑 `cargo fmt/clippy/test` 与 `integrations/mem-mcp` 的 `npm ci/test/build`。  
- **新增** 单独 workflow，例如 `.github/workflows/release.yml`（名称实现时可调整）。

### 5.2 触发条件

- **`push` tags**：匹配 `v*.*.*`（或 `v*`）；或  
- **`workflow_dispatch`**：可选手动输入版本 / 是否发 npm / 是否推镜像（实现时二选一或组合）。

### 5.3 Jobs 设计

| Job | 职责 | 主要 secret / 权限 |
|-----|------|---------------------|
| **publish-npm** | `cd integrations/mem-mcp`，`npm ci`，`npm publish --access public`（scoped 时） | `NPM_TOKEN`（npm automation token） |
| **docker-mem** | `docker build` + push 到 **GHCR** `ghcr.io/<owner>/mem:<tag>` | `GITHUB_TOKEN`（`packages: write`）或 PAT |

- **并发**：两 job 可并行；若希望「同一 tag 必须两者都成功」，用 workflow 级别策略或最终 summary job。  
- **npm**：使用 `actions/setup-node` + 注册 `registry-url: https://registry.npmjs.org`。  
- **Docker**：`docker/login-action` 对 `ghcr.io`；标签同时打 **`latest`**（仅默认分支 tag 时）的策略在实现计划中写清，避免误覆盖。

### 5.4 版本与 tag

- 推荐：**以 git tag 为真源**（如 `v0.2.0`），npm 版本在发布前用脚本与 `package.json` 对齐，或手动 bump 后 tag（实现计划写死一种流程）。  
- **Cargo.toml** `version` 可与镜像 tag 文档对齐，本期不要求自动同步 crates.io。

## 6. 安全提示

- **npm token**、**GHCR**：仅存 GitHub Actions secrets，不入库。  
- mem 默认无鉴权；公网部署必须配 **反向代理 + 认证** 或仅内网访问——本 spec 只文档提示，不实现网关。

## 7. 验收标准

- 在干净环境中：`npm install -g @scope/mem-mcp`（或 `npx`）可启动 stdio MCP，并能对 Docker 中 mem 调用 `mem_health` / `memory_search`。  
- `docker run` 挂载 volume 后重启容器，DuckDB 数据仍在。  
- 对 `v0.x.y` tag 推送后，Actions 成功发布 npm 包并推送 GHCR 镜像（在配置好 secrets 的前提下）。

## 8. 后续

- 用户确认本 spec 后，使用 **writing-plans** 生成实现计划（Dockerfile、package.json 变更、`release.yml`、文档与冒烟步骤）。  
- 可选后续：crates.io、MCP Streamable HTTP、镜像签名（cosign）。
