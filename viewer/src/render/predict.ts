/** Own-unit client-side prediction: your tarsius and your bird move the
 * moment you press a key. Every frame their current input is stepped through
 * a `MoveSim` — the WASM movement oracle over the engine's real movement
 * math (proven bit-exact by the core's `predict_fidelity` test) — and the
 * rendered position integrates that displacement directly. Each server
 * observation reconciles any drift through the same blended correction the
 * dead reckoner uses, so a misprediction is a smooth nudge, never a snap.
 *
 * What the oracle can't see: an unseen enemy pushing your unit (unit
 * separation) or the server refusing an action on stale energy. Those are
 * ordinary mispredictions — reconciled on the next observation. While
 * snapshots stall, prediction keeps running (it is local), and the server's
 * momentum fill keeps the true unit moving on your last input too. */

import { MoveSim } from "../wasm/gunbatte_wasm.js";
import { ensureWasm } from "../sim.js";
import type { PlayClient, PlayObs } from "../play.js";

/** How long an observation that disagrees with a locally started action is
 * treated as predating the server's processing of the press: one
 * observation cadence plus one round trip. A genuinely refused action is
 * corrected by the first post-grace observation. */
const PREDICT_GRACE_MS = 350;

interface OwnState {
  /** MoveSim kind code: 0 main · 1 companion · 2 boss. */
  kind: 0 | 1 | 2;
  dashing: number;
  dashDir: [number, number];
  shielding: number;
  energy: number;
  sprint: boolean;
  alive: boolean;
}

/** One oracle step's render-facing outputs (walk cycle + facing). */
interface StepView {
  vel: [number, number];
  facing: number;
}

export class OwnPredictor {
  private sim: MoveSim | null = null;
  private main: OwnState | null = null;
  private comp: OwnState | null = null;
  /** Predicted positions — re-synced from the dead reckoner's blended state
   * every frame so corrections feed back into the prediction. */
  private mainPos = { x: 0, y: 0 };
  /** Separation pushes whose own driver already ran this frame: applied at
   * the next frame's step (the engine separates after both units move). */
  private pendingMainPush: [number, number] = [0, 0];
  private lastMainPrev: { x: number; y: number } = { x: 0, y: 0 };
  private lastMainNew: { x: number; y: number } = { x: 0, y: 0 };
  private lastDashSeen = 0;
  private lastSprintSeen = 0;
  /** Wallclock stamps of locally started/held actions. An observation
   * generated before the server processed the press carries no action
   * status — without a grace window it would cancel the predicted clock a
   * third of the way in, and the confirming observation would then restart
   * it at full length. Reconciliation of "not dashing/shielding/sprinting"
   * therefore waits one obs cadence + one RTT (~350 ms); a genuinely
   * refused action is corrected by the first post-grace observation. */
  private dashPredictedAt = 0;
  private shieldPredictedAt = 0;
  private sprintPredictedAt = 0;
  /** Frame context, set by beginFrame before the drivers run. */
  private obs: PlayObs | null = null;
  private client: PlayClient | null = null;

  mainView: StepView | null = null;
  compView: StepView | null = null;

  /** A new match: build the oracle for its map. The WASM load is async —
   * drivers no-op (frozen at the observed position) until it lands, at most
   * a few hundred ms into the match. */
  onMatchStart(mapId: string, role: "boss" | "raider"): void {
    this.sim = null;
    this.main = null;
    this.comp = null;
    this.mainView = null;
    this.compView = null;
    this.pendingMainPush = [0, 0];
    this.dashPredictedAt = 0;
    this.shieldPredictedAt = 0;
    this.sprintPredictedAt = 0;
    // The syncObs that follows match start may run before the WASM lands;
    // it records kind/energy and the drivers no-op until the oracle exists.
    ensureWasm()
      .then(() => {
        this.sim = new MoveSim(mapId);
        if (this.main) this.main.kind = role === "boss" ? 2 : 0;
      })
      .catch(() => {
        // No oracle: the dead reckoner keeps rendering from observations.
        this.sim = null;
      });
  }

  /** Match over / left: release the oracle and all predicted state. */
  reset(): void {
    this.sim = null;
    this.main = null;
    this.comp = null;
    this.mainView = null;
    this.compView = null;
    this.obs = null;
    this.client = null;
    this.dashPredictedAt = 0;
    this.shieldPredictedAt = 0;
    this.sprintPredictedAt = 0;
  }

  /** Fold a fresh observation in (called once per new server snapshot):
   * energy is server truth; dash/shield clocks stay local (they were
   * started locally and run a known duration), except a dash the server
   * reports that we never predicted (resync after a stall). Disagreeing
   * action flags reconcile only after `PREDICT_GRACE_MS`: the first
   * observation after a locally started action was generated before the
   * server processed it, and cancelling on it would stutter the very
   * movement this predictor exists to make crisp. */
  syncObs(obs: PlayObs, role: "boss" | "raider"): void {
    const me = obs.you.main;
    if (!this.main) {
      this.main = {
        kind: role === "boss" ? 2 : 0,
        dashing: 0,
        dashDir: [0, 0],
        shielding: 0,
        energy: me.energy,
        sprint: false,
        alive: me.alive,
      };
    }
    const wasAlive = this.main.alive;
    this.main.alive = me.alive;
    this.main.energy = me.energy;
    const obsSprint = !!me.status?.includes("sprint");
    if (obsSprint !== this.main.sprint &&
        performance.now() - this.sprintPredictedAt > PREDICT_GRACE_MS) {
      this.main.sprint = obsSprint;
    }
    if (!wasAlive && me.alive) {
      // Respawning: a fresh unit — clear every local clock.
      this.main.dashing = 0;
      this.main.shielding = 0;
    }
    const obsDashing = !!me.status?.includes("dashing");
    if (obsDashing && this.main.dashing <= 0 && this.sim) {
      this.main.dashing = this.sim.dash_seconds();
      const v = me.vel ?? [0, 0];
      const n = Math.hypot(v[0], v[1]) || 1;
      this.main.dashDir = [v[0] / n, v[1] / n];
    }
    if (!me.status?.includes("shielding") && this.main.shielding > 0 &&
        performance.now() - this.shieldPredictedAt > PREDICT_GRACE_MS) {
      // The server's shield ended (or never started): trust the server —
      // past the grace window that absorbs in-flight observations.
      this.main.shielding = 0;
    }
    if (!me.status?.includes("dashing") && this.main.dashing > 0 &&
        performance.now() - this.dashPredictedAt > PREDICT_GRACE_MS) {
      // Same for dashes — past the grace window, the observation is the
      // authority on state that we could only guess at; our local clock
      // merely bridges between obs.
      this.main.dashing = 0;
    }

    // Companion: predicted only while alive.
    const comp = obs.you.companion;
    if (!comp.alive || !comp.pos) {
      this.comp = null;
      this.compView = null;
    } else {
      if (!this.comp) {
        this.comp = {
          kind: 1,
          dashing: 0,
          dashDir: [0, 0],
          shielding: 0,
          energy: comp.energy,
          sprint: false,
          alive: true,
        };
      }
      this.comp.energy = comp.energy;
    }
  }

  /** Per-frame context: the drivers read the same observation and input
   * state the wire send uses. */
  beginFrame(obs: PlayObs, client: PlayClient): void {
    this.obs = obs;
    this.client = client;
  }

  /** The main's driver: one oracle step from the dead reckoner's current
   * (blended) position. Returns the displacement to integrate. */
  driveMain = (st: { x: number; y: number }, dt: number): [number, number] => {
    this.mainView = null;
    if (!this.sim || !this.main || !this.main.alive || !this.obs || !this.client) {
      return [0, 0];
    }
    const s = this.main;
    // Re-sync from the blended render state, plus any separation push whose
    // frame already passed.
    this.mainPos.x = st.x + this.pendingMainPush[0];
    this.mainPos.y = st.y + this.pendingMainPush[1];
    this.pendingMainPush = [0, 0];
    const input = this.client.mainMoveInput();
    const action = this.mainAction();
    const out = this.sim.step(
      s.kind,
      this.mainPos.x, this.mainPos.y,
      this.obs.you.main.facing,
      s.sprint,
      s.dashing, s.dashDir[0], s.dashDir[1],
      s.shielding,
      s.energy,
      input?.dir ?? 0, input?.throttle ?? 0,
      action,
      dt,
      NaN, NaN, NaN, NaN,
    );
    this.lastMainPrev = { x: this.mainPos.x, y: this.mainPos.y };
    this.mainPos.x = out[0];
    this.mainPos.y = out[1];
    this.lastMainNew = { x: out[0], y: out[1] };
    s.dashing = out[6];
    s.shielding = out[7];
    s.energy = out[5];
    this.mainView = { vel: [out[2], out[3]], facing: out[4] };
    return [out[0] - st.x, out[1] - st.y];
  };

  /** The companion's driver: steps with the leash anchored on the main's
   * fresh position, then separates the pair — the bird's push lands this
   * frame, the main's rides the next (the engine separates after both
   * move). */
  driveComp = (st: { x: number; y: number }, dt: number): [number, number] => {
    this.compView = null;
    if (!this.sim || !this.comp || !this.obs || !this.client) return [0, 0];
    const s = this.comp;
    const compObs = this.obs.you.companion;
    if (!compObs.alive || !compObs.pos) return [0, 0];
    const startX = st.x;
    const startY = st.y;
    const input = this.client.companionMoveInput(this.obs);
    const heel = this.client.companionHeel(this.obs);
    const out = this.sim.step(
      s.kind,
      startX, startY,
      compObs.facing,
      false,
      0, 0, 0,
      0,
      s.energy,
      input?.dir ?? 0, input?.throttle ?? 0,
      heel ? 5 : 0,
      dt,
      this.lastMainPrev.x, this.lastMainPrev.y,
      this.lastMainNew.x, this.lastMainNew.y,
    );
    s.energy = out[5];
    // Separation: both predicted positions, exact own-pair push.
    const sep = this.sim.separate(0, this.mainPos.x, this.mainPos.y, 1, out[0], out[1]);
    this.mainPos.x = sep[0];
    this.mainPos.y = sep[1];
    this.pendingMainPush = [sep[0] - this.lastMainNew.x, sep[1] - this.lastMainNew.y];
    this.compView = { vel: [out[2], out[3]], facing: out[4] };
    return [sep[2] - startX, sep[3] - startY];
  };

  /** The action slot this frame: dash > sprint > shield, mirroring the wire
   * send's priority. One-shot edges are tracked here so a press predicts
   * exactly once; the sprint toggle lands locally the instant it is pressed
   * and the observation sync corrects any drift. */
  private mainAction(): number {
    if (!this.client || !this.main) return 0;
    if (this.client.dashRequestedAt !== this.lastDashSeen) {
      this.lastDashSeen = this.client.dashRequestedAt;
      this.dashPredictedAt = performance.now();
      return 1;
    }
    if (this.client.sprintToggledAt !== this.lastSprintSeen) {
      this.lastSprintSeen = this.client.sprintToggledAt;
      this.main.sprint = !this.main.sprint;
      this.sprintPredictedAt = performance.now();
      return this.main.sprint ? 3 : 4;
    }
    if (this.client.shieldHeld()) {
      // Held: the stamp refreshes every frame, so the reconcile stays
      // suppressed for as long as we keep predicting the shield.
      this.shieldPredictedAt = performance.now();
      return 2;
    }
    return 0;
  }
}
