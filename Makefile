# mem — Rust memory service
# 运行 `make help` 看所有可用目标。

.DEFAULT_GOAL := help
.PHONY: help build release install run serve mcp repair-check repair-rebuild \
        test test-unit test-fast fmt fmt-check clippy lint check watch watch-check \
        cross cross-linux-gnu cross-linux-musl cross-arm64 \
        clean

CARGO ?= cargo

help: ## 显示可用目标
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ==== 构建 ====

build: ## 调试构建
	$(CARGO) build

release: ## release 构建
	$(CARGO) build --release

install: release ## 安装到 ~/.cargo/bin
	$(CARGO) install --path .

# ==== 运行（与 AGENTS.md 列出的子命令一致） ====

run: serve ## 默认 = serve

serve: ## 启动 HTTP 服务（127.0.0.1:3000）
	$(CARGO) run -- serve

mcp: ## 启动 stdio MCP，转发到 $$MEM_BASE_URL
	$(CARGO) run -- mcp

repair-check: ## 诊断 vector index sidecar（只读）
	$(CARGO) run -- repair --check

repair-rebuild: ## 强制重建 sidecar（先停掉 mem serve）
	$(CARGO) run -- repair --rebuild

# ==== 测试 ====

test: ## 全套测试（含 tests/ 集成测试）
	$(CARGO) test -q

test-unit: ## 仅单测（lib 内 #[cfg(test)] mod tests）
	$(CARGO) test --lib -q

test-fast: test-unit ## test-unit 的别名

# ==== 代码质量 ====

fmt: ## 格式化所有代码
	$(CARGO) fmt --all

fmt-check: ## 检查格式（CI 用，不修改文件）
	$(CARGO) fmt --all -- --check

clippy: ## clippy，视警告为错
	$(CARGO) clippy --all-targets -- -D warnings

lint: fmt-check clippy ## fmt-check + clippy

# ==== 流程 ====

check: fmt-check clippy test ## pre-commit gate：fmt-check + clippy + 全套测试

# 只观察影响 binary 输出的路径，避免 docs / Dockerfile / .github / hooks
# 等改动把 mem serve mid-handler SIGTERM 了。`db/schema/*.sql` 经
# `include_str!` 编入二进制，必须看；tests/ 不影响 `cargo run` 产物，跳过。
WATCH_PATHS := -w src -w Cargo.toml -w Cargo.lock -w db

watch: ## 仅 src/ Cargo.* db/ 改动时自动重启 mem serve（release 构建，避免 debug 模式下向量打分慢到把 SessionStart hook 拖死；需 `cargo install cargo-watch`）
	$(CARGO) watch $(WATCH_PATHS) -x 'run --release -- serve'

watch-check: ## 仅 src/ Cargo.* db/ 改动时跑 cargo check --all-targets（快速类型反馈，不启服务）
	$(CARGO) watch $(WATCH_PATHS) -x 'check --all-targets'

# ==== 跨平台（Cross.toml） ====

cross: cross-linux-gnu ## 默认 cross 目标 = linux-gnu

cross-linux-gnu: ## release 构建 x86_64-unknown-linux-gnu
	cross build --release --target x86_64-unknown-linux-gnu

cross-linux-musl: ## release 构建 x86_64-unknown-linux-musl（静态/Alpine）
	cross build --release --target x86_64-unknown-linux-musl

cross-arm64: ## release 构建 aarch64-unknown-linux-gnu
	cross build --release --target aarch64-unknown-linux-gnu

# ==== 清理 ====

clean: ## cargo clean
	$(CARGO) clean
