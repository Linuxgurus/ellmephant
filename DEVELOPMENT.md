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
- [Mirroring to Harbor](#mirroring-to-harbor)
- [Pre-releases](#pre-releases)
- [Hotfixes](#hotfixes)
- [Gotchas](#gotchas)

## Branch model

| Branch | Role | Publishing |
|---|---|---|
| `main` | Stable. Also the release trigger: a version bump merged here publishes. | `X.Y.Z`, `X.Y`, `X`, `latest` |
| `dev` | Integration. Where work lands first. | `edge`, `sha-<commit>` |
| topic branches | One feature/fix, off `dev`, merged back into `dev` | none |

Work lands on `dev` first. Merging `dev` into `main` is a deliberate act that marks the code
release-grade - and, if the version was bumped, publishes an image.

## The workflows

- **`.github/workflows/ci.yml`** - runs on every push to `main` and every pull request. Format,
  clippy, tests (both feature sets), a no-push Docker build, and the chart gate (lint, render
  every value shape, the token guard, and the two version guards). Gate, not publisher.
- **`.github/workflows/docker-release.yml`** - the image publisher. Fires on a push to `main`, a
  push to `dev`, a `v*` tag, or manually. It reads the version from `Cargo.toml` and pushes the
  image to `ghcr.io/<owner>/ellmephant`.
- **`.github/workflows/chart-release.yml`** - the chart publisher. Fires on a push to `main` that
  touches `charts/**`. Runs [`helm/chart-releaser`](https://github.com/helm/chart-releaser-action),
  which packages the chart, cuts a GitHub Release with the `.tgz` attached, and refreshes the
  `gh-pages` Helm repo index. No container registry involved.

What the publisher emits, per event:

| Event | Condition | Image tag(s) |
|---|---|---|
| push to `main` | `Cargo.toml` version bumped, `vX.Y.Z` not yet tagged | `X.Y.Z`, `X.Y`, `X`, `latest` |
| push to `main` | `Cargo.toml` version already tagged | *(nothing - a no-op)* |
| push to `main` | version is a pre-release, e.g. `1.1.0-rc.1` | `1.1.0-rc.1` only |
| push to `dev` | always | `edge`, `sha-<commit>` |
| push tag `v1.1.0` | - | `1.1.0`, `1.1`, `1`, `latest` |
| push tag `v1.1.0-rc.1` | - | `1.1.0-rc.1` only |
| manual dispatch | - | `edge`, `sha-<commit>` |

On a release it also creates and pushes the annotated tag `vX.Y.Z`, so the git history records
the release without a separate step.

`edge` is a **moving** tag: it always points at the newest `dev` build. It is not a version, and
a `-dev` suffix is deliberately not used - a version per commit would be meaningless and would
churn. `sha-<commit>` pins one exact build if you need to go back.

## Day to day: when to commit and push

- **Commit and push to `dev` freely.** Small, self-contained commits. Every push runs CI and
  republishes `edge`. If it builds and the tests pass, it may go in.
- **Merge `dev` -> `main` when you want to release.** Bump the version first (see below); if the
  version is unchanged, the merge publishes nothing.
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

Versions are [SemVer](https://semver.org/), and `Cargo.toml`'s `version` is the source of truth:
for `ellmephant --version`, and for the release workflow. The `Cargo.lock` entry for the crate
must agree. **Bumping this version is what triggers a release.**

- `MAJOR` - incompatible change to the CLI, the MCP tool surface, or the on-disk schema.
- `MINOR` - new, backwards-compatible capability.
- `PATCH` - backwards-compatible bug fix.

Pre-release suffixes are ordered: `1.1.0-alpha.1 < 1.1.0-beta.1 < 1.1.0-rc.1 < 1.1.0`.

### The chart carries two numbers

The Helm chart has its own version, deliberately **not** locked to the crate:

| Field | Meaning | Rule |
|---|---|---|
| `version` (e.g. `0.1.0`) | the chart's own SemVer | bump it in the same PR that touches `charts/**` |
| `appVersion` (e.g. `1.0.0`) | the app release the chart defaults to | keep it equal to `Cargo.toml`'s `version` |

`image.tag` is empty by default, so it resolves to `appVersion` - a stale `appVersion` silently
ships a stale image. CI enforces both: `appVersion` must equal `Cargo.toml`, and a chart change
must touch `Chart.yaml` (otherwise `chart-releaser` no-ops on the existing version and publishes
nothing). Staying on `0.x` until the values surface settles is intentional.

## Publishing a release

The workflow does the tagging and the image; you only bump the version and merge.

1. On `dev`, bump `version` in `Cargo.toml` (and its `Cargo.lock` entry); commit and push.
2. Merge `dev` into `main` and push.
3. The workflow reads the version, tags `vX.Y.Z`, and publishes `X.Y.Z`, `X.Y`, `X`, `latest`.

```sh
# version already bumped and committed on dev
git switch main && git merge --no-ff dev && git push origin main
# -> tag v1.1.0 created; image 1.1.0, 1.1, 1, latest published
```

4. Optionally publish a GitHub Release for the new tag:

```sh
gh release create v1.1.0 --title 'v1.1.0' --generate-notes
```

If you merge to `main` without changing the version, the workflow detects that `vX.Y.Z` already
exists and does nothing. Only a version bump publishes.

## Releasing the chart

The chart lives in [`charts/ellmephant/`](charts/ellmephant/) and is published to a GitHub-hosted
Helm repo by [`chart-release.yml`](.github/workflows/chart-release.yml), using
[`helm/chart-releaser`](https://github.com/helm/chart-releaser-action). There is no container
registry in this path - releases are a GitHub Release plus a `gh-pages` index.

**One-time setup (manual, once):** enable Pages - *Settings -> Pages -> Source: Deploy from a
branch -> `gh-pages` / `(root)`*. The branch is created by the first chart-releaser run, so enable
this after that run if Pages refuses a branch that does not exist yet.

Then, to cut a chart release:

1. Bump `version` in `charts/ellmephant/Chart.yaml` (patch is almost always right).
2. Commit and merge into `main`.

`chart-releaser` packages every chart under `charts/`, skips any version it has already released,
creates the Release `ellmephant-<version>` with the `.tgz` attached, and refreshes `gh-pages`.
Users then:

```sh
helm repo add ellmephant https://jdblack.github.io/ellmephant
helm repo update
helm install ellmephant ellmephant/ellmephant --version 0.1.1
```

Local sanity before you push (also what CI runs):

```sh
make chart-lint
make chart-template
```

## Mirroring to Harbor

The in-cluster registry is `harbor.vn.linuxguru.net`. Both artifacts can be mirrored there - the
image for a cluster that should not reach GHCR, and the chart as an OCI artifact. Neither path
touches CI; they are `podman`/`helm` one-liners.

```sh
# 1. Image: take the published multi-arch amd64 variant and re-tag it into Harbor.
podman pull --platform linux/amd64 ghcr.io/jdblack/ellmephant:1.0.0
podman tag  ghcr.io/jdblack/ellmephant:1.0.0 harbor.vn.linuxguru.net/library/ellmephant:1.0.0
podman tag  ghcr.io/jdblack/ellmephant:1.0.0 harbor.vn.linuxguru.net/library/ellmephant:latest
podman push harbor.vn.linuxguru.net/library/ellmephant:1.0.0
podman push harbor.vn.linuxguru.net/library/ellmephant:latest

# 2. Chart, as an OCI artifact under the same project.
helm registry login harbor.vn.linuxguru.net
make chart-push          # packages, then `helm push ... oci://harbor.vn.linuxguru.net/library`
```

Point the chart at the mirror with:

```sh
--set image.repository=harbor.vn.linuxguru.net/library/ellmephant
```

Harbor holds an OCI chart and a container image under the same repository name happily (unlike
GHCR, where a chart tag colliding with an image tag is a rejected push), so the house convention
of `oci://<project>` with the chart named after the app is safe here.

## Pre-releases

A pre-release is a versioned, immutable artifact for testing ahead of a stable release.

**Automatic (preferred):** set the `Cargo.toml` version to the pre-release and merge to `main`,
exactly as for a stable release. A `-rc`/`-beta`/`-alpha` version publishes **only its own tag**
- it never moves `latest`, `1`, or `1.1`, so a broken candidate cannot leak into production
pulls. To promote it, bump the version to the final one and merge again.

**Manual:** tag a `dev` (or `main`) commit directly, when you do not want to touch the manifest:

```sh
git switch dev && git pull
git tag -a v1.1.0-rc.1 -m 'ellmephant 1.1.0-rc.1'
git push origin v1.1.0-rc.1                # -> image 1.1.0-rc.1 ONLY
```

Mark pre-release GitHub Releases accordingly so they do not claim "Latest":

```sh
gh release create v1.1.0-rc.1 --prerelease --generate-notes
```

## Hotfixes

A stable release is broken. Base the fix on the tag, not on `dev`.

```sh
git switch -c hotfix-1.0.1 v1.0.0
# ...fix, bump Cargo.toml (and Cargo.lock) to 1.0.1, commit...
git switch main && git merge --no-ff hotfix-1.0.1
git push origin main                       # -> tag v1.0.1 + image 1.0.1 published
git switch dev && git merge --no-ff hotfix-1.0.1 && git push   # keep dev from regressing
```

## Gotchas

- **A version bump is the release trigger.** A merge to `main` that does not change `Cargo.toml`
  publishes nothing. Bump to release.
- **The auto-created tag does not start a second run.** The workflow pushes `vX.Y.Z` with the
  built-in `GITHUB_TOKEN`, and GitHub does not let `GITHUB_TOKEN`-triggered pushes start new
  workflow runs. That is why the image is published in the same run; you get one run, not two.
- **Workflow permissions must allow writes.** This workflow asks for `contents: write` and
  `packages: write`. If the repository or organisation sets *Settings -> Actions -> Workflow
  permissions* to read-only, the block cannot elevate and the tag push / image push fails.
- **A tag builds the workflow as of the tagged commit.** A tag-triggered run uses the workflow
  file that exists *at that tag*. Tag a commit that already contains the workflow, or nothing
  fires.
- **Never add a raw `latest` tag rule.** `latest` is owned by `type=semver`'s `latest=auto`, which
  skips pre-releases. A hand-rolled `latest` rule would move it onto an `-rc`.
- **ghcr packages start private.** After the first publish, set visibility under Package settings
  if anonymous pulls are wanted.
- **Platforms.** Images are published for `linux/amd64` and `linux/arm64` (QEMU on the runner),
  so Apple Silicon and arm64 hosts pull a native image instead of emulating amd64.
- **A chart change without a version bump publishes nothing.** `chart-releaser` skips a version
  it has already released, silently. CI fails the PR instead - but only on a PR, so a direct push
  to `main` gets the silent no-op.
- **The chart release cannot hang off the `v*` tag.** `docker-release.yml` pushes that tag with
  `GITHUB_TOKEN`, which does not start other workflows - so `chart-release.yml` triggers on the
  `charts/**` push to `main` instead, and fires independently of app releases.
- **Pages is a one-time manual step.** No workflow can create it; see
  [Releasing the chart](#releasing-the-chart).
