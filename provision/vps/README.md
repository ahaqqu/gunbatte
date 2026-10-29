# provision/vps — deploy GUNBATTE ROYALE to the VPS

Target topology (cohabiting with the existing apps — add-only, nothing shared
is ever edited):

```
internet ──▶ nginx :80/:443 (shared; kajianq.conf + gunbatte.conf by Host header)
                ├── play.<domain>  ──proxy──▶ 127.0.0.1:8321  gunbatte-server (gunbatte.service)
                └── site.<domain>  ──static── $GUNBATTE_DIR/website/
```

State lives in `$GUNBATTE_DIR` (default `~/gunbatte`): the binary, `viewer/dist/`,
`website/`, and the server's own data (`ladder.db`, `replays/`).

## One-time bootstrap

DNS first: point `play.<domain>` and `site.<domain>` (A records, **DNS-only**
— a proxied record hides the player's address from nginx's per-IP ceilings,
see [LIMITS.md](../../LIMITS.md), and adds a proxy hop of jitter to a 10 Hz
game) at the VPS. The zone is hand-managed in Cloudflare: the box manifest's
DNS table is the inventory, and the post-deploy gate fails if a name stops
resolving. Then, from the repo root:

```sh
GUNBATTE_GAME_HOST=play.example.com \
GUNBATTE_SITE_HOST=gunbatte.example.com \
GUNBATTE_EMAIL=you@example.com \
./provision/vps/deploy.sh --bootstrap
```

This builds the viewer + server locally (no Rust toolchain needed on the VPS),
uploads everything, then runs `apply.sh` on the VPS over ssh — it will prompt
for the sudo password once. `apply.sh`:

1. installs `gunbatte.service` (systemd, dedicated user, binds `127.0.0.1`)
2. installs a sudoers drop-in: passwordless `systemctl restart/status
   gunbatte.service` for the deploy user **only**
3. installs the nginx site file (add-only; `nginx -t` before reload)
4. issues Let's Encrypt certificates via `certbot --nginx` (skipped with a
   warning until both hostnames resolve to the VPS — just re-run)
5. starts the service

## Updates

```sh
GUNBATTE_GAME_HOST=play.gunbatte.ahaqqu.com ./provision/vps/deploy.sh
```

No sudo, no prompts: the sudoers drop-in covers the restart. The server's
state (`ladder.db`, `replays/`) is never touched by uploads.

## Automatic deploys (GitHub Actions)

`.github/workflows/deploy.yml` fires **after CI finishes green on main** and
runs the same `deploy.sh` — one deploy path for CI and humans. **All values
live as repository variables** (Settings → Secrets and variables → Actions):
`VPS_USER`, `VPS_HOST`, `GUNBATTE_GAME_HOST`, `GUNBATTE_SITE_HOST`,
`GUNBATTE_PORT`, `GUNBATTE_EMAIL`. deploy.sh reads the same variables via
`gh`, so there is exactly one source of configuration. CI never holds an
admin identity — the admin account appears only when **you** run the
one-time bootstrap, named by the `GUNBATTE_ADMIN_SSH` env var. One-time key
setup:

```sh
# 1. a dedicated, revocable keypair for the deploy identity (no passphrase):
ssh-keygen -t ed25519 -f ~/.ssh/gunbatte-deploy -N "" -C "gunbatte-vps-deploy-key"

# 2. pin the host key:
ssh-keyscan 62.83.35.220 > /tmp/vps_known_hosts
```

Then add two GitHub **repository secrets** (Settings → Secrets and variables →
Actions):

| Secret | Value |
|---|---|
| `VPS_DEPLOY_SSH_KEY` | contents of `~/.ssh/gunbatte-deploy` (the private key) |
| `VPS_KNOWN_HOSTS` | contents of `/tmp/vps_known_hosts` |

The **public** half is installed into the deploy identity's
`authorized_keys` by `--bootstrap` itself (idempotent append — kajianq's own
key line is kept). After that, every merge to main that passes CI deploys
itself. To revoke CI's access, delete its line from the deploy identity's
`authorized_keys` on the VPS. Until the first `--bootstrap` has run, the
workflow only uploads artifacts and prints a reminder — the final restart
needs the service to exist.

## Files

| File | Runs on | Purpose |
|---|---|---|
| `deploy.sh` | local | build + upload + (bootstrap or restart) + post-deploy doctor gate |
| `apply.sh` | VPS | install unit / sudoers / nginx / certs, reconcile limits, start service |
| `gunbatte.service` | — | systemd unit template (`__GUNBATTE_*__` placeholders) |
| `nginx/gunbatte.conf` | — | nginx site template (HTTP-only; certbot adds TLS in place) |
| `nginx/reconcile-limits.sh` | VPS | add-only reconcile of the #37 abuse limits + the #38 CSP header into an installed site file (`--dry-run` previews) |

## Abuse limits (issue #37)

Availability and ladder-integrity ceilings, split by who can see the real
client IP: nginx limits per IP, the app limits totals (behind the proxy the
app's socket address is 127.0.0.1, so per-IP state there would be useless).

| Layer | Limit | Purpose |
|---|---|---|
| nginx `limit_conn` | 10 per IP on `/ws/bot` + `/ws/spectate` | per-IP socket ceiling |
| nginx `limit_req` | 10 WS handshakes/min per IP, burst 20 | reconnect churn, room-code brute-forcing |
| nginx `client_max_body_size` | 1m | inputs are ~200 bytes; nothing takes uploads |
| app `--max-connections` | 256 | concurrent sockets (bots + spectators share one pool); full pool refuses the upgrade |
| app `--max-lobbies` | 64 | live private rooms |
| app `--join-attempts-per-min` | 30 | global failed-`join` bucket (~1M code space stays impractical to brute-force) |
| app `--new-names-per-min` | 60 | global first-time-registration bucket (name cycling cannot spam `bots` rows; reconnects bypass it) |
| app `--max-replays` | 100 | startup sweep deletes the oldest `match-*.json` (hand-placed fixtures are never touched) |
| app HTTP layers | 30s request timeout on dynamic routes, 256 global in-flight | slowloris / run-away request insurance; replay downloads are exempt from the timeout |

`0` on any app knob disables that ceiling. The request timeout is applied only
to dynamic routes — a tens-of-MB replay download over a slow link is
legitimate — while the in-flight cap covers everything, replays included.

**Applying the nginx half:** every deploy ships the updated template files,
but only `apply.sh` may touch nginx — and it never re-installs an installed
site file, because certbot owns its TLS edits. To land a template change on
the VPS, re-run `apply.sh` (as root, as in the bootstrap); its limits
reconcile (`nginx/reconcile-limits.sh`) is add-only — it inserts the zones,
the `/ws/*` location blocks, the 1m body cap, and the #38 CSP header if
missing, never touching hostnames, TLS, or redirects — and is a verified
no-op when everything is already in place. Preview first with
`sudo bash $GUNBATTE_DIR/provision/vps/nginx/reconcile-limits.sh --dry-run`
(the `--dry-run` flag is deliberate: `sudo` strips environment variables, so
a `DRY_RUN=1` prefix would silently not survive it).
Normal deploys deliberately cannot do this step: the deploy identity's sudo
grant is scoped to the app service, so CI can never edit nginx.

**The post-deploy gate:** every non-bootstrap deploy ends with the shared-box
doctor sweep — nginx config test, `sites-enabled/` hygiene, failed units,
every app's listeners and certs, and every DNS name in the box manifest's
table. A gunbatte deploy that leaves the box unhealthy turns its own run
red, with no cross-repo review needed. The DNS row earns its place: the zone
is hand-managed in Cloudflare, no IaC owns it, so a name that stops
resolving is invisible until a player complains — which is exactly what
happened when the 2026-10-25 zone re-creation dropped this app's pair and
nothing noticed for four days (incident note in MACHINE.md) — and the sweep
now fails the deploy on one. It is **one copy on the box**: homepage (the
box's owner) updates `/srv/vps-ops/doctor.sh` on every one of its deploys,
and this repo's deploy runs that file — nothing vendored here, nothing
fetched, no credential. If
homepage has not deployed since the ops dir was bootstrapped, the gate prints
a warning instead of failing. It runs as the deploy identity, whose sudoers
drop-in carries two read-only diagnostics for it (`nginx -t`,
`certbot certificates` — neither changes state); re-run `apply.sh` once after
merging to install that widened grant. Until then the sweep degrades to
warnings. The box-wide view of what doctor checks lives in homepage's private
`provision/vps/MACHINE.md`.

When one of these fires in production, [LIMITS.md](../../LIMITS.md) maps the
symptom the client sees to the knob that caused it.

## Defense in depth (issue #38)

Blast-radius armor rather than closed holes. The CI pieces (RustSec audit,
garbage-input property tests) need no VPS work; the nginx header and the
process cage land with one `apply.sh` re-run — the same path as the #37
limits ("Applying the nginx half" above).

| Piece | Lives | What it buys |
|---|---|---|
| Content-Security-Policy | nginx template + reconcile | the browser refuses to run any script that did not ship with the viewer — a future name/replay escaping slip cannot execute; the viewer itself is eval-free (`pixi.js/unsafe-eval`) so `script-src 'self'` stays strict, with only the WebAssembly-only `'wasm-unsafe-eval'` grant for the sim |
| systemd cage | `gunbatte.service` | filesystem read-only except the state dir (`ProtectSystem=strict` + `ReadWritePaths`), syscall + address-family allowlists, `MemoryMax=1G` + `CPUQuota=150%` — a runaway match costs live matches one 3s restart instead of starving the shared box |
| RustSec audit | CI (`ci.yml`) | known dependency vulnerabilities fail CI and block the merge; hygiene advisories annotate |
| Garbage-input tests | CI (`gunbatte-lobby`) | random malformed wire input through parse → submit → tick may never panic and never push a unit outside the world |
| WS origin check | app + unit env | browsers claiming a foreign page cannot open bot/spectate sockets; header-less homemade bots and scripts are unaffected; empty allowlist (dev) disables it |

Browser floor for the header: `'wasm-unsafe-eval'` — the grant the sim's
instantiate needs — landed in Chrome 95, Firefox 102, and Safari 16.4. Older
browsers (notably Safari ≤ 16.3) load the page but cannot start the sim
under the header; everything else still renders. That is the accepted floor.

**Landing it on the VPS** — one `apply.sh` re-run as root (admin login, like
the bootstrap; CI deliberately cannot do this), previewing the nginx change
first:

```sh
sudo bash $GUNBATTE_DIR/provision/vps/nginx/reconcile-limits.sh --dry-run
GUNBATTE_GAME_HOST=… GUNBATTE_SITE_HOST=… GUNBATTE_EMAIL=… \
  GUNBATTE_ADMIN_SSH=ahaqqu@62.83.35.220 \
  ./provision/vps/deploy.sh --bootstrap
```

`--bootstrap` hard-requires `GUNBATTE_ADMIN_SSH` (the admin login CI never
holds) and will prompt for the sudo password once, over ssh. The re-run
re-renders the unit (cage + `GUNBATTE_ALLOWED_ORIGINS`), inserts
the CSP header into the certbot-owned site file (add-only, timestamped
backup beside the resolved target, automatic rollback if `nginx -t`
objects), and restarts the service. Afterwards verify:

- `systemctl status gunbatte` is active, the ladder page loads, a match runs.
- `curl -sI https://$GUNBATTE_GAME_HOST/ | grep -i content-security-policy`.
- The viewer's browser console shows no CSP violation reports (the shipped
  policy was verified against Google Fonts, wasm, and the Pixi renderer).
- `ladder.db-wal` still grows beside `ladder.db` — writes live in the single
  `ReadWritePaths` pocket.

Rollback: the reconcile leaves a `.bak-<timestamp>` beside the resolved
target in `sites-available/` to restore nginx; the unit always re-renders
from the template, so reverting this branch and re-running `apply.sh`
removes the cage.

## Why this can't disturb the other apps

- One new nginx file, symlinked into `sites-enabled/`; existing server blocks
  are never edited, and `nginx -t` gates every reload (reload is graceful).
- Own systemd unit on its own loopback port; restarts affect only this app.
- Separate Let's Encrypt lineage; certbot edits only the gunbatte site file.
- The `--bind 127.0.0.1` flag keeps the raw game port off the public
  interface, so nginx+TLS is the only way in.
