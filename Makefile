# mem — Rust memory service
# Run `make help` to list all available targets.

.DEFAULT_GOAL := help
.PHONY: help build release install run serve mcp repair-check repair-rebuild \
        test test-unit test-fast fmt fmt-check clippy lint check watch watch-check \
        cross cross-linux-gnu cross-linux-musl cross-arm64 \
        clean bench-recall

CARGO ?= cargo

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ==== Build ====

build: ## Debug build
	$(CARGO) build

release: ## Release build
	$(CARGO) build --release

install: release ## Install to ~/.cargo/bin
	$(CARGO) install --path .

# ==== Run (matches the subcommands listed in AGENTS.md) ====

run: serve ## Default = serve

serve: ## Start the HTTP service (127.0.0.1:3000)
	$(CARGO) run -- serve

mcp: ## Start stdio MCP, forwarding to $$MEM_BASE_URL
	$(CARGO) run -- mcp

repair-check: ## Diagnose the vector index sidecar (read-only)
	$(CARGO) run -- repair --check

repair-rebuild: ## Force-rebuild the sidecar (stop mem serve first)
	$(CARGO) run -- repair --rebuild

# ==== Tests ====

test: ## Full test suite (includes tests/ integration tests)
	$(CARGO) test -q

test-unit: ## Unit tests only (in-lib #[cfg(test)] mod tests)
	$(CARGO) test --lib -q

test-fast: test-unit ## Alias for test-unit

# ==== Code quality ====

fmt: ## Format all code
	$(CARGO) fmt --all

fmt-check: ## Check formatting (CI; does not modify files)
	$(CARGO) fmt --all -- --check

clippy: ## clippy, treating warnings as errors
	$(CARGO) clippy --all-targets -- -D warnings

lint: fmt-check clippy ## fmt-check + clippy

# ==== Workflow ====

check: fmt-check clippy test ## Pre-commit gate: fmt-check + clippy + full test suite

# Only watch paths that affect the binary output, so docs / Dockerfile /
# .github / hooks changes don't SIGTERM mem serve mid-handler. The schema
# is now inlined into src/storage/lance_store/, so db/ no longer needs to
# be watched; tests/ does not affect `cargo run` artifacts, so it's
# skipped.
WATCH_PATHS := -w src -w Cargo.toml -w Cargo.lock

watch: ## Auto-restart mem serve only on src/ Cargo.* changes (release build, since debug-mode vector scoring is slow enough to stall the SessionStart hook; requires `cargo install cargo-watch`)
	$(CARGO) watch $(WATCH_PATHS) -x 'run --release -- serve'

watch-check: ## Run cargo check --all-targets only on src/ Cargo.* changes (fast type feedback, no service startup)
	$(CARGO) watch $(WATCH_PATHS) -x 'check --all-targets'

# ==== Cross-compilation (Cross.toml) ====

cross: cross-linux-gnu ## Default cross target = linux-gnu

cross-linux-gnu: ## Release build for x86_64-unknown-linux-gnu
	cross build --release --target x86_64-unknown-linux-gnu

cross-linux-musl: ## Release build for x86_64-unknown-linux-musl (static / Alpine)
	cross build --release --target x86_64-unknown-linux-musl

cross-arm64: ## Release build for aarch64-unknown-linux-gnu
	cross build --release --target aarch64-unknown-linux-gnu

# ==== Cleanup ====

clean: ## cargo clean
	$(CARGO) clean

bench-recall: ## Run the capsule recall ablation bench
	$(CARGO) test --test recall_bench -- --ignored --nocapture
