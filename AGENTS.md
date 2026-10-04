# AGENTS.md — working rules for coding agents

Engineering conventions for anyone (human or agent) changing this repo. The
public-facing overview lives in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md);
the internal architecture and its decision record live in
[docs/ARCHITECTURE_DECISION_RECORD.md](docs/ARCHITECTURE_DECISION_RECORD.md);
this file covers the working rules and the internal boundary that keeps the
project able to scale out.

## Reliability outranks policing

This is a game for humans on real networks: hotel wifi, mobile data, a
laptop that drops off the wifi for ten seconds. Latency, jitter, and lost
connections are normal playing conditions, not misbehavior. Every rule that
can refuse, forfeit, or lock a player out is designed in that order: an
honest player on a bad link must always be able to play — or get back in —
even when the price is tolerating some abuse.

Concretely:

- Never punish a player for what the network does to them: distance
  (latency), jitter, or a dropped socket. Forfeiture is for clients that are
  truly gone or frozen, after generous grace — and every disconnect path
  must recover the moment the link comes back.
- Every refusal shows what happened and offers a way forward (a retry, a
  reconnect, a new name). Nothing may render as a silent stuck state — a
  player staring at "queued" forever is a bug, not a rule working.
- Identity protects the player, not the rule: a hung old tab never locks an
  owner out of their own name — the latest connection holding the correct
  secret wins.
- Anti-abuse throttles (buckets, caps, nginx limits) exist to protect the
  server's health, are as generous as that goal allows, and are keyed on
  what an abuser controls, never on what a distant honest player cannot.

Review question for any PR that adds or tightens a limit: *what does an
honest player on a slow, flaky link experience at this rule's worst case?*
If the answer is kicked, locked out, or stuck, the design is wrong — relax
the honest path and accept the minor abuse it permits.

## Ask the user through the harness question tool

Decisions belong to the user. Whenever a question needs their answer — a
clarification, a design choice, a grill-style round — ask it via the harness
question tool (AskUserQuestion) with concrete options and your recommendation
first, not as free-text prose. Facts findable in the repo or environment are
never questions for the user.

Every question must be answerable from behavior, not from the source: frame
the stakes in user flows, gameplay, and architecture — what changes about who
can do what, what breaks, what it costs — and keep code references (files,
functions, line numbers) to the minimum the decision actually needs. Do not
assume the user knows the codebase; if a question only makes sense after
reading the code, the fact-finding isn't done — go find the facts yourself,
then ask the decision.

## The workflow: grill → worktree → implement → PR → review code → resolve review code → merge & deploy → verify → clean up

When the user asks to implement or fix something, run this full loop; don't
stop halfway and don't skip stages unless the user explicitly waves one off.
The loop is split across three agents, each owning its stages end to end,
with the PR and its review comments as the handoff between them:

- **First agent** — grill → worktree → implement → PR (steps 1–5).
- **Second agent** — review code (step 6).
- **Third agent** — resolve review code → merge & deploy → verify → clean
  up (steps 7–10).

1. **Grill first (grill-me / the grilling skill).** Before writing any code,
   load the grilling skill and work the design tree in rounds: fact-find in
   the repo until the frontier is real decisions only, then present each
   round — numbered questions, concrete options, recommendation first — via
   the harness question tool, and wait. Small mechanical bug fixes with an
   unambiguous cause are exempt; anything with a design choice is not.
2. **Confirm, then implement.** When the frontier is empty, restate the
   agreed plan in one block and get an explicit go — then implement.
3. **Work in a dedicated worktree off latest `main`.** Never build on
   whatever branch the primary checkout happens to sit on. Fetch, then
   create one worktree per task, branched from fresh `origin/main` —
   several agents (or one agent on several tasks) can then work at the
   same time without colliding, and the primary checkout stays untouched:

   ```sh
   git fetch origin
   git worktree add ../gunbatte-<task-slug> -b <branch> origin/main
   ```

   Every edit, build, and test run for the task happens inside that
   worktree. Its first build is cold (fresh `target/` and `node_modules`) —
   that is the price of isolation, not something to route around.

4. **Prove it locally before it leaves the machine.** `make test` (both
   profiles) and `make ci` for Rust; `npm run build` (tsc + vite) for the
   viewer; for anything the player sees or hears, drive the real client in a
   real browser against a local server (`./start-server.sh`) and verify on
   screen — screenshots as evidence, console clean of errors.
5. **PR, then make it merge-ready before handing it over.** Keep the branch
   current with `main`: `git fetch origin && git rebase origin/main` right
   before opening the PR, and again before handing it over whenever `main`
   has moved since (`git push --force-with-lease` when the PR already
   exists; re-run step 4 if the rebase changed anything). Open the PR, watch
   its checks, and hand it to the review agent only when GitHub reports it
   genuinely ready: `gh pr view --json mergeable,mergeStateStatus` says
   `MERGEABLE` / `CLEAN` and every check is green (a docs/meta-only PR runs
   no checks at all — mergeability is then the whole gate). That is the
   first agent's last stage.
6. **Review the code (review-code skill).** A second agent — fresh eyes,
   never the implementation agent — runs the review-code skill on the PR:
   an adversarial security-expert audit for bugs, regressions,
   vulnerabilities, and code quality (here that includes the seam rules,
   database-as-identity-arbiter, and determinism claims). Every finding
   goes to GitHub as one inline review comment — stable ID, priority,
   evidence, recommended fix — followed by one consolidated summary comment
   indexing them. The reviewer posts findings only; it does not fix or
   push. Its stage ends when the findings are on the record.
7. **Resolve the review findings (resolve-review-findings skill).** A third
   agent picks up from the review comments: disposition every finding with
   a threaded accept/reject reply on the original comment, fix the accepted
   ones in the same worktree, re-run the step-4 gates for anything the
   fixes touch, push to the same branch, post the resolution report on the
   PR, and iterate until CI is green. Rejections cite evidence; nothing
   stays silent.
8. **Merge, then deploy is part of the task.** With every finding
   dispositioned and GitHub reporting the PR genuinely ready (the same gate
   as step 5), merge, confirm CI is green on main, then watch the `deploy`
   workflow (it fires on CI success via `workflow_run`) until it succeeds —
   it runs `provision/vps/deploy.sh`: build, rsync, restart
   `gunbatte.service`. Docs/meta-only merges (the `paths-ignore` list in
   ci.yml) start no CI run and so fire no deploy — verify by absence in the
   Actions tab instead.
9. **Verify the work on the live URL** ([play.gunbatte.ahaqqu.com]). CI
   rebuilds on its own toolchain, so asset hashes differ from any local
   build — verify by content (does the served page/bundle contain the
   change?) and by playing the actual flow the PR was about. Report what was
   verified and what couldn't be (e.g. audibility in a headless browser).
10. **After merge: clean up the worktree.** The third agent owns the
    teardown; the user never does it. Once the PR is merged (or closed
    without merging), remove the worktree and the branch, then prune:

   ```sh
   git worktree remove ../gunbatte-<task-slug>
   git branch -d <branch>
   git fetch --prune
   ```

## The architecture docs are the map — update them in the same PR

`docs/ARCHITECTURE.md` (public claims) and
`docs/ARCHITECTURE_DECISION_RECORD.md` (internal truths and their whys) are
how the next agent orients; a drifted map lies confidently, which is worse
than none. Any change that makes a sentence in either false — crates and
their roles, the seam, the wire protocol, identity and tiers, determinism
guarantees, security boundaries, the scale-out contract, the known debts —
updates the affected doc in the same PR, exactly like the box-manifest rule
below works for VPS changes. If you built something a future agent must
understand to work here, it gets a paragraph in the decision record; if you
removed something, its paragraph goes too; if you reversed a recorded
decision, the section is rewritten to say why the old one gave way. Review
question for every PR: *does anything in either architecture doc now read
stale?*

## VPS changes start at the box manifest

The box behind `*.ahaqqu.com` is shared, and its single manifest — every app,
identity, port, nginx file, TLS lineage, plus the shared rules — lives in
[ahaqqu/homepage](https://github.com/ahaqqu/homepage) →
`provision/vps/MACHINE.md`. Before writing any script or template that changes
VPS configuration (nginx, systemd, sudoers, TLS), read that manifest first and
let it decide where the change belongs: each app repo provisions itself,
box-level facts and shared rules belong to the manifest, and the manifest is
updated in the same PR as the change that touches them.

## Commands

- `make test` — full workspace tests, then again in release (the gateway
  integration tests are timing-sensitive; both passes must be green).
- `make ci` — tests plus `cargo clippy --workspace --all-targets -- -D
  warnings`. Clippy with `-D warnings` is the merge gate; keep it clean.
- `make build` / `make viewer` — native sim; WASM viewer into `viewer/dist`.

## The one architectural rule: matchmaker ≠ game server

The project is heading toward a two-role topology: a **matchmaker** that owns
people (connections, rooms, the queue, the ladder) and **game servers** that
own matches. They are deployed together today and that stays supported — the
boundary is about code, not processes — but the two roles must never grow
into each other:

- **Matchmaker role** — WebSocket intake and registration, token/identity
  handling, the public queue, private lobbies and room codes, house-bot
  assembly at draft time, matchmaking and lane scheduling, ELO/standings,
  the ladder page, replay listing.
- **Game-server role** — everything inside one match: the 10 Hz loop, fog
  observations, the 50 ms reply deadline with its bounded acceptance window
  (`--input-window-ticks`), stall/forfeit handling, replay recording, and
  the results/ELO write-back at match end.
- **Shared** — `crates/gunbatte-core` (engine, match config, wire types,
  replay format) and `crates/gunbatte-node` (the seam: the entrant handoff,
  per-match resources, and the ladder database). Both roles depend on the
  shared crates; the roles never depend on each other.

### Package layout (the split is physical)

- `crates/gunbatte-core` — the deterministic engine. Pure; no role owns it.
- `crates/gunbatte-node` — the seam: `MatchEntrant`/`BotMsg` (the roster
  handoff), `MatchContext` (per-match resources), and the ladder database.
  The database is the only cross-role state.
- `crates/gunbatte-lobby` — the matchmaker role.
- `crates/gunbatte-gameserver` — the game-server role.
- `crates/gunbatte-server` — the binary that runs both roles in one process,
  the `GameHost` binding between them, and the end-to-end tests.

Dependency graph: `gunbatte-server` → {lobby, gameserver, node};
lobby → {node, core}; gameserver → {node, core}. There is no
lobby ↔ gameserver edge anywhere, not even a dev-dependency.

### Hard dependency rules

1. **No cross-dependency, ever.** Matchmaker code may not reach into
   game-server code and vice versa. The only things that cross the seam are
   the roster handoff (entrants + config in) and the results/replay
   write-back (out).
2. **One match, one owner.** A running match belongs to exactly one process
   for its whole life. Its state never crosses a network, a store, or a
   lock shared with the matchmaker. Never add code that reads or writes a
   live match from outside its owner.
3. **The match loop does not touch the lobby or the queue mid-match.**
   Identity, ELO lookups, and queue state are resolved at draft time; the
   loop consumes an entrant view (name, rates, connected flag, channels)
   and nothing else. Post-match requeue of survivors is matchmaking work
   and lives in `Server::spawn_match`, after the host future resolves.
4. **The database is the identity arbiter.** Names and tokens are claimed
   once, under database serialization, on the matchmaker side at lock-in.
   No other component invents, rotates, or trusts identity.
5. **A feature that seems to need both roles** either routes through the
   narrow interface (roster in → results out) or stops and raises an issue.
   A change that widens the seam is preferable to one that bridges it.

### Where the seam sits

- The handoff: `BotHandle::entrant()` (lobby) builds a `MatchEntrant`
  (node); `Server::spawn_match` passes entrants + `MatchContext` (db, replay
  dir, spectate sink, `rated` verdict for house-filled matches) to
  `MatchHost::host_match`; `GameHost` (gunbatte-server) binds that to
  `run_match` (gameserver).
- `MatchHost` is the only way matchmaking reaches a match. When the roles
  split across processes, the trait's implementation becomes the assignment
  protocol and no lobby code changes.
- Lobby-side state on a handle (queue/lobby membership, mode, human flag)
  is matchmaking-private; game-side code consumes only the `MatchEntrant`.

### Known seam debts (future work; don't entrench them)

- Spectate frames flow through one global broadcast channel the lobby owns
  and passes via `MatchContext`. Per-match channels owned by whoever runs
  the match is the scale-out shape.
- House bots are assembled by the matchmaker (`house.rs`) but run in
  matches. That stays fine as long as they enter only as ordinary entrants
  on the roster — the types now enforce it.
- `replays/match-<millis>.json` naming assumes a single writer; multiple
  processes need collision-safe names and host-aware replay paths.

## The scale-out contract (why the rule exists)

When matches outgrow one process, the topology becomes: one matchmaker, N
game servers. Refactoring stays bounded to the seam only if these hold:

- The matchmaker picks a game server from a heartbeat registry (address,
  free lanes) and assigns the match: match id, roster of verified
  identities, a one-time short-lived ticket, a gather window.
- Members are forwarded, not proxied: each client reconnects to the game
  server with its ticket. Match traffic never passes through the matchmaker.
- The game server writes results, placements, ELO, and the replay path to
  the shared database. That is its only output contract.
- Failure policy: gather timeout → house fill (the existing mechanic) or
  abort back to the queue; game-server death mid-match → the match is
  aborted with no ELO change. Live matches are never migrated.
- Identity claims happen on the matchmaker at lock-in, via the database —
  this stays true with N game servers.
- SQLite holds while every process shares one machine (WAL, local disk);
  moving game servers to separate machines is also the move to Postgres.

## Tests are the contract

`crates/gunbatte-server/tests/gateway.rs` drives real sockets through the whole
pipeline — registration, drafting, lobbies, the tick loop, idle timeouts.
Any refactor that touches the seam must keep these green unchanged, or
change them as a deliberate protocol decision — never as a side effect.
