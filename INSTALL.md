# Installing ellmephant

ellmephant is a single binary backed by one SQLite file. You can build it from source or run the
container image. Either way the store is a single directory (`~/.ellmephant` by default, `/data`
in the container) and the model weights are fetched into it once, then reused offline.

- [Prerequisites](#prerequisites)
- [Build from source](#build-from-source)
- [Install the binary on your `PATH`](#install-the-binary-on-your-path)
- [Store-only build](#store-only-build)
- [Create the store and fetch weights](#create-the-store-and-fetch-weights)
- [Docker](#docker)
- [Kubernetes (Helm)](#kubernetes-helm)
- [Verify the installation](#verify-the-installation)

## Prerequisites

- **Rust 1.88+** (edition 2024) and `cargo` — install via [rustup](https://rustup.rs).
- **A C compiler / linker** (`cc`, `clang`, or `gcc`). SQLite and `sqlite-vec` are compiled from
  source. On Debian/Ubuntu: `apt install build-essential`.
- **~3 GB disk** to build, plus **~1.2 GB** for the model weights.
- **macOS or Linux.** Apple Silicon can use `--device metal`; other platforms run on the CPU.
  Windows is untested; WSL2 is expected to work.

## Build from source

```sh
git clone <this-repo> && cd ellmephant
cargo build --release --locked        # or: make release
./target/release/ellmephant --version
```

The first build is slow: it compiles the SQLite/`sqlite-vec` C sources, the candle ML stack, and
roughly four hundred crates. Later builds are incremental.

## Install the binary on your `PATH`

```sh
cargo install --path .                # -> ~/.cargo/bin/ellmephant
# or, without cargo:
install -m 0755 target/release/ellmephant ~/.local/bin/ellmephant
```

## Store-only build

A build without the embedding stack, for machines that only read a store. `show`, `list`, `tags`,
`stats`, `backlinks` and **phrase-less filter searches** work; a *semantic phrase* needs the
encoder and reports an error without it.

```sh
cargo build --release --locked --no-default-features
```

## Create the store and fetch weights

```sh
ellmephant doctor            # creates ~/.ellmephant/ and the schema; prints health
ellmephant model fetch       # one-time ~1.2 GB download into ~/.ellmephant/models/
ellmephant doctor --verify   # optional: re-hash every weight file
```

Every command honours `ELLMEPHANT_HOME`; without it the store is `~/.ellmephant`. After the fetch,
runs need no network. `ellmephant --offline <cmd>` opens no socket.

## Docker

The image is a multi-stage build: dependencies compile in `rust:1-bookworm`, and the runtime is
`debian:bookworm-slim` plus the one binary. The store is a volume at `/data`
(`ELLMEPHANT_HOME=/data`).

```sh
make docker-build                     # docker build -t ellmephant:dev .
# or:
docker build -t ellmephant:dev .
```

The image's default command is the MCP server (`mcp`, HTTP on port 8888). To bootstrap a volume,
name a different command:

```sh
docker run --rm -v ellmephant-data:/data ellmephant:dev doctor
docker run --rm -v ellmephant-data:/data ellmephant:dev model fetch
```

Do not bake the weights into the image. Fetch them into the volume (above), or mount an existing
cache read-only:

```sh
docker run --rm -it -v ellmephant-data:/data \
                    -v ~/.ellmephant/models:/data/models:ro \
                    ellmephant:dev search "where do the logs live"
```

See the [README](README.md) for usage, including the MCP server on a host and in Docker.

## Kubernetes (Helm)

A chart ships in [`charts/ellmephant/`](charts/ellmephant/): one Deployment, a store PVC, and an
optional separate weights-cache PVC, serving MCP over HTTP on port 8888. It defaults to the public
image `ghcr.io/jdblack/ellmephant` — **note that nothing in this repository publishes to ghcr any
more**, so override the repository if you pull from somewhere else (the maintainer's cluster points it
at Harbor via `image.repository`).

```sh
helm install ellmephant ./charts/ellmephant --namespace ellmephant --create-namespace
```

The chart is published to **Harbor**, as an OCI artifact, by hand (`make chart-push`): the workflow
that attached it to a GitHub Release was deleted with the rest of the GitHub Actions setup, so there
is no release asset to install from any more. Install straight from OCI:

```sh
helm install ellmephant oci://harbor.vn.linuxguru.net/library/charts/ellmephant --version 0.2.0
```

Point the MCP client at `http://ellmephant.ellmephant.svc:8888/mcp`. The store lives on the
`ellmephant-data` claim — keep it, or the memories die with the pod. First start downloads the
embedder (~1.2 GB) on first use; pre-fetch it with `models.fetch.enabled=true`, or mount a warm
cache read-only.

The chart also knows about this house's neighbours: `gateway.enabled` renders the Gateway API
`ListenerSet` / `ReferenceGrant` / `HTTPRoute` trio (NGINX Gateway Fabric, cert-manager,
external-dns) instead of an Ingress. Every value is documented in
[`charts/ellmephant/README.md`](charts/ellmephant/README.md).

## Verify the installation

```sh
ellmephant --version
ellmephant doctor
```
