# Development & releases

How work moves through this repository and how releases are cut. For internals see
[`ENGINEERING.md`](ENGINEERING.md); for build/install see [`INSTALL.md`](INSTALL.md); for the
normative specification see [`SPEC.md`](SPEC.md).

- [Branch model](#branch-model)
- [The two workflows](#the-two-workflows)
- [Day to day: when to commit and push](#day-to-day-when-to-commit-and-push)
- [Versioning](#versioning)
- [Cutting a release](#cutting-a-release)
- [Pre-releases](#pre-releases)
- [Hotfixes](#hotfixes)
- [Gotchas](#gotchas)

## Branch model

| Branch | Role | Publishing |
|---|---|---|
| `main` | Stable. Always reflects the last release. | None. CI only. |
| `dev` | Integration. Where work lands first. | `edge`, `sha-<commit>` |
| topic branches | One feature/fix, off `dev`, merged back into `dev` | None |

`main` is a merge target, never a workspace. Do not commit to it directly: land work on `dev`,
then merge `dev` into `main` when a release is due. A push to `main` runs the test suite but
publishes no image - images come from tags (below).

## The two workflows

- **`.github/workflows/ci.yml`** - runs on every push to `main` and every pull request. Format,
  clippy, tests (both feature sets), and a no-push Docker build. Gate, not publisher.
- **`.github/workflows/docker-release.yml`** - the publisher. Fires on a push to `dev`, on a
  `v*` tag, or manually. Builds and pushes the image to `ghcr.io/<owner>/ellmephant`.

What the publisher emits, per event:

| Event | Ref | Image tag(s) | Carries a version? |
|---|---|---|---|
| push to `dev` | `refs/heads/dev` | `edge`, `sha-<commit>` | no |
| push tag `v1.1.0-rc.1` | `refs/tags/v*` | `1.1.0-rc.1` | pre-release |
| push tag `v1.1.0` | `refs/tags/v*` | `1.1.0`, `1.1`, `1`, `latest` | stable |
| push to `main` | `refs/heads/main` | *(nothing)* | - |
| manual dispatch | any branch | `edge`, `sha-<commit>` | no |

`edge` is a **moving** tag: it always points at the newest `dev` build. It is not a version and
the `-dev` suffix is deliberately not used - a per-commit version would be meaningless and would
churn. `sha-<commit>` pins one exact build if you need to go back.

## Day to day: when to commit and push

- **Commit and push to `dev` freely.** Small, self-contained commits. Every push runs CI and
  republishes `edge`. If it builds and the tests pass, it may go in.
- **Merge `dev` -> `main` only when a release is due.** This is a deliberate act, not a sync.
  It marks the code release-grade and runs CI; it does **not** publish an image.
- **Tag only what you intend to ship.** A tag is immutable and is the trigger for a published
  image. Tag at milestones, not per commit.

Feature work is one topic branch off `dev`, PR'd back into `dev`:

```sh
git switch dev && git pull
git switch -c fix-search-ranking
git commit -m 'fix(search): rank by distance, not insertion order'
git push -u origin fix-search-ranking      # open a PR into dev
```

## Versioning

Versions are [SemVer](https://semver.org/), and `Cargo.toml`'s `version` is the source of truth
for `ellmephant --version`. The `Cargo.lock` entry for the crate must agree.

The tag and the manifest must match: tag `v1.1.0` on a commit whose `Cargo.toml` says `1.1.0`.
Bump the manifest in the same commit you are about to tag, then tag that commit.

- `MAJOR` - incompatible change to the CLI, the MCP tool surface, or the on-disk schema.
- `MINOR` - new, backwards-compatible capability.
- `PATCH` - backwards-compatible bug fix.

Pre-release suffixes are ordered: `1.1.0-alpha.1 < 1.1.0-beta.1 < 1.1.0-rc.1 < 1.1.0`.

## Cutting a release

1. Bump `Cargo.toml` (and `Cargo.lock`) to the release version; commit it on `dev`.
2. Merge `dev` into `main`; push.
3. Verify CI is green on `main`.
4. Create an annotated tag on the `main` commit and push it.

```sh
# release version already bumped and on main
git switch main && git pull
git tag -a v1.1.0 -m 'ellmephant 1.1.0'
git push origin v1.1.0                     # -> image 1.1.0, 1.1, 1, latest
```

5. Mark the GitHub Release (optional but conventional):

```sh
gh release create v1.1.0 --title 'v1.1.0' --generate-notes
```

## Pre-releases

A pre-release is a versioned, immutable artifact for testing ahead of a stable release. Cut it
when you want testers on a specific build, not on the moving `edge`.

```sh
# off dev (or a main commit about to ship)
git switch dev && git pull
git tag -a v1.1.0-rc.1 -m 'ellmephant 1.1.0-rc.1'
git push origin v1.1.0-rc.1                # -> image 1.1.0-rc.1 ONLY
```

A pre-release tag publishes **only its own tag**. It never moves `latest`, `1`, or `1.1` - those
stay on the last stable release, so a broken candidate cannot leak into production pulls. To
promote a candidate, merge its commit to `main` and tag the same version without the suffix.

```sh
gh release create v1.1.0-rc.1 --prerelease --generate-notes
```

## Hotfixes

A stable release is broken. Do not touch `dev` first; base the fix on the tag.

```sh
git switch -c hotfix-1.0.1 v1.0.0
# ...fix, bump Cargo.toml to 1.0.1, commit...
git switch main && git merge --no-ff hotfix-1.0.1
git push origin main
git tag -a v1.0.1 -m 'ellmephant 1.0.1' && git push origin v1.0.1
git switch dev && git merge --no-ff hotfix-1.0.1 && git push   # keep dev from regressing
```

## Gotchas

- **A tag builds the workflow as of the tagged commit.** A tag-triggered run uses the workflow
  file that exists *at that tag*. Tag a commit that already contains the workflow, or nothing
  fires.
- **Never add a raw `latest` tag rule.** `latest` is owned by `type=semver`'s `latest=auto`, which
  skips pre-releases. A hand-rolled `latest` rule would move it onto an `-rc`.
- **`main` is silent by design.** Merging to `main` publishes nothing. Tag to publish.
- **ghcr packages start private.** After the first publish, set visibility under Package settings
  if anonymous pulls are wanted.
- **Platforms.** Images are `linux/amd64` only. Add `linux/arm64` in
  `docker-release.yml` if native arm pulls are needed.
