#!/usr/bin/env bash
# Publish the current commit to Harbor - the in-cluster registry that GitHub runners cannot
# reach (it is LAN-only), which is the whole reason this runs locally.
#
# Two artifacts go up, under SEPARATE repos so their tags never collide:
#   1. the chart, as an OCI artifact at `.../library/charts/ellmephant` (pushed immediately);
#   2. the image at `.../library/ellmephant`, tagged `edge` and `sha-<short>`.
#
# The image is MIRRORED from GHCR by default rather than built here. CI already built the exact
# linux/amd64 artifact for this commit (`sha-<short>`), so pulling it takes seconds, whereas
# building it on Apple Silicon is an emulated Rosetta rebuild that takes half an hour.
# HARBOR_IMAGE_MODE=build builds locally instead (no CI, no GHCR - but slow).
#
# Run in the background by .githooks/post-commit on `dev`; also by hand:
#   make harbor-publish
#   SOURCE_TAG=1.0.1 make harbor-publish      # mirror a released main image
#
# Needs a one-time `helm registry login harbor.vn.linuxguru.net` (podman already holds its own).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

HARBOR=${HARBOR:-harbor.vn.linuxguru.net/library}
HARBOR_IMAGE=${HARBOR_IMAGE:-$HARBOR/ellmephant}
# The chart gets its own nested repo so its tags never share a namespace with the image tags
# (a chart version like `1.0.0` would otherwise collide with the image tag `1.0.0`).
HARBOR_CHART=${HARBOR_CHART:-$HARBOR/charts}
GHCR_IMAGE=${GHCR_IMAGE:-ghcr.io/jdblack/ellmephant}
CHART=${CHART:-charts/ellmephant}
PLATFORM=${PLATFORM:-linux/amd64}
IMAGE_MODE=${HARBOR_IMAGE_MODE:-mirror}   # mirror | build | skip
WAIT_SECONDS=${WAIT_SECONDS:-1800}
SHORT=$(git rev-parse HEAD | cut -c1-7)
SOURCE_TAG=${SOURCE_TAG:-sha-$SHORT}

log() { printf '[harbor] %s\n' "$*"; }

if [ "${SKIP_HARBOR:-0}" = "1" ]; then
  log "SKIP_HARBOR=1 - nothing to do"
  exit 0
fi

# --- chart ------------------------------------------------------------------------------------
if [ "${SKIP_CHART:-0}" = "1" ]; then
  log "SKIP_CHART=1 - chart not pushed"
else
  rm -rf .tmp/harbor-chart && mkdir -p .tmp/harbor-chart
  log "packaging $CHART"
  helm package "$CHART" --destination .tmp/harbor-chart > /dev/null
  pkg=$(ls .tmp/harbor-chart/*.tgz)
  log "pushing $(basename "$pkg") -> oci://$HARBOR_CHART"
  helm push "$pkg" "oci://$HARBOR_CHART"
fi

# --- image ------------------------------------------------------------------------------------
case "$IMAGE_MODE" in
  skip)
    log "HARBOR_IMAGE_MODE=skip - image not pushed"
    exit 0
    ;;
  build)
    log "building $PLATFORM image locally (slow under emulation)"
    podman build --format docker --platform "$PLATFORM" \
      -t "$HARBOR_IMAGE:edge" -t "$HARBOR_IMAGE:sha-$SHORT" .
    ;;
  mirror)
    log "waiting for GHCR to build $GHCR_IMAGE:$SOURCE_TAG (up to ${WAIT_SECONDS}s)"
    deadline=$(( $(date +%s) + WAIT_SECONDS ))
    until podman pull --quiet --platform "$PLATFORM" "$GHCR_IMAGE:$SOURCE_TAG" > /dev/null 2>&1; do
      if [ "$(date +%s)" -ge "$deadline" ]; then
        log "timed out waiting for $SOURCE_TAG - has this commit been pushed to dev?"
        exit 1
      fi
      sleep 15
    done
    podman tag "$GHCR_IMAGE:$SOURCE_TAG" "$HARBOR_IMAGE:edge"
    podman tag "$GHCR_IMAGE:$SOURCE_TAG" "$HARBOR_IMAGE:sha-$SHORT"
    ;;
  *)
    log "unknown HARBOR_IMAGE_MODE=$IMAGE_MODE (want mirror|build|skip)"
    exit 1
    ;;
esac

log "pushing $HARBOR_IMAGE:sha-$SHORT + :edge"
podman push "$HARBOR_IMAGE:sha-$SHORT"
podman push "$HARBOR_IMAGE:edge"
log "done ($SHORT)"
