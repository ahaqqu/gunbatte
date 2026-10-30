#!/usr/bin/env bash
# tests/deploy-contract.sh — pin the deploy path to the ahaqqu/vps contract.
#
# The deploy's trust property is the artifact shape: ahaqqu/vps
# `gunbatte/deploy.conf` expects a tar.gz whose root is exactly
# `gunbatte-server`, `viewer/dist/` and `website/`, uploaded as
# `gunbatte-deploy` and applied only through `vps-deploy gunbatte`. A wrong
# layout, leaked state, or a second writer fails silently in production (the
# helper rejects it, or worse, a hand-rolled path writes the tree), so this
# test stages a fake repo and checks the produced tar plus the script's fixed
# identity/path/command.
set -euo pipefail

cd "$(dirname "$0")/.."
repo="$PWD"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() {
    printf 'not ok - %s\n' "$*" >&2
    exit 1
}
ok() {
    printf 'ok - %s\n' "$*"
}

# A fake repo with every build output the deploy needs, plus the state and
# host-config decoys that must never be packaged.
fake="$tmp/repo"
mkdir -p "$fake/target/release" \
    "$fake/viewer/dist/assets" \
    "$fake/website/characters" \
    "$fake/replays" \
    "$fake/provision/vps"
printf '#!/bin/sh\nexit 0\n' >"$fake/target/release/gunbatte-server"
chmod 755 "$fake/target/release/gunbatte-server"
printf '<html>viewer</html>\n' >"$fake/viewer/dist/index.html"
printf 'console.log("asset")\n' >"$fake/viewer/dist/assets/app.js"
printf 'body {}\n' >"$fake/website/style.css"
cat >"$fake/website/index.html" <<'HTML'
<a href="http://127.0.0.1:8321/">PLAY</a>
<a href="http://127.0.0.1:8321/ladder">LADDER</a>
HTML
cat >"$fake/website/characters/index.html" <<'HTML'
<a href="http://127.0.0.1:8321/">PLAY</a>
HTML
printf 'state\n' >"$fake/ladder.db"
printf '{}\n' >"$fake/replays/match-1.json"
printf '#!/bin/sh\necho old writer\n' >"$fake/provision/vps/apply.sh"

archive="$tmp/gunbatte.tar.gz"
VPS_USER=gunbatte-deploy GUNBATTE_GAME_HOST=play.example.test \
    "$repo/deploy.sh" --package-only --skip-build --repo "$fake" --out "$archive" >/dev/null ||
    fail "deploy.sh --package-only failed"
[ -f "$archive" ] || fail "no archive produced"

entries="$(tar -tzf "$archive" | sed 's|^\./||')"

for required in \
    gunbatte-server \
    viewer/dist/index.html \
    viewer/dist/assets/app.js \
    website/index.html \
    website/characters/index.html \
    website/style.css; do
    printf '%s\n' "$entries" | grep -qxF "$required" ||
        fail "artifact is missing $required"
done
ok "artifact root has gunbatte-server, viewer/dist/ and website/"

for forbidden in ladder.db replays provision; do
    if printf '%s\n' "$entries" | grep -Eq "(^|/)$forbidden(/|$)"; then
        fail "artifact leaked $forbidden"
    fi
done
if printf '%s\n' "$entries" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
    fail "artifact contains an absolute or parent-relative path"
fi
ok "artifact excludes ladder.db, replays/ and provision/, and has no leading directory"

mkdir "$tmp/out"
tar -xzf "$archive" -C "$tmp/out"
[ -x "$tmp/out/gunbatte-server" ] || fail "gunbatte-server is not executable"
grep -q 'https://play.example.test/' "$tmp/out/website/index.html" ||
    fail "website CTA was not rewritten to GUNBATTE_GAME_HOST"
grep -q 'https://play.example.test/ladder' "$tmp/out/website/index.html" ||
    fail "website ladder CTA was not rewritten"
grep -q 'https://play.example.test/' "$tmp/out/website/characters/index.html" ||
    fail "nested website page CTA was not rewritten"
if grep -Rq '127\.0\.0\.1:8321' "$tmp/out/website"; then
    fail "a localhost CTA survived into the artifact"
fi
ok "website CTAs point at the deployed game host"

grep -q '/srv/vps-incoming/gunbatte.tar.gz' "$repo/deploy.sh" ||
    fail "deploy.sh does not upload to the contract's fixed path"
grep -q 'sudo -n /usr/local/sbin/vps-deploy gunbatte' "$repo/deploy.sh" ||
    fail "deploy.sh does not invoke the shared applier"
grep -q 'VPS_USER_DEFAULT=gunbatte-deploy' "$repo/deploy.sh" ||
    fail "deploy.sh does not default to the gunbatte-deploy identity"
if grep -nE 'systemctl[[:space:]]+(restart|start|reload)' "$repo/deploy.sh"; then
    fail "deploy.sh restarts a unit directly"
fi
if grep -nE 'rsync.*(/srv/gunbatte|/home/kajianq-deploy)' "$repo/deploy.sh"; then
    fail "deploy.sh rsyncs into the target tree"
fi
ok "deploy.sh is upload + vps-deploy only"

if VPS_USER=gunbatte-deploy GUNBATTE_GAME_HOST=play.example.test \
    "$repo/deploy.sh" --bootstrap >"$tmp/bootstrap.out" 2>&1; then
    fail "deploy.sh accepted --bootstrap"
fi
grep -q 'no bootstrap path' "$tmp/bootstrap.out" ||
    fail "--bootstrap did not explain where the host bootstrap lives"
ok "there is no --bootstrap second writer"

if VPS_USER=kajianq-deploy VPS_HOST=box.example GUNBATTE_GAME_HOST=play.example.test \
    "$repo/deploy.sh" --skip-build --repo "$fake" --out "$tmp/old-identity.tar.gz" \
    >"$tmp/old-identity.out" 2>&1; then
    fail "deploy.sh accepted the retired shared deploy identity"
fi
grep -q 'must be gunbatte-deploy' "$tmp/old-identity.out" ||
    fail "the old identity was not rejected with a clear message"
ok "the retired shared identity fails closed"

if grep -R -n --exclude-dir=.git 'provision/vps' \
    "$repo/deploy.sh" "$repo/README.md" "$repo/AGENTS.md" "$repo/LIMITS.md" \
    "$repo/docs" "$repo/.github/workflows" 2>/dev/null; then
    fail "an old provision/vps reference survived"
fi
ok "no old provision/vps reference survives in the deploy docs"

grep -q 'run: ./deploy.sh' "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow does not run ./deploy.sh"
grep -q 'VPS_DEPLOY_SSH_KEY' "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow lost the deploy-key secret"
grep -q 'VPS_KNOWN_HOSTS' "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow lost the known-hosts secret"
grep -q "vars.VPS_USER || 'gunbatte-deploy'" "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow does not default VPS_USER to gunbatte-deploy"
ok "deploy workflow uses the shared identity/path and keeps its secret names"

# The docs must point at the host-config repo, name the running unit, and say
# that state survives the swap. Otherwise the next operator learns the old
# topology from the README.
for doc in README.md AGENTS.md docs/ARCHITECTURE_DECISION_RECORD.md; do
    grep -q 'ahaqqu/vps' "$repo/$doc" ||
        fail "$doc does not point at the shared vps repository"
done
grep -q 'gunbatte.service' "$repo/README.md" ||
    fail "README does not name the running unit"
grep -q 'ladder.db' "$repo/README.md" ||
    fail "README does not name the preserved state"
grep -q 'replays/' "$repo/README.md" ||
    fail "README does not name the preserved replays"
grep -q 'fails closed' "$repo/README.md" ||
    fail "README does not state the fail-closed owner step"
ok "docs point at ahaqqu/vps and state the unit + preserved state + fail-closed step"

echo "all deploy-contract checks passed"
