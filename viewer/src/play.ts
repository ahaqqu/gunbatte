/** Human play client (hybrid play): a human enters the SAME match queue as
 * AI bots over the same WebSocket protocol — the gateway cannot tell the
 * difference. The player sees strictly through their own observation (fog)
 * and drives WASD + mouse; the server resolves their actions every tick. */


export interface PlayYouUnit {
  id: number;
  alive: boolean;
  pos: [number, number];
  vel: [number, number];
  facing: number;
  hp: number;
  energy: number;
  cooldown: { fire?: number };
  status: string[];
  respawn_in_s?: number;
  /** Mains only: the gun currently equipped ("pea", "scatter", …). */
  weapon?: string;
}

export interface PlayObs {
  tick: number;
  you: { main: PlayYouUnit; companion: PlayYouUnit };
  seen: {
    players: { id: number; pos: [number, number]; vel?: [number, number]; facing?: number; detail: string; hp?: number; weapon?: string }[];
    companions: { id: number; owner: number; pos: [number, number]; detail: string }[];
    projectiles: { id: number; pos: [number, number]; vel: [number, number]; owner: number; weapon?: string }[];
    pickups: { id: number; pos: [number, number]; kind: string }[];
  };
  heard: { kind: string; bearing: number; band: string }[];
  global: {
    bots: number;
    alive: number;
    zone: { center: [number, number]; radius: number; next?: { center: [number, number]; radius: number; locks_at_tick: number } | null };
    match_time_left_s: number;
    kill_feed: { tick: number; killer: number | null; victim: number }[];
  };
}

export type PlayStatus = "connecting" | "queued" | "playing" | "over" | "disconnected";

/** A private room the socket is waiting in (server's `lobby_joined`/`lobby_roster`). */
export interface LobbyInfo {
  code: string;
  host: string;
  you: string;
  mode: "royale" | "boss";
  members: string[];
}

/** How this socket enters the server: the public queue, or a private room. */
export interface LobbyIntent {
  action: "create" | "join";
  /** Room code, required for "join". */
  code?: string;
}

export interface PlayCallbacks {
  onStatus: (s: PlayStatus, detail?: string) => void;
  onStart: (youIndex: number, entrants: string[], role: "boss" | "raider") => void;
  onObs: (obs: PlayObs) => void;
  onOver: (place: number, replay: string | null) => void;
  /** You are in a room: show the code + roster (fired on create and on join). */
  onLobby?: (info: LobbyInfo) => void;
  /** The roster changed (someone joined or left). */
  onRoster?: (info: LobbyInfo) => void;
  /** The room died (host left, or it was consumed by a match). */
  onLobbyClosed?: (reason: string) => void;
  /** Server-side rejection (bad code, full room, not the host…). */
  onError?: (message: string) => void;
  /** The session hit a dead end (permanent refusal, eviction, or retries
   * exhausted): the UI shows a card. `retryable` offers the reconnect button. */
  onFatal?: (message: string, retryable: boolean) => void;
}

interface InputState {
  keys: Set<string>;
  mouseDown: boolean;
  /** A click shorter than one observation tick would be missed by the
   * level-sampled mouseDown — latch it so a tap still fires one shot. */
  fireQueued: boolean;
  dashQueued: boolean;
  sprintToggled: boolean;
}

/** The sim's `Fix` is a raw Q16.16 i64 on the wire (1.0 = 65536); JS numbers
 * must be scaled before sending or the gateway reads 1 as 1/65536 — and a
 * fractional value fails i64 parsing, dropping the WHOLE input message. */
const FIX_ONE = 65536;
const toFix = (v: number): number => Math.round(v * FIX_ONE);

export class PlayClient {
  private ws: WebSocket | null = null;
  private state: InputState = {
    keys: new Set(),
    mouseDown: false,
    fireQueued: false,
    dashQueued: false,
    sprintToggled: false,
  };

  playing = false;
  sprinting = false;
  /** Latest observation, for HUD rendering between ticks. */
  lastObs: PlayObs | null = null;
  /** Where the reticle sits on screen (HUD renders it each frame). */
  mouseScreen = { x: 0, y: 0 };
  /** True while the left button is held (HUD reticle state). */
  firing = false;
  /** Which mode this socket asked for (boss = Slain the Boss raid). */
  mode: "royale" | "boss" = "royale";
  /** This socket's room, while it waits in one. */
  lobby: LobbyInfo | null = null;

  constructor(
    private name: string,
    private cb: PlayCallbacks,
    mode: "royale" | "boss" = "royale",
    /** Private-room entry, or null for the public quick-match queue. */
    private lobbyIntent: LobbyIntent | null = null,
  ) {
    this.mode = mode;
  }

  /** Reconnection policy state (reliability over policing): a dropped
   * socket retries itself with backoff — a blip on a slow link must not
   * end a session. Permanent refusals and eviction never retry. */
  private url: string | null = null;
  private attempts = 0;
  private leaveRequested = false;
  /** A refusal the server will repeat verbatim (bad token, invalid name):
   * retrying cannot succeed, so the card's way out is the truth instead. */
  private noRetry = false;
  /** This page was replaced by a newer connection for the same name:
   * redialing would steal the name back and fight the other tab. */
  private evicted = false;
  /** The last refusal the server sent on this session's sockets: when the
   * retry budget runs out, the card reports that answer instead of guessing
   * at reachability (a deterministic refusal is not "server unreachable"). */
  private lastRefusal: string | null = null;
  private retryTimer: number | null = null;

  private static MAX_ATTEMPTS = 6;

  /** This player's identity secret, persisted per name (issue #42). */
  private tokenKey(): string {
    return `gb.token.${this.name}`;
  }

  /** The secret held in memory when storage is unwritable: it covers this
   * session's reconnects, but once the page is gone the name can no longer
   * be proven ours (the server never re-issues a secret). */
  private sessionToken: string | null = null;

  private storedToken(): string | null {
    try {
      return localStorage.getItem(this.tokenKey());
    } catch {
      return null;
    }
  }

  connect(url: string): void {
    this.url = url;
    this.leaveRequested = false;
    this.noRetry = false;
    this.evicted = false;
    this.lastRefusal = null;
    this.attempts = 0;
    this.open();
  }

  /** Manual retry from the error card: clears the backoff budget and dials
   * again with the same identity (name + token → the server evicts any
   * hung predecessor and admits us). */
  reconnect(): void {
    this.clearRetryTimer();
    this.attempts = 0;
    this.noRetry = false;
    this.open();
  }

  private clearRetryTimer(): void {
    if (this.retryTimer !== null) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.leaveRequested || this.noRetry || this.evicted || !this.url) {
      this.setStatus("disconnected");
      return;
    }
    if (this.attempts >= PlayClient.MAX_ATTEMPTS) {
      this.setStatus("disconnected");
      this.cb.onError?.("connection lost");
      this.cb.onFatal?.(
        this.lastRefusal
          ? `connection lost — the server kept answering: ${this.lastRefusal}`
          : "connection lost — the server is unreachable right now",
        true,
      );
      return;
    }
    this.attempts += 1;
    const wait = Math.min(1500 * this.attempts, 6000);
    this.setStatus("connecting", `reconnecting in ${Math.round(wait / 1000)}s…`);
    this.clearRetryTimer();
    this.retryTimer = window.setTimeout(() => this.open(), wait);
  }

  private open(): void {
    if (!this.url) return;
    this.setStatus("connecting");
    if (this.ws) {
      // A manual retry while a socket still exists: detach all of its
      // handlers so its events can neither double-schedule a reconnect nor
      // write into the new session's state — the detachment is by
      // construction, not by current call-flow.
      this.ws.onopen = null;
      this.ws.onmessage = null;
      this.ws.onerror = null;
      this.ws.onclose = null;
      this.ws.close();
    }
    const ws = new WebSocket(this.url);
    this.ws = ws;
    ws.onopen = () => {
      // `human` marks this entrant for house-bot fill: the server tops the
      // match up to the full size with reference brains so nobody waits.
      // Humans auto-enroll on the ladder (issue #42): `rated` asks the
      // server for an identity secret, which the ack carries and we store
      // per-name — replayed on every later connect, invisible to the
      // player. Without it the identity would be casual (off-ladder).
      const reg: Record<string, unknown> = {
        type: "register",
        name: this.name,
        decision_rate: 1,
        auto_heel: false,
        human: true,
        mode: this.mode,
        rated: true,
      };
      const saved = this.storedToken() ?? this.sessionToken;
      if (saved) reg.token = saved;
      if (this.lobbyIntent?.action === "create") {
        reg.lobby_action = "create";
      } else if (this.lobbyIntent?.action === "join") {
        reg.lobby_action = "join";
        reg.lobby = this.lobbyIntent.code ?? "";
      }
      ws.send(JSON.stringify(reg));
    };
    ws.onmessage = (ev) => {
      let v: any;
      try {
        v = JSON.parse(ev.data as string);
      } catch {
        return;
      }
      if (v.type === "registered") {
        // The identity was accepted again: the retry budget is back to full
        // and any earlier refusal is stale — a later outage must be judged
        // on its own answers, not this session's history.
        this.attempts = 0;
        this.lastRefusal = null;
        if (typeof v.token === "string" && v.token) {
          this.sessionToken = v.token;
          try {
            localStorage.setItem(this.tokenKey(), v.token);
          } catch {
            // Storage unwritable (private mode): the secret lives in memory
            // for this session only. The server has ALREADY claimed the name
            // and never re-issues a secret — once this page is gone, this
            // browser cannot prove ownership and the name must be replaced
            // (a "bad token" error points the player at that).
          }
        }
        this.setStatus("queued");
      } else if (v.type === "lobby_joined" || v.type === "lobby_roster") {
        const info: LobbyInfo = {
          code: v.lobby ?? "",
          host: v.host ?? "",
          you: this.name,
          mode: v.mode === "boss" ? "boss" : "royale",
          members: v.members ?? [],
        };
        const first = this.lobby === null;
        this.lobby = info;
        if (first) this.cb.onLobby?.(info);
        else this.cb.onRoster?.(info);
      } else if (v.type === "lobby_closed") {
        this.lobby = null;
        this.cb.onLobbyClosed?.(v.reason ?? "closed");
      } else if (v.type === "queued_as_boss") {
        this.setStatus("queued", "boss (waiting for a raid)");
      } else if (v.type === "match_start") {
        this.playing = true;
        this.lobby = null;
        // Drop the previous match's final snapshot: a heartbeat firing
        // before this match's first observation would otherwise send the
        // old match's tick stamp (rejected as a future claim) and consume
        // the dash/fire latches with it.
        this.lastObs = null;
        this.startHeartbeat();
        this.setStatus("playing");
        this.cb.onStart(v.you_index ?? 0, v.bots ?? [], v.role === "boss" ? "boss" : "raider");
      } else if (v.type === "match_over") {
        this.playing = false;
        this.setStatus("over");
        this.cb.onOver(v.place ?? 0, v.replay ?? null);
      } else if (v.type === "error") {
        const msg: string = v.error ?? "error";
        this.lastRefusal = msg;
        if (msg === "bad token" || msg === "invalid name") {
          this.noRetry = true;
        } else if (msg.includes("newer connection")) {
          this.evicted = true;
        }
        this.setStatus("queued", msg);
        this.cb.onError?.(msg);
        if (this.noRetry || this.evicted) {
          this.cb.onFatal?.(msg, false);
        }
      } else {
        // Observation.
        const obs = v as PlayObs;
        this.lastObs = obs;
        if (this.playing) this.sendInput(obs);
        this.cb.onObs(obs);
      }
    };
    ws.onclose = () => {
      this.playing = false;
      this.scheduleReconnect();
    };
  }

  /** Host action: start the room's match now (server checks who the host is).
   * `fill` tops the roster up to that many entrants; `boss` picks the boss in
   * a raid room ("ai", "boss", or a member name). */
  startLobby(fill?: number, boss?: string): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(JSON.stringify({ type: "lobby_start", action: "start", fill, boss }));
  }

  /** Attach global input listeners (keyboard + mouse). Mouse → world
   * conversion is delegated to the renderer via screenToWorld. */
  attachInput(host: HTMLElement, screenToWorld: (x: number, y: number) => { x: number; y: number }): void {
    window.addEventListener("keydown", (e) => {
      const k = e.key.toLowerCase();
      if (k === " ") e.preventDefault();
      if (k === " " && !this.state.keys.has(" ")) this.state.dashQueued = true;
      if (k === "q" && !this.state.keys.has("q")) this.state.sprintToggled = true;
      this.state.keys.add(k);
    });
    window.addEventListener("keyup", (e) => this.state.keys.delete(e.key.toLowerCase()));
    host.addEventListener("mousedown", (e) => {
      if (e.button === 0) {
        this.state.mouseDown = true;
        this.state.fireQueued = true;
        this.firing = true;
      }
    });
    window.addEventListener("mouseup", (e) => {
      if (e.button === 0) {
        this.state.mouseDown = false;
        this.firing = false;
      }
    });
    host.addEventListener("mousemove", (e) => {
      this.mouseScreen = { x: e.clientX, y: e.clientY };
    });
    host.addEventListener("contextmenu", (e) => e.preventDefault());
    this.screenToWorld = screenToWorld;
  }

  private screenToWorld: (x: number, y: number) => { x: number; y: number } = (x, y) => ({ x, y });

  /** Bearing (0 = north, clockwise) of the WASD chord; null = stand still.
   * The sim's compass 0 is world +Y, which renders as screen DOWN (the
   * viewer has no Y flip), so screen-up keys must negate dy. */
  private moveDir(): { dir: number; throttle: number } | null {
    const k = this.state.keys;
    let dx = 0;
    let dy = 0;
    if (k.has("w") || k.has("arrowup")) dy += 1;
    if (k.has("s") || k.has("arrowdown")) dy -= 1;
    if (k.has("d") || k.has("arrowright")) dx += 1;
    if (k.has("a") || k.has("arrowleft")) dx -= 1;
    if (dx === 0 && dy === 0) return null;
    const deg = ((Math.atan2(dx, -dy) * 180) / Math.PI + 360) % 360;
    return { dir: Math.round(deg) % 360, throttle: 1 };
  }

  /** Companion input: the jalak trails the reticle (scout where you aim),
   * F recalls it to your side (PLAN §2.3 leash clamps). */
  private companionInput(obs: PlayObs): { mv: { dir: number; throttle: number }; action?: Record<string, unknown> } {
    const comp = obs.you.companion;
    if (this.state.keys.has("f")) {
      return { mv: { dir: 0, throttle: 0 }, action: { type: "heel" } };
    }
    if (!comp.alive) return { mv: { dir: 0, throttle: 0 } };
    const w = this.screenToWorld(this.mouseScreen.x, this.mouseScreen.y);
    const dx = w.x - comp.pos[0];
    const dy = w.y - comp.pos[1];
    const d = Math.hypot(dx, dy);
    if (d < 26) return { mv: { dir: 0, throttle: 0 } };
    const dir = ((Math.atan2(dx, dy) * 180) / Math.PI + 360) % 360;
    return { mv: { dir: Math.round(dir) % 360, throttle: Math.min(1, d / 90) } };  }

  private sendInput(obs: PlayObs): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const mv = this.moveDir();
    let action: Record<string, unknown> | undefined;
    if (this.state.dashQueued) {
      // The sim dashes toward facing when standing still, so no move check.
      action = { type: "dash" };
    } else if (this.state.sprintToggled) {
      // Sprint is a server-side toggle (PLAN §2.2); it takes the action slot
      // this tick, and firing stays blocked while it is on.
      this.sprinting = !this.sprinting;
      this.state.sprintToggled = false;
      action = { type: "sprint", on: this.sprinting };
    } else if (this.state.keys.has("shift")) {
      action = { type: "shield" };
    } else if ((this.state.mouseDown || this.state.fireQueued) && !this.sprinting) {
      const w = this.screenToWorld(this.mouseScreen.x, this.mouseScreen.y);
      action = { type: "fire", target: { x: toFix(w.x), y: toFix(w.y) } };
    }
    this.state.fireQueued = false;
    this.state.dashQueued = false;

    const compIn = this.companionInput(obs);
    const compAction = compIn.action;

    const msg = {
      tick: obs.tick,
      main: {
        move: { dir: mv?.dir ?? 0, throttle: mv ? toFix(mv.throttle) : 0 },
        action,
      },
      companion: {
        move: { dir: compIn.mv.dir, throttle: toFix(compIn.mv.throttle) },
        action: compAction,
      },
    };
    this.ws.send(JSON.stringify(msg));
  }

  /** Input heartbeat (reliability over policing): input used to ride
   * observation arrival alone, so a stalled link stalled input too — fewer
   * observations meant fewer inputs, compounding exactly when the link was
   * worst. This 50ms pulse keeps input flowing off the observation path;
   * the server accepts slightly stale tick stamps (input window) and
   * repeats the last move when a tick arrives empty (momentum fill), so an
   * extra send can never hurt — worst case it re-sends what the player
   * still holds. Latched one-shots (tap fire, dash) are consumed on first
   * send, so a heartbeat right behind an observation send is a no-op. */
  private heartbeat: number | null = null;

  private startHeartbeat(): void {
    if (this.heartbeat !== null) return;
    this.heartbeat = window.setInterval(() => {
      if (
        this.playing && this.lastObs && this.ws &&
        this.ws.readyState === WebSocket.OPEN
      ) {
        this.sendInput(this.lastObs);
      }
    }, 50);
  }

  private stopHeartbeat(): void {
    if (this.heartbeat !== null) {
      clearInterval(this.heartbeat);
      this.heartbeat = null;
    }
  }

  leave(): void {
    this.leaveRequested = true;
    this.clearRetryTimer();
    this.stopHeartbeat();
    this.ws?.close();
    this.ws = null;
    this.playing = false;
  }

  private setStatus(s: PlayStatus, detail?: string): void {
    this.cb.onStatus(s, detail);
  }
}
