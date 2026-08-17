# mem — Rust memory service
# Run `make help` to list all available targets.

.DEFAULT_GOAL := help
.PHONY: help build release install run serve mcp repair-check repair-rebuild \
        test test-full test-unit test-fast test-one test-filter test-rounds test-candidates test-skill-compiler \
        fmt fmt-check clippy lint check watch watch-check \
        cross cross-linux-gnu cross-linux-musl cross-arm64 \
        clean bench-recall

CARGO ?= cargo
CARGO_TEST_JOBS ?= 4
RUST_TEST_THREADS ?= 2

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ==== Build ====

build: ## Debug build
	$(CARGO) build

release: ## Release build
	$(CARGO) build --release

install: release ## Install to ~/.cargo/bin
	# --locked: `cargo install` otherwise IGNORES Cargo.lock and re-resolves,
	# which pulls pgvector's sqlx onto sqlx-core 0.9.0 while our sqlx stays
	# 0.8.6 → two sqlx-core versions → pgvector::Vector fails its sqlx
	# Encode/Decode/Type bounds. Now that postgres/clickhouse are default deps
	# (always compiled), the install MUST use the tested locked resolution.
	$(CARGO) install --path . --locked

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

test: test-full ## Full test suite; prefer test-one/test-filter while iterating

test-full: ## Full suite (55 integration crates); reserved for explicit gates
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) -q

test-unit: ## Unit tests only (in-lib #[cfg(test)] mod tests)
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib -q

test-fast: test-unit ## Alias for test-unit

test-one: ## One integration crate: make test-one TEST=search_api
	@test -n "$(TEST)" || { echo "usage: make test-one TEST=<tests filename without .rs>" >&2; exit 2; }
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test $(TEST)

test-filter: ## One named lib test: make test-filter FILTER=ingest::compute_content
	@test -n "$(FILTER)" || { echo "usage: make test-filter FILTER=<test name or module path>" >&2; exit 2; }
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib $(FILTER)

test-rounds: ## completed_tool_round projector, rebuild, and HTTP contract only
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib completed_tool_round_service::tests::
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib cli::completed_tool_rounds::tests::
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib config::tests::admin_bearer_is_fail_closed
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test completed_tool_rounds
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test completed_tool_round_rebuild
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test completed_tool_round_api

test-candidates: ## deterministic Skill-candidate planner and durable queue only
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib config::tests::skill_candidate
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib app::tests::candidate_worker_rejects
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib skill_candidate_store::tests::
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_candidate_jobs

test-skill-compiler: ## Skill proposal compiler and review-gated lifecycle only
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib skill_proposal
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib skill_bundle
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib skill_store
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib mcp::config::tests::
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --lib mcp::compiler::tests::
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_proposal_lifecycle
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_bundle_lifecycle
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_proposal_api
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_proposal_safety
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_bundle_validation
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_role_auth
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test skill_feedback_candidates
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test mcp_skill_compiler_tools
	RUST_TEST_THREADS=$(RUST_TEST_THREADS) $(CARGO) test -j $(CARGO_TEST_JOBS) --test plugin_assets
	npm --prefix packaging/pi/compiler run check

# ==== Code quality ====

fmt: ## Format all code
	$(CARGO) fmt --all

fmt-check: ## Check formatting (CI; does not modify files)
	$(CARGO) fmt --all -- --check

clippy: ## clippy, treating warnings as errors
	$(CARGO) clippy --all-targets -- -D warnings

lint: fmt-check clippy ## fmt-check + clippy

# ==== Workflow ====

check: fmt-check clippy test-full ## Explicit pre-commit gate, including the full suite

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
