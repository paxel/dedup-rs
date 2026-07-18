#!/usr/bin/env bash
# Build the release .deb(s) and (optionally) upload them to your host.
#
# Used two ways (see docs/hosting.md): the sr.ht build runs it on a tag to push
# each freshly built .deb, and you can run it locally for a CI-less / hotfix
# publish. Locally it builds the host-architecture .deb and uploads everything in
# dist/ (drop the sr.ht arm64 artifact in first for a full two-arch release).
#
# Upload backend is selected by DEDUP_UPLOAD (default: pi):
#
#   pi      WebDAV-PUT (or scp) the .deb(s) to a self-hosted server (dufs on a
#           Pi). The Pi rebuilds and signs its apt index automatically on upload
#           (systemd path watcher) — nothing here has to touch the repo layout.
#           - dufs:   set DEDUP_PI_URL=https://files.example.org/apt/pool/main/d/dedup/
#                     and DEDUP_PI_TOKEN=<upload token>  (dufs --auth, over HTTPS)
#           - scp:    set DEDUP_PI_SCP=user@host:/srv/dedup/apt/pool/main/d/dedup/
#   none    build only, do not upload.
#
# DEDUP_PI_TOKEN is a scoped upload token (the dufs pool-write credential) — set
# it in your shell for a local run, or as a builds.sr.ht secret for CI. It only
# grants writes to the pool path and can be revoked/rotated any time.
#
# Usage:
#   packaging/release.sh                 # build host-arch release deb + upload
#   DEDUP_UPLOAD=none packaging/release.sh
#   packaging/release.sh --no-build      # upload whatever is already in dist/
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

DO_BUILD=1
[ "${1:-}" = "--no-build" ] && DO_BUILD=0

if [ "$DO_BUILD" -eq 1 ]; then
  ./packaging/mkdeb.sh --release
fi

backend="${DEDUP_UPLOAD:-pi}"
shopt -s nullglob
debs=(dist/*.deb)
shopt -u nullglob
[ ${#debs[@]} -gt 0 ] || { echo "release: no .deb files in dist/" >&2; exit 1; }

upload_pi() {
  if [ -n "${DEDUP_PI_URL:-}" ]; then
    local base="${DEDUP_PI_URL%/}"
    for f in "${debs[@]}"; do
      echo "release: PUT $(basename "$f") → $base/"
      # dufs authenticates the upload path with a single user + token over
      # HTTPS (HTTP basic). The token is the only credential we send.
      curl -fSs ${DEDUP_PI_TOKEN:+-u "upload:$DEDUP_PI_TOKEN"} \
        -T "$f" "$base/$(basename "$f")"
    done
  elif [ -n "${DEDUP_PI_SCP:-}" ]; then
    echo "release: scp ${#debs[@]} file(s) → $DEDUP_PI_SCP"
    scp "${debs[@]}" "$DEDUP_PI_SCP"
  else
    echo "release: set DEDUP_PI_URL=https://…/apt/pool/main/d/dedup/ (+DEDUP_PI_TOKEN) or DEDUP_PI_SCP=user@host:/path/" >&2
    exit 1
  fi
}

case "$backend" in
  none)   echo "release: built ${#debs[@]} package(s); upload skipped (DEDUP_UPLOAD=none)." ;;
  pi)     upload_pi ;;
  *)      echo "release: unknown DEDUP_UPLOAD='$backend' (use pi|none)" >&2; exit 2 ;;
esac

echo "release: done."
