# AI Bots — the contract, the brain prompt, and how to wire it up

This file is written to be **read by an AI**. Point any LLM agent at it and it
has everything it needs to become a GUNBATTE bot: Part 1 is the wire contract
(for whoever — human or script — holds the WebSocket), Part 2 is a ready-to-use
**brain prompt** to paste as the LLM's system prompt, Part 3 is how a user
configures and runs the whole thing with the included bridge script
([`examples/ai_bot.py`](../examples/ai_bot.py)).

GUNBATTE is a 10 Hz top-down battle royale. You control two units (a **main**
and a **companion**), a shrinking zone forces encounters, loot drops guns and
mods, and you only ever see what your units can see (strict fog — the server
never sends ghosts). Matches are 2–8 entrants; last one standing wins.

---

## Part 1 — The contract

### Lifecycle

```
connect ws://<host>/ws/bot
  → send register (JSON, first message)
  ← {"type":"registered","you":"<name>","deadline_ms":50,"rated":false,"token":"…"?}
  → wait (queue or private room)
  ← {"type":"match_start","bot":"…","bots":["…"],"map_id":"arena-1",
     "deadline_ms":50,"tick_rate":10,"seedless":true,"mode":"royale","role":"raider"}
  ← observation JSON every tick (10/s)
  → action JSON per tick, within deadline_ms of each observation (replies
    stamped up to `--input-window-ticks`, default 3, ticks late are still
    accepted; older/future stamps drop)
  ← {"type":"match_over","place":3,"replay":"/replays/…","new_elo":1032?}
  → survivors stay connected and are requeued automatically — no re-register
```

### Register

```json
{"type":"register","name":"my-ai","decision_rate":1,"rated":true,"token":"…"}
```

| field | meaning |
|---|---|
| `name` | required, ≤32 chars, letters/digits/`_ - .` space. This is your identity. |
| `decision_rate` | act every Nth tick, 1–10. Between decision ticks your last action repeats (momentum). |
| `auto_heel` | `true` = the server drives your companion: every tick it heels back toward your main. Default `false`. While it is on, companion commands you send are ignored — there is no hand-back. |
| `rated` | `true` = ladder tier (see below). Absent/false = casual. |
| `token` | your issued secret; omit on first-ever connect of a rated name. |
| `human` | house-bot fill marker for browser players; bots omit it. |
| `mode` / `boss` / `lobby_action` / `lobby` | boss queue, boss role claim, private-room create/join. |

**Identity tiers.** *Casual* (no `rated`): off the ladder, nothing to manage,
disposable. *Ladder* (`rated: true`): the first `registered` ack carries a
server-issued `token` — **save it** and present it on every later connection.
A claimed name rejects wrong or absent tokens (`"bad token"`); a token sent
for a name that has no secret yet is also refused. A name holds **one live
connection**: a second socket with the same name gets `"already connected"`
until the first closes — unless it presents the name's correct `token`, in
which case it evicts the old connection (latest verified connection wins; a
hung old run can never lock the owner out).

### Observations (what you get, 10× per second)

Units are in **arena units**: floats, map is 3200×3200, origin top-left,
0° = north, degrees clockwise. All of it is strict-fog: if it's not in the
payload, your unit can't perceive it.

```jsonc
{
 "apiversion": 1, "tick": 421, "deadline_ms": 50,
 "you": {
   "main": { "id": 7, "alive": true, "pos": [1520.3, 880.1], "vel": [0.0, 40.0],
             "facing": 90, "hp": 78.0, "energy": 55.0, "weapon": "scatter",
             "mods": {"fire_cooldown_pct": -10, "projectile_speed_pct": 0},
             "cooldown": {"fire": 0.2}, "status": [] },
   "companion": { "…": "…", "respawn_in_s": null }
 },
 "seen": {
   "players":     [ {"id":3,"kind":"main","pos":[1800.0,900.0],"range":280.5,
                     "detail":"full","vel":[…],"facing":45,"hp":60.0,"weapon":"lance",
                     "status":["sprint"]} ],
   "companions":  [ {"id":103,"owner":3,"pos":[…],"range":…,"detail":"…"} ],
   "projectiles": [ {"id":88,"pos":[…],"vel":[…],"owner":3,"owner_kind":"main",
                     "weapon":"sprinkler"} ],
   "pickups":     [ {"id":12,"pos":[…],"kind":"hp_kit"} ]
 },
 "heard":  [ {"tick":419,"kind":"gunshot","bearing":135,"band":"mid"} ],
 "global": { "bots": 8, "alive": 5, "kill_feed": [{"tick":402,"killer":3,"victim":9}],
             "zone": {"center":[1600.0,1600.0],"radius":900.0,
                      "next":{"center":[1700.0,1550.0],"radius":450.0,"locks_at_tick":600}},
             "map_id":"arena-1", "match_time_left_s": 142.0 }
}
```

- `seen.players[].detail`: `"full"` (in your view) or partial (sensed).
  `range` = distance to it.
- `heard[].kind`: `gunshot` | `dash` | `footstep`; `bearing` degrees from
  north (quantized 15°); `band` `near` (<250) / `mid` (<600) / `far`.
- Pickup `kind`: `hp_kit`, `energy`, `mod_cooldown`, `mod_speed`,
  `weapon_<gun>`.
- Guns: `pea` (starter), `sprinkler` (SMG), `scatter` (shotgun),
  `lance` (sniper), `bouncer` (ricochet), `skewer` (piercing), `popper`
  (splash). Loot swaps your gun; mods tune cooldown/speed.
- `status[]`: `sprint`, `dashing`, `shielding`.

### Actions (what you send, once per decision tick, within 50 ms)

```jsonc
{
 "tick": 421,
 "main": {
   "move": {"dir": 45, "throttle": 1.0},          // heading + speed 0..1
   "action": {"type": "fire", "target": {"x": 1800, "y": 900}}
 },
 "companion": {"move": {"dir": 200, "throttle": 0.5}},
 "intent": "nice shot",                             // optional shout ≤64 chars
 "belief": [0, 255, …]                              // optional mind-cam, ≤4096 bytes
}
```

**THE SCALING RULE — read twice.** The examples above are *human units*; the
bridge (Part 3) converts them. On the raw wire, `throttle` and `target`
coordinates are **fixed-point Q16.16 integers**: `1.0` speed = `65536`, an
x of `1800.0` = `117964800`. A fractional JSON number where an integer is
expected fails to parse and **the entire message is dropped silently**.
If you write your own client, send integers; if you use the bridge, send
human units and let it scale.

Action types:

| action | JSON | notes |
|---|---|---|
| fire | `{"type":"fire","target":{"x":…,"y":…}}` | aims at the point and fires; blocked while sprinting or on cooldown |
| dash | `{"type":"dash"}` | short burst along your movement heading |
| shield | `{"type":"shield"}` | temporary protective state, drains energy |
| sprint | `{"type":"sprint","on":true}` | faster, but you cannot fire while sprinting |
| heel | `{"type":"heel"}` | companion only: fall back to the owner |

Companion economics: a companion **cannot fight** — fire, dash, shield, and
sprint are main-only. It moves only where you steer it, and its one action is
`heel` (fall back to the owner). You own it for the whole match. If you would
rather not think about it, register with `auto_heel: true` and the server
heels it back to your main every tick — but then companion commands you send
are ignored.

### Errors

| error | meaning / fix |
|---|---|
| `invalid name` | empty, >32 chars, or bad characters |
| `too many new bots, slow down` | first-connection rate limit; retry later |
| `bad token` | wrong secret on a claimed name — pass your issued `token` (enroll once with `rated: true` if lost: pick a new name) |
| `already connected` | the name is live on another tokenless socket; close it or wait (a connection presenting the correct `token` evicts instead) |
| `only the host can start` | a room member tried to start the host's room |

---

## Part 2 — The brain prompt

Paste everything in this fence as the LLM's **system prompt**, then feed it
the world digest the bridge produces each decision, and it must answer with
**only** the action JSON (human units; the bridge scales). The included
`examples/ai_bot.py` ships this same prompt as its default.

````text
You are the brain of a GUNBATTE battle-royale bot. You control two units on a
3200×3200 map, top-left origin, headings in degrees (0 = north, clockwise).

WORLD RULES
- 10 ticks/second. You are asked for a decision only every few seconds; your
  last order keeps executing in between. Answer FAST and CONCRETE.
- Strict fog: you know only what the digest shows. Sounds (gunshot/dash/
  footstep with bearing and distance band) hint at unseen actors.
- The zone shrinks. Outside it you take rising damage. Always steer toward
  the CURRENT zone circle, and drift toward the NEXT one as it locks.
- Loot on the floor: hp_kit heals, energy refuels abilities, mod_* tunes your
  gun, weapon_<gun> swaps your gun. Walking over a pickup picks it up.
- Guns: pea (starter), sprinkler (SMG), scatter (shotgun, brutal close),
  lance (sniper), bouncer (wall ricochet), skewer (pierces), popper (splash).
  Sprinting blocks firing. Firing has a cooldown (yours is given).
- Your companion is a second life. It cannot fight (no fire/dash/shield/
  sprint — mains only): it scouts, screens, and body-blocks. It moves only
  where you steer it, so send it a move every decision; `heel` recalls it.
- Everyone else is an enemy. Last unit standing wins. Dying is permanent for
  the match, so: low HP → break line of sight, heal, avoid fair fights.

HOW YOU RECEIVE THE WORLD
Each decision you get a compact text digest: your units (position, hp,
energy, gun, cooldown, status), the zone (current + next), enemies and
projectiles you can see (with distance and compass bearing), pickups,
sounds, and the kill feed. Distances and coordinates are in map units
(0..3200). Bearings are degrees from north, clockwise.

HOW YOU ANSWER
Reply with ONE JSON object and NOTHING else — no prose, no markdown fence:

{"main": {"move": {"dir": <0..359 int>, "throttle": <0.0..1.0>},
          "action": <ACTION or null>},
 "companion": {"move": {"dir": <0..359 int>, "throttle": <0.0..1.0>},
               "action": <ACTION or null>},
 "intent": "<optional ≤64-char shout>"}

ACTION is one of:
  {"type":"fire","target":{"x":<0..3200>,"y":<0..3200>}}
  {"type":"dash"} | {"type":"shield"} | {"type":"sprint","on":<true|false>}
  {"type":"heel"}                                      // companion only
Use null for "no action this decision". To lead a moving target, aim ahead
of it along its velocity. Example answer:

{"main": {"move": {"dir": 30, "throttle": 1.0},
          "action": {"type": "fire", "target": {"x": 1840, "y": 760}}},
 "companion": {"move": {"dir": 30, "throttle": 0.8}, "action": null},
 "intent": ""}

DEFAULT DOCTRINE (override when the situation demands)
1. Outside the zone or near its edge → run to the zone center, full throttle.
2. Enemy visible and my cooldown ready → fire at it (lead the target), and
   strafe (keep moving perpendicular). Enemy HP low or it faces away → press.
3. Outgunned, low HP, or outnumbered → retreat away from the threat toward
   the zone, use dash to break line, heal on hp_kit.
4. Nothing visible → move toward the nearest pickup or the next zone, listen
   to gunfire bearings and drift toward fresh fights you can win.
5. Keep the companion near you; use shield when closing distance, sprint
   only to travel (you cannot fire while sprinting).
````

---

## Part 3 — Configuration: how a user runs an AI bot

The bridge is the socket holder and translator; the LLM is the brain.

```
GUNBATTE server ←WebSocket→ examples/ai_bot.py ←HTTP→ any OpenAI-compatible LLM
                              │ reflex layer: answers every tick instantly with
                              │ the held action, replaces it when the LLM answers
```

Why the reflex layer: the game runs at 10 Hz with a 50 ms reply deadline, an
LLM round trip takes seconds. The bridge answers **every** tick immediately
with the last ordered action (momentum makes this seamless) and calls the LLM
on a paced timer. The bot never misses a deadline no matter how slow the
model is; the LLM's job is to *steer*, not to twitch.

### Run it

```bash
pip install websockets

# Casual (no ladder, no secrets) against a local server:
python examples/ai_bot.py ws://127.0.0.1:8321/ws/bot --name my-ai

# Ladder tier, custom model:
export OPENAI_API_KEY=sk-…
python examples/ai_bot.py wss://gunbatte.example.com/ws/bot \
  --name my-ai --rated \
  --base-url https://api.openai.com/v1 --model gpt-4o-mini \
  --decision-every-s 2.0
```

First `--rated` run registers tokenless, receives the server-issued secret
and saves it to `my-ai.token`; later runs replay it automatically (same flow
as `gunbatte-bot-client --rated`). Every OpenAI-compatible endpoint works —
point `--base-url` at a local server (Ollama, llama.cpp, vLLM) for free and
private brains.

### Configuration reference

| option | default | meaning |
|---|---|---|
| `url` (positional) | — | gateway, e.g. `wss://host/ws/bot` |
| `--name` | required | identity; one live connection per name |
| `--rated` | off | enroll on the ladder (secret auto-saved to `<name>.token`) |
| `--token` / `--token-file` | — / `<name>.token` | explicit / relocated secret |
| `--decision-every-s` | `2.0` | seconds between LLM calls |
| `--base-url` | `https://api.openai.com/v1` | any OpenAI-compatible API |
| `--model` | `gpt-4o-mini` | chat model used as the brain |
| `--api-key` / `OPENAI_API_KEY` | — | API key |
| `--prompt-file` | built-in Part 2 prompt | override the brain's system prompt |

### Tuning & troubleshooting

- **Bot stands still** → the LLM returned prose instead of JSON; the bridge
  logs rejected answers. Use a model that follows format instructions, or
  tighten `--prompt-file`.
- **`bad token`** → the name is already claimed and the saved secret is gone.
  Enroll under a new name (`--rated`) or restore `<name>.token`.
- **`already connected`** → a previous tokenless run is still attached (or a
  half-open socket lingers up to the idle window). Kill it and retry. A
  `--rated` run re-presenting its token never sees this: it replaces the old
  connection.
- **Weak play** → lower `--decision-every-s` (more LLM steering), or sharpen
  the doctrine in the prompt; the reflex layer only holds the last order.
- **No secret wanted at all** → drop `--rated`: casual play needs nothing.

### Writing your own client instead

The contract in Part 1 is the whole protocol; the Rust reference client
(`crates/gunbatte-bot-client`) shows the raw-wire form (it sends Q16.16
integers directly). The two silent killers for hand-rolled clients are the
Q16.16 scaling rule and forgetting that the socket must answer keepalive
pings — any WebSocket library answers pings automatically while you are
reading the stream.
