//! WASM bindings over the deterministic core (PLAN §5.2, §7.1): the SAME
//! `step()`/`observe()` code the server runs re-simulates replays in the
//! browser, so replays stay thin and views stay exact.
//!
//! The viewer drives a `ReplaySim`: step() returns one spectator frame JSON
//! per tick; `observe_current(bot)` yields a strict-fog observation for
//! player-cam rendering. A `MoveSim` exposes the movement-only slice of the
//! same code as a client-side prediction oracle for live play.

use gunbatte_core::engine::MatchEngine;
use gunbatte_core::predict::{MoveAction, MoveOracle, MoveState};
use gunbatte_core::replay::Replay;
use gunbatte_core::types::{MoveInput, UnitKind, Vec2};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct ReplaySim {
    engine: MatchEngine,
    ticks: Vec<gunbatte_core::replay::ReplayTick>,
    cursor: usize, // ticks consumed so far
    bot_names: Vec<String>,
    decision_rates: Vec<u64>,
    auto_heel: Vec<bool>,
}

#[wasm_bindgen]
impl ReplaySim {
    #[wasm_bindgen(constructor)]
    pub fn new(replay_json: &str) -> Result<ReplaySim, JsValue> {
        let replay: Replay = serde_json::from_str(replay_json)
            .map_err(|e| JsValue::from_str(&format!("bad replay: {e}")))?;
        let bot_names = replay.header.bot_names.clone();
        let decision_rates = replay.header.decision_rates.clone();
        let auto_heel = replay.header.auto_heel.clone();
        let mut engine = MatchEngine::new(
            replay.header.config.clone(),
            replay.header.seed,
            &replay.header.bot_names,
        );
        engine.decision_rate = decision_rates.clone();
        engine.auto_heel = auto_heel.clone();
        let ticks = replay.ticks;
        Ok(ReplaySim {
            engine,
            ticks,
            cursor: 0,
            bot_names,
            decision_rates,
            auto_heel,
        })
    }

    /// Advance one tick, returning the full-state spectator frame as JSON.
    /// Returns null once the replay is exhausted.
    pub fn step(&mut self) -> Option<String> {
        if self.cursor >= self.ticks.len() {
            return None;
        }
        let rt = &self.ticks[self.cursor];
        for (b, inp) in rt.inputs.iter().enumerate() {
            if let Some(inp) = inp {
                if inp.intent.is_some() || inp.belief.is_some() {
                    // Mind-cam debug channel rides the recorded inputs.
                    self.engine
                        .submit_mind(b as u32, inp.intent.clone(), inp.belief.clone());
                }
                self.engine.submit(b as u32, inp.clone(), 0);
            }
        }
        let events = self.engine.step_tick();
        self.cursor += 1;
        Some(
            serde_json::to_string(&self.engine.spectator_frame(&events)).expect("frame serializes"),
        )
    }

    /// Strict-fog observation for one bot at the CURRENT tick (player-cam).
    pub fn observe_current(&self, bot: u32) -> Option<String> {
        if bot >= self.engine.state.bots {
            return None;
        }
        Some(serde_json::to_string(&self.engine.observe(bot)).expect("obs serializes"))
    }

    /// Jump back to tick 0 (e.g. before a player-cam re-simulation pass).
    pub fn reset(&mut self) {
        let seed = self.engine.seed;
        let cfg = self.engine.config.clone();
        let mut engine = MatchEngine::new(cfg, seed, &self.bot_names);
        engine.decision_rate = self.decision_rates.clone();
        engine.auto_heel = self.auto_heel.clone();
        self.engine = engine;
        self.cursor = 0;
    }

    pub fn total_ticks(&self) -> usize {
        self.ticks.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn bots(&self) -> usize {
        self.engine.state.bots as usize
    }

    pub fn bot_name(&self, bot: u32) -> Option<String> {
        self.bot_names.get(bot as usize).cloned()
    }

    pub fn map_id(&self) -> String {
        self.engine.config.map_id.clone()
    }

    /// Static map geometry (public knowledge, PLAN §2.4) for rendering.
    pub fn map_json(&self) -> String {
        serde_json::to_string(&self.engine.map.to_wire()).expect("map serializes")
    }

    pub fn seed(&self) -> f64 {
        self.engine.seed as f64
    }
}

/// Client-side prediction oracle for live play: the movement-only slice of
/// the engine, no match state. The play client steps its own main and
/// companion through this every frame so their movement renders instantly,
/// and each observation reconciles the drift. Proven bit-exact against the
/// server's `step()` by `gunbatte-core`'s `predict_fidelity` test.
#[wasm_bindgen]
pub struct MoveSim {
    oracle: MoveOracle,
}

/// Action codes on the wire-in: 0 none · 1 dash · 2 shield · 3 sprint on ·
/// 4 sprint off · 5 heel.
const ACTION_NONE: u8 = 0;

#[wasm_bindgen]
impl MoveSim {
    #[wasm_bindgen(constructor)]
    pub fn new(map_id: &str) -> Result<MoveSim, JsValue> {
        // Maps are embedded in the core and shared with the server, so the
        // client and the engine always step the same geometry.
        let oracle = MoveOracle::standard(map_id)
            .ok_or_else(|| JsValue::from_str(&format!("unknown map {map_id}")))?;
        Ok(MoveSim { oracle })
    }

    /// The map id this sim was built for.
    pub fn map_id(&self) -> String {
        self.oracle.map().id.clone()
    }

    /// Dash / shield duration in seconds, for clients that keep their own
    /// countdown clocks between steps.
    pub fn dash_seconds(&self) -> f64 {
        gunbatte_core::fixed::to_f64(gunbatte_core::fixed::mul(
            gunbatte_core::fixed::from_int(self.oracle.params().dash_ticks as i64),
            self.oracle.params().dt,
        ))
    }

    pub fn shield_seconds(&self) -> f64 {
        gunbatte_core::fixed::to_f64(gunbatte_core::fixed::mul(
            gunbatte_core::fixed::from_int(self.oracle.params().shield_ticks as i64),
            self.oracle.params().dt,
        ))
    }

    /// Advance one unit by `dt_s` seconds. `main_prev`/`main_new` are the
    /// companion's main's position before/after its own step (heel
    /// direction, leash anchor); NaN means "not applicable". Returns
    /// `[x, y, vel_x, vel_y, facing_deg, energy, dashing_s, shielding_s]`.
    #[allow(clippy::too_many_arguments)]
    pub fn step(
        &self,
        kind: u8,
        x: f64,
        y: f64,
        facing_deg: u32,
        sprint: bool,
        dashing_s: f64,
        dash_dir_x: f64,
        dash_dir_y: f64,
        shielding_s: f64,
        energy: f64,
        dir_deg: u32,
        throttle: f64,
        action: u8,
        dt_s: f64,
        main_prev_x: f64,
        main_prev_y: f64,
        main_new_x: f64,
        main_new_y: f64,
    ) -> Vec<f64> {
        let from_opt = |vx: f64, vy: f64| -> Option<Vec2> {
            if vx.is_nan() || vy.is_nan() {
                None
            } else {
                Some(Vec2::new(
                    gunbatte_core::fixed::from_f64(vx),
                    gunbatte_core::fixed::from_f64(vy),
                ))
            }
        };
        let mut st = MoveState {
            kind: match kind {
                1 => UnitKind::Companion,
                2 => UnitKind::Boss,
                _ => UnitKind::Main,
            },
            pos: Vec2::new(
                gunbatte_core::fixed::from_f64(x),
                gunbatte_core::fixed::from_f64(y),
            ),
            facing: facing_deg as u16,
            sprint,
            dashing: gunbatte_core::fixed::from_f64(dashing_s),
            dash_dir: Vec2::new(
                gunbatte_core::fixed::from_f64(dash_dir_x),
                gunbatte_core::fixed::from_f64(dash_dir_y),
            ),
            shielding: gunbatte_core::fixed::from_f64(shielding_s),
            energy: gunbatte_core::fixed::from_f64(energy),
        };
        let mv = MoveInput {
            dir: dir_deg as u16,
            throttle: gunbatte_core::fixed::from_f64(throttle),
        };
        let action = match action {
            1 => MoveAction::Dash,
            2 => MoveAction::Shield,
            3 => MoveAction::Sprint { on: true },
            4 => MoveAction::Sprint { on: false },
            5 => MoveAction::Heel,
            _ => MoveAction::None,
        };
        use gunbatte_core::fixed as f;
        let vel = self.oracle.step(
            &mut st,
            &mv,
            action,
            f::from_f64(dt_s),
            from_opt(main_prev_x, main_prev_y),
            from_opt(main_new_x, main_new_y),
        );
        vec![
            f::to_f64(st.pos.x),
            f::to_f64(st.pos.y),
            f::to_f64(vel.x),
            f::to_f64(vel.y),
            st.facing as f64,
            f::to_f64(st.energy),
            f::to_f64(st.dashing),
            f::to_f64(st.shielding),
        ]
    }

    /// Unit separation for the caller's own main↔companion pair, exactly as
    /// `step()`'s section 2 pushes one overlapping pair. Returns
    /// `[ax, ay, bx, by]` (unchanged when not overlapping).
    pub fn separate(&self, a_kind: u8, ax: f64, ay: f64, b_kind: u8, bx: f64, by: f64) -> Vec<f64> {
        use gunbatte_core::fixed as f;
        let kind = |k: u8| match k {
            1 => UnitKind::Companion,
            2 => UnitKind::Boss,
            _ => UnitKind::Main,
        };
        let mut a = MoveState {
            kind: kind(a_kind),
            pos: Vec2::new(f::from_f64(ax), f::from_f64(ay)),
            facing: 0,
            sprint: false,
            dashing: f::from_f64(0.0),
            dash_dir: Vec2::default(),
            shielding: f::from_f64(0.0),
            energy: f::from_f64(0.0),
        };
        let mut b = MoveState {
            kind: kind(b_kind),
            pos: Vec2::new(f::from_f64(bx), f::from_f64(by)),
            facing: 0,
            sprint: false,
            dashing: f::from_f64(0.0),
            dash_dir: Vec2::default(),
            shielding: f::from_f64(0.0),
            energy: f::from_f64(0.0),
        };
        self.oracle.separate(&mut a, &mut b);
        vec![
            f::to_f64(a.pos.x),
            f::to_f64(a.pos.y),
            f::to_f64(b.pos.x),
            f::to_f64(b.pos.y),
        ]
    }
}

// Keep the action constant referenced (it documents the wire-in codes).
const _: u8 = ACTION_NONE;
