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
- [Dev publishing to Harbor](#dev-publishing-to-harbor)
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
  every value shape, and the two version guards). Gate, not publisher.
- **`.github/workflows/docker-release.yml`** - the image publisher. Fires on a push to `main`, a
  push to `dev`, a `v*` tag, or manually. It reads the version from `Cargo.toml` and pushes the
  image to `ghcr.io/<owner>/ellmephant`.
- **`.github/workflows/chart-release.yml`** - the chart publisher. Fires on a push to `main` that
  touches `charts/**`. Packages each chart and attaches it to a GitHub Release tagged
  `<chart>-<version>` (`--latest=false`, so a chart release never claims the repository's "Latest
  release"). Release assets only: no Pages, no `gh-pages`, no registry, no third-party action.

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
must touch `Chart.yaml` (otherwise the `<chart>-<version>` release already exists, the job skips
it, and nothing publishes). Staying on `0.x` until the values surface settles is intentional.

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

The chart lives in [`charts/ellmephant/`](charts/ellmephant/).
[`chart-release.yml`](.github/workflows/chart-release.yml) packages each chart under `charts/` and
attaches it to a GitHub Release tagged `<chart>-<version>`. That is the whole publish path:
`helm package` plus the preinstalled `gh` CLI, so there is **nothing to set up by hand** - no
Pages, no `gh-pages` branch, no registry.

To cut a chart release:

1. Bump `version` in `charts/ellmephant/Chart.yaml` (patch is almost always right).
2. Commit and merge into `main`.

The workflow skips a `<chart>-<version>` release that already exists, so it is a no-op unless the
version moved. There is no `index.yaml`, so `helm repo add` does not apply - users install the
asset directly:

```sh
helm install ellmephant \
  https://github.com/jdblack/ellmephant/releases/download/ellmephant-0.2.1/ellmephant-0.2.1.tgz
```

Local sanity before you push (also what CI runs):

```sh
make chart-lint
make chart-template
```

## Dev publishing to Harbor

Harbor is `harbor.vn.linuxguru.net`, reachable only from the LAN, so a GitHub runner cannot push
to it: this step is local by necessity. `dev` is wired to publish itself.

On a commit to `dev`, `.githooks/post-commit` backgrounds `scripts/harbor-publish.sh`, which:

1. packages the chart and `helm push`es it to `oci://harbor.vn.linuxguru.net/library/charts`;
2. builds the `linux/amd64` image with `podman` and pushes it to Harbor as `edge` + `sha-<short>`.

Building locally is the normal process: the publish owns its own build, so a green CI run is never
on the critical path and there is nothing to wait for. (`podman build` on Apple Silicon is an
emulated amd64 rebuild, so it is slow - but self-contained.) `HARBOR_IMAGE_MODE=mirror` pulls the
CI-built GHCR image instead (fast, but couples the publish to CI); `HARBOR_IMAGE_MODE=skip` skips
the image.

One-time, per clone:

```sh
make harbor-hooks                               # git config core.hooksPath .githooks
helm registry login harbor.vn.linuxguru.net     # podman already holds its own credential
```

Watch it, skip it, or run it by hand:

```sh
tail -f .tmp/harbor-publish.log
SKIP_HARBOR=1 git commit -m '...'               # one commit, no publish
make harbor-publish                             # run it now
SOURCE_TAG=1.1.0 HARBOR_IMAGE_MODE=mirror make harbor-publish   # mirror a released main image
```

Point the cluster at the mirror with:

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
git switch -c hotfix-1.1.1 v1.1.0
# ...fix, bump Cargo.toml (and Cargo.lock) to 1.1.1, commit...
git switch main && git merge --no-ff hotfix-1.1.1
git push origin main                       # -> tag v1.1.1 + image 1.1.1 published
git switch dev && git merge --no-ff hotfix-1.1.1 && git push   # keep dev from regressing
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
- **Platforms.** Versioned releases are published for `linux/amd64` and `linux/arm64` (QEMU on the
  runner), so Apple Silicon and arm64 hosts pull a native image instead of emulating amd64.
  `dev`/`edge` builds are **amd64 only** - the cluster is amd64, and building the arm64 half too
  would double a local, emulated build for no user.
- **A chart change without a version bump publishes nothing.** `chart-release.yml` skips a
  `<chart>-<version>` release that already exists, silently. CI fails the PR instead - but only on
  a PR, so a direct push to `main` gets the silent no-op.
- **The chart release cannot hang off the `v*` tag.** `docker-release.yml` pushes that tag with
  `GITHUB_TOKEN`, which does not start other workflows - so `chart-release.yml` triggers on the
  `charts/**` push to `main` instead, and fires independently of app releases.
- **Chart releases are release assets, not a Helm repo.** There is no `index.yaml`, so
  `helm repo add` does not apply: install by asset URL, or from the Harbor OCI mirror.
- **Chart releases must not claim "Latest".** `--latest=false` keeps a chart release from taking
  the repository's "Latest release" badge away from the app's `v*` releases.
- **The Harbor hook is per clone.** `core.hooksPath` is local config, not committed - run
  `make harbor-hooks` in a fresh clone or the hook silently does not exist.
- **The Harbor hook builds locally, so it never waits on CI.** The image is built and pushed from
  the working tree, so it does not depend on a green Actions run (nor does it require the commit to
  be pushed first) - just expect a slow, emulated amd64 build on Apple Silicon. `SKIP_HARBOR=1`
  skips the hook.
- **Harbor needs its own login for helm.** podman and helm keep separate credential stores; the
  hook's chart push fails until `helm registry login harbor.vn.linuxguru.net` has been run once.
- **`artifacthub.io/images` is guarded too.** CI asserts the tag it names equals `Cargo.toml`'s
  version, so a bump that misses it fails the build instead of shipping a stale reference.
