# Development & releases

How work moves through this repository and how releases are cut. For internals see
[`ENGINEERING.md`](ENGINEERING.md); for build/install see [`INSTALL.md`](INSTALL.md); for the
normative specification see [`SPEC.md`](SPEC.md).

- [Branch model](#branch-model)
- [The workflows](#the-workflows)
- [Day to day: when to commit and push](#day-to-day-when-to-commit-and-push)
- [Versioning](#versioning)
- [Publishing a release](#publishing-a-release)
- [Releasing the chart](#releasing-the-chart)
- [Publishing](#publishing)
- [Pre-releases](#pre-releases)
- [Hotfixes](#hotfixes)
- [Gotchas](#gotchas)

## Branch model

| Branch | Role | Publishing |
|---|---|---|
| `main` | Stable. A push here is a **release**: image + chart, versioned, to both registries. | `X.Y.Z`, `X.Y`, `X`, `latest` |
| `dev` | Integration. Where work lands first. | `edge`, `sha-<commit>` — image only, Harbor only |
| topic branches | One feature/fix, off `dev`, merged back into `dev` | none |

Work lands on `dev` first, and every push there rebuilds **`edge`** in private Harbor — the moving
dev tag, which the cluster may track. Merging `dev` into `main` is the deliberate act that releases:
the image and the chart are published under the version in `Cargo.toml`, to Harbor (private) **and**
GHCR (public).

Two paths, two sets of tags, and neither writes the other's:

| | dev push | release (a `main` push) |
|---|---|---|
| image tags | `edge`, `sha-<commit>` | `1.1.0`, `1.1`, `1`, `latest` (from `Cargo.toml`) |
| chart | — | `helm package` + push to both registries |
| registries | **Harbor only** | **Harbor + GHCR** |
| arch | amd64 | amd64 |

`edge` is the *dev* tag — the reason it is not mirrored to GHCR is that a public tag nothing keeps
moving is a trap. `latest` is the *release* tag. They never mean the same thing.

## The workflows

Everything runs on **Forgejo** (`git.vn.linuxguru.net`) from
[`.forgejo/workflows/`](.forgejo/workflows/). The GitHub Actions workflows this section used to
describe - `ci.yml`, `docker-release.yml`, `chart-release.yml` - were deleted in `6b16f5b`, so
every path and claim that mentioned them was stale.

- **`.forgejo/workflows/publish-dev.yml`** - fires on a push to `dev`, plus manual dispatch. Builds
  the runtime image (amd64) and pushes it to **private Harbor** as `edge` + `sha-<commit>`. This is
  the everyday path: every push to `dev` republishes `edge`. See [Publishing](#publishing).
- **`.forgejo/workflows/publish.yml`** - fires on a push to `main`, a `v*` tag, or manual dispatch.
  The **release** path: reads the version from `Cargo.toml`, builds the image once and tags it
  `1.1.0` / `1.1` / `1` / `latest` (a pre-release gets only its own tag), pushes all of them to
  Harbor **and** GHCR, then packages the chart and pushes that to both. It replaced the old
  `.githooks/post-commit` hook, which built with podman on whichever developer machine happened to be
  awake.
  - **A workflow only runs from the branch it is pushed to** (Forgejo reads it from that branch), so
    `publish-dev.yml` runs from `dev` and `publish.yml` from `main`. A change to the release workflow
    publishes nothing until it reaches `main` - and then that push is a release. `workflow_dispatch`
    is the manual re-run.
- **`.forgejo/workflows/smoke.yml`** - manual dispatch only, on `dev`. A diagnostic for the runner
  itself (does a job start, does `actions/checkout` resolve, can a job reach the Docker daemon),
  written when nothing on the instance had yet executed a job. Not a check of the code.
- **Nothing runs on a pull request.** There is no CI gate on this repository: a PR shows **no
  statuses at all**, so a green-looking PR means nothing has been verified. The checks are yours to
  run, and they are the ones that matter:

  ```sh
  cargo fmt --check
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test                                  # and again with --no-default-features
  make chart-lint && make chart-template
  ```

  Run the cargo steps one at a time - they contend on the target-dir lock.

What gets published, per event:

| Event | Image tag(s) | Chart | Registries |
|---|---|---|---|
| push to `dev` | `edge`, `sha-<commit>` | — | Harbor |
| push to `main` | `1.1.0`, `1.1`, `1`, `latest` (+ `sha-<commit>`) | yes | Harbor + GHCR |
| push tag `v*` | the same version tags | yes | Harbor + GHCR |
| manual dispatch of `publish-dev` | `edge`, `sha-<commit>` | — | Harbor |
| manual dispatch of `publish` | the version tags | yes | Harbor + GHCR |
| a push anywhere else | nothing | — | — |

`edge` is a **moving** tag: it always points at the newest `dev` build. It is not a version, and
a `-dev` suffix is deliberately not used - a version per commit would be meaningless and would
churn. `sha-<commit>` pins one exact build if you need to go back, on either path.

## Day to day: when to commit and push

- **Commit and push to `dev` freely.** Small, self-contained commits. Every push republishes
  `edge`. Run the gates yourself first - nothing else will (there is no PR CI).
- **Merge `dev` -> `main` when the code is release-grade.** That merge publishes nothing by itself:
  a stable release is a deliberate, manual step, and today that step does not exist (below).
- **Tag by hand only for out-of-band releases** - a pre-release cut from `dev`, or a hotfix. The
  normal path needs no manual tag.

Feature work is one topic branch off `dev`, PR'd back into `dev`:

```sh
git switch dev && git pull
git switch -c fix-search-ranking
git commit -m 'fix(search): rank by distance, not insertion order'
git push -u origin fix-search-ranking      # open a PR into dev
```

## Versioning

Versions are [SemVer](https://semver.org/), and `Cargo.toml`'s `version` is the source of truth: for
`ellmephant --version`, for the chart's `appVersion`, and for the **image tags a release publishes**.
The `Cargo.lock` entry for the crate must agree.

**Bumping the version is how you name a release.** A release happens on a push to `main`, and it
publishes whatever `Cargo.toml` says: `1.1.0` also produces `1.1`, `1` and `latest`. So a `main` push
without a bump republishes the same tags with newer bytes - bump before you merge if the change
deserves a new number.

- `MAJOR` - incompatible change to the CLI, the MCP tool surface, or the on-disk schema.
- `MINOR` - new, backwards-compatible capability.
- `PATCH` - backwards-compatible bug fix.

Pre-release suffixes are ordered: `1.1.0-alpha.1 < 1.1.0-beta.1 < 1.1.0-rc.1 < 1.1.0`. A pre-release
publishes **only its own tag**, so it can never move `latest`, `1` or `1.1`.

### The chart carries two numbers

The Helm chart has its own version, deliberately **not** locked to the crate:

| Field | Meaning | Rule |
|---|---|---|
| `version` (e.g. `0.2.0`) | the chart's own SemVer, and the OCI tag it is pushed under | bump it whenever `charts/**` changes |
| `appVersion` (e.g. `1.1.0`) | the app version the chart claims to carry, and the image tag it defaults to | keep it equal to `Cargo.toml`'s `version` |

`image.tag` is left empty so it resolves to `appVersion` - the right default now that releases carry
version tags, since `appVersion` tracks a released version. **Bump the chart's `version` when you
change the chart**: `publish.yml` pushes the packaged chart under that version, so an unbumped chart
silently *overwrites* the old `0.2.0` in both registries rather than sitting there as a no-op.
Nothing enforces either rule (the guard died with the old `ci.yml`). Staying on `0.x` until the
values surface settles is intentional.

## Publishing a release

**A release is a push to `main`.** `publish.yml` reads the version from `Cargo.toml`, builds the image
once, and publishes it as `X.Y.Z`, `X.Y`, `X`, `latest` - to Harbor (private) **and** GHCR (public) -
then packages the chart and pushes that to both. A pre-release version (`1.1.0-rc.1`) publishes only
its own tag, so a candidate can never move `latest`.

So shipping is: **bump `Cargo.toml`, merge `dev` -> `main`.** The bump names the release; without one
the merge republishes the previous version's tags with newer bytes.

## Releasing the chart

The chart lives in [`charts/ellmephant/`](charts/ellmephant/), and **`publish.yml` publishes it** on
every push to `main`: `helm package`, then `helm push` to Harbor (private) and GHCR (public). There
is no separate chart-release step and no GitHub Release assets any more (`chart-release.yml` was
deleted in `6b16f5b`).

To cut one:

1. Bump `version` in `charts/ellmephant/Chart.yaml` (patch is almost always right) and keep
   `appVersion` equal to `Cargo.toml`'s version. **Unbumped, a changed chart silently overwrites the
   old tag of the same name in both registries.**
2. Commit, merge to `main`.

There is no `index.yaml`, so `helm repo add` does not apply - the chart installs straight from OCI.
Public, for anyone:

```sh
helm install ellmephant oci://ghcr.io/jdblack/charts/ellmephant --version 0.2.0
```

On the LAN, which is what the maintainer's cluster does:

```sh
helm install ellmephant oci://harbor.vn.linuxguru.net/library/charts/ellmephant --version 0.2.0
```

Local sanity before you push - the two commands the chart gate used to run in CI - plus `make
chart-push` for the by-hand path to private Harbor (when you need it out without merging to `main`):

```sh
make chart-lint
make chart-template
```

## Publishing

Two registries, one build:

- **Harbor** (`harbor.vn.linuxguru.net/library/ellmephant`) is **private** — LAN-only, and what the
  cluster pulls.
- **GHCR** (`ghcr.io/jdblack/ellmephant`) is **public** — the same image, mirrored, for everyone else.

A push to `main` publishes both, and a push to `dev` publishes only Harbor:

| path | workflow | what goes where |
|---|---|---|
| `dev` push | `publish-dev.yml` | `edge` + `sha-<commit>` → **Harbor only** (amd64) |
| `main` push | `publish.yml` | `1.1.0` / `1.1` / `1` / `latest` + chart → **Harbor + GHCR** (amd64) |

The release builds once and tags per registry, so the image the cluster runs and the image anyone can
pull are provably the same bytes. The public mirror is why that runner has to reach `ghcr.io` as well
as the LAN.

`edge` is deliberately absent from GHCR: it is the *dev* tag, and a public tag that stops moving is a
trap. `latest` is the release tag; the two never mean the same thing.

Credentials are repo-level Actions secrets:

| secret | what it is |
|---|---|
| `HARBOR_USER` / `HARBOR_TOKEN` | the Harbor robot `robot$library+forgejo` (pull, push, tag-create — no delete), written by `module.forgejo_setup` in stacks/mantle |
| `GHCR_USER` / `GHCR_TOKEN` | the public mirror — **not** written by that module; the token needs `write:packages` |

Without the GHCR pair the release **fails and says so** rather than skipping the mirror quietly: a
public publish that silently stops is worse than one that complains.

The chart is packaged and pushed by the release workflow, after the image (so a chart whose defaults
name an image tag is never ahead of that tag existing), to both registries: the public home is
`oci://ghcr.io/jdblack/charts/ellmephant` and the cluster's is
`oci://harbor.vn.linuxguru.net/library/charts`. There is no `index.yaml`: `helm repo add` does not
apply, install by OCI reference.

**Architectures: amd64 only, on both paths.** The dependency image (`Dockerfile.builder`, ~250 crates)
is amd64-only, so a multi-platform release needs an arm64 dependency image first — and building that
means compiling every crate under QEMU once, which is a project rather than a flag. The path when it
matters: build `ellmephant-builder` for `linux/arm64` once, then switch the release's build step to
buildx with `--platform linux/amd64,linux/arm64` (one `--push` already carries both registries, so the
digests stay identical). Until then, arm64 hosts pull amd64 and emulate.

An image built by hand (`make docker-build`, needs `make builder-image` once) goes up with
`podman push`, but leave that to the workflows unless you are publishing something they cannot.

Point the cluster at Harbor:

```sh
--set image.repository=harbor.vn.linuxguru.net/library/ellmephant
```

The chart installs from its own nested repo over OCI - the closest thing to a `helm repo add`
here, and the path the cluster would actually use:

```sh
helm install ellmephant oci://harbor.vn.linuxguru.net/library/charts/ellmephant --version 0.2.0
```

The chart and the image live under *separate* Harbor repos - `library/charts/ellmephant` versus
`library/ellmephant`. Harbor would happily co-locate an OCI chart and a container image under one
repository name, but sharing a repo means sharing a tag namespace, and the chart's own SemVer
(`0.2.0` today, `1.0.0` eventually) would collide with an image tag. Separate paths rule that out.
Because `helm push` derives the final path segment from the chart's `name`, the split is a
different *parent* path, not a renamed chart: the chart stays `ellmephant`, so its labels, selectors
and resource names are unchanged.

## Pre-releases

A pre-release is a versioned artifact built ahead of a stable release, and it *is* published: set
`Cargo.toml`'s version to e.g. `1.1.0-rc.1` and push to `main`. It publishes **only its own tag**
(`1.1.0-rc.1`, plus `sha-<commit>`) to both registries, and never moves `latest`, `1` or `1.1` - so a
candidate cannot leak into ordinary pulls. Promote it by bumping `Cargo.toml` to the final version and
merging again.

```sh
git switch dev && git pull
# ...set version = "1.1.0-rc.1" in Cargo.toml (and the Cargo.lock entry), commit...
git switch main && git merge --no-ff dev && git push   # publishes 1.1.0-rc.1 only
```

The `v*` tag trigger exists for the same reason: tag a commit to release it without a version bump in
a merge commit's history.

## Hotfixes

`main` is broken. Base the fix on the last good state, not on whatever `dev` has wandered into:

```sh
git switch -c hotfix-search-ranking main
# ...fix, bump Cargo.toml (and Cargo.lock) to the next patch, commit...
git switch main && git merge --no-ff hotfix-search-ranking
git push origin main                       # releases: image + chart, X.Y.Z / X.Y / X / latest
git switch dev && git merge --no-ff hotfix-search-ranking && git push   # keep dev from regressing
```

The push to `main` **is** the release - it publishes the bumped version to both registries, and the
chart goes with it. (If the fix should not become `latest`, publish it as a pre-release instead.)

## Gotchas

- **A version bump is how a release is named.** `main` publishes whatever `Cargo.toml` says, so a
  merge without a bump republishes the previous version's tags with newer bytes.
- **`publish.yml` only runs from `main`.** Forgejo reads the workflow from the branch being pushed,
  so a publish change cannot be exercised by pushing `dev` - it takes effect when it reaches `main`,
  and then every push there publishes. `workflow_dispatch` re-runs it by hand.
- **There is no CI.** A PR shows zero statuses, so a green-looking PR has verified nothing. Run
  `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` (both
  feature sets), `make chart-lint` and `make chart-template` yourself - one cargo step at a time, since
  they contend on the target-dir lock.
- **Publishing needs Harbor access, which is LAN-only.** The Forgejo runner has it (repo secrets
  `HARBOR_USER`/`HARBOR_TOKEN`; the Harbor robot `robot$library+forgejo`, scoped to pull, push and
  tag-create on `library` - notably it cannot delete). A GitHub-hosted runner never will.
- **The chart is published by `publish.yml`, under its own `version`.** Changing `charts/**` without
  bumping that version overwrites the same OCI tag in both registries instead of publishing
  something new.
- **`scripts/` is gone.** `scripts/harbor-publish.sh` was deleted once Forgejo took over the image and
  the chart: it only ever pushed `edge` + `sha-<short>`, its `mirror` mode waited on a GHCR image
  nothing builds any more, and its build path was the slow one (no `--build-arg BUILDER`).
- **A chart change needs its own `version` bump, and a matching `appVersion`.** Nothing checks either
  any more - a stale `appVersion` silently ships a stale image, since `image.tag` is empty by default.
- **Chart and image live in separate Harbor repos** (`library/charts/ellmephant` vs
  `library/ellmephant`). One repo would share a tag namespace, and a chart version like `1.0.0` would
  collide with an image tag.
- **Harbor needs its own login for helm.** podman and helm keep separate credential stores, so
  `make chart-push` fails until `helm registry login harbor.vn.linuxguru.net` has been run once.
- **Platforms.** `edge`/`sha-*` are built **amd64 only** - the cluster is amd64, and an arm64 half
  would double an emulated local build for no user. A by-hand `make docker-build` on Apple Silicon
  is that same slow, emulated build (and it wants `make builder-image` once, or it rebuilds every
  dependency).
- **Harbor is private; GHCR is the public face — and they carry different things.** `dev` pushes
  publish `edge` + `sha-<commit>` to Harbor only; releases (a `main` push) publish the version tags
  and the chart to both. `edge` is never public, `latest` is never private-only.
- **The GHCR mirror needs its own secrets.** `GHCR_USER` / `GHCR_TOKEN` (token needs
  `write:packages`) are not written by `module.forgejo_setup` the way `HARBOR_*` are. Missing them
  fails the mirror step loudly.
- **A ghcr package can start private.** After the first successful mirror, check the package's
  visibility under Package settings if anonymous pulls are wanted — "public registry" is a setting,
  not a consequence of pushing.
- **`charts/ellmephant/values.yaml` defaults to the ghcr image** — correct, since ghcr is public. But
  `artifacthub.io/images` names `:1.1.0`, which does not exist publicly (no release publisher), and
  nothing guards the version any more.
- **`artifacthub.io/images` is no longer guarded.** The check that kept its tag equal to
  `Cargo.toml`'s version lived in the deleted `ci.yml`, so an unbumped entry now ships a stale
  reference.
