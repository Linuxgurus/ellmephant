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

Everything runs on **Forgejo** (`git.vn.linuxguru.net`) from
[`.forgejo/workflows/`](.forgejo/workflows/). The GitHub Actions workflows this section used to
describe - `ci.yml`, `docker-release.yml`, `chart-release.yml` - were deleted in `6b16f5b`, so
every path and claim that mentioned them was stale.

- **`.forgejo/workflows/publish-dev.yml`** - fires on a push to `dev`, plus manual dispatch. Builds
  the runtime image on the cluster (natively amd64) and pushes it to Harbor as `edge` and
  `sha-<commit>`. It replaced the old `.githooks/post-commit` hook, which built with podman on
  whichever developer machine happened to be awake. It publishes **the image only**.
- **`.forgejo/workflows/smoke.yml`** - manual dispatch only. A diagnostic for the runner itself
  (does a job start, does `actions/checkout` resolve, can a job reach the Docker daemon), written
  when nothing on the instance had yet executed a job. Not a check of the code.
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
| push to `dev` | `edge`, `sha-<commit>` (Harbor, built on the cluster) |
| manual dispatch of `publish-dev` | the same |
| anything else | nothing |

**Nothing publishes a version.** No tag is created for you, and a push to `main` publishes nothing -
see [Publishing a release](#publishing-a-release).

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

Versions are [SemVer](https://semver.org/), and `Cargo.toml`'s `version` is the source of truth:
for `ellmephant --version`, and for the image tags. The `Cargo.lock` entry for the crate must agree.
**A version bump used to trigger a release when it reached `main`; that publisher is gone** (see
[The workflows](#the-workflows)), so a bump now only records intent - publishing it is a manual
step, and for a stable image there is currently no step at all.

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
ships a stale image. **Nothing enforces either rule any more**: the guard lived in the deleted
GitHub `ci.yml`, so check them yourself (a chart change must also carry its own `version` bump, or
nothing distinguishes the new chart from the old one). Staying on `0.x` until the values surface
settles is intentional.

## Publishing a release

**There is no automated stable release.** The workflow that read `Cargo.toml`, created the tag
`vX.Y.Z` and published `X.Y.Z` / `X.Y` / `X` / `latest` was `docker-release.yml`, deleted in
`6b16f5b`. Nothing tags a release for you, nothing moves `latest`, and `main` is not wired to
publish anything: `dev` is the only branch that publishes, and what it publishes is `edge`.

What remains is manual:

```sh
make harbor-publish        # chart -> oci://…/library/charts + image rebuilt locally (edge, sha-<short>)
make chart-push            # the chart alone
```

Neither produces a **versioned** tag - the script pushes `edge` and `sha-<short>`, in every mode. So
releasing `1.1.0` today means: bump the version, build the commit, and push it under its own tag by
hand (`podman tag` + `podman push`, once `helm registry login` has been done). Restoring a publisher
is open work, and it has to run somewhere that can reach Harbor - the **Forgejo** runner can, since
it is on the LAN; a GitHub runner could not, which is what the old workflow papered over.

## Releasing the chart

The chart lives in [`charts/ellmephant/`](charts/ellmephant/). `chart-release.yml` - which packaged
it and attached it to a GitHub Release - was deleted with the rest of the GitHub workflows, so the
chart's publish path is now **Harbor, by hand**:

```sh
make chart-push            # helm package + helm push to oci://harbor.vn.linuxguru.net/library/charts
```

To cut a chart release:

1. Bump `version` in `charts/ellmephant/Chart.yaml` (patch is almost always right), and keep
   `appVersion` equal to `Cargo.toml`'s version.
2. Commit, then push the chart: `make chart-push`.

There is no `index.yaml`, so `helm repo add` does not apply - the chart installs straight from OCI:

```sh
helm install ellmephant oci://harbor.vn.linuxguru.net/library/charts/ellmephant --version 0.2.0
```

Local sanity before you push - the same two commands the chart gate used to run in CI:

```sh
make chart-lint
make chart-template
```

## Dev publishing to Harbor

Harbor is `harbor.vn.linuxguru.net`, reachable only from the LAN - which is why the publisher runs
**inside the cluster**: the Forgejo runner can reach Harbor, a GitHub runner could not.

A push to `dev` publishes itself. `.forgejo/workflows/publish-dev.yml` builds the image natively
(amd64) and pushes `edge` + `sha-<commit>`. It is the **image only** - the chart is not part of that
workflow, so a chart change needs `make chart-push`.

To publish by hand from the LAN (chart, image, or both):

```sh
make harbor-publish                             # chart + image (image built locally with podman)
SKIP_CHART=1 make harbor-publish                # image only
make chart-push                                 # chart only
tail -f .tmp/harbor-publish.log                 # when it runs in the background
```

One-time setup for the by-hand path:

```sh
helm registry login harbor.vn.linuxguru.net     # podman keeps its own credential store
```

**`HARBOR_IMAGE_MODE=mirror` no longer works.** It pulls `ghcr.io/jdblack/ellmephant:$SOURCE_TAG`
and waits up to `WAIT_SECONDS` (1800) for that image to appear, but nothing publishes to GHCR any
more: it hangs for half an hour and exits 1. Leave the mode at its default (`build`).

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

A pre-release is a versioned, immutable artifact for testing ahead of a stable release. Both forms
below used to be a workflow; with the publisher gone they are the *intent* plus the tag you make by
hand, and the image has to be pushed under that tag yourself.

**Preferred:** set the `Cargo.toml` version to the pre-release, e.g. `1.1.0-rc.1`, so the build
records what it was - and merge to `main` as usual. Nothing publishes from that merge; push the image
under the pre-release tag only, never `latest`.

**Manual:** tag the commit directly, when you do not want to touch the manifest. The tag is a record:
nothing builds from it any more.

```sh
git switch dev && git pull
git tag -a v1.1.0-rc.1 -m 'ellmephant 1.1.0-rc.1'
git push origin v1.1.0-rc.1
```

## Hotfixes

A stable release is broken. Base the fix on the tag, not on `dev`.

```sh
git switch -c hotfix-1.1.1 v1.1.0
# ...fix, bump Cargo.toml (and Cargo.lock) to 1.1.1, commit...
git switch main && git merge --no-ff hotfix-1.1.1
git push origin main                       # publishes nothing - the publisher is gone (above)
git switch dev && git merge --no-ff hotfix-1.1.1 && git push   # keep dev from regressing
```

The fix is not released until an image carrying it is pushed to Harbor under the tag you mean to
stand behind - by hand, since nothing does it for you.

## Gotchas

- **A version bump publishes nothing.** It records intent in `Cargo.toml` (`--version`) and feeds the
  chart's `appVersion`; the publisher that used to act on it is gone.
- **There is no CI.** A PR shows zero statuses, so a green-looking PR has verified nothing. Run
  `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` (both
  feature sets), `make chart-lint` and `make chart-template` yourself - one cargo step at a time, since
  they contend on the target-dir lock.
- **Publishing needs Harbor access, which is LAN-only.** The Forgejo runner has it (repo secrets
  `HARBOR_USER`/`HARBOR_TOKEN`; the Harbor robot `robot$library+forgejo`, scoped to pull, push and
  tag-create on `library` - notably it cannot delete). A GitHub-hosted runner never will.
- **`HARBOR_IMAGE_MODE=mirror` is broken.** It waits 30 minutes for a GHCR image that no longer gets
  built, then exits 1. Leave the mode at its default (`build`).
- **The git hook is gone.** `.githooks/` was deleted with the old workflows, along with the
  `make harbor-hooks` target it needed - publishing on `dev` is the Forgejo workflow's job.
- **The chart is not published by the workflow.** `publish-dev.yml` pushes the image only; a chart
  change needs `make chart-push` by hand.
- **A chart change needs its own `version` bump, and a matching `appVersion`.** Nothing checks either
  any more - a stale `appVersion` silently ships a stale image, since `image.tag` is empty by default.
- **Chart and image live in separate Harbor repos** (`library/charts/ellmephant` vs
  `library/ellmephant`). One repo would share a tag namespace, and a chart version like `1.0.0` would
  collide with an image tag.
- **Harbor needs its own login for helm.** podman and helm keep separate credential stores, so
  `make chart-push` fails until `helm registry login harbor.vn.linuxguru.net` has been run once.
- **Platforms.** `edge`/`sha-*` are built **amd64 only** - the cluster is amd64, and an arm64 half
  would double an emulated local build for no user. A by-hand `make harbor-publish` on Apple Silicon
  is that same slow, emulated build.
- **`charts/ellmephant/values.yaml` defaults to a ghcr image.** Nothing in this repository publishes
  to ghcr any more; the cluster overrides `image.repository` to Harbor. Until the public registry is
  decided, treat that default as stale.
- **`artifacthub.io/images` is no longer guarded.** The check that kept its tag equal to
  `Cargo.toml`'s version lived in the deleted `ci.yml`, so an unbumped entry now ships a stale
  reference.
