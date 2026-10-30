#!/usr/bin/env bash
# tests/deploy-contract.sh — pin the deploy path to the ahaqqu/vps contract.
#
# The deploy's trust property is the artifact and the gate that follows it:
# ahaqqu/vps `gunbatte/deploy.conf` expects a tar.gz whose root is exactly
# `gunbatte-server`, `viewer/dist/` and `website/`, uploaded as
# `gunbatte-deploy`, applied only through `vps-deploy gunbatte`, and followed
# by the shared `/srv/vps-ops/doctor.sh` gate. A wrong layout, a leaked
# dotfile, a second writer or a skipped doctor fails in production with no
# other signal, so this test puts decoys where the script actually copies
# them, pins the exact root, and drives the upload/apply/doctor path through
# fake `ssh`/`rsync` binaries. It also runs the real build path with stub
# `make`/`cargo` binaries, so the clean-tree check cannot slip below the build
# without going red.
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

# --- fake repos --------------------------------------------------------------

# Base tree: every build input the deploy needs, plus the state, host-config
# and extra-root decoys that must never reach the artifact.
base="$tmp/base"
mkdir -p "$base/target/release" \
    "$base/viewer/dist/assets" \
    "$base/website/characters" \
    "$base/docs" \
    "$base/replays" \
    "$base/provision/vps"
printf '#!/bin/sh\nexit 0\n' >"$base/target/release/gunbatte-server"
chmod 755 "$base/target/release/gunbatte-server"
printf '<html>viewer</html>\n' >"$base/viewer/dist/index.html"
printf 'console.log("asset")\n' >"$base/viewer/dist/assets/app.js"
printf 'body {}\n' >"$base/website/style.css"
cat >"$base/website/index.html" <<'HTML'
<a href="http://127.0.0.1:8321/">PLAY</a>
<a href="http://127.0.0.1:8321/ladder">LADDER</a>
HTML
cat >"$base/website/characters/index.html" <<'HTML'
<a href="http://127.0.0.1:8321/">PLAY</a>
HTML
printf '<html>docs</html>\n' >"$base/docs/index.html"
printf 'state\n' >"$base/ladder.db"
printf '{}\n' >"$base/replays/match-1.json"
printf '#!/bin/sh\necho old writer\n' >"$base/provision/vps/apply.sh"

package() { # <repo> <archive>
    VPS_USER=gunbatte-deploy GUNBATTE_GAME_HOST=play.example.test \
        "$repo/deploy.sh" --package-only --skip-build --repo "$1" --out "$2"
}

expect_package_failure() { # <label> <message> <repo> <archive>
    local label="$1" message="$2" source="$3" archive_path="$4"
    local slug log
    slug="$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '-')"
    log="$tmp/failure-$slug.log"
    if package "$source" "$archive_path" >"$log" 2>&1; then
        fail "$label: package succeeded but must fail"
    fi
    grep -qF -- "$message" "$log" || {
        cat "$log" >&2
        fail "$label: expected message containing '$message'"
    }
    ok "$label fails closed"
}

# --- the happy path: exact root, no state, rewritten CTAs --------------------

happy="$tmp/happy"
cp -a "$base" "$happy"
archive="$tmp/happy.tar.gz"
package "$happy" "$archive" >/dev/null || fail "deploy.sh --package-only failed on a valid tree"
[ -f "$archive" ] || fail "no archive produced"

entries="$(tar -tzf "$archive" | sed 's|^\./||')"
top="$(printf '%s\n' "$entries" | sed '/^$/d' | cut -d/ -f1 | sort -u | tr '\n' ' ')"
top="${top% }"
[ "$top" = "gunbatte-server viewer website" ] ||
    fail "artifact root is '$top'; expected exactly 'gunbatte-server viewer website'"
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
ok "artifact root is exactly gunbatte-server, viewer/ and website/"

if printf '%s\n' "$entries" | grep -Eq '(^|/)(ladder\.db|replays|provision|target|\.git)(/|$)'; then
    fail "artifact leaked state or host configuration"
fi
if printf '%s\n' "$entries" | grep -Eq '(^|/)\.'; then
    fail "artifact contains a hidden path segment"
fi
if printf '%s\n' "$entries" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
    fail "artifact contains an absolute or parent-relative path"
fi
if printf '%s\n' "$entries" | grep -Eq '^docs(/|$)'; then
    fail "artifact contains an extra top-level docs/ tree"
fi
ok "artifact excludes state, host config, hidden entries and extra roots"

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

# --- decoys in the tree the script actually stages ---------------------------

case="$tmp/decoy-env"
cp -a "$base" "$case"
printf 'SECRET=1\n' >"$case/website/.env"
expect_package_failure "website/.env" "hidden path segment" "$case" "$tmp/decoy-env.tar.gz"

case="$tmp/decoy-viewer-env"
cp -a "$base" "$case"
printf 'SECRET=1\n' >"$case/viewer/dist/.env"
expect_package_failure "viewer/dist/.env" "hidden path segment" "$case" "$tmp/decoy-viewer-env.tar.gz"

case="$tmp/decoy-state"
cp -a "$base" "$case"
printf 'state\n' >"$case/website/ladder.db"
expect_package_failure "website/ladder.db" "state or host configuration" "$case" "$tmp/decoy-state.tar.gz"

case="$tmp/decoy-replays"
cp -a "$base" "$case"
mkdir -p "$case/website/replays"
printf '{}\n' >"$case/website/replays/match-2.json"
expect_package_failure "website/replays/" "state or host configuration" "$case" "$tmp/decoy-replays.tar.gz"

case="$tmp/decoy-provision"
cp -a "$base" "$case"
mkdir -p "$case/website/provision/vps"
printf 'x\n' >"$case/website/provision/vps/apply.sh"
expect_package_failure "website/provision/" "state or host configuration" "$case" "$tmp/decoy-provision.tar.gz"

case="$tmp/decoy-target"
cp -a "$base" "$case"
mkdir -p "$case/viewer/dist/target/release"
printf 'x\n' >"$case/viewer/dist/target/release/stray"
expect_package_failure "viewer/dist/target/" "state or host configuration" "$case" "$tmp/decoy-target.tar.gz"

# --- fail-closed build inputs ------------------------------------------------

case="$tmp/missing-binary"
cp -a "$base" "$case"
rm "$case/target/release/gunbatte-server"
expect_package_failure "missing gunbatte-server" \
    "missing $case/target/release/gunbatte-server" "$case" "$tmp/missing-binary.tar.gz"

case="$tmp/non-executable-binary"
cp -a "$base" "$case"
chmod 644 "$case/target/release/gunbatte-server"
expect_package_failure "non-executable gunbatte-server" \
    "missing $case/target/release/gunbatte-server" "$case" "$tmp/non-executable-binary.tar.gz"

case="$tmp/missing-viewer-index"
cp -a "$base" "$case"
rm "$case/viewer/dist/index.html"
expect_package_failure "missing viewer/dist/index.html" \
    "missing $case/viewer/dist/index.html" "$case" "$tmp/missing-viewer-index.tar.gz"

case="$tmp/missing-website-dir"
cp -a "$base" "$case"
rm -rf "$case/website"
expect_package_failure "missing website/" \
    "missing $case/website" "$case" "$tmp/missing-website-dir.tar.gz"

case="$tmp/missing-website-index"
cp -a "$base" "$case"
rm "$case/website/index.html"
expect_package_failure "missing website/index.html" \
    "artifact is missing website/index.html" "$case" "$tmp/missing-website-index.tar.gz"

# --- the real deploy path: upload, apply, then the doctor gate --------------

# A deploy requires a clean git checkout, so the live fixture is committed.
live="$tmp/live"
cp -a "$base" "$live"
git -C "$live" init -q
git -C "$live" add -A
git -C "$live" -c user.name=deploy-contract \
    -c user.email=deploy-contract@example.test commit -qm base

mkdir -p "$tmp/bin"
cat >"$tmp/bin/rsync" <<'EOF'
#!/bin/sh
exit 0
EOF
cat >"$tmp/bin/ssh" <<'EOF'
#!/bin/sh
remote=""
for arg in "$@"; do
    remote="$arg"
done
printf '%s\n' "$remote" >>"${SSH_LOG:?SSH_LOG is required}"
case "$remote" in
    *"/usr/local/sbin/vps-deploy gunbatte"*) exit "${FAKE_APPLY_EXIT:-0}" ;;
    *"/srv/vps-ops/doctor.sh"*) exit "${FAKE_DOCTOR_EXIT:-0}" ;;
esac
exit 0
EOF
chmod +x "$tmp/bin/rsync" "$tmp/bin/ssh"

run_deploy() { # <repo> <archive>; fake ssh honours FAKE_APPLY_EXIT/FAKE_DOCTOR_EXIT
    PATH="$tmp/bin:$PATH" \
        SSH_LOG="$tmp/ssh.log" \
        VPS_USER=gunbatte-deploy \
        VPS_HOST=box.example.test \
        GUNBATTE_GAME_HOST=play.example.test \
        "$repo/deploy.sh" --skip-build --repo "$1" --out "$2"
}

: >"$tmp/ssh.log"
if ! run_deploy "$live" "$tmp/live.tar.gz" >"$tmp/live.out" 2>&1; then
    cat "$tmp/live.out" >&2
    fail "full deploy failed against a healthy fake box"
fi
mapfile -t ssh_lines <"$tmp/ssh.log"
[ "${#ssh_lines[@]}" -eq 2 ] ||
    fail "expected the apply and the doctor over SSH; got ${#ssh_lines[@]} SSH call(s)"
case "${ssh_lines[0]}" in
    *"/usr/local/sbin/vps-deploy gunbatte"*) ;;
    *) fail "the first SSH call is not vps-deploy gunbatte" ;;
esac
case "${ssh_lines[1]}" in
    *"/srv/vps-ops/doctor.sh"*) ;;
    *) fail "the second SSH call is not the doctor gate" ;;
esac
doctor_cmd="${ssh_lines[1]}"
printf '%s\n' "$doctor_cmd" | grep -q 'if \[ -x /srv/vps-ops/doctor.sh \]' ||
    fail "the doctor gate does not check that the file exists"
printf '%s\n' "$doctor_cmd" | grep -q 'exit 1' ||
    fail "the doctor gate does not fail closed when the file is absent"
# Execute the literal remote command under a relocated root so absence and
# presence are simulated here instead of depending on this host: the check
# must run everywhere, including a box where the real path exists.
doctor_root="$tmp/remote-doctor"
doctor_at_root="${doctor_cmd//\/srv\/vps-ops/$doctor_root}"
[ "$doctor_at_root" != "$doctor_cmd" ] ||
    fail "the doctor command does not hard-code /srv/vps-ops/doctor.sh"
if printf '%s\n' "$doctor_at_root" | grep -qF '/srv/vps-ops'; then
    fail "the relocated doctor command still points at the host path"
fi

# Absent: non-zero and a message naming the missing path, never a skipped gate.
if sh -c "$doctor_at_root" >"$tmp/absent-doctor.out" 2>&1; then
    fail "the doctor command exited 0 with its path absent"
fi
grep -q 'is missing' "$tmp/absent-doctor.out" ||
    fail "an absent doctor does not say it is missing"
grep -qF "$doctor_root/doctor.sh" "$tmp/absent-doctor.out" ||
    fail "an absent doctor does not name the missing path"

# Present: the command runs the doctor and propagates its exit status.
mkdir -p "$doctor_root"
printf '#!/bin/sh\nexit 0\n' >"$doctor_root/doctor.sh"
chmod 755 "$doctor_root/doctor.sh"
if ! sh -c "$doctor_at_root" >"$tmp/present-doctor.out" 2>&1; then
    fail "the doctor command failed with an executable doctor present"
fi
printf '#!/bin/sh\nexit 7\n' >"$doctor_root/doctor.sh"
chmod 755 "$doctor_root/doctor.sh"
if sh -c "$doctor_at_root" >"$tmp/failing-doctor.out" 2>&1; then
    fail "the doctor command swallowed a failing doctor"
fi
ok "post-deploy doctor gate is the second SSH call, runs the doctor when present and fails closed when absent"

live_entries="$(tar -tzf "$tmp/live.tar.gz" | sed 's|^\./||')"
live_top="$(printf '%s\n' "$live_entries" | sed '/^$/d' | cut -d/ -f1 | sort -u | tr '\n' ' ')"
live_top="${live_top% }"
[ "$live_top" = "gunbatte-server viewer website" ] ||
    fail "full-deploy artifact root is '$live_top'; expected exactly three contract paths"
if printf '%s\n' "$live_entries" | grep -Eq '(^|/)(docs|target|replays|provision|\.git)(/|$)'; then
    fail "full-deploy artifact leaked a repo tree that is present in the checkout"
fi
ok "full-deploy artifact keeps the exact contract root and drops repo trees"

: >"$tmp/ssh.log"
if FAKE_DOCTOR_EXIT=1 run_deploy "$live" "$tmp/doctor-fail.tar.gz" \
    >"$tmp/doctor-fail.out" 2>&1; then
    fail "the deploy stayed green when the doctor gate failed"
fi
grep -q 'post-deploy doctor gate failed' "$tmp/doctor-fail.out" ||
    fail "a failed doctor did not red the deploy with a clear message"
grep -q 'was applied' "$tmp/doctor-fail.out" ||
    fail "the doctor failure does not say the tree was already applied"
[ "$(grep -c . "$tmp/ssh.log")" -eq 2 ] ||
    fail "the doctor did not run after the apply"
ok "a non-zero doctor reds an already-applied deploy"

: >"$tmp/ssh.log"
if FAKE_APPLY_EXIT=1 run_deploy "$live" "$tmp/apply-fail.tar.gz" \
    >"$tmp/apply-fail.out" 2>&1; then
    fail "the deploy stayed green when vps-deploy rejected it"
fi
grep -q 'vps-deploy gunbatte rejected the deploy' "$tmp/apply-fail.out" ||
    fail "an apply failure does not name vps-deploy"
[ "$(grep -c . "$tmp/ssh.log")" -eq 1 ] ||
    fail "the doctor ran even though the apply failed"
ok "the doctor gate runs only after a successful apply"

dirty="$tmp/dirty"
cp -a "$live" "$dirty"
printf 'scratch\n' >"$dirty/website/notes.txt"
: >"$tmp/ssh.log"
if run_deploy "$dirty" "$tmp/dirty.tar.gz" >"$tmp/dirty.out" 2>&1; then
    fail "a dirty checkout was deployed"
fi
grep -q 'source tree is dirty' "$tmp/dirty.out" ||
    fail "a dirty checkout did not fail with a clear message"
[ "$(grep -c . "$tmp/ssh.log")" -eq 0 ] ||
    fail "a dirty checkout reached upload/apply"
ok "an untracked path fails before upload"

plain="$tmp/plain"
cp -a "$base" "$plain"
: >"$tmp/ssh.log"
if run_deploy "$plain" "$tmp/plain.tar.gz" >"$tmp/plain.out" 2>&1; then
    fail "a non-git checkout was deployed"
fi
grep -q 'not a git checkout' "$tmp/plain.out" ||
    fail "a non-git checkout did not fail closed"
ok "a non-git checkout fails closed instead of skipping the clean check"

# --- the clean-tree check runs before the build ------------------------------
# Every deploy above passes --skip-build, so moving the clean-tree guard below
# the build would stay green. Drive the real build path with stub make/cargo
# binaries: a dirty checkout must fail before either one runs, and a clean one
# must build viewer then the release binary.

mkdir -p "$tmp/buildbin"
write_build_stub() { # <tool>; records the call and dirties a tracked path
    cat >"$tmp/buildbin/$1" <<EOF
#!/bin/sh
printf '%s %s\n' "$1" "\$*" >>"\${BUILD_LOG:?BUILD_LOG is required}"
printf 'build churn\n' >>"\$PWD/docs/index.html"
exit 0
EOF
    chmod +x "$tmp/buildbin/$1"
}
write_build_stub make
write_build_stub cargo

run_deploy_build() { # <repo> <archive>; real build path, stub make/cargo
    PATH="$tmp/buildbin:$tmp/bin:$PATH" \
        BUILD_LOG="$tmp/build.log" \
        SSH_LOG="$tmp/ssh.log" \
        VPS_USER=gunbatte-deploy \
        VPS_HOST=box.example.test \
        GUNBATTE_GAME_HOST=play.example.test \
        "$repo/deploy.sh" --repo "$1" --out "$2"
}

dirty_build="$tmp/dirty-build"
cp -a "$live" "$dirty_build"
printf 'scratch\n' >"$dirty_build/website/notes.txt"
: >"$tmp/build.log"
: >"$tmp/ssh.log"
if run_deploy_build "$dirty_build" "$tmp/dirty-build.tar.gz" >"$tmp/dirty-build.out" 2>&1; then
    fail "a dirty checkout deployed on the real build path"
fi
grep -q 'source tree is dirty' "$tmp/dirty-build.out" ||
    fail "a dirty checkout did not fail before the build"
[ ! -s "$tmp/build.log" ] ||
    fail "the build ran before the clean-tree check: $(tr '\n' ';' <"$tmp/build.log")"
[ "$(grep -c . "$tmp/ssh.log")" -eq 0 ] ||
    fail "a dirty checkout reached upload/apply on the real build path"
ok "a dirty checkout fails the clean-tree check before the build runs"

clean_build="$tmp/clean-build"
cp -a "$live" "$clean_build"
: >"$tmp/build.log"
: >"$tmp/ssh.log"
if ! run_deploy_build "$clean_build" "$tmp/clean-build.tar.gz" >"$tmp/clean-build.out" 2>&1; then
    cat "$tmp/clean-build.out" >&2
    fail "a clean checkout did not deploy on the real build path"
fi
[ "$(sed -n '1p' "$tmp/build.log")" = "make viewer" ] ||
    fail "the real build path did not run 'make viewer' first"
[ "$(sed -n '2p' "$tmp/build.log")" = "cargo build --release -p gunbatte-server" ] ||
    fail "the real build path did not run the release build second"
[ "$(grep -c . "$tmp/build.log")" -eq 2 ] ||
    fail "the real build path ran unexpected build commands: $(tr '\n' ';' <"$tmp/build.log")"
[ "$(grep -c . "$tmp/ssh.log")" -eq 2 ] ||
    fail "a clean built deploy did not apply and run the doctor"
ok "the real build path builds viewer then gunbatte-server and deploys"

# --- the script's fixed identity/path/command -------------------------------

grep -q '/srv/vps-incoming/gunbatte.tar.gz' "$repo/deploy.sh" ||
    fail "deploy.sh does not upload to the contract's fixed path"
grep -q 'sudo -n /usr/local/sbin/vps-deploy gunbatte' "$repo/deploy.sh" ||
    fail "deploy.sh does not invoke the shared applier"
grep -q '/srv/vps-ops/doctor.sh' "$repo/deploy.sh" ||
    fail "deploy.sh does not run the shared doctor gate"
grep -q 'VPS_USER_DEFAULT=gunbatte-deploy' "$repo/deploy.sh" ||
    fail "deploy.sh does not default to the gunbatte-deploy identity"
if grep -nE 'systemctl[[:space:]]+(restart|start|reload)' "$repo/deploy.sh"; then
    fail "deploy.sh restarts a unit directly"
fi
if grep -nE 'rsync.*(/srv/gunbatte|/home/kajianq-deploy)' "$repo/deploy.sh"; then
    fail "deploy.sh rsyncs into the target tree"
fi
ok "deploy.sh is upload + vps-deploy + doctor only"

if VPS_USER=gunbatte-deploy GUNBATTE_GAME_HOST=play.example.test \
    "$repo/deploy.sh" --bootstrap >"$tmp/bootstrap.out" 2>&1; then
    fail "deploy.sh accepted --bootstrap"
fi
grep -q 'no bootstrap path' "$tmp/bootstrap.out" ||
    fail "--bootstrap did not explain where the host bootstrap lives"
ok "there is no --bootstrap second writer"

if VPS_USER=kajianq-deploy VPS_HOST=box.example GUNBATTE_GAME_HOST=play.example.test \
    "$repo/deploy.sh" --skip-build --repo "$base" --out "$tmp/old-identity.tar.gz" \
    >"$tmp/old-identity.out" 2>&1; then
    fail "deploy.sh accepted the retired shared deploy identity"
fi
grep -q 'must be gunbatte-deploy' "$tmp/old-identity.out" ||
    fail "the old identity was not rejected with a clear message"
ok "the retired shared identity fails closed"

if grep -R -n --exclude-dir=.git --exclude-dir=node_modules 'provision/vps' \
    "$repo/deploy.sh" "$repo/README.md" "$repo/AGENTS.md" "$repo/LIMITS.md" \
    "$repo/docs" "$repo/.github/workflows" 2>/dev/null; then
    fail "an old provision/vps reference survived"
fi
ok "no old provision/vps reference survives in the deploy docs"

if grep -R -n --exclude-dir=.git --exclude-dir=target --exclude-dir=node_modules \
    'apply\.sh' "$repo/crates" "$repo/README.md" "$repo/AGENTS.md" \
    "$repo/LIMITS.md" "$repo/docs" 2>/dev/null; then
    fail "a stale apply.sh reference survives"
fi
ok "no stale apply.sh reference survives"

grep -q 'run: ./deploy.sh' "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow does not run ./deploy.sh"
grep -q 'VPS_DEPLOY_SSH_KEY' "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow lost the deploy-key secret"
grep -q 'VPS_KNOWN_HOSTS' "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow lost the known-hosts secret"
grep -q "vars.VPS_USER || 'gunbatte-deploy'" "$repo/.github/workflows/deploy.yml" ||
    fail "deploy workflow does not default VPS_USER to gunbatte-deploy"
ok "deploy workflow uses the shared identity/path and keeps its secret names"

# The docs must point at the host-config repo, name the running unit, say that
# state survives the swap, and name the shared doctor gate. Otherwise the next
# operator learns the old topology from the README.
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
grep -q '/srv/vps-ops/doctor.sh' "$repo/README.md" ||
    fail "README does not name the shared post-deploy doctor gate"
ok "docs point at ahaqqu/vps and state the unit, preserved state and doctor gate"

grep -q 'gunbatte/nginx/gunbatte.conf' "$repo/LIMITS.md" ||
    fail "LIMITS does not name the nginx template"
grep -q '/etc/nginx/sites-available/gunbatte.conf' "$repo/LIMITS.md" ||
    fail "LIMITS does not name the installed nginx site"
grep -q 'add-only' "$repo/LIMITS.md" ||
    fail "LIMITS does not say gunbatte-nginx-reconcile is add-only"
ok "LIMITS names the template, the installed site and the add-only helper"

echo "all deploy-contract checks passed"
