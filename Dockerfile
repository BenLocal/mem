# Same glibc toolchain as `cross build` (see Cross.toml).
FROM ghcr.io/cross-rs/x86_64-unknown-linux-gnu:edge AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock Cross.toml ./
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
