# syntax=docker/dockerfile:1.7
#
# Three-stage build:
#   1. builder  — compiles the `mem` Rust binary (release).
#   2. model    — pre-downloads `Qwen/Qwen3-Embedding-0.6B` into the
#                 standard HF hub cache layout so the runtime image works
#                 fully offline.
#   3. runtime  — debian-slim with binary + baked-in HF cache.
#
# Image is ~1.6 GB once the 1.2 GB safetensors blob is included; do not be
# surprised. `EMBEDDING_PROVIDER=fake` (or override `EMBEDDING_MODEL`) at
# `docker run` time if a smaller / different model is wanted.

# ─────────────────────────────────────────────────────────────────────────
# Stage 1: builder
# ─────────────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake build-essential clang git \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock Cross.toml ./
COPY src ./src
COPY db ./db
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin mem && \
    cp target/release/mem /usr/local/bin/mem

# ─────────────────────────────────────────────────────────────────────────
# Stage 2: model — pre-bake Qwen/Qwen3-Embedding-0.6B into HF hub cache.
# `embed_anything` (via the `hf-hub` Rust crate) honours $HF_HOME and the
# `<HF_HOME>/hub/models--<org>--<name>/snapshots/<sha>/…` layout that
# `huggingface-cli download` produces. No --local-dir, no symlinks — keeps
# the layout portable across container runtimes.
# ─────────────────────────────────────────────────────────────────────────
FROM python:3.11-slim AS model
ARG MODEL_ID=Qwen/Qwen3-Embedding-0.6B
ENV HF_HOME=/opt/hf-cache \
    HF_HUB_DISABLE_PROGRESS_BARS=1
RUN pip install --no-cache-dir "huggingface-hub>=0.24"
# Use the Python API directly. The legacy `huggingface-cli download <id>`
# subcommand was deprecated/renamed in hub-cli ≥0.34 (the new CLI is `hf
# download …`); calling `snapshot_download` cuts that drift out and writes
# exactly the same `<HF_HOME>/hub/models--…` layout the Rust `hf-hub` crate
# (and embed_anything) expect at runtime.
RUN mkdir -p "$HF_HOME" \
 && python -c "from huggingface_hub import snapshot_download; snapshot_download('${MODEL_ID}')" \
 && du -sh "$HF_HOME"

# ─────────────────────────────────────────────────────────────────────────
# Stage 3: runtime
# ─────────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*

# Binary
COPY --from=builder /usr/local/bin/mem /usr/local/bin/mem

# Pre-populated HF cache so the first `mem serve` boot does not pull the
# 1.2 GB safetensors blob over the network.
COPY --from=model /opt/hf-cache /opt/hf-cache

ENV BIND_ADDR=0.0.0.0:3000 \
    MEM_DB_PATH=/data/mem.duckdb \
    HF_HOME=/opt/hf-cache \
    EMBEDDING_PROVIDER=embedanything \
    EMBEDDING_MODEL=Qwen/Qwen3-Embedding-0.6B \
    EMBEDDING_DIM=1024

VOLUME ["/data"]
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/health || exit 1

CMD ["mem", "serve"]
