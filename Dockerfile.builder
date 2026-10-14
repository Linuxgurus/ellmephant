# syntax=docker/dockerfile:1
#
# The dependency layer as an IMAGE: ~250 crates prebuilt, published to Harbor and used as the
# `builder` stage of the main Dockerfile. Because it is an image rather than a cache layer, it
# survives the runner pod being replaced -- which is the whole point. The DinD's docker cache is
# ephemeral (it dies with the pod, and the pod rolls on any runner config change), so a cold
# build used to recompile every dependency: 190 of the 309 seconds.
#
# Rebuilt only when Cargo.toml or Cargo.lock change -- see .forgejo/workflows/build-builder.yml.
#
# The stub-crate trick is deliberately IDENTICAL to the main Dockerfile's: build every dependency
# against a do-nothing `src/`, so the expensive result depends on Cargo.lock alone and cannot be
# invalidated by a source change. Keep the two in step -- if this stub and the main one differ,
# cargo recompiles here for nothing.

FROM rust:1-bookworm

# The lean release profile -- and this MUST match the main Dockerfile exactly. Profile settings are
# part of cargo's dependency fingerprint, so if this image's dependencies were built with a
# different profile than the build that consumes them, cargo treats every dependency as changed
# and recompiles all of them -- which is the whole cost this image exists to avoid.
#
# `codegen-units = 16` and `lto = false` are cargo's own release defaults. They remove the
# cross-crate LLVM work from the build and hand back a slower runtime binary. Deliberate for an
# `edge` image: to get the optimised build back, drop both ENVs here and in the Dockerfile, and
# bump the builder tag (see .forgejo/workflows/publish-dev.yml).
ENV CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    CARGO_PROFILE_RELEASE_LTO=false

# `lld`, not `gold`: rustc now warns on every gold link -- "the gold linker is deprecated and has
# known bugs with Rust" -- and its advice is LLD. The Debian package is what a `cc -fuse-ld=lld`
# resolves to; rustc's own preference on this toolchain is its bundled `rust-lld`.
RUN apt-get update \
 && apt-get install -y --no-install-recommends lld \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTFLAGS="-C link-arg=-fuse-ld=lld"

WORKDIR /src

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && printf '\n' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

# /src/target is the payload: it carries every compiled dependency. Nothing else here matters.
