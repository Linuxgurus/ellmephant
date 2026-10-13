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

WORKDIR /src

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && printf '\n' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

# /src/target is the payload: it carries every compiled dependency. Nothing else here matters.
