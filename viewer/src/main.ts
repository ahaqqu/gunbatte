/** Viewer entrypoint: replay playback, auto-director, mind-cam, and the
 * hybrid human play client (humans join the same queue as AI bots). */

// Issue #38: the shipped Content-Security-Policy forbids eval (script-src
// 'self'), and pixi's WebGL renderer generates uniform-sync functions with
// `new Function` by default. This side-effect import swaps in pixi's
// eval-free polyfills so the strict header can stay strict. Keep it first:
// it must install before any renderer initializes.
import "pixi.js/unsafe-eval";
import { Graphics } from "pixi.js";
import { CamMode, Frame, MapData, ReplayData, PICKUP_STRIDE, PROJ_STRIDE, WEAPONS, botColor, pickupKindIdx, weaponIdx, K, P } from "./types.js";
import { buildPlayerCam, LoadedReplay, loadReplay } from "./sim.js";
import { LobbyInfo, PlayClient, PlayObs } from "./play.js";
import { Stage } from "./render/stage.js";
import { drawArena } from "./render/arena.js";
import { UnitViews } from "./render/units.js";
import { DeadReckoner } from "./render/deadreckon.js";
import { OwnPredictor } from "./render/predict.js";
import { Fx } from "./render/fx.js";
import { PickupLayer, ProjectileLayer, ZoneLayerView } from "./render/world.js";
import { FogView } from "./render/fog.js";
import { MindCam } from "./render/mindcam.js";
import { Director } from "./render/director.js";
import { Hud } from "./ui/hud.js";
import { Timeline } from "./ui/timeline.js";
import { sfx, panVol } from "./audio.js";
import { startAmbient, stopAmbient } from "./ambient.js";
import { startHero, stopHero } from "./render/hero.js";

const TICK_RATE = 10;

(window as unknown as { __errors: string[] }).__errors = [];
window.addEventListener("error", (e) => {
  (window as unknown as { __errors: string[] }).__errors.push(String(e.message));
});
window.addEventListener("unhandledrejection", (e) => {
  const reason = (e as PromiseRejectionEvent).reason;
  (window as unknown as { __errors: string[] }).__errors.push("rejection: " + String(reason?.stack ?? reason));
});

const loading = document.getElementById("loading")!;
const loadStatus = document.getElementById("load-status")!;
const loadBar = document.getElementById("load-bar")!;
const picker = document.getElementById("picker")!;
const replaysPage = document.getElementById("replays-page")!;
const lobbyPage = document.getElementById("lobby-page")!;

/** Debug HUD (?debug=true): the HP/energy/cooldowns panel exists for
 * development and spectating internals. Normal play hides it — it covered
 * the arena, blocked mouse aiming over it, and every value it shows the
 * player needs is visible on the field itself. */
const DEBUG_HUD = new URLSearchParams(location.search).get("debug") === "true";

const stage = new Stage();
const hud = new Hud();
const timeline = new Timeline();
const director = new Director();

let replay: LoadedReplay | null = null;
let unitViews: UnitViews | null = null;
let projs: ProjectileLayer | null = null;
let pickups: PickupLayer | null = null;
let zoneView: ZoneLayerView | null = null;
let fx: Fx | null = null;
let fog: FogView | null = null;
let mindcam: MindCam | null = null;
let mapCache: MapData | null = null;
let mindHeat: import("pixi.js").Graphics | null = null;
let mindBubbles: import("pixi.js").Graphics | null = null;

let idx = 0;            // fractional frame cursor
let playing = true;
let speed = 1;
let lastTs = performance.now();
let firedEvents = new Set<number>();
let camData: { bot: number; frames: unknown[] } | null = null;
let placementsFinal: number[] = [];
let winnerShown = false;
/** Juice: freeze/slow the timeline on big moments (PLAN §7.3). */
let hitstopUntil = 0;

function setProgress(p: number, label: string): void {
  loadBar.style.width = `${Math.min(100, p * 100)}%`;
  loadStatus.textContent = label;
}

async function boot(): Promise<void> {
  try {
    const params = new URLSearchParams(location.search);
    const replayParam = params.get("replay");
    if (replayParam) {
      await startReplayUrl(replayParam);
    } else if (params.has("join")) {
      // A shared invite link: the code in the URL puts you straight in the room.
      await startPlay(nameFromUrl(), params.get("mode") === "boss" ? "boss" : "royale", {
        action: "join",
        code: params.get("join")!,
      });
    } else if (params.has("host")) {
      await startPlay(nameFromUrl(), params.get("mode") === "boss" ? "boss" : "royale", { action: "create" });
    } else if (params.has("play")) {
      const mode = params.get("mode") === "boss" ? "boss" : "royale";
      await startPlay((params.get("name") || "human").replace(/[^a-zA-Z0-9_-]/g, "").slice(0, 16) || "human", mode);
    } else {
      await showPicker();
    }
  } catch (e) {
    // Never leave a black screen: surface fatal boot errors on the overlay.
    loadStatus.textContent = "failed to start: " + String((e as Error)?.message ?? e);
    (window as unknown as { __errors: string[] }).__errors.push("boot: " + String((e as Error)?.stack ?? e));
  }
}

interface ReplayItem { name: string; url: string; size_kb: number }

/** Playful names for the name-field placeholder. Alphanumeric only: the value
 * gets sanitized on submit, so anything else would silently change. */
const NAME_A = [
  "Sleepy", "Wobbly", "Spicy", "Grumpy", "Turbo", "Soggy", "Crispy", "Sneaky",
  "Sleepy", "Bouncy", "Salty", "Fluffy", "Dizzy", "Chunky", "Sassy", "Sweaty",
  "Grumpy", "Toasty", "Prickly", "Zooming", "Hungry", "Silly", "Sleepy", "Noodle",
];
const NAME_B = [
  "Tarsius", "Jalak", "Bean", "Slime", "Cactus", "Nugget", "Waffle", "Pickle",
  "Mango", "Tofu", "Dumpling", "Pigeon", "Llama", "Potato", "Goblin", "Muffin",
  "Biscuit", "Penguin", "Avocado", "Meatball", "Coconut", "Raccoon", "Banan",
  "Turnip", "Noodle", "Walnut", "Sardine", "Pancake",
];

export function funnyName(): string {
  const a = NAME_A[Math.floor(Math.random() * NAME_A.length)];
  const b = NAME_B[Math.floor(Math.random() * NAME_B.length)];
  return `${a}${b}`;
}

const REPLAYS_PER_PAGE = 10;
let replaysCache: ReplayItem[] | null = null;
let replaysPageNum = 0;

function hideMenus(): void {
  picker.classList.add("hidden");
  replaysPage.classList.add("hidden");
  lobbyPage.classList.add("hidden");
  stopHero();
  sfx.stopMenuTheme();
}

async function listReplays(): Promise<ReplayItem[]> {
  if (replaysCache) return replaysCache;
  const res = await fetch("./api/replays");
  replaysCache = await res.json();
  return replaysCache!;
}

async function showPicker(): Promise<void> {
  loading.classList.add("hidden");
  hideMenus();
  picker.classList.remove("hidden");
  sfx.startMenuTheme();
  // Home mascots: the dancing tarsius with its jalak circling overhead.
  void startHero(document.getElementById("hero-host")!);
  // Ambient home screen: the newest recorded match plays behind the menu.
  startAmbient(stage, ensureStage);

  // Badge the library button with the replay count (metadata-only fetch).
  listReplays().then((items) => {
    if (items.length === 0) return;
    const badge = document.getElementById("replay-count")!;
    badge.textContent = String(items.length);
    badge.classList.remove("hidden");
  }).catch(() => { /* offline: button still opens the library page */ });

  const fileInput = document.getElementById("file-input") as HTMLInputElement;
  fileInput.addEventListener("change", async () => {
    const f = fileInput.files?.[0];
    if (!f) return;
    const text = await f.text();
    await startReplay(text, f.name);
  });

  // Hybrid play: humans enter the same queue as the AI bots — either the
  // public quick-match queue, or a private lobby whose code they share.
  const playBtn = document.getElementById("play-btn") as HTMLButtonElement | null;
  const nameInput = document.getElementById("play-name") as HTMLInputElement | null;
  const playerName = (): string =>
    (nameInput?.value || nameInput?.placeholder || "human")
      .replace(/[^a-zA-Z0-9_-]/g, "")
      .slice(0, 16) || "human";
  if (playBtn && nameInput) {
    // Seed a random funny name as the placeholder hint: the field stays empty,
    // so it reads as a suggestion the user can take or replace — but a bare
    // click on ENTER still gets them in with that name.
    const suggestion = funnyName();
    nameInput.placeholder = suggestion;
    playBtn.addEventListener("click", () => {
      location.href = "?play=1&name=" + encodeURIComponent(playerName());
    });
    nameInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") playBtn.click();
    });
  }
  // Slain the Boss: queue as a raider for a co-op raid on the AI boss.
  document.getElementById("boss-btn")?.addEventListener("click", () => {
    location.href = "?play=1&mode=boss&name=" + encodeURIComponent(playerName());
  });
  // Private rooms keep living behind one quiet link: the popup centers over
  // the menu, and its create/join buttons navigate to self-contained URLs.
  document.getElementById("lobby-open")?.addEventListener("click", () => {
    lobbyPage.classList.remove("hidden");
    document.getElementById("lobby-setup")!.classList.remove("hidden");
    document.getElementById("lobby-room")!.classList.add("hidden");
    setLobbyNotice("", false);
    wireLobbySetup(playerName(), "royale");
  });

  document.getElementById("browse-replays-btn")!.addEventListener("click", () => {
    location.hash = "#replays";
  });
  document.getElementById("replays-back")!.addEventListener("click", () => {
    location.hash = "";
  });
  document.getElementById("replays-prev")!.addEventListener("click", () => {
    if (replaysPageNum > 0) { replaysPageNum--; renderReplays(); }
  });
  document.getElementById("replays-next")!.addEventListener("click", () => {
    const items = replaysCache ?? [];
    if ((replaysPageNum + 1) * REPLAYS_PER_PAGE < items.length) { replaysPageNum++; renderReplays(); }
  });
  window.addEventListener("hashchange", () => {
    if (location.hash === "#replays") {
      // Don't overlay the library on a running replay/play session.
      if (!replay && !playClient) void openReplays();
      else location.hash = "";
    } else if (!picker.classList.contains("hidden") || !replaysPage.classList.contains("hidden")) {
      hideMenus();
      picker.classList.remove("hidden");
      void startHero(document.getElementById("hero-host")!);
    }
  });

  if (location.hash === "#replays") void openReplays();
}

async function openReplays(): Promise<void> {
  hideMenus();
  replaysPage.classList.remove("hidden");
  const list = document.getElementById("replays-list")!;
  const indicator = document.getElementById("replays-page-indicator")!;
  let items: ReplayItem[];
  try {
    items = await listReplays();
  } catch {
    list.innerHTML = `<div style="color:var(--text-dim);font-size:13px;padding:12px 0">No replay server detected.<br>Open a replay file from the home screen.</div>`;
    indicator.textContent = "—";
    return;
  }
  if (items.length === 0) {
    list.innerHTML = `<div style="color:var(--text-dim);font-size:13px;padding:12px 0">No replays found on the server.<br>Generate one: <code>gunbatte-runner run --preset default16 --seed 42 --out replays/match.json</code></div>`;
    indicator.textContent = "—";
    return;
  }
  replaysPageNum = Math.min(replaysPageNum, Math.floor((items.length - 1) / REPLAYS_PER_PAGE));
  renderReplays();
}

function renderReplays(): void {
  const items = replaysCache ?? [];
  const list = document.getElementById("replays-list")!;
  list.innerHTML = "";
  const start = replaysPageNum * REPLAYS_PER_PAGE;
  for (const it of items.slice(start, start + REPLAYS_PER_PAGE)) {
    const row = document.createElement("div");
    row.className = "picker-row";
    const name = document.createElement("span");
    name.textContent = it.name;
    const meta = document.createElement("span");
    meta.className = "picker-meta";
    meta.textContent = `${it.size_kb.toFixed(0)} KB`;
    row.append(name, meta);
    row.addEventListener("click", () => startReplayUrl(it.url));
    list.appendChild(row);
  }
  const pages = Math.max(1, Math.ceil(items.length / REPLAYS_PER_PAGE));
  document.getElementById("replays-page-indicator")!.textContent = `${replaysPageNum + 1} / ${pages}`;
  (document.getElementById("replays-prev") as HTMLButtonElement).disabled = replaysPageNum <= 0;
  (document.getElementById("replays-next") as HTMLButtonElement).disabled = replaysPageNum + 1 >= pages;
}

// Stage init is shared by the ambient background, replay playback and live
// play; memoize the in-flight promise so concurrent callers (e.g. the user
// clicks a replay while the ambient background is still initializing) wait
// on the same init instead of racing past didInit into an uninitialized stage.
let stageReady: Promise<void> | null = null;

function ensureStage(): Promise<void> {
  stageReady ??= initStage();
  return stageReady;
}

async function initStage(): Promise<void> {
  try {
    // Give the display font a beat to load so Pixi-canvas text doesn't bake
    // in the fallback; never block longer than ~1.2s (offline is fine).
    try {
      await Promise.race([
        (document as Document & { fonts?: FontFaceSet }).fonts?.load('800 16px "Baloo 2"') ?? Promise.resolve(),
        new Promise((r) => setTimeout(r, 1200)),
      ]);
    } catch { /* font stays fallback */ }
    await stage.init(document.getElementById("stage-host")!, (s) => setProgress(0.005, s));
    if (!mindcam && stage.didInit) {
      mindHeat = new Graphics();
      mindBubbles = new Graphics();
      stage.fogLayer.addChild(mindHeat, mindBubbles);
      mindcam = new MindCam(mindHeat, mindBubbles);
    }
  } catch (e) {
    stageReady = null; // allow a later attempt to actually retry
    throw e;
  }
}

async function fetchMap(mapId: string): Promise<MapData> {
  if (mapCache && (mapCache as any).id === mapId) return mapCache;
  try {
    const res = await fetch(`./api/map/${mapId}`);
    if (res.ok) {
      mapCache = await res.json();
      return mapCache!;
    }
  } catch { /* offline */ }
  return { id: mapId, size: 3200, walls: [], spawns: [] };
}

async function startReplayUrl(url: string): Promise<void> {
  const name = decodeURIComponent(url.split("/").pop() ?? "replay");
  const res = await fetch(url);
  const json = await res.text();
  await startReplay(json, name);
}

async function startReplay(json: string, name: string): Promise<void> {
  hideMenus();
  stopAmbient(stage);
  loading.classList.remove("hidden");
  setProgress(0.01, "loading replay…");
  await ensureStage();
  replay = await loadReplay(json, setProgress);
  const data = replay.data;

  document.title = `${name} — GUNBATTE ROYALE`;
  loading.classList.add("hidden");

  drawArena(stage, data.map);
  unitViews = new UnitViews(stage, data.botNames);
  projs = new ProjectileLayer(stage);
  pickups = new PickupLayer(stage);
  zoneView = new ZoneLayerView(stage);
  fx = new Fx(stage);
  fog = new FogView(stage);

  hud.showAll();
  hud.setHeader(data.botNames, data.mapId, data.seed);
  hud.buildLegend(data.botNames, (bot) => {
    setCamMode(`follow:${bot}`);
  });
  timeline.show();
  timeline.setRange(data.totalTicks);
  timeline.addMarkers(data.killMarkers, data.botNames);
  timeline.setCamOptions(data.botNames, setCamMode);
  timeline.onPlayToggle = () => { playing = !playing; };
  timeline.onSeek = (frac) => { idx = frac * (data.totalTicks - 1); winnerShown = false; hud.hideWinner(); };
  timeline.onSpeed = (s) => { speed = s; };
  timeline.onCam = setCamMode;

  stage.onWheel = (delta) => {
    const t = stage.getTarget();
    stage.setTarget(t.x, t.y, Math.min(3, Math.max(0.12, t.zoom * (delta > 0 ? 0.88 : 1.14))));
    director.mode = "global"; // manual zoom takes over
    timeline.camSelect.value = "global";
  };
  stage.onDrag = (dx, dy) => {
    const t = stage.getTarget();
    stage.setTarget(t.x - dx / t.zoom, t.y - dy / t.zoom, t.zoom);
    director.mode = "global";
    timeline.camSelect.value = "global";
  };

  placementsFinal = computePlacements(data);

  requestAnimationFrame(loop);
}

function setCamMode(mode: string): void {
  director.mode = mode as CamMode;
  timeline.camSelect.value = mode;
  const camBot = mode.startsWith("cam:") ? Number(mode.slice(4)) : null;
  if (camBot !== null) {
    fog?.show();
    if (!camData || camData.bot !== camBot) {
      // Build the fog view lazily with a mini progress overlay.
      playing = false;
      loading.classList.remove("hidden");
      setProgress(0, `reconstructing ${replay!.data.botNames[camBot]}'s memory…`);
      buildPlayerCam(replay!.sim, camBot, (p) => setProgress(p, `reconstructing memory… ${Math.round(p * 100)}%`))
        .then((cam) => {
          camData = { bot: camBot, frames: cam.frames };
          loading.classList.add("hidden");
          playing = true;
        });
    }
  } else {
    fog?.hide();
  }
  hud.legendActive(mode.startsWith("follow:") ? Number(mode.slice(7)) : mode.startsWith("cam:") ? Number(mode.slice(4)) : null);
}

/** Placement order: winner first, then elimination order reversed. */
function computePlacements(data: ReplayData): number[] {
  const n = data.botNames.length;
  const order: number[] = [];
  if (data.winner !== null) order.push(data.winner);
  const seen = new Set(order);
  for (let i = data.killMarkers.length - 1; i >= 0; i--) {
    const v = data.killMarkers[i].victim;
    if (!seen.has(v)) { seen.add(v); order.push(v); }
  }
  for (let b = 0; b < n; b++) if (!seen.has(b)) { order.push(b); seen.add(b); }
  return order;
}

function loop(ts: number): void {
  requestAnimationFrame(loop);
  const dt = Math.min(0.1, (ts - lastTs) / 1000);
  lastTs = ts;
  if (!replay || !unitViews) return;
  const data = replay.data;
  const total = data.totalTicks;

  if (playing) {
    // Slow-mo kill cam: the final seconds play at 30% speed (PLAN §6.2).
    const effSpeed = idx > total - 25 ? speed * 0.3 : speed;
    if (ts >= hitstopUntil) {
      idx += dt * TICK_RATE * effSpeed;
    }
    if (idx >= total - 1) {
      idx = total - 1;
      playing = false;
      if (!winnerShown && data.winner !== null) {
        winnerShown = true;
        hud.winner(data.winner, data.botNames, placementsFinal);
        sfx.play("victory", 0, 1);
      }
    }
  }
  const iA = Math.min(total - 1, Math.max(0, Math.floor(idx)));
  const iB = Math.min(total - 1, iA + 1);
  const t = idx - iA;
  const frameA = data.frames[iA];
  const frameB = data.frames[iB];

  // Fire FX once per crossed frame.
  if (!firedEvents.has(iB)) {
    firedEvents.add(iB);
    for (const e of frameB.events) {
      fx!.handleEvents([e]);
      // Audio: distance-attenuated, stereo-panned around the camera.
      const at = (e.at ?? e.from) as [number, number] | undefined;
      if (at) {
        const { pan, vol } = panVol(stage.cam, at, window.innerWidth);
        switch (e.type) {
          case "shot": sfx.play("shot", pan, vol); break;
          case "hit": sfx.play("hit", pan, vol); break;
          case "death": sfx.play("boom", pan, Math.max(0.55, vol)); break;
          case "companion_down": sfx.play("boom", pan, vol * 0.45); break;
          case "pickup": sfx.play("pickup", pan, vol * 0.8); break;
        }
      } else if (e.type === "zone_locked" || e.type === "zone_shrink_started") {
        sfx.play("zone", 0, 0.85);
      }
      if (e.type === "death") {
        const bot = e.bot as number;
        hud.legendDead(bot);
        hud.legendElim(bot, `t${frameB.tick}`);
        hud.kill((e.killer as number | null) ?? null, bot, data.botNames);
        // Hitstop on every elimination: 120ms freeze (PLAN §7.3 juice).
        hitstopUntil = ts + 120;
      }
    }
  }

  // World layers (units interpolate A→B for smooth 60fps motion).
  unitViews.update(frameA.units, frameB.units, t, frameA.unitCount, true);
  projs!.update(
    frameA.projs, frameA.projCount,
    frameB.projs, frameB.projCount, t,
    (x, y, color, intense) => fx!.tracer(x, y, color, intense),
  );
  pickups!.update(frameA.pickups, frameA.pickupCount, frameA.tick);
  const shrinking = Math.abs(frameB.zone[2] - frameA.zone[2]) > 0.0001;
  zoneView!.update(frameA.zone, shrinking, frameA.zone[5] > 0);

  // Dash afterimages: dashing mains leave a glowing wake; sprinters kick dust.
  for (let s = 0; s < frameA.unitCount; s++) {
    const o = s * 11;
    const fl = frameA.units[o + 9];
    if ((fl & 4) !== 0) {
      const bot = frameA.units[o + 1];
      fx!.tracer(frameA.units[o + 3], frameA.units[o + 4], parseInt(botColor(bot).slice(1), 16), false);
    } else if ((fl & 3) === 3 && Math.random() < 0.1) {
      fx!.dust(frameA.units[o + 3] - frameA.units[o + 5] * 0.05, frameA.units[o + 4] - frameA.units[o + 6] * 0.05);
    }
  }

  // Mind-cam overlay (M toggles).
  const botPositions = new Map<number, { x: number; y: number }>();
  for (let s = 0; s < frameA.unitCount; s += 2) {
    if ((frameA.units[s * 11 + 9] & 1) !== 0) {
      botPositions.set(frameA.units[s * 11 + 1], { x: frameA.units[s * 11 + 3], y: frameA.units[s * 11 + 4] });
    }
  }
  mindcam?.update(frameA.minds, Math.floor(frameA.tick / 5), botPositions);

  // Fog (player-cam).
  const mode = director.mode as CamMode;
  if (mode.startsWith("cam:") && camData) {
    const camFrame = (camData.frames as (import("./types.js").CamFrame | null)[])[iB] ?? (camData.frames as unknown as (import("./types.js").CamFrame | null)[])[iA];
    if (camFrame) {
      fog!.update(camFrame, camData.bot);
    }
  }

  // Camera.
  const camBot = mode.startsWith("follow:") ? Number(mode.slice(7))
    : mode.startsWith("cam:") ? Number(mode.slice(4)) : null;
  let botPos: { x: number; y: number } | null = null;
  if (camBot !== null && camBot * 2 < frameA.unitCount) {
    const o = (camBot * 2) * 11;
    botPos = { x: frameA.units[o + 3], y: frameA.units[o + 4] };
  }
  const target = director.targetFor(frameA, frameB, botPos);
  stage.setTarget(target.x, target.y, target.zoom);

  // HUD.
  hud.stats(frameA.tick, frameA.zone[6], zonePhaseOf(frameA), shrinking);
  timeline.update(idx / Math.max(1, total - 1), frameA.tick, playing);

  fx!.update(dt);
}

function zonePhaseOf(frame: Frame): number {
  const r = frame.zone[2];
  const ladder = [1600, 1200, 850, 550, 300, 0];
  for (let i = 0; i < ladder.length; i++) {
    if (r > ladder[i] - 1) return i;
  }
  return ladder.length - 1;
}

// ---------------------------------------------------------------------------
// Hybrid play: a human enters the same bot queue as the AI (PLAN §4.1).
// The page loads fresh via ?play=1&name=<name> so no replay state lingers.
// ---------------------------------------------------------------------------

let playClient: PlayClient | null = null;
let playUnits: UnitViews | null = null;
let playZone: ZoneLayerView | null = null;
let playFx: Fx | null = null;
let playFog: FogView | null = null;
let playProjs: ProjectileLayer | null = null;
let playPickups: PickupLayer | null = null;
let playLoopRunning = false;
// Seen-projectile frames for the pellet renderer: the previous and current
// observation packed into the shared layout, interpolated between in playLoop.
let playProjPrev: Float32Array = new Float32Array(0);
let playProjCur: Float32Array = new Float32Array(0);
let playProjTick = -1;
let playProjObsTs = 0;
// Dead reckoning for live units (render/deadreckon.ts): each fresh snapshot
// is folded in once; every frame then renders carried-forward positions so
// motion is smooth between the 10Hz observations.
const playDr = new DeadReckoner();
let playUnitTick = -1;
// Own-unit prediction (render/predict.ts): the own main and companion are
// driven through the WASM movement oracle every frame; enemies keep the
// velocity dead reckoning above.
const playPredictor = new OwnPredictor();
let playEntrants: string[] = [];
let playYouIndex = 0;
// Combat-feel state: diffs between the last two observations drive the
// hit confirms, hurt flashes, kill feed and audio (all client-side juice).
let prevHp = 100;
let prevEnergy = 100;
let prevMainAlive = true;
let prevCompAlive = true;
let prevEnemyHp = new Map<number, number>();
/** Last observation's projectile set: new = muzzle flash, gone popper = boom. */
let prevProjectiles = new Map<number, { x: number; y: number; w: number; mine: boolean }>();
let prevWeapon: string | null = null;
let killProcessed = 0;
let lastZoneBeep = 0;
let bannerTimer = 0;
// Play-mode FX state: last-seen enemy positions (death blasts), dash/shield
// rising edges, last companion position, and a camera zoom punch on kills.
let lastSeenPos = new Map<number, [number, number]>();
let prevDashOn = false;
let prevShieldOn = false;
let prevCompPos: [number, number] | null = null;
let zoomPunch = 0;

const reticle = document.getElementById("reticle")!;
const vignette = document.getElementById("vignette")!;
const dmgArrow = document.getElementById("dmg-arrow")!;
const playBanner = document.getElementById("play-banner")!;

/** One-shot center banner ("ELIMINATED!", zone warnings). */
function showBanner(text: string, color: string, ms = 1600): void {
  playBanner.textContent = text;
  playBanner.style.color = color;
  playBanner.classList.remove("hidden");
  playBanner.style.animation = "none";
  void playBanner.offsetWidth; // restart the pop animation
  playBanner.style.animation = "";
  clearTimeout(bannerTimer);
  bannerTimer = window.setTimeout(() => playBanner.classList.add("hidden"), ms);
}

function flashVignette(strength: number): void {
  vignette.style.opacity = String(Math.min(0.9, strength));
  setTimeout(() => { vignette.style.opacity = "0"; }, 60);
}

/** Direction the last hit came from: rotate the edge arrow, auto-hide. */
function showDamageArrow(angleRad: number): void {
  dmgArrow.classList.remove("hidden");
  dmgArrow.style.transform = `translate(-50%, -50%) rotate(${angleRad}rad)`;
  clearTimeout((showDamageArrow as unknown as { t?: number }).t);
  (showDamageArrow as unknown as { t?: number }).t = window.setTimeout(
    () => dmgArrow.classList.add("hidden"), 700,
  );
}

function updateReticle(): void {
  if (!playClient) return;
  const m = playClient.mouseScreen;
  reticle.style.transform = `translate(${m.x}px, ${m.y}px) translate(-50%, -50%)`;
  reticle.classList.toggle("fire", playClient.firing);
  const obs = playClient.lastObs;
  reticle.classList.toggle("cd", !!obs && (obs.you.main.cooldown.fire ?? 0) > 0);
  reticle.classList.toggle("sprint", playClient.sprinting);
  // The reticle wears the gun's color — you always know what you're holding.
  if (obs) {
    reticle.style.setProperty("--gun", WEAPONS[weaponIdx(obs.you.main.weapon)].color);
  }
}

const esc = (s: string): string =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

function playOverShow(crown: string, title: string, sub: string, retryable = false): void {
  const card = document.querySelector("#play-over .crown")!;
  card.textContent = crown;
  document.getElementById("play-over-title")!.textContent = title;
  const subEl = document.getElementById("play-over-sub")!;
  subEl.innerHTML = sub;
  document.getElementById("play-retry")!.classList.toggle("hidden", !retryable);
  document.getElementById("play-over")!.classList.remove("hidden");
}

function playOverHide(): void {
  document.getElementById("play-over")!.classList.add("hidden");
}

function setPlayStatus(s: string, detail?: string): void {
  const text = detail ? s + " — " + detail : s;
  if (DEBUG_HUD) {
    const el = document.getElementById("play-status")!;
    el.textContent = text;
    document.getElementById("play-hud")!.classList.remove("hidden");
  } else {
    // No panel in normal play: the status line lives on a click-through
    // chip that can never sit under the mouse.
    document.getElementById("play-note")!.textContent = text;
  }
}

/** The one entry into a live match. `lobby` turns it into a private room:
 * the server keeps this socket out of the public queue, the room overlay shows
 * the share code + roster, and the host presses START when everyone is in. */
async function startPlay(
  name: string,
  mode: "royale" | "boss" = "royale",
  lobby: { action: "create" | "join"; code?: string } | null = null,
): Promise<void> {
  hideMenus();
  stopAmbient(stage);
  await ensureStage();
  loading.classList.add("hidden");
  document.body.classList.add("playing");

  const map = await fetchMap("arena-1");
  drawArena(stage, map);
  playZone = new ZoneLayerView(stage);
  playFx = new Fx(stage);
  playFog = new FogView(stage);
  // Live play shares the spectator's candy-pellet bullets (slightly enlarged)
  // and pickup tins; both layers sit under the fog overlay and the strict
  // observation lists keep them inside the vision hole.
  playProjs = new ProjectileLayer(stage, 1.45);
  playPickups = new PickupLayer(stage);

  if (lobby) {
    // The room screen replaces the match HUD until the match begins.
    showLobbyRoom(lobby);
  } else {
    showMatchHud();
    setPlayStatus("connecting…");
  }

  playClient = new PlayClient(name, {
    // In a room, connection status belongs on the room screen — the match HUD
    // is not up yet and must not peek out from behind the lobby card.
    onStatus: lobby ? (s, d) => setLobbyNotice(d ? `${s} ${d}` : s, false) : setPlayStatus,
    onLobby: (info) => {
      lobbyInfo = info;
      // A roster arrived: the room exists — reveal it (hiding the setup form)
      // and make the URL shareable.
      document.getElementById("lobby-setup")!.classList.add("hidden");
      document.getElementById("lobby-room")!.classList.remove("hidden");
      setLobbyNotice("", false);
      history.replaceState(
        null,
        "",
        `${location.pathname}?${new URLSearchParams({ mode: info.mode, name, join: info.code })}`,
      );
      renderLobby(info, name);
    },
    onRoster: (info) => {
      lobbyInfo = info;
      renderLobby(info, name);
    },
    onLobbyClosed: (reason) => {
      lobbyInfo = null;
      renderLobby(null, name);
      setLobbyNotice(`lobby closed — ${reason}`, true);
    },
    onError: (msg) => {
      setLobbyNotice(msg, true);
    },
    onFatal: (msg, retryable) => {
      // A dead end the socket cannot talk its way out of. Every card names
      // what happened and what the way out is — nothing renders as a
      // silent "queued" forever.
      if (msg === "bad token") {
        // A "bad token" refusal is otherwise a dead end: the name is claimed
        // and this browser holds no working secret for it (unwritable storage
        // in private mode, or a secret that never belonged to this name). The
        // server never re-issues one — the only way forward is another name,
        // so surface the modal that carries the way back to the menu.
        playOverShow(
          "🔒",
          "NAME PROTECTED",
          "this name is claimed and this browser can't prove it's you —<br>LEAVE ARENA, then pick another name",
        );
      } else if (msg.includes("newer connection")) {
        playOverShow(
          "🔁",
          "OPENED ELSEWHERE",
          "this name was just opened in a newer tab or window —<br>this page is no longer in control",
        );
      } else {
        playOverShow(
          "⚠",
          "CONNECTION LOST",
          `${esc(msg)}<br>a retry puts you straight back in the queue`,
          retryable,
        );
      }
    },
    onStart: (youIndex, entrants, role) => {
      playEntrants = entrants;
      playYouIndex = youIndex;
      const realNames = entrants.map((n, i) => (i === youIndex ? n + " (YOU)" : n));
      playUnits = new UnitViews(stage, realNames);
      showMatchHud();
      if (lobby) hideLobbyRoom();
      // Fresh prediction state for the new match: the oracle builds on the
      // same embedded map the server sims, and the dead reckoner hands the
      // own units' between-snapshot motion to it.
      playDr.reset();
      playPredictor.onMatchStart(playClient?.mapId ?? "arena-1", role);
      playDr.drive(1 + youIndex, playPredictor.driveMain);
      if (role !== "boss") playDr.drive(101 + youIndex, playPredictor.driveComp);
      // The room's own mode wins: an invite code can be pasted into a link
      // whose ?mode disagrees (a royale link with a raid room's code).
      const header =
        (lobbyInfo?.mode ?? mode) === "boss"
          ? role === "boss"
            ? "SLAIN THE BOSS — you ARE the boss"
            : "SLAIN THE BOSS — raid!"
          : "live match";
      hud.setHeader(realNames, header, 0);
      prevHp = 100; prevEnergy = 100;
      prevMainAlive = true; prevCompAlive = true;
      prevEnemyHp = new Map(); prevProjectiles = new Map(); prevWeapon = null;
      playProjPrev = new Float32Array(0); playProjCur = new Float32Array(0);
      playProjTick = -1; playProjObsTs = 0;
      playUnitTick = -1;
      killProcessed = 0;
      lastSeenPos = new Map();
      prevDashOn = false; prevShieldOn = false; prevCompPos = null;
      zoomPunch = 0;
      playOverHide();
      setPlayStatus("in match — good luck");
    },
    onObs: () => {
      if (!playLoopRunning) {
        playLoopRunning = true;
        requestAnimationFrame(playLoop);
      }
    },
    onOver: (place, replayUrl) => {
      playPredictor.reset();
      setPlayStatus("match over — place " + place);
      const watch = replayUrl
        ? `<a href='${replayUrl}' target='_blank'>▶ watch the replay</a> · `
        : "";
      if (place === 1) {
        sfx.play("victory", 0, 1);
        playOverShow("👑", "GUNBATTE!!!", `${watch}you outlasted the whole lobby. necessary.`);
      } else {
        sfx.play("defeat", 0, 0.9);
        playOverShow("💀", `#${place} PLACE`, `${watch}five more minutes. then you're re-queued`);
      }
    },
  }, mode, lobby ? (lobby.action === "create" ? { action: "create" } : { action: "join", code: lobby.code ?? "" }) : null);

  document.getElementById("play-leave")!.addEventListener("click", () => {
    playClient?.leave();
    location.href = location.pathname; // back to the home screen
  });
  document.getElementById("play-retry")!.addEventListener("click", () => {
    playOverHide();
    playClient?.reconnect();
  });

  if (lobby) {
    wireLobbyRoom(name, mode, lobby);
    wireLobbySetup(name, mode);
  }

  const wsProto = location.protocol === "https:" ? "wss" : "ws";
  playClient.attachInput(document.getElementById("stage-host")!, (x, y) => stage.screenToWorld(x, y));
  playClient.connect(wsProto + "://" + location.host + "/ws/bot");
  requestAnimationFrame(playLoop);
}

/* ---------- private lobby screen (drawn over the arena) ---------- */

/** The room the current socket sits in, from the server's roster messages. */
let lobbyInfo: LobbyInfo | null = null;

function showLobbyRoom(lobby: { action: "create" | "join"; code?: string }): void {
  lobbyPage.classList.remove("hidden");
  // The setup form stays up until the server answers with a roster: a bad code
  // then leaves the form in place instead of stranding an empty room screen.
  document.getElementById("lobby-setup")!.classList.remove("hidden");
  document.getElementById("lobby-room")!.classList.add("hidden");
  setLobbyNotice(lobby.action === "create" ? "opening your room\u2026" : "joining\u2026", false);
}

/** Wire the lobby setup form (shown before a room exists, and if a code was
 * rejected): the two create buttons and the join box all navigate to a URL
 * that carries the intent, so a reload or a shared link is self-contained. */
function wireLobbySetup(name: string, mode: "royale" | "boss"): void {
  const nameEl = document.getElementById("lobby-name") as HTMLInputElement;
  const codeEl = document.getElementById("lobby-code") as HTMLInputElement;
  nameEl.value = name === "human" ? "" : name;
  nameEl.placeholder = name;
  codeEl.value = "";
  const fresh = (): string =>
    (nameEl.value || name).replace(/[^a-zA-Z0-9_-]/g, "").slice(0, 16) || "human";
  const goHost = (m: "royale" | "boss"): void => {
    location.href = `${location.pathname}?${new URLSearchParams({ mode: m, name: fresh(), host: "1" })}`;
  };
  const goJoin = (): void => {
    const code = codeEl.value.replace(/[^a-zA-Z0-9]/g, "").toUpperCase().slice(0, 8);
    if (code.length < 4) {
      setLobbyNotice("type the 4-letter code from your invite link", true);
      return;
    }
    location.href = `${location.pathname}?${new URLSearchParams({ mode, name: fresh(), join: code })}`;
  };
  document.getElementById("lobby-create-royale")!.onclick = () => goHost("royale");
  document.getElementById("lobby-create-boss")!.onclick = () => goHost("boss");
  document.getElementById("lobby-join")!.onclick = goJoin;
  codeEl.onkeydown = (e) => { if (e.key === "Enter") goJoin(); };
  // From the home screen the popup just closes; from a ?host=1 load, back
  // means a fresh home (the form has no other exit there).
  document.getElementById("lobby-back")!.onclick = () => {
    if (!picker.classList.contains("hidden")) lobbyPage.classList.add("hidden");
    else location.href = location.pathname;
  };
}

function hideLobbyRoom(): void {
  lobbyPage.classList.add("hidden");
}

function showMatchHud(): void {
  document.getElementById("topbar")!.classList.remove("hidden");
  document.getElementById("killfeed")!.classList.remove("hidden");
  if (DEBUG_HUD) document.getElementById("play-hud")!.classList.remove("hidden");
  reticle.classList.remove("hidden");
}

function setLobbyNotice(msg: string, bad: boolean): void {
  const el = document.getElementById("lobby-err")!;
  el.textContent = msg;
  el.classList.toggle("hidden", msg === "");
  el.style.color = bad ? "var(--danger)" : "var(--ink-dim)";
}

/** Wire the room screen's controls: copy the invite link, host START, leave. */
function wireLobbyRoom(
  name: string,
  mode: "royale" | "boss",
  lobby: { action: "create" | "join"; code?: string },
): void {
  const invite = (): string => {
    const q = new URLSearchParams({ join: lobbyInfo?.code ?? lobby.code ?? "", mode, name });
    return `${location.origin}${location.pathname}?${q.toString()}`;
  };
  document.getElementById("lobby-copy")!.onclick = async () => {
    const url = invite();
    try {
      await navigator.clipboard.writeText(url);
      setLobbyNotice("invite link copied ✓", false);
    } catch {
      setLobbyNotice(url, false); // clipboard blocked: show it to copy by hand
    }
  };
  document.getElementById("lobby-start")!.onclick = () => {
    // A raid room defaults to the built-in boss AI ("ai"); a royale room just
    // fills the roster up to 8.
    playClient?.startLobby(undefined, mode === "boss" ? "ai" : undefined);
  };
  document.getElementById("lobby-leave")!.onclick = () => {
    playClient?.leave();
    location.href = location.pathname; // back to the home screen
  };
}

/** Paint the room roster: share code, mode, members with host/boss tags. */
function renderLobby(info: LobbyInfo | null, you: string): void {
  document.getElementById("lobby-code-label")!.textContent = info?.code ?? "…";
  const mode = info?.mode ?? "royale";
  const isHost = info !== null && info.host === you;
  document.getElementById("lobby-mode-label")!.textContent =
    mode === "boss" ? "boss raid — raiders vs one giant tarsius" : "royale — last tarsius standing";
  const list = document.getElementById("lobby-members")!;
  list.innerHTML = "";
  const members = info?.members ?? [you];
  for (const m of members) {
    const li = document.createElement("li");
    const left = document.createElement("span");
    left.textContent = m;
    if (m === you) {
      const tag = document.createElement("span");
      tag.className = "you";
      tag.textContent = "(you)";
      left.appendChild(tag);
    }
    li.appendChild(left);
    const tags = document.createElement("span");
    if (info && m === info.host) {
      const t = document.createElement("span");
      t.className = "tag host";
      t.textContent = "host";
      tags.appendChild(t);
    }
    li.appendChild(tags);
    list.appendChild(li);
  }
  if (mode === "boss") {
    // The boss is not a room member by default — the server's AI plays it
    // (the host can cast a member instead, from the bot protocol).
    const li = document.createElement("li");
    const left = document.createElement("span");
    left.textContent = "THE BOSS";
    left.style.color = "var(--danger)";
    li.appendChild(left);
    const t = document.createElement("span");
    t.className = "tag boss";
    t.textContent = "server AI";
    li.appendChild(t);
    list.appendChild(li);
  }
  document.getElementById("lobby-host-controls")!.classList.toggle("hidden", !isHost);
  document.getElementById("lobby-wait")!.classList.toggle("hidden", isHost);
  if (isHost) setLobbyNotice("", false);
}

/** The name a share link asked for. */
function nameFromUrl(): string {
  const raw = new URLSearchParams(location.search).get("name") ?? "";
  return raw.replace(/[^a-zA-Z0-9_-]/g, "").slice(0, 16) || "human";
}

function playLoop(ts: number): void {
  if (!playClient) { playLoopRunning = false; return; }
  requestAnimationFrame(playLoop);
  const dt = Math.min(0.1, (ts - lastTs) / 1000);
  lastTs = ts;
  updateReticle();
  const obs = playClient.lastObs;
  if (!obs || !playEntrants.length) {
    playFx?.update(dt);
    return;
  }
  if (!playUnits) return;
  const bots = obs.global.bots;
  const me = obs.you.main;
  const names = playEntrants.map((n, i) => (i === playYouIndex ? n + " (YOU)" : n));

  // Fold each fresh snapshot into the dead reckoner once per observation,
  // and into the own-unit predictor (server truth for energy and action
  // flags).
  if (obs.tick !== playUnitTick) {
    playUnitTick = obs.tick;
    const drUnits: { id: number; pos: [number, number]; vel?: [number, number] }[] = [
      { id: 1 + playYouIndex, pos: me.pos, vel: me.vel },
    ];
    if (obs.you.companion.alive && obs.you.companion.pos) {
      drUnits.push({ id: 101 + playYouIndex, pos: obs.you.companion.pos, vel: obs.you.companion.vel });
    }
    for (const p of obs.seen.players) {
      if (p.id - 1 !== playYouIndex) drUnits.push({ id: p.id, pos: p.pos, vel: p.vel });
    }
    for (const c of obs.seen.companions) drUnits.push({ id: 100000 + c.id, pos: c.pos });
    playDr.observe(drUnits, ts);
    playPredictor.syncObs(obs, playClient.role);
  }
  // Per-frame context for the prediction drivers (input state, observation).
  playPredictor.beginFrame(obs, playClient);
  // One advance per unit per frame, reused by every consumer (sprites, fog,
  // camera, FX placement) — a second call would integrate the state twice.
  const drPos = new Map<number, [number, number]>();
  const drPosOf = (id: number, raw: [number, number]): [number, number] => {
    let p = drPos.get(id);
    if (!p) {
      p = playDr.pos(id, raw, ts, dt);
      drPos.set(id, p);
    }
    return p;
  };
  const mePos = drPosOf(1 + playYouIndex, me.pos);

  // Frame-shaped float view: self from obs.you, enemies through fog.
  const units = new Float32Array(bots * 2 * 11);
  const setUnit = (slot: number, id: number, bot: number, kind: number, u: { pos: [number, number]; vel?: [number, number]; facing?: number; hp?: number; alive: boolean; status?: string[]; maxhp: number }) => {
    const o = slot * 11;
    units[o] = id; units[o + 1] = bot; units[o + 2] = kind;
    units[o + 3] = u.pos[0]; units[o + 4] = u.pos[1];
    units[o + 5] = u.vel?.[0] ?? 0; units[o + 6] = u.vel?.[1] ?? 0;
    units[o + 7] = u.facing ?? 0;
    units[o + 8] = (u.hp ?? 0) / u.maxhp;
    units[o + 9] = (u.alive ? 1 : 0) | (u.status?.includes("sprint") ? 2 : 0) | (u.status?.includes("dashing") ? 4 : 0) | (u.status?.includes("shielding") ? 8 : 0);
    units[o + 10] = u.maxhp;
  };
  // The own units' walk cycle and facing ride the predicted motion (falls
  // back to the observation when the oracle has not loaded yet); enemies
  // ride their reported velocity.
  const mainView = playPredictor.mainView;
  setUnit(playYouIndex * 2, 1 + playYouIndex, playYouIndex, 0, {
    pos: mePos, vel: mainView?.vel ?? me.vel, facing: mainView?.facing ?? me.facing, hp: me.hp,
    alive: me.alive, status: me.status, maxhp: 100,
  });
  if (obs.you.companion.pos && obs.you.companion.alive) {
    const compPos = drPosOf(101 + playYouIndex, obs.you.companion.pos);
    const compView = playPredictor.compView;
    setUnit(playYouIndex * 2 + 1, 101 + playYouIndex, playYouIndex, 1, {
      pos: compPos,
      vel: compView?.vel ?? obs.you.companion.vel,
      facing: compView?.facing ?? obs.you.companion.facing,
      hp: obs.you.companion.hp, alive: true, maxhp: 30,
    });
  }
  for (const p of obs.seen.players) {
    const bot = p.id - 1;
    if (bot === playYouIndex || bot < 0 || bot >= bots) continue;
    const pPos = drPosOf(p.id, p.pos);
    lastSeenPos.set(p.id, pPos);
    setUnit(bot * 2, p.id, bot, 0, { pos: pPos, vel: p.vel, facing: p.facing, hp: p.hp, alive: true, maxhp: 100 });
  }

  // Rising-edge status FX for the local tarsius: dash streak, shield pop.
  const dashOn = !!me.status?.includes("dashing");
  const shieldOn = !!me.status?.includes("shielding");
  if (dashOn && !prevDashOn) {
    playFx!.dashStreak(mePos[0], mePos[1], me.facing ?? 0, playYouIndex);
    zoomPunch = Math.max(zoomPunch, 0.12);
  }
  if (shieldOn && !prevShieldOn) playFx!.shieldPop(mePos[0], mePos[1]);
  prevDashOn = dashOn;
  prevShieldOn = shieldOn;
  playUnits.update(units, units, 0, bots * 2, true);

  const zoneArr = Float32Array.of(
    obs.global.zone.center[0], obs.global.zone.center[1], obs.global.zone.radius,
    obs.global.zone.next?.center[0] ?? 0, obs.global.zone.next?.center[1] ?? 0,
    obs.global.zone.next?.radius ?? 0, obs.global.alive,
  );
  playZone!.update(zoneArr, false, !!obs.global.zone.next);

  // The human always sees through the fog — holes and markers ride the same
  // carried-forward positions as the sprites, so nobody pokes outside their
  // own vision hole between snapshots.
  playFog!.show();
  playFog!.update({
    me: {
      main: { pos: mePos, alive: me.alive },
      comp: { pos: drPosOf(101 + playYouIndex, obs.you.companion.pos), alive: obs.you.companion.alive },
    },
    seenPlayers: obs.seen.players.map((p) => ({
      ...p,
      pos: drPosOf(p.id, p.pos),
      // Server observations carry raw hp (0..100); the fog view wants a fraction.
      hp: p.hp === undefined ? undefined : Math.max(0, p.hp) / 100,
    })),
    seenCompanions: obs.seen.companions.map((c) => ({
      ...c,
      pos: drPosOf(100000 + c.id, c.pos),
    })),
    seenProjectiles: obs.seen.projectiles,
    seenPickups: obs.seen.pickups,
    heard: obs.heard,
    zone: { center: obs.global.zone.center, radius: obs.global.zone.radius, next: obs.global.zone.next },
  }, playYouIndex);

  // Candy-pellet projectiles + pickup tins in their own layers, interpolated
  // between the last two observations (server obs runs at 10Hz; bullets move
  // far per tick). A = previous obs, B = current; new-in-B bullets draw at
  // once via the layer's B-only pass.
  if (obs.tick !== playProjTick) {
    playProjPrev = playProjCur;
    playProjCur = packPlayProjs(obs);
    playProjTick = obs.tick;
    playProjObsTs = ts;
  }
  const tFrac = Math.max(0, Math.min(1, (ts - playProjObsTs) / 100));
  playProjs!.update(
    playProjPrev, playProjPrev.length / PROJ_STRIDE,
    playProjCur, playProjCur.length / PROJ_STRIDE,
    tFrac,
    (x, y, col, intense) => playFx!.tracer(x, y, col, intense),
  );
  playPickups!.update(packPlayPickups(obs), obs.seen.pickups.length, obs.tick);

  // Camera rides the carried-forward position of the player; zoomPunch kicks
  // in on kills/hits/dashes.
  stage.setTarget(mePos[0], mePos[1], 1.05 + zoomPunch);
  zoomPunch *= Math.exp(-dt * 5);

  // ---------------- combat feel: diff this observation against the last one
  if (me.alive && prevMainAlive) {
    const hpDrop = prevHp - me.hp;
    if (hpDrop > 0.5) {
      sfx.play("hurt", 0, 0.9);
      flashVignette(0.45 + Math.min(0.4, hpDrop / 30));
      stage.shake(2.5);
      zoomPunch = Math.max(zoomPunch, 0.18);
      // Point the damage arrow at the likeliest shooter: the closest enemy
      // projectile in flight, else the strongest recent gunshot bearing.
      const threat = obs.seen.projectiles
        .filter((p) => p.owner !== playYouIndex)
        .map((p) => ({ p, d: Math.hypot(p.pos[0] - me.pos[0], p.pos[1] - me.pos[1]) }))
        .sort((a, b) => a.d - b.d)[0];
      if (threat) {
        showDamageArrow(Math.atan2(threat.p.pos[1] - me.pos[1], threat.p.pos[0] - me.pos[0]));
      }
    }
    const hpGain = me.hp - prevHp;
    if (hpGain > 4 || me.energy - prevEnergy > 10) sfx.play("pickup", 0, 0.8);

    // Hit confirms: a seen enemy's HP dropped → your shot (or an ally's) landed.
    for (const p of obs.seen.players) {
      if (p.hp === undefined) continue;
      const before = prevEnemyHp.get(p.id);
      if (before !== undefined && before - p.hp > 0.5) {
        const dx = p.pos[0] - me.pos[0];
        const dy = p.pos[1] - me.pos[1];
        const d = Math.hypot(dx, dy);
        const pan = Math.max(-1, Math.min(1, (dx / Math.max(60, d)) * 0.85));
        sfx.play("hit", pan, 0.75);
        playFx!.hitmark(p.pos[0], p.pos[1]);
      }
      prevEnemyHp.set(p.id, p.hp);
    }

    // Bullet FX: play mode gets no event stream, so diff the projectile set
    // against the last observation — a new bullet is a muzzle flash, a
    // vanished Pop Rock detonates where we last saw it.
    const seenProjs = new Map<number, { x: number; y: number; vx: number; vy: number; w: number; mine: boolean }>();
    for (const p of obs.seen.projectiles) {
      seenProjs.set(p.id, {
        x: p.pos[0], y: p.pos[1], vx: p.vel[0], vy: p.vel[1],
        // `owner` is the 0-based bot index — `me.id` (a unit id) never
        // matched it, so own-shot sound/FX never fired.
        w: weaponIdx(p.weapon), mine: p.owner === playYouIndex,
      });
    }
    for (const [id, pr] of seenProjs) {
      if (prevProjectiles.has(id)) continue;
      const wcol = pr.w === 0 ? undefined : WEAPONS[pr.w].color;
      const col = wcol ? parseInt(wcol.slice(1), 16) : undefined;
      if (pr.mine) {
        // Own muzzle rides my facing; the bullet spawns just past the barrel.
        const a = (90 - (me.facing ?? 0)) * Math.PI / 180;
        playFx!.muzzleFlash(
          mePos[0] + Math.cos(a) * 18, mePos[1] + Math.sin(a) * 18,
          90 - (me.facing ?? 0), pr.w, col,
        );
        sfx.play("shot", 0, 0.95);
      } else {
        // Enemy shots flash where the bullet first appeared in view.
        playFx!.muzzleFlash(pr.x, pr.y, Math.atan2(pr.vy, pr.vx) * 180 / Math.PI, pr.w, col);
      }
    }
    for (const [id, pr] of prevProjectiles) {
      if (seenProjs.has(id) || pr.w !== 6) continue;
      playFx!.explosion(pr.x, pr.y);
      const dx = pr.x - mePos[0], dy = pr.y - mePos[1];
      const pan = Math.max(-1, Math.min(1, dx / Math.max(60, Math.hypot(dx, dy)) * 0.85));
      sfx.play("boom", pan, 0.55);
    }
    prevProjectiles = seenProjs;

    // Gun pickup: banner + sparkle when the observation says our gun changed.
    const wName = me.weapon ?? "pea";
    if (prevWeapon !== null && wName !== prevWeapon) {
      const w = WEAPONS[weaponIdx(wName)];
      showBanner(`picked up ${w.label}!`, w.color, 1900);
      sfx.play("pickup", 0, 1);
      playFx!.sparkle(mePos[0], mePos[1]);
      zoomPunch = Math.max(zoomPunch, 0.1);
    }
    prevWeapon = wName;
  }
  prevHp = me.hp;
  prevEnergy = me.energy;

  // Companion status pips.
  if (obs.you.companion.alive && obs.you.companion.pos) prevCompPos = obs.you.companion.pos;
  if (!prevCompAlive && obs.you.companion.alive && obs.you.companion.pos) {
    const cPos = drPosOf(101 + playYouIndex, obs.you.companion.pos);
    playFx!.sparkle(cPos[0], cPos[1]);
  }
  if (prevCompAlive && !obs.you.companion.alive) {
    sfx.play("boom", 0, 0.4);
    if (prevCompPos) playFx!.deathBlast(prevCompPos[0], prevCompPos[1], playYouIndex);
    showBanner("companion down", "#ff9d3b", 1300);
  }
  prevCompAlive = obs.you.companion.alive;

  // Kill feed (obs.global.kill_feed only ever appends).
  const feed = obs.global.kill_feed;
  while (killProcessed < feed.length) {
    const k = feed[killProcessed++];
    hud.kill(k.killer, k.victim, names);
    // Death blast at the victim's last seen position (own death is handled
    // by the elimination block below — skip it here to avoid doubling).
    if (k.victim !== playYouIndex) {
      const pos = lastSeenPos.get(k.victim + 1);
      if (pos) playFx!.deathBlast(pos[0], pos[1], k.victim);
    }
    if (k.killer === playYouIndex) {
      sfx.play("kill", 0, 1);
      zoomPunch = Math.max(zoomPunch, 0.24);
      showBanner(`eliminated ${names[k.victim]}!`, "#43d66e");
    }
  }

  // Your elimination.
  if (prevMainAlive && !me.alive) {
    sfx.play("boom", 0, 1);
    playFx!.deathBlast(mePos[0], mePos[1], playYouIndex);
    flashVignette(0.9);
    zoomPunch = Math.max(zoomPunch, 0.3);
    showBanner("you were eliminated", "#e6455f", 2400);
    playOverShow("💀", "ELIMINATED", `place revealed at match end — ${obs.global.alive} still fighting`);
  }
  prevMainAlive = me.alive;

  // Zone discipline: beep + banner while taking zone damage.
  const zd = Math.hypot(me.pos[0] - obs.global.zone.center[0], me.pos[1] - obs.global.zone.center[1]);
  const outside = me.alive && zd > obs.global.zone.radius;
  if (outside && ts - lastZoneBeep > 900) {
    lastZoneBeep = ts;
    sfx.play("zone", 0, 0.9);
  }
  playBanner.classList.toggle("zone-warn", outside);

  // Heard events → positional audio. Gunfire intel is deliberately off (the
  // red wedge strobe + rapid pings read as noise, so no shot sound from
  // unseen shooters); dashes keep their whoosh, footsteps stay wedge-only.
  for (const h of obs.heard) {
    if (h.kind !== "dash") continue;
    const a = (h.bearing * Math.PI) / 180;
    const pan = Math.sin(a);
    const vol = h.band === "near" ? 0.85 : h.band === "mid" ? 0.5 : 0.26;
    sfx.play("dash", pan, vol * 0.8);
  }

  // HUD. The debug panel's bars exist only under ?debug=true; normal play
  // skips the DOM churn entirely.
  hud.stats(obs.tick, obs.global.alive, zonePhaseOfFloat(obs.global.zone.radius), false);
  const gun = WEAPONS[weaponIdx(me.weapon)];
  const fireCd = me.cooldown.fire ?? 0;
  const hpColor = me.hp > 55 ? "#43d66e" : me.hp > 25 ? "#ffc93c" : "#ff5f7e";
  const zoneLeft = obs.global.zone.next
    ? ` · zone locks ${Math.max(0, Math.round((obs.global.zone.next.locks_at_tick - obs.tick) / 10))}s`
    : "";
  const comp = obs.you.companion.alive
    ? `<div class="pbar"><span>JALAK ${Math.round(obs.you.companion.hp)}</span><div><i style="width:${Math.max(0, obs.you.companion.hp / 30 * 100)}%;background:#35c1f0"></i></div></div>`
    : `<div class="pcd">jalak respawning ${obs.you.companion.respawn_in_s ? obs.you.companion.respawn_in_s.toFixed(0) + "s" : "…"}</div>`;
  if (DEBUG_HUD) document.getElementById("play-bars")!.innerHTML = `
    <div class="pbar"><span>HP ${Math.round(me.hp)}</span><div><i style="width:${Math.max(0, me.hp)}%;background:${hpColor}"></i></div></div>
    <div class="pbar"><span>EN ${Math.round(me.energy)}</span><div><i style="width:${me.energy}%;background:#35c1f0"></i></div></div>
    ${comp}
    <div class="pbar mini"><span>FIRE</span><div><i style="width:${(1 - Math.min(1, fireCd / gun.cd)) * 100}%;background:${fireCd > 0 ? "#8d82b5" : gun.color}"></i></div></div>
    <div class="pcd">GUN <b style="color:${gun.color}">${gun.label}</b> — ${gun.blurb}</div>
    <div class="pcd">sprint ${playClient.sprinting ? "ON (no firing)" : "off"} · ${outside ? "<b style='color:#e6455f'>OUTSIDE ZONE — RUN!</b>" : "zone ok"}${zoneLeft}</div>`;

  playFx!.update(dt);
}

function zonePhaseOfFloat(r: number): number {
  const ladder = [1600, 1200, 850, 550, 300, 0];
  for (let i = 0; i < ladder.length; i++) {
    if (r > ladder[i] - 1) return i;
  }
  return ladder.length - 1;
}

/** Play observation → shared projectile layout for ProjectileLayer. The wire's
 * `owner` is the 0-based bot index (mains and their companions alike), which
 * is exactly what the layer's botColor expects. */
function packPlayProjs(obs: PlayObs): Float32Array {
  const list = obs.seen.projectiles;
  const out = new Float32Array(list.length * PROJ_STRIDE);
  for (let i = 0; i < list.length; i++) {
    const p = list[i];
    const o = i * PROJ_STRIDE;
    out[o + P.ID] = p.id;
    out[o + P.BOT] = typeof p.owner === "number" && p.owner >= 0 ? p.owner : 0;
    out[o + P.X] = p.pos[0];
    out[o + P.Y] = p.pos[1];
    out[o + P.VX] = p.vel[0];
    out[o + P.VY] = p.vel[1];
    out[o + P.WEAPON] = weaponIdx(p.weapon);
  }
  return out;
}

/** Play observation → shared pickup layout for PickupLayer. */
function packPlayPickups(obs: PlayObs): Float32Array {
  const list = obs.seen.pickups;
  const out = new Float32Array(list.length * PICKUP_STRIDE);
  for (let i = 0; i < list.length; i++) {
    const p = list[i];
    const o = i * PICKUP_STRIDE;
    out[o + K.ID] = p.id;
    out[o + K.KIND] = pickupKindIdx(p.kind);
    out[o + K.X] = p.pos[0];
    out[o + K.Y] = p.pos[1];
  }
  return out;
}

// Keyboard shortcuts (replay mode only — play mode uses its own handlers).
window.addEventListener("keydown", (e) => {
  if (playClient?.playing) {
    if (e.code === "KeyM") mindcam?.toggle();
    return;
  }
  if (e.code === "Space") { playing = !playing; e.preventDefault(); }
  if (e.code === "ArrowRight" && replay) { idx = Math.min(replay.data.totalTicks - 1, idx + TICK_RATE); }
  if (e.code === "ArrowLeft" && replay) { idx = Math.max(0, idx - TICK_RATE); }
  if (e.code === "KeyM") mindcam?.toggle();
  if (location.hash === "#replays" && !replay && !playClient) {
    location.hash = ""; // library open: Esc goes home instead
    return;
  }
  if (e.code === "Escape") { hud.hideWinner(); if (replay) setCamMode("auto"); }
});

// Sound toggle — persists, works in both replay and play modes.
{
  const btn = document.getElementById("btn-audio")!;
  const paint = () => { btn.textContent = sfx.muted ? "🔇" : "🔊"; };
  paint();
  btn.addEventListener("click", () => { sfx.toggleMute(); paint(); });
}

boot();
