# mem：npm（mem-mcp）+ Docker（mem）+ Release Actions 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use @superpowers/subagent-driven-development (recommended) or @superpowers/executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 可发布的 npm 包 `mem-mcp`（stdio MCP）、可运行的 Docker 镜像（仅 mem HTTP 服务）、以及 tag 触发的 GitHub Actions（npm publish + GHCR 推送）。

**Architecture:** `integrations/mem-mcp` 通过 `prepack` 构建 `dist/`，`bin` 指向带 shebang 的入口；仓库根 `Dockerfile` 多阶段构建 Rust `mem` 二进制到 `debian:bookworm-slim`；`.github/workflows/release.yml` 在 `push` 匹配 `v*.*.*` tag 时并行执行 npm 与 Docker（可选 `workflow_dispatch` 兜底）。

**Tech Stack:** Rust（cargo release）、Node 20、Docker Buildx、GitHub Actions、`actions/setup-node`、`docker/build-push-action`。

**Spec:** `docs/superpowers/specs/2026-03-22-mem-publish-docker-actions-design.md`

**包名占位：** 实现时将 `package.json` 的 `name` 设为 **`@<npm-org-or-username>/mem-mcp`**（与 npm 上实际 scope 一致）；下文用 **`@scope/mem-mcp`** 指代。

---

## File map

| 路径 | 职责 |
|------|------|
| `integrations/mem-mcp/package.json` | 去掉 `private`；`files`、`bin`、`prepack`；`repository`、`license`、`keywords`（可选） |
| `integrations/mem-mcp/src/index.ts` | 首行 shebang `#!/usr/bin/env node`（保证 `tsc` 输出在 `dist/index.js` 第一行） |
| `integrations/mem-mcp/README.md` | npm 安装、`npx`、`mcp.json`、Docker 联调 `MEM_BASE_URL` |
| `Dockerfile`（仓库根） | 多阶段 build + runtime；`BIND_ADDR` 默认 `0.0.0.0:3000`；`HEALTHCHECK`；`EXPOSE 3000` |
| `.dockerignore` | 忽略 `target/`（非必要层）、`node_modules`、`integrations/mem-mcp/node_modules` 等，加速 build context |
| `deploy/docker-compose.yml`（可选） | `ports` + `volumes` + `environment` 示例 |
| `.github/workflows/release.yml` | tag：`mem-binaries`（gnu + musl → GitHub Release）、`publish-npm`、`docker-mem` |
| `Cross.toml` | `cross` 默认 gnu；musl target；与 Dockerfile builder 对齐 |
| `.github/workflows/ci.yml` | `rust-cross`：`cross build` gnu + musl |
| `README.md`（根） | 短链：Docker 快速启动、发布流程、所需 secrets |
| `docs/superpowers/specs/2026-03-22-mem-publish-docker-actions-design.md` | 将状态改为 Accepted（实现收尾时） |

---

### Task 1: npm 包元数据与可执行入口

**Files:**
- Modify: `integrations/mem-mcp/package.json`
- Modify: `integrations/mem-mcp/src/index.ts`
- Modify: `integrations/mem-mcp/README.md`

- [ ] **Step 1:** 在 `package.json` 设置 `name` 为 `@scope/mem-mcp`（替换为真实 scope）、移除 `"private": true`。

- [ ] **Step 2:** 增加 `"files": ["dist", "README.md", "package.json"]`；若需 SPDX，增加 `"license": "MIT"` 并在仓库根添加 `LICENSE`（与组织策略一致；若无统一许可证，先省略 `license` 字段会阻塞 npm publish——**必须二选一**）。

- [ ] **Step 3:** 增加 scripts：

```json
"prepack": "npm run build"
```

- [ ] **Step 4:** 增加 `bin`：

```json
"bin": {
  "mem-mcp": "dist/index.js"
}
```

- [ ] **Step 5:** 在 `src/index.ts` **第一行**（在任何 import 之前）写入：

```typescript
#!/usr/bin/env node
```

运行 `npm run build` 后打开 `dist/index.js` 确认首行仍为 shebang。

- [ ] **Step 6:** `npm pack --dry-run` 于 `integrations/mem-mcp` 目录，确认 tarball **仅**含 `package.json`、`README.md`、`dist/**`，不含 `src/`。

- [ ] **Step 7:** README 增加：`npm install -g @scope/mem-mcp`、Cursor `mcp.json` 示例：

```json
{
  "mcpServers": {
    "mem": {
      "command": "mem-mcp",
      "env": { "MEM_BASE_URL": "http://127.0.0.1:3000" }
    }
  }
}
```

以及 `npx @scope/mem-mcp` 等价写法。

- [ ] **Step 8:** Commit：`feat(mem-mcp): prepare package for npm publish`

---

### Task 2: 根目录 Dockerfile（mem 服务端）

**Files:**
- Create: `Dockerfile`
- Create: `.dockerignore`

- [ ] **Step 1:** `.dockerignore` 至少包含：

```
target/
integrations/mem-mcp/node_modules/
.git/
```

（按需追加 `*.md` 若希望更小 context——注意不要忽略 `db/schema` 若构建需要；当前 `include_str!` 在编译期读源码树，context 必须含 `db/`、`src/`。）

- [ ] **Step 2:** `Dockerfile` 草案（实现时按 `ldd` 调整 runtime 包）：

```dockerfile
# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY db ./db
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/mem /usr/local/bin/mem
ENV BIND_ADDR=0.0.0.0:3000
ENV MEM_DB_PATH=/data/mem.duckdb
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:3000/health || exit 1
VOLUME ["/data"]
CMD ["mem"]
```

- [ ] **Step 3:** 本地验证：

```bash
docker build -t mem:local .
docker run --rm -p 3000:3000 -e MEM_DB_PATH=/data/test.duckdb -v memdata:/data mem:local
```

另开终端：`curl -sS http://127.0.0.1:3000/health` **Expected:** `ok`

- [ ] **Step 4:** 若 `mem` 启动失败（缺 `.so`），在 runtime 阶段用 `ldd /usr/local/bin/mem` 补包装依赖（常见：`libgcc-s1`、`libc6` 已含；DuckDB bundled 若链到 `libstdc++` 一般已在 slim 中——以实测为准）。

- [ ] **Step 5:** Commit：`feat(docker): add Dockerfile for mem HTTP server`

---

### Task 3（可选）: docker-compose 示例

**Files:**
- Create: `deploy/docker-compose.yml`

- [ ] **Step 1:** 定义 `services.mem`：`build: ..` 或 `image: ghcr.io/OWNER/mem:tag`；`ports: "3000:3000"`；`volumes: mem_data:/data`；`environment: MEM_DB_PATH=/data/mem.duckdb`。

- [ ] **Step 2:** 根 `README` 链到 `deploy/docker-compose.yml`。

- [ ] **Step 3:** Commit：`docs(deploy): add docker-compose example`

---

### Task 4: GitHub Actions — `release.yml`

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1:** 触发器：

```yaml
on:
  push:
    tags: ["v*.*.*"]
  workflow_dispatch:
```

- [ ] **Step 2:** 顶层 `permissions`：

```yaml
permissions:
  contents: read
  packages: write
```

- [ ] **Step 3 — Job `publish-npm`：**  
  - `runs-on: ubuntu-latest`  
  - `actions/checkout@v4`  
  - `actions/setup-node@v4` with `node-version: "20"`, `registry-url: https://registry.npmjs.org`  
  - `working-directory: integrations/mem-mcp`：`npm ci` → `npm version from-git` **或** 用 `jq`/`sed` 将 `package.json` 的 `version` 设为 `${GITHUB_REF_NAME#v}`（与 tag `v0.2.0` 对齐 `0.2.0`）  
  - `npm publish --access public`  
  - `env: NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}`

**注意：** 若采用「改 `package.json` version」步骤，避免把带版本号的 `package.json` 提交回仓库——仅在 CI 内存中改；或要求发布前在 main 上手动 bump 再 tag（实现时选一种并写进 workflow 注释）。

- [ ] **Step 4 — Job `docker-mem`：**  
  - `docker/setup-buildx-action@v3`  
  - `docker/login-action@v3` with `registry: ghcr.io`, `username: ${{ github.actor }}`, `password: ${{ secrets.GITHUB_TOKEN }}`  
  - `docker/build-push-action@v6`：`context: .`, `file: ./Dockerfile`, `push: true`, `tags:`  
    - `ghcr.io/${{ github.repository_owner }}/mem:${{ github.ref_name }}`  
    - **策略（本期简单约定）：** 同时打 `ghcr.io/${{ github.repository_owner }}/mem:latest`（仅当接受「每次 tag 移动 latest」；若不接受，删除 latest 行并在 README 说明）。

- [ ] **Step 5:** 文档列出 **Repository secrets**：`NPM_TOKEN`（npm Automation Access Token）。

- [ ] **Step 6:** Commit：`ci: add release workflow for npm and GHCR`

---

### Task 5: 根 README 与 spec 状态

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-03-22-mem-publish-docker-actions-design.md`

- [ ] **Step 1:** 根 `README` 增加小节 **Docker**（`docker build` / `docker run` 一行示例）与 **Release**（打 tag `v0.x.y`、配置 `NPM_TOKEN`、查看 Actions）。

- [ ] **Step 2:** 将 spec 文首 `状态` 改为 **Accepted**（或 Implemented，与仓库惯例一致）。

- [ ] **Step 3:** Commit：`docs: document Docker and release process`

---

## 验证清单（实现全部 Task 后）

- [ ] `cargo test`、`integrations/mem-mcp npm test` 仍绿。  
- [ ] `docker build` + `curl /health` 成功。  
- [ ] `npm pack` 内容干净；本地 `npm publish --dry-run`（若支持）或通过 fork 测试 workflow。  
- [ ] 对测试 tag 跑 `release.yml`（fork 上需启用 packages 与 secrets）。

---

## Execution handoff

Plan 路径：`docs/superpowers/plans/2026-03-22-mem-publish-docker-actions.md`。

**下一步：** 用 @superpowers/subagent-driven-development 按 Task 1→5 实施，或在本会话逐项执行并在每 Task 后 `git commit`。
