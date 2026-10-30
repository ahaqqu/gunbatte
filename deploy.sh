#!/usr/bin/env bash
# deploy.sh — build GUNBATTE ROYALE and hand the code artifact to the box's
# shared applier.
#
# Host configuration (the systemd unit, nginx site, sudoers grant and deploy
# contract) lives in ahaqqu/vps → gunbatte/. This repository only builds and
# ships the code tree:
#
#   gunbatte-server
#   viewer/dist/
#   website/
#
# Those contents are tarred at the root, uploaded as the `gunbatte-deploy`
# identity to /srv/vps-incoming/gunbatte.tar.gz, and applied with
# `sudo -n /usr/local/sbin/vps-deploy gunbatte`. The helper preserves
# ladder.db and replays/ across the swap, publishes website/ to the
# world-readable web root, restarts gunbatte.service and health-checks
# /ladder. This script never writes the target tree, never touches state and
# never restarts the unit directly.
#
# There is no --bootstrap path here. The one-time host bootstrap (applier,
# deploy identities, /etc/gunbatte/proxy.env, the key append) is an owner
# action in ahaqqu/vps (docs/DEPLOY.md, docs/ACCESS.md, runbook/BOOTSTRAP.md).
# Until that has happened this deploy fails closed at the helper.
#
# Configuration comes from the environment first, then the repository's
# GitHub Actions variables (the same source CI reads):
#   VPS_USER              deploy identity (default: gunbatte-deploy)
#   VPS_HOST              box address
#   GUNBATTE_GAME_HOST    public game host used by the website CTAs
#
# Usage:
#   ./deploy.sh
#   ./deploy.sh --package-only [--skip-build] [--out FILE] [--repo DIR]
#
#   --package-only  build (unless --skip-build) and tar the artifact, but do
#                   not upload or apply it
#   --skip-build    reuse target/release/gunbatte-server and viewer/dist
#   --out FILE      write the archive to FILE (default: a temp file)
#   --repo DIR      repo root to build and stage from (default: this script's
#                   directory)
set -euo pipefail

APP=gunbatte
VPS_USER_DEFAULT=gunbatte-deploy
UPLOAD_PATH=/srv/vps-incoming/gunbatte.tar.gz
APPLIER=/usr/local/sbin/vps-deploy

die() {
    printf '!! %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
usage: ./deploy.sh [--package-only] [--skip-build] [--out FILE] [--repo DIR]

Builds the GUNBATTE ROYALE code artifact (gunbatte-server, viewer/dist/,
website/), uploads it as gunbatte-deploy to
/srv/vps-incoming/gunbatte.tar.gz, and runs the box's shared
`sudo -n /usr/local/sbin/vps-deploy gunbatte` helper.

Options:
  --package-only  build and tar the artifact; do not upload or deploy
  --skip-build    reuse target/release/gunbatte-server and viewer/dist
  --out FILE      write the archive to FILE (default: a temp file)
  --repo DIR      repo root to build and stage from (default: this script's directory)
EOF
}

REPO="$(cd "$(dirname "$0")" && pwd)"
PACKAGE_ONLY=0
SKIP_BUILD=0
OUT=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --package-only)
            PACKAGE_ONLY=1
            ;;
        --skip-build)
            SKIP_BUILD=1
            ;;
        --out)
            shift
            [ "$#" -gt 0 ] || die "--out needs a path"
            OUT="$1"
            ;;
        --out=*)
            OUT="${1#*=}"
            [ -n "$OUT" ] || die "--out needs a path"
            ;;
        --repo)
            shift
            [ "$#" -gt 0 ] || die "--repo needs a directory"
            [ -d "$1" ] || die "--repo is not a directory: $1"
            REPO="$(cd "$1" && pwd)"
            ;;
        --repo=*)
            [ -d "${1#*=}" ] || die "--repo is not a directory: ${1#*=}"
            REPO="$(cd "${1#*=}" && pwd)"
            ;;
        --bootstrap)
            die "this repository has no bootstrap path. The one-time VPS bootstrap is an owner action in ahaqqu/vps (docs/DEPLOY.md, docs/ACCESS.md, runbook/BOOTSTRAP.md); this script only uploads the artifact and runs vps-deploy $APP."
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (see --help)"
            ;;
    esac
    shift
done

# Read a deploy value from the environment, then the repository's GitHub
# Actions variables. `eval` is used because bash's indirect expansion cannot
# carry a default modifier.
from_env_or_gh() { # <name> <default>
    local name="$1" default="${2:-}" value=""
    eval "value=\"\${$name:-}\""
    if [ -z "$value" ] && command -v gh >/dev/null 2>&1; then
        value="$(gh variable get "$name" 2>/dev/null || true)"
    fi
    [ -n "$value" ] || value="$default"
    printf '%s\n' "$value"
}

VPS_USER="$(from_env_or_gh VPS_USER "$VPS_USER_DEFAULT")"
GUNBATTE_GAME_HOST="$(from_env_or_gh GUNBATTE_GAME_HOST "")"

[ -n "$GUNBATTE_GAME_HOST" ] ||
    die "GUNBATTE_GAME_HOST is not set — export it or add it as a GitHub repository variable (Settings → Secrets and variables → Actions → Variables)"
case "$GUNBATTE_GAME_HOST" in
    *://* | */*)
        die "GUNBATTE_GAME_HOST must be a bare hostname (no scheme or path): $GUNBATTE_GAME_HOST"
        ;;
esac

if [ "$PACKAGE_ONLY" -ne 1 ]; then
    [ "$VPS_USER" = "$VPS_USER_DEFAULT" ] ||
        die "VPS_USER must be $VPS_USER_DEFAULT, the identity named by ahaqqu/vps gunbatte/deploy.conf; got '$VPS_USER'. The old shared deploy identity has no vps-deploy grant and must not be used."
    VPS_HOST="$(from_env_or_gh VPS_HOST "")"
    [ -n "$VPS_HOST" ] ||
        die "VPS_HOST is not set — export it or add it as a GitHub repository variable (Settings → Secrets and variables → Actions → Variables)"
fi

if [ "$SKIP_BUILD" -ne 1 ]; then
    echo "▶ building viewer (wasm-pack + vite)…"
    ( cd "$REPO" && make viewer )
    echo "▶ building gunbatte-server (release)…"
    ( cd "$REPO" && cargo build --release -p gunbatte-server )
fi

binary="$REPO/target/release/gunbatte-server"
viewer_dist="$REPO/viewer/dist"
[ -x "$binary" ] ||
    die "missing $binary — run without --skip-build, or build it with: cargo build --release -p gunbatte-server"
[ -f "$viewer_dist/index.html" ] ||
    die "missing $viewer_dist/index.html — run without --skip-build, or build it with: make viewer"
[ -d "$REPO/website" ] || die "missing $REPO/website"

stage="$(mktemp -d)"
archive=""
archive_is_temp=0
cleanup() {
    rm -rf "$stage"
    if [ "$archive_is_temp" -eq 1 ] && [ -n "$archive" ]; then
        rm -f "$archive"
    fi
}
trap cleanup EXIT

install -m 0755 "$binary" "$stage/gunbatte-server"
mkdir -p "$stage/viewer"
cp -R "$viewer_dist" "$stage/viewer/dist"
mkdir -p "$stage/website"
cp -R "$REPO/website/." "$stage/website/"

# The repo copy of the website keeps the localhost default for local dev;
# the deployed copy points its CTAs at the real game host.
find "$stage/website" -type f -name '*.html' -print0 |
    while IFS= read -r -d '' page; do
        sed "s|http://127\.0\.0\.1:8321|https://$GUNBATTE_GAME_HOST|g" "$page" >"$page.tmp"
        mv "$page.tmp" "$page"
    done

# State and host configuration must never ride along in a code artifact.
for forbidden in ladder.db replays provision; do
    if [ -e "$stage/$forbidden" ]; then
        die "refusing to package state or host configuration: $forbidden"
    fi
done

if [ -n "$OUT" ]; then
    archive="$OUT"
    mkdir -p "$(dirname "$archive")"
else
    archive="$(mktemp "${TMPDIR:-/tmp}/gunbatte-deploy.XXXXXX")"
    archive_is_temp=1
fi

tar -czf "$archive" -C "$stage" .

# Assert the artifact before it leaves the machine: relative paths only, the
# three contract paths present, state and host configuration absent.
entries="$(tar -tzf "$archive" | sed 's|^\./||')"
for required in gunbatte-server viewer/dist/index.html website/index.html; do
    printf '%s\n' "$entries" | grep -qxF "$required" ||
        die "artifact is missing $required"
done
if printf '%s\n' "$entries" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
    die "artifact contains an absolute or parent-relative path"
fi
if printf '%s\n' "$entries" | grep -Eq '(^|/)(ladder\.db|replays|provision|target|\.git)(/|$)'; then
    die "artifact contains state or host configuration (ladder.db, replays, provision, target or .git)"
fi

if [ "$PACKAGE_ONLY" -eq 1 ]; then
    # Keep a temp archive so the caller can inspect or upload it.
    archive_is_temp=0
    echo "✓ packaged $archive"
    exit 0
fi

SSH_OPTS=(-o ServerAliveInterval=15 -o ServerAliveCountMax=12 -o ConnectTimeout=20)

echo "▶ uploading to $VPS_USER@$VPS_HOST:$UPLOAD_PATH …"
if ! rsync -a --partial --timeout=120 -e "ssh ${SSH_OPTS[*]}" \
    "$archive" "$VPS_USER@$VPS_HOST:$UPLOAD_PATH"; then
    die "upload failed — nothing was applied on the box"
fi

echo "▶ applying: sudo -n $APPLIER $APP"
if ! ssh "${SSH_OPTS[@]}" "$VPS_USER@$VPS_HOST" 'sudo -n /usr/local/sbin/vps-deploy gunbatte'; then
    die "vps-deploy $APP rejected the deploy; the box kept the previous tree and its preserved state. If this is the first run, the owner must bootstrap ahaqqu/vps and append this repository's key to $VPS_USER (ahaqqu/vps docs/ACCESS.md, runbook/BOOTSTRAP.md §8a)."
fi

echo "✔ deployed $APP through $VPS_USER@$VPS_HOST"
