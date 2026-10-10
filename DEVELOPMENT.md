# Development & releases

How work moves through this repository and how releases are cut. For internals see
[`ENGINEERING.md`](ENGINEERING.md); for build/install see [`INSTALL.md`](INSTALL.md); for the
normative specification see [`SPEC.md`](SPEC.md).

- [Branch model](#branch-model)
- [The two workflows](#the-two-workflows)
- [Day to day: when to commit and push](#day-to-day-when-to-commit-and-push)
- [Versioning](#versioning)
- [Publishing a release](#publishing-a-release)
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

## The two workflows

- **`.github/workflows/ci.yml`** - runs on every push to `main` and every pull request. Format,
  clippy, tests (both feature sets), and a no-push Docker build. Gate, not publisher.
- **`.github/workflows/docker-release.yml`** - the publisher. Fires on a push to `main`, a push
  to `dev`, a `v*` tag, or manually. It reads the version from `Cargo.toml` and pushes the image
  to `ghcr.io/<owner>/ellmephant`.

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
- **Platforms.** Images are `linux/amd64` only. Add `linux/arm64` in `docker-release.yml` if
  native arm pulls are needed.
