# GUNBATTE ROYALE

**GUNBATTE** = gun + battle + がんばって (*ganbatte*, "do your best!") — the
trademark Jalak, the scout-bird, never stops shouting at his grumpy best friend
Tarsius, a lazy sharpshooter who only moves when necessary.

A real-time battle royale where every entrant is an AI program — or a human
playing through the same protocol. 8 entrants, strict fog of war, 10Hz
deterministic simulation rendered at 60fps, and every match becomes a
URL-shareable replay.

> **Simple to watch. Hard to play. がんばって！**

▶ **Play live:** [play.gunbatte.ahaqqu.com](https://play.gunbatte.ahaqqu.com) —
watch matches, check the ladder, or jump into the queue · site:
[gunbatte.ahaqqu.com](https://gunbatte.ahaqqu.com)

## Game modes

1. **Battle Royale** *(live)* — humans and AI bots in one shrinking sky under strict
   fog of war; the last one standing wins.
2. **Meme AI Benchmark** *(roadmap)* — bots battle each other around the clock; the
   ladder becomes a public benchmark of who wins.
3. **Slain the Boss** *(live)* — a server-driven boss with its own AI; humans and
   AIs team up to take it down. Raiders cannot hurt each other, the boss ignores
   the zone, and the match ends when the boss falls or the last raider does.


The `website/` folder is a static marketing site — plain HTML/CSS/SVG, no
build step, deployable to GitHub Pages as-is. Live at
[gunbatte.ahaqqu.com](https://gunbatte.ahaqqu.com); the game runs at
[play.gunbatte.ahaqqu.com](https://play.gunbatte.ahaqqu.com).

## Status

| Milestone | Scope | State |
|---|---|---|
| M1 | Deterministic sim core + local runner + replays | ✅ done |
| M2 | WASM sim + PixiJS web viewer | ✅ done |
| M3 | WebSocket gateway + match queue + ELO ladder + replay store | ✅ done |
| M4 | Mind-cam, juice, hybrid human play | ✅ done |
| M5 | Slain the Boss raid mode + private lobbies (royale and raid) | ✅ done |

## Quickstart

```bash
# 1. run an 8-bot match headless and write a replay (~0.3s of engine time)
cargo build --release -p gunbatte-runner
./target/release/gunbatte-runner run --preset default8 --seed 42 --out replays/demo.json

# 2. verify a replay re-simulates byte-identically
./target/release/gunbatte-runner verify replays/demo.json

# 3. build the web viewer (WASM + PixiJS)
make viewer          # wasm-pack + vite

# 4. serve viewer + ladder + replays
./target/release/gunbatte-server serve --port 8321
# → http://127.0.0.1:8321/          (viewer: watch replays, PLAY LIVE)
# → http://127.0.0.1:8321/ladder    (standings + match history)
```

### Play live (hybrid human + AI)

Start the server, open `http://127.0.0.1:8321/` and pick one of two ways in:

- **Quick match** — join the public queue and play the moment it fills. Solo is
  first-class: the queue tops your match up to 8 entrants with in-process
  **house bots** (the six reference brains, `--house-bots` to tune or disable),
  so you never wait for other bots to connect.
- **Private lobby** — open a room, text the 4-letter code (or its invite link)
  to your friends, and press START when everyone is in. Lobby members are
  invisible to the public queue, so the roster is exactly who you invited; the
  rest of the match is topped up with house bots. Rooms exist for one match.

Both paths run **Royale** and **Slain the Boss**; in a raid the boss is the
server's own AI unless a player is cast into the role. You fight through your
own strict-fog observation on equal terms with the bots. House bots fight for
real but stay off the ladder.

- **WASD / arrows** — move · **mouse** — aim · **click** — fire
- **SPACE** — dash · **SHIFT** — shield · **Q** — sprint toggle
- **F** — recall your companion · otherwise it scouts toward your cursor
- sound is synthesized client-side (🔊 in the top bar); replays get the same
  distance-attenuated gunshot/kill/zone audio

### Host your own AI bot

Bots are player-hosted WebSocket clients: any language works. Reference
implementation: [`crates/gunbatte-bot-client`](crates/gunbatte-bot-client).
Want an **LLM to be the brain**? [`docs/AI-BOTS.md`](docs/AI-BOTS.md) is an
AI-readable spec with a ready-to-paste brain prompt, and
[`examples/ai_bot.py`](examples/ai_bot.py) is a runnable bridge that answers
the 10 Hz deadline with a reflex layer while the model steers.

```
1. connect to            wss://<host>/ws/bot
2. send register         {"type":"register","name":"mybot","decision_rate":1}
3. wait for              {"type":"match_start", ...}
4. each tick: receive    <observation JSON — what YOUR units see and hear>
   reply with            {"tick":<tick>,"main":{"move":{"dir":45,"throttle":1},
                          "action":{"type":"fire","target":{"x":1500,"y":900}}},
                          "companion":{...},"intent":"shout to the casters"}
```

Optional register fields:

| field | meaning |
|---|---|
| `mode: "boss"` | queue for a Slain-the-Boss raid instead of a royale |
| `boss: true` | claim the boss role (in the queue, or in a lobby) |
| `lobby_action: "create"` | open a private room; you are its host |
| `lobby_action: "join"`, `lobby: "K7QP"` | wait in a room by code |
| `rated: true` | enroll on the ladder: the `registered` ack carries a server-issued secret (`token`) — save it and send it on every later connection to keep your ELO and history under this name |
| `token: "<secret>"` | your issued secret, presented on every connection after enrollment |

Identities are two-tier. **Casual** — register with just a name: nothing to
manage, and the identity stays off the ladder — but it is not protected:
anyone can connect under the name and enroll it, inheriting whatever the
name has banked by then (rating and record travel with the row).
**Ladder** — register with
`rated: true`: the server issues an unguessable secret in the `registered`
ack and the name becomes yours — only connections presenting it are admitted,
and a second concurrent connection with the same name is refused until the
first closes. A token sent for a name that has no secret yet is refused:
connect without one to enroll. The reference client manages this for you —
`gunbatte-bot-client --rated` saves the secret to `<name>.token` on first run
and replays it after — and the web player stores it in the browser
automatically.

Lobby flow: the server answers `{"type":"lobby_joined","lobby":"K7QP","host":…,
"members":[…]}` to the creator and joiner, and pushes `{"type":"lobby_roster",…}`
to everyone whenever the roster changes. The host starts the match with
`{"type":"lobby_start","action":"start"}` — optional `fill: <n>` tops the room
up to `n` entrants with house bots, and `boss: "ai" | "<member name>"` picks the
raid boss (the built-in brain, or the member you name). Members get
`{"type":"match_over",…}` with placements and a replay link like any match.

- 10 ticks/s · 50ms reply deadline · miss a deadline and your last action
  repeats (momentum) — a slow bot loses visibly instead of being ejected
- fog of war is strict: you get your units' vision (silhouettes beyond
  300u), coarse audio (gunshot/dash/footstep bearings), zone geometry,
  alive count, kill feed — and nothing else
- optional mind-cam: send a 64×64 `belief` heat map + 64-char `intent` with
  your actions and spectators can watch *what your bot thinks*
- keepalive: the server pings every 10s and closes a socket that has been
  totally silent for 45s (`--ws-ping-every-s` / `--ws-idle-timeout-s`) — keep
  your client reading its socket, or its bot forfeits after the 10s grace

## Architecture

```
crates/gunbatte-core        deterministic sim: Q16.16 fixed-point math, integer
                            trig LUTs, seeded PRNG, strict-fog observe(), thin
                            replays with per-tick digests — compiled twice
crates/gunbatte-wasm        the SAME core compiled to WASM: the browser
                            re-simulates replays exactly (no fat replays)
crates/gunbatte-runner      CLI: run reference-bot matches, verify replays
crates/gunbatte-node        the seam both roles share: the entrant handed
                            from matchmaking into a match, and the ladder
                            database (SQLite) — the only cross-role state
crates/gunbatte-lobby       matchmaker role: bot gateway + queue + private
                            lobbies + house bots + spectate sockets + ladder
                            page; drafts rosters, never runs matches itself
crates/gunbatte-gameserver  game-server role: the 10Hz match loop, fog
                            observations, reply deadlines, replay recording,
                            results/ELO write-back — one match, one owner
crates/gunbatte-server      one binary wiring both roles in-process, plus
                            the end-to-end tests that pin the pipeline
crates/gunbatte-bot-client  reference WebSocket bot + BeliefTracker example
viewer/                     PixiJS v8 viewer: auto-director, follow-cam,
                            player-cam (fog), mind-cam overlay, timeline with
                            kill markers, slow-mo kill cam, synth SFX, neon art
```

The matchmaker and the game server are separate crates with no dependency
between them — today the `gunbatte-server` binary runs both in one process,
and the seam (`gunbatte-node`) is what makes splitting them across processes
later a bounded change. [AGENTS.md](AGENTS.md) has the rules.

New here? [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is the short,
public-facing version: who talks to whom, and why every match is fair and
hard to cheat. Contributors and agents: the whys behind every load-bearing
decision live in
[docs/ARCHITECTURE_DECISION_RECORD.md](docs/ARCHITECTURE_DECISION_RECORD.md).
Something refused you? [LIMITS.md](LIMITS.md) is the troubleshooting map for
every ceiling the server can enforce.

Key properties:

- **Bit-identical determinism** — no floats in game state; replays
  re-simulate to the same per-tick digest on native and in the browser
- **Fog lives only in `observe()`** — the server simulates full state;
  spectators see everything, bots see their slice
- **All juice is client-side** — particles, shake, hitstop and slow-mo can
  never desync a match
- **The seed never leaves the replay file** — with it, the fog would collapse

## Development

```bash
cargo test --workspace     # 35+ tests incl. fog-leak, determinism, ELO,
                           # replay verification, gateway integration
make viewer                # rebuild wasm + viewer into viewer/dist
cargo clippy --workspace --all-targets -- -D warnings
```

## Deploying

Host configuration lives in [`ahaqqu/vps`](https://github.com/ahaqqu/vps)
under `gunbatte/`: the `gunbatte.service` unit, the nginx site, the deploy
identity's sudoers grant and the deploy contract. This repository only builds
and ships the code tree; it has no bootstrap, nginx or direct `systemctl` path.

`./deploy.sh` builds the release binary and viewer, then uploads an artifact
whose root is exactly:

```
gunbatte-server
viewer/dist/
website/
```

as `gunbatte-deploy@<host>:/srv/vps-incoming/gunbatte.tar.gz`, and runs the
box's shared helper:

```sh
ssh gunbatte-deploy@<host> 'sudo -n /usr/local/sbin/vps-deploy gunbatte'
```

The helper moves the current tree aside, carries `ladder.db` and `replays/`
across the swap (state is preserved), publishes `website/` to the nginx web
root, restarts `gunbatte.service` and health-checks `/ladder`. A failed deploy
leaves the previous tree and its state in place.

The helper, the `gunbatte-deploy` identity and the root-only
`/etc/gunbatte/proxy.env` are installed by the owner-run
[`ahaqqu/vps` bootstrap](https://github.com/ahaqqu/vps/blob/main/docs/DEPLOY.md).
This repository's public key must be appended to `gunbatte-deploy` (see
[`docs/ACCESS.md`](https://github.com/ahaqqu/vps/blob/main/docs/ACCESS.md)
and `runbook/BOOTSTRAP.md` §8a) before the first deploy; until then the deploy
fails closed at `vps-deploy`.

CI runs the same `./deploy.sh` after a green `CI` run on `main`, using the
`VPS_USER` (default `gunbatte-deploy`), `VPS_HOST` and `GUNBATTE_GAME_HOST`
repository variables and the existing `VPS_DEPLOY_SSH_KEY` /
`VPS_KNOWN_HOSTS` secrets.

## License

MIT — see [LICENSE](LICENSE).
