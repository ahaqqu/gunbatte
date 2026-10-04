/** Dead reckoning for live play: the server mails unit positions 10 times a
 * second, but the screen draws 60 times a second — between snapshots each
 * unit keeps moving along its last reported velocity, and every fresh
 * snapshot's correction is blended out over ~100ms instead of snapping.
 *
 * Why extrapolation and not interpolation: this game has no server-side lag
 * compensation (bullets resolve against where enemies actually are), so
 * showing enemies close to their real current position keeps aim honest;
 * interpolation would render everyone ~100ms deeper in the past with nothing
 * paying that back. The cost — a brief slide when someone stops or turns —
 * is bounded by the cap and always self-corrects.
 *
 * All positions are world units; velocities are world units per second, as
 * reported by the server's observations. */

/** Maximum seconds of un-anchored extrapolation: past this (packet loss, a
 * stalled link), a unit holds its position instead of flying on blindly. */
const CAP_S = 0.12;
/** Window over which a snapshot's correction is blended out. */
const BLEND_S = 0.1;
/** Corrections larger than this snap instead of blending (respawn,
 * re-entry after fog, a long stall) — a slow glide across the map is worse
 * than one honest jump. */
const SNAP_DIST = 48;

interface DRState {
  x: number;
  y: number;
  vx: number;
  vy: number;
  /** Pending correction left to blend out (world units). */
  ex: number;
  ey: number;
  /** Wallclock (ms) of the snapshot this state was anchored from. */
  anchoredAt: number;
}

export class DeadReckoner {
  private s = new Map<number, DRState>();

  /** Fold a fresh snapshot in for every unit the client can currently see:
   * each present unit's drift becomes a blended correction, absent units are
   * forgotten (their next appearance re-initializes). `ts` is the wallclock
   * the snapshot arrived. */
  observe(
    units: Iterable<{ id: number; pos: [number, number]; vel?: [number, number] }>,
    ts: number,
  ): void {
    const seen = new Set<number>();
    for (const u of units) {
      seen.add(u.id);
      const st = this.s.get(u.id);
      const vx = u.vel?.[0] ?? 0;
      const vy = u.vel?.[1] ?? 0;
      if (!st) {
        // First sighting: start exactly at the server's truth.
        this.s.set(u.id, {
          x: u.pos[0], y: u.pos[1], vx, vy, ex: 0, ey: 0, anchoredAt: ts,
        });
        continue;
      }
      const dx = u.pos[0] - st.x;
      const dy = u.pos[1] - st.y;
      if (dx * dx + dy * dy > SNAP_DIST * SNAP_DIST) {
        // Too far off to glide — teleport the render position.
        st.x = u.pos[0];
        st.y = u.pos[1];
        st.ex = 0;
        st.ey = 0;
      } else {
        st.ex += dx;
        st.ey += dy;
      }
      st.vx = vx;
      st.vy = vy;
      st.anchoredAt = ts;
    }
    for (const id of [...this.s.keys()]) {
      if (!seen.has(id)) this.s.delete(id);
    }
  }

  /** Advance to render time and return the position to draw for `id`.
   * Falls back to `raw` (the snapshot position) when the unit has no state
   * yet this frame — callers pass the observation's own position. */
  pos(id: number, raw: [number, number], nowMs: number, dt: number): [number, number] {
    const st = this.s.get(id);
    if (!st) return raw;
    // Keep guessing along the last reported velocity, but only while the
    // server is still answering — past the cap a stalled link must not send
    // sprites flying.
    const ageS = (nowMs - st.anchoredAt) / 1000;
    const moving = ageS <= CAP_S;
    const b = Math.min(1, dt / BLEND_S);
    st.x += (moving ? st.vx * dt : 0) + st.ex * b;
    st.y += (moving ? st.vy * dt : 0) + st.ey * b;
    st.ex -= st.ex * b;
    st.ey -= st.ey * b;
    return [st.x, st.y];
  }

  /** Forget one unit (death, left vision): its next sighting re-initializes. */
  drop(id: number): void {
    this.s.delete(id);
  }
}
