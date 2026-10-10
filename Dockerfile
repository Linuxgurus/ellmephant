# syntax=docker/dockerfile:1

# ---------- build ----------
FROM rust:1-bookworm AS builder
WORKDIR /src

# Warm the dependency cache against a stub crate so editing src/ does not rebuild
# ~400 crates. Invalidated only when Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && printf '\n' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY . .
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime

# ca-certificates is only needed for the first-run HuggingFace download; the binary
# itself needs no interpreter, no Python, no Node.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 ellmephant

COPY --from=builder /src/target/release/ellmephant /usr/local/bin/ellmephant

# The MCP server serves Streamable HTTP on a port by default; bind all interfaces so the port
# is reachable from outside the container (localhost is the safer default off Docker).
# `ellmephant` at info (startup, writes, searches, encoder load), `rmcp` at info (client
# session lifecycle); `warn` is the default for everything else.
ENV ELLMEPHANT_HOME=/data \
    RUST_LOG=ellmephant=info,rmcp=info,warn \
    ELLMEPHANT_MCP_HOST=0.0.0.0 \
    ELLMEPHANT_MCP_PORT=8888
RUN install -d -o ellmephant -g ellmephant /data
VOLUME ["/data"]
USER ellmephant

EXPOSE 8888

# Liveness only: `doctor` opens/migrates the store (a write) on every poll, which is both
# wasted work and a failure on a read-only volume.
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["ellmephant", "--version"]

ENTRYPOINT ["ellmephant"]
# Default to the MCP server on `ELLMEPHANT_MCP_PORT`; use `mcp --stdio` for the stdio transport.
CMD ["mcp"]
