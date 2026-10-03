# LIMITS — every refusal the server can hand you, and what to do about it

GUNBATTE's server deliberately refuses things at the edges (issue #37: ladder
integrity & abuse resistance). Every limit here was added so that one abusive
client cannot farm the ladder, spam the database, brute-force private rooms,
or exhaust the 2 vCPU / 4 GB VPS. The ordering principle (AGENTS.md,
"Reliability outranks policing"): limits exist to protect the server's
health, never to judge a player's network — latency, jitter, and dropped
connections are conditions to ride out, not misbehavior to punish.

This page is the troubleshooting map: **symptom → which limit fired → the fix.**
All app limits are flags on `gunbatte-server serve` with defaults below; `0`
disables any of them. App limits are *global* (one shared allowance for
everyone — behind the reverse proxy the server cannot see real client IPs);
nginx limits are *per IP*.

| Knob | Default | Bounds |
|---|---|---|
| `--max-connections` | 256 | concurrent sockets, bots + spectators in one pool |
| `--max-lobbies` | 64 | live private rooms |
| `--join-attempts-per-min` | 30 | wrong room-code guesses (global allowance) |
| `--new-names-per-min` | 60 | first-time bot registrations (global allowance) |
| `--max-replays` | 100 | replay files kept (startup sweep) |
| `--input-window-ticks` | 10 | reply-stamp acceptance window in ticks (≈1 s of one-way latency); 0 = strict (only the exact tick accepted) |
| HTTP request timeout | 30 s | dynamic routes (replay downloads exempt) |
| HTTP concurrency | 256 | in-flight requests of any kind |
| nginx `limit_conn` | 10 / IP | concurrent WS connections per IP |
| nginx `limit_req` | 10/min, burst 20 / IP | WS handshakes per IP |

---

## "My rating didn't change after a match"

**Not a bug — by design.** Any match that contains house bots (scripted
sparring partners) is unrated: solo play, a lobby the host topped up with
house fill, and typical boss raids. Placement, win/loss tally and the replay
all still record; only the rating is frozen. The ladder only moves on matches
where every entrant is a real connected player.

- Verify: the match-start log line prints `[unrated]` for such matches.
- If a match you believe was all-real showed no rating change, check the
  roster — one house bot in the room makes the whole match unrated.

Consequence to plan around: the ladder moves slower now, and solo play can no
longer be used to climb.

## "Connection fails outright (HTTP 503 on the WS handshake)"

The socket pool is full: bots and spectators draw from one pool of
`--max-connections` (256). The 257th connection is refused before the server
spawns anything for it.

- Check: how many bots/spectators are currently connected? Idle bots linger
  until the idle timeout closes them.
- Fix: disconnect something, or raise `--max-connections`.
- If the refusals come from nginx instead (`/var/log/nginx/gunbatte.error.log`
  shows "limiting connections"), the per-IP ceiling fired — see the fleet
  section below.

## "Too many new bots, slow down"

First-time registrations draw from a global allowance of
`--new-names-per-min` (60). When it's spent, brand-new names are refused until
it refills (continuously, over about a minute). This is the dam against
cycling unique names to spam the ladder database.

- Reconnecting with a **name the server already knows** never trips this —
  fleet operators should keep bot names stable across reconnects.
- A legit wave of genuinely new bots (classroom demo, bot jam) can outrun 60
  per minute: have the extras retry, or raise the knob for the event.

## "no such lobby" — then "too many join attempts, slow down"

Wrong room-code guesses share one global allowance of
`--join-attempts-per-min` (30). While it's empty, **even a correct code is
refused** — the throttle can't tell a brute-forcer from a typo.

- Fix: wait up to a minute and join again.
- If it happens often with no attacker, someone (or a bot in a retry loop) is
  burning the allowance; raise the knob.
- Room codes are uppercase, 4 characters, no lookalike letters (no I, O, 0, 1).

## "lobby limit reached, try later"

`--max-lobbies` (64) live rooms. Rooms free up when consumed by their match,
when the host disconnects, or when they empty. Honest impact should be nil;
if you see this, something is creating rooms without ever starting them.

## "My bot plays sluggish / its decisions seem ignored"

The server accepts a reply stamped with the tick currently being decided, or
up to `--input-window-ticks` (default 10, ≈1 s of one-way latency) ticks
older — beyond that the reply is dropped (first drop per bot is logged:
`input_dropped_stale`).
A reply with **no tick field at all is accepted**. A late-but-in-window
reply IS applied and only records its latency: the ladder forfeits the gone,
not the distant — so sluggish play never comes from this gate.

- Cause of actual drops: the bot echoes a very stale tick, or fabricates a
  future one — e.g. replying to an observation it already replied to, or
  replaying old decisions.
- Fix: always stamp the action with the tick of the **newest** observation
  received. That is what the reference bot client and the viewer do, and why
  honest clients never see this.
- Distant-but-honest links show up as `input_late_accepted` (first per
  entrant per match) and in the per-entrant `input_summary` at match end
  (`avg_staleness`, `avg_latency_ms`) — tuning evidence, not refusals.

## "Why was a player forfeited?" — the timeout ladder

The ladder retires a client that is actually gone or frozen — never one that
is merely far away (slow replies are counted as a stat, `slow_replies`, and
nothing else). Exactly three things forfeit:

| Trigger | Threshold | Journal reason |
|---|---|---|
| a reply measured past the fatal deadline | > 1000 ms (first one) | `reply exceeded fatal deadline (1s)` |
| missed decision ticks with nothing pending | > 20% and ≥ 10 misses | `missing too many deadlines (>20%)` |
| disconnected for the whole grace | 30 s of momentum | `connection lost (grace expired)` |

Reliability mechanics around it:

- A stalled observation stream (channel backed up → `obs_stall`) disconnects
  the entrant — and **recovers** (`obs_recovered`) the moment one observation
  goes through again. A brief slow patch costs nothing.
- Disconnection grace is 30 s (100 was too short to survive a wifi hop);
  momentum keeps the body playing while it runs.
- The viewer auto-reconnects with backoff after any drop; a reconnected
  player is queued for the next match. (Re-entering the *same* live match
  is a planned feature — see the rejoin issue.)

## "This name is already connected" — it isn't anymore

The one-connection-per-name rule (issue #42) stands for unproven
registrations, but a registration presenting a tokened name's **correct
token** is its owner: it evicts the old connection (`register_evicted`) and
takes over — latest verified connection wins. A hung tab, a crashed page, or
a half-open socket can never lock the owner out of their own name. Refusals
that remain: a wrong or absent token on a claimed name (`bad token`), and a
duplicate of a **tokenless casual** row (`already connected`) — a casual row
has no secret, so first-come is all the protection it has.

## "An old replay link 404s"

Replay retention: at startup the server keeps the newest `--max-replays`
(100) `match-<millis>.json` files and deletes the rest. The match-history
entry survives; the file behind the link is gone.

- Hand-placed fixture files (e.g. `demo8.json`) are never swept — only
  auto-recorded matches are.
- Fix: copy replays worth keeping off the box before a deploy restart, or
  raise `--max-replays` (`0` keeps everything — unbounded disk, the problem
  that motivated the limit: 21 GB accumulated before it existed).

## "A fleet of bots from one machine gets bounced"

nginx enforces per-IP ceilings on both WS endpoints: at most 10 concurrent
connections per IP, and ~10 handshakes per minute per IP (burst 20). One
office, one home, or one VPS running 15+ bots will hit this no matter what
the app allows.

- Fix: split the fleet across IPs, or raise the ceilings in
  `provision/vps/nginx/gunbatte.conf` (edit → `nginx -t` → reload; certbot
  also edits that file, so re-read it first).
- Legit humans are unaffected: one player needs one connection and rarely
  reconnects.

## "Requests hang or return 408 / the site feels stuck"

- HTTP 408: the 30-second request timeout on dynamic routes fired (a
  slow-client attack backstop). Honest API/page use never takes 30 s. Replay
  downloads are deliberately exempt — tens of MB over a slow link is normal.
- Everything queued/refused: the global in-flight cap (256) covers all
  routes, replays included. If it's saturated, something is hammering the
  box; the connection pool above is usually the first thing to give.

## Planning an event?

Raise the knobs, don't remove them: every one exists because an unbounded
version of the same feature was the attack. `0` disables any app knob.
Remember bots and spectators share the connection pool — a big audience can
crowd out players, so size `--max-connections` for spectators + bots combined.

---

## Journal events (the ops view of every limit above)

One line per event, stable token + `key=value` fields — grep-friendly under
`journalctl -u gunbatte`. High-rate paths log state changes and per-match
aggregates, not per-tick lines.

| Event | Meaning |
|---|---|
| `match_started entrants=N mode=M rated=B names="…"` | a match began |
| `input_late_accepted entrant=X staleness=S latency_ms=L` | first windowed (late-but-applied) reply of that entrant |
| `input_dropped_stale entrant=X client_tick=T current_tick=C` | first reply dropped as too stale for that entrant |
| `obs_stall entrant=X` | stopped reading observations; disconnect grace begins |
| `obs_recovered entrant=X` | observations flowing again after a stall; grace cancelled |
| `disconnect entrant=X cause=socket_closed` | socket died; momentum, then forfeit |
| `ladder_forfeit entrant=X bot=I tick=T reason=R` | timeout ladder forfeited the entrant |
| `input_summary entrant=X accepted=N late=N avg_staleness=S avg_latency_ms=L max_staleness=M dropped_stale=D` | per-entrant input-path totals at match end |
| `match_over ticks=T winner=W replay=R mode=M rated=B` | a match finished and persisted |
| `register_refused name=X reason=invalid_name\|name_bucket\|bad_token` | registration refused at the door |
| `register_evicted name=X (newer connection)` | the name's owner (correct token) replaced a live connection |
| `lobby_join_failed name=X code=C reason=no_such_lobby\|join_bucket\|lobby_full` | private-room join refused |

New operational behavior ships with its event and a LIMITS.md row in the
same PR (ADR, "Journal events").
