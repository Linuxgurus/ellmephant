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
 && useradd --create-home --uid 10001 llmphant

COPY --from=builder /src/target/release/llmphant /usr/local/bin/llmphant

ENV LLMPHANT_HOME=/data \
    RUST_LOG=warn
RUN install -d -o llmphant -g llmphant /data
VOLUME ["/data"]
USER llmphant

HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["llmphant", "doctor"]

ENTRYPOINT ["llmphant"]
CMD ["doctor"]
