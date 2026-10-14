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
| `main` | Stable, and the **publish trigger**: every push here builds the image *and* the chart, and pushes both to both registries. | image + chart as `edge`, `sha-<commit>` |
| `dev` | Integration. Where work lands first. | **nothing** |
| topic branches | One feature/fix, off `dev`, merged back into `dev` | none |

Work lands on `dev` first. Merging `dev` into `main` is a deliberate act that marks the code
publishable - and it publishes: image and chart, to Harbor (private) and GHCR (public).

`edge` is the tag that moves, and the only one. There are no `X.Y.Z` / `X.Y` / `X` / `latest` tags:
what is published is whatever `main` is, and the version in `Cargo.toml` is a *label* on that rather
than a tag. (The deleted `docker-release.yml` did publish version tags from a bump; nothing does
now.)

## The workflows

Everything runs on **Forgejo** (`git.vn.linuxguru.net`) from
[`.forgejo/workflows/`](.forgejo/workflows/). The GitHub Actions workflows this section used to
describe - `ci.yml`, `docker-release.yml`, `chart-release.yml` - were deleted in `6b16f5b`, so
every path and claim that mentioned them was stale.

- **`.forgejo/workflows/publish.yml`** - fires on a push to `main`, plus manual dispatch. Builds the
  runtime image on the cluster (natively amd64), pushes `edge` + `sha-<commit>` to private Harbor and
  mirrors the same digests to public GHCR, then packages the chart and pushes **that** to both
  registries too. It replaced the old `.githooks/post-commit` hook, which built with podman on
  whichever developer machine happened to be awake. See [Publishing](#publishing).
  - **It only runs from `main`** (Forgejo reads the workflow from the branch being pushed), so a
    change to this file publishes nothing until it reaches `main` - and then every push there
    publishes. `workflow_dispatch` is the manual re-run.
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

| Event | Image tag(s) |
|---|---|
| push to `main` | `edge`, `sha-<commit>` — image + chart, to Harbor *and* GHCR, built on the cluster |
| manual dispatch of `publish` | the same |
| a push anywhere else | nothing |

**Nothing publishes a version.** No tag is created for you, `latest` does not exist, and `edge` is the
only moving tag - see [Publishing a release](#publishing-a-release).

`edge` is a **moving** tag: it always points at the newest `dev` build. It is not a version, and
a `-dev` suffix is deliberately not used - a version per commit would be meaningless and would
churn. `sha-<commit>` pins one exact build if you need to go back.

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

Versions are [SemVer](https://semver.org/), and `Cargo.toml`'s `version` is the source of truth for
`ellmephant --version` and for the chart's `appVersion`. The `Cargo.lock` entry for the crate must
agree.

**A version is a label, not a tag.** Nothing publishes `1.1.0`: publishing happens on `main` pushes
and produces `edge` + `sha-<commit>` (see [The workflows](#the-workflows)). Bump the version so the
binary and the chart say what the code is; the registries will not notice.

- `MAJOR` - incompatible change to the CLI, the MCP tool surface, or the on-disk schema.
- `MINOR` - new, backwards-compatible capability.
- `PATCH` - backwards-compatible bug fix.

Pre-release suffixes are ordered: `1.1.0-alpha.1 < 1.1.0-beta.1 < 1.1.0-rc.1 < 1.1.0`.

### The chart carries two numbers

The Helm chart has its own version, deliberately **not** locked to the crate:

| Field | Meaning | Rule |
|---|---|---|
| `version` (e.g. `0.2.0`) | the chart's own SemVer, and the OCI tag it is pushed under | bump it whenever `charts/**` changes |
| `appVersion` (e.g. `1.1.0`) | the app version the chart claims to carry | keep it equal to `Cargo.toml`'s `version` |

`image.tag` is set to `edge` rather than left empty: an empty tag resolves to `appVersion`, and
`1.1.0` is not a published tag (nothing publishes versions). **Bump the chart's `version` when you
change the chart**: `publish.yml` pushes the packaged chart under that version, so an unbumped chart
silently *overwrites* the old `0.2.0` in both registries rather than sitting there as a no-op.
Nothing enforces either rule (the guard died with the old `ci.yml`). Staying on `0.x` until the
values surface settles is intentional.

## Publishing a release

**There is no "release" step, and no version tags.** `edge` is the word for it (James, 2026-10-15):
every push to `main` builds the image and the chart and publishes both to both registries as `edge`
plus `sha-<commit>`, so what the world can pull is whatever `main` is. The workflow that used to read
`Cargo.toml`, create `vX.Y.Z` and publish `X.Y.Z` / `X.Y` / `X` / `latest` was `docker-release.yml`,
deleted in `6b16f5b`, and nothing replaced that behaviour.

So shipping is: **merge `dev` -> `main`.** Bump `Cargo.toml`'s version first if you like - that
changes what `ellmephant --version` and the chart's `appVersion` say, and nothing else.

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

A push to `main` publishes both. `.forgejo/workflows/publish.yml` builds natively on the cluster
(amd64), pushes `edge` + `sha-<commit>` to Harbor, then **re-tags** and pushes those same digests to
GHCR: `docker tag`, never a rebuild, so the image the cluster runs and the image anyone can pull are
provably the same bytes. The public mirror is why the runner has to reach `ghcr.io` as well as the
LAN. Then it packages the chart and pushes that to both registries too.

Credentials are repo-level Actions secrets:

| secret | what it is |
|---|---|
| `HARBOR_USER` / `HARBOR_TOKEN` | the Harbor robot `robot$library+forgejo` (pull, push, tag-create — no delete), written by `module.forgejo_setup` in stacks/mantle |
| `GHCR_USER` / `GHCR_TOKEN` | the public mirror — **not** written by that module; the token needs `write:packages` |

Without the GHCR pair the mirror step **fails and says so** rather than skipping quietly: a public
publish that silently stops is worse than one that complains.

Tags published today are `edge` and `sha-<commit>`, on both registries and for **both** artifacts
(image and chart) — so `ghcr.io/jdblack/ellmephant:edge` and
`oci://ghcr.io/jdblack/charts/ellmephant --version 0.2.0` are both pullable right now. There is **no
`latest` and no version tag**; `edge` is the term for the moving one.

The chart is packaged and pushed by this same workflow, after the image (so a chart whose defaults
name an image tag is never ahead of that tag existing).

The **chart is published by the same workflow**, to both registries, so the public home is
`oci://ghcr.io/jdblack/charts/ellmephant` and the cluster's is
`oci://harbor.vn.linuxguru.net/library/charts`. There is no `index.yaml`: `helm repo add` does not
apply, install by OCI reference. (Before this workflow existed, the chart went only to private
Harbor, which meant nobody outside the LAN could install it at all.)

An image built by hand (`make docker-build`, needs `make builder-image` once) goes up with
`podman push`, but leave that to the workflow unless you are publishing a version the workflow cannot.

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

A pre-release used to be a versioned, immutable artifact built ahead of a stable release. There are
no version tags now, so it is a **label**: set `Cargo.toml`'s version to e.g. `1.1.0-rc.1`, and say so
in the commit or tag message. Nothing on a registry distinguishes it.

```sh
git switch dev && git pull
git tag -a v1.1.0-rc.1 -m 'ellmephant 1.1.0-rc.1'   # a record, not a trigger
git push origin v1.1.0-rc.1
```

If a candidate genuinely needs its own image, publishing it means `sha-<commit>` on `main` - that is
the only immutable handle the registries carry.

## Hotfixes

`main` is broken. `edge` will follow the fix, so there is nothing to "release" - but base the fix on
the last good state, not on whatever `dev` has wandered into:

```sh
git switch -c hotfix-search-ranking main
# ...fix, bump Cargo.toml (and Cargo.lock) if you want the label to move, commit...
git switch main && git merge --no-ff hotfix-search-ranking
git push origin main                       # publishes: image + chart, edge + sha-<commit>
git switch dev && git merge --no-ff hotfix-search-ranking && git push   # keep dev from regressing
```

The push to `main` is the release: `edge` moves to the fixed build on both registries, and the chart
goes with it.

## Gotchas

- **A version bump publishes nothing.** Versions are labels: `--version`, the chart's `appVersion`.
  The registries only ever see `edge` and `sha-<commit>`.
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
- **Harbor is private; GHCR is the public face.** One build, two registries, mirrored with `docker
  tag` (so both carry the same digest) for the image, and pushed twice for the chart. `edge` and
  `sha-<commit>` are the only tags; there is no `latest`.
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
