//! Client-side movement oracle: the movement and ability math of `step()`,
//! extracted for a single unit with no match state. The WASM viewer feeds it
//! the player's own input every frame to predict their own units instantly
//! (client-side prediction), and each observation reconciles any drift; fog
//! is irrelevant because a unit's movement depends only on its own input,
//! the static map, and its own flags — with one exception: overlapping units
//! push each other apart (section 2 of `step()`). The own main↔companion
//! pair is fully predictable (both are locally predicted, `separate()`
//! below); a push from an unseen enemy is a misprediction the observation
//! reconcile absorbs.
//!
//! `step()` below must replicate the engine's per-tick order exactly:
//! movement (with the state as of the previous tick), then dash start,
//! then shield/sprint, then countdowns + regen — with `separate()` applied
//! between movement and dash start, exactly where `step()` runs unit
//! separation. The `predict_fidelity` test drives a real `MatchEngine` and
//! this oracle through the same input script and asserts bit-exact
//! agreement every tick.

use crate::config::MatchConfig;
use crate::fixed::{self, Fix, ONE};
use crate::map::{self, GameMap};
use crate::params::SimParams;
use crate::state::Unit;
use crate::types::{MoveInput, UnitAction, UnitKind, Vec2};

pub struct MoveOracle {
    map: GameMap,
    params: SimParams,
}

impl MoveOracle {
    pub fn new(map: GameMap, params: SimParams) -> Self {
        MoveOracle { map, params }
    }

    /// The deployment's movement reality: an embedded map by id plus the
    /// standard config. A server run with custom config speeds would make
    /// predictions drift (reconciled every observation, but wobbling) —
    /// the standard config is the contract.
    pub fn standard(map_id: &str) -> Option<Self> {
        let map = map::load_map(map_id)?;
        Some(MoveOracle::new(
            map,
            SimParams::from_config(&MatchConfig::standard()),
        ))
    }

    pub fn map(&self) -> &GameMap {
        &self.map
    }

    pub fn params(&self) -> &SimParams {
        &self.params
    }

    /// Advance one unit by `dt` — the server's per-tick dt reproduces
    /// `step()` bit for bit; frame-rate dt integrates the same paths with
    /// slightly finer wall resolution. The unit must be alive: death and
    /// respawn are server truth the caller reconciles against.
    ///
    /// `main_prev` is the companion's main's position before this frame's
    /// movement (heel direction reads it, as pass A reads live state);
    /// `main_new` is that main's position after its own step (the leash
    /// anchor, as pass B reads the fresh position buffer). Both `None` for
    /// a main.
    pub fn step(
        &self,
        u: &mut MoveState,
        mv: &MoveInput,
        action: MoveAction,
        dt: Fix,
        main_prev: Option<Vec2>,
        main_new: Option<Vec2>,
    ) -> Vec2 {
        let p = &self.params;
        let is_main = u.kind == UnitKind::Main;
        let combatant = u.kind != UnitKind::Companion;
        let base_speed = match u.kind {
            UnitKind::Main => p.main_speed,
            UnitKind::Companion => p.comp_speed,
            UnitKind::Boss => p.boss_speed,
        };

        // Pass A: velocity + facing (step.rs movement), with the companion
        // heel override reading the main's pre-step position.
        let mut move_in = *mv;
        if !is_main && action == MoveAction::Heel {
            if let Some(mp) = main_prev {
                let d = mp.dist(u.pos);
                if d > fixed::from_int(100) {
                    move_in = MoveInput {
                        dir: fixed::norm_deg(u.pos.bearing_to(mp)),
                        throttle: ONE,
                    };
                } else {
                    move_in = MoveInput::stop();
                }
            } else {
                move_in = MoveInput::stop();
            }
        }
        let (vel, facing) = if combatant && u.dashing > 0 {
            let v = u.dash_dir.scale(fixed::mul(base_speed, p.dash_mult));
            let f = fixed::norm_deg(fixed::atan2_deg(u.dash_dir.y, u.dash_dir.x));
            (v, f)
        } else if move_in.throttle > 0 {
            let mut s = fixed::mul(base_speed, move_in.throttle);
            if is_main && u.sprint {
                s = fixed::mul(s, p.sprint_mult);
            }
            if u.shielding > 0 {
                s = fixed::mul(s, p.shield_speed);
            }
            (Vec2::dir(move_in.dir as i32).scale(s), move_in.dir)
        } else {
            (Vec2::default(), u.facing)
        };

        // Pass B: integrate + wall resolve + companion leash.
        let radius = match u.kind {
            UnitKind::Main => p.main_radius,
            UnitKind::Companion => p.comp_radius,
            UnitKind::Boss => p.boss_radius,
        };
        let mut pos = u.pos;
        if vel != Vec2::default() {
            pos = pos.add(vel.scale(dt));
            map::resolve_circle(&self.map, &mut pos, radius);
        }
        if u.kind == UnitKind::Companion {
            if let Some(mp) = main_new {
                let d2 = pos.dist2(mp);
                if d2 > fixed::mul(p.leash, p.leash) && d2 > 0 {
                    let d = fixed::sqrt(d2);
                    let target = mp.add(pos.sub(mp).scale(fixed::div(p.leash, d)));
                    pos = target;
                    map::resolve_circle(&self.map, &mut pos, radius);
                }
            }
        }
        u.pos = pos;
        u.facing = facing;

        // Dash start (section 3): gated on energy, direction from the RAW
        // input move (the heel override is a pass-A local, not this).
        if combatant && u.dashing <= 0 && action == MoveAction::Dash && u.energy >= p.dash_cost {
            u.energy -= p.dash_cost;
            u.dash_dir = if mv.throttle > 0 {
                Vec2::dir(mv.dir as i32)
            } else {
                Vec2::dir(u.facing as i32)
            };
            u.dashing = fixed::mul(fixed::from_int(p.dash_ticks as i64), p.dt);
        }

        // Shield / sprint (section 6).
        match action {
            MoveAction::Shield if combatant => {
                if u.shielding <= 0 && u.energy >= p.shield_cost {
                    u.energy -= p.shield_cost;
                    u.shielding = fixed::mul(fixed::from_int(p.shield_ticks as i64), p.dt);
                }
            }
            MoveAction::Sprint { on } if is_main => {
                u.sprint = on;
            }
            _ => {}
        }

        // Countdown + regen (section 9). The engine counts integer ticks;
        // this oracle counts seconds (ticks scaled by the tick length), so a
        // per-tick dt reproduces the engine exactly while frame-rate dts
        // count down smoothly. `separate()` and the dash/shield gates read
        // the same fields either way.
        let energy_max = match u.kind {
            UnitKind::Main => p.energy_max,
            UnitKind::Companion => p.comp_energy_max,
            UnitKind::Boss => p.boss_energy_max,
        };
        if !u.sprint && u.dashing <= 0 && u.shielding <= 0 {
            u.energy = (u.energy + fixed::mul(p.energy_regen, dt)).min(energy_max);
        }
        u.dashing = (u.dashing - dt).max(0);
        u.shielding = (u.shielding - dt).max(0);

        vel
    }

    /// Unit separation (section 2 of `step()`) for one overlapping pair:
    /// both pushed apart by half the overlap along the center line, no wall
    /// re-resolve (the engine's push can park a unit in a wall until the
    /// next tick's resolve — replicated as-is). The client calls this for
    /// its own main↔companion pair after both steps; pushes against
    /// unseen units are left to observation reconcile.
    pub fn separate(&self, a: &mut MoveState, b: &mut MoveState) {
        let p = &self.params;
        let ra = match a.kind {
            UnitKind::Main => p.main_radius,
            UnitKind::Companion => p.comp_radius,
            UnitKind::Boss => p.boss_radius,
        };
        let rb = match b.kind {
            UnitKind::Main => p.main_radius,
            UnitKind::Companion => p.comp_radius,
            UnitKind::Boss => p.boss_radius,
        };
        let min = ra + rb;
        let d2 = a.pos.dist2(b.pos);
        if d2 < fixed::mul(min, min) && d2 > 0 {
            let d = fixed::sqrt(d2);
            let push = (min - d) / 2;
            let nx = fixed::div(b.pos.x - a.pos.x, d);
            let ny = fixed::div(b.pos.y - a.pos.y, d);
            a.pos.x -= fixed::mul(nx, push);
            a.pos.y -= fixed::mul(ny, push);
            b.pos.x += fixed::mul(nx, push);
            b.pos.y += fixed::mul(ny, push);
        }
    }
}

/// The movement-relevant slice of `Unit` (exactly what `step()` reads and
/// mutates for movement and abilities).
#[derive(Clone, Debug)]
pub struct MoveState {
    pub kind: UnitKind,
    pub pos: Vec2,
    /// Whole degrees, 0 = north, clockwise.
    pub facing: u16,
    pub sprint: bool,
    /// Seconds remaining on dash / shield (Fix). The engine counts integer
    /// ticks; the oracle scales them by the tick length so per-tick steps
    /// reproduce the engine exactly and frame-rate steps count down smoothly.
    pub dashing: Fix,
    pub dash_dir: Vec2,
    pub shielding: Fix,
    pub energy: Fix,
}

impl MoveState {
    pub fn from_unit(u: &Unit, dt: Fix) -> Self {
        MoveState {
            kind: u.kind,
            pos: u.pos,
            facing: u.facing,
            sprint: u.sprint,
            dashing: fixed::mul(fixed::from_int(u.dashing as i64), dt),
            dash_dir: u.dash_dir,
            shielding: fixed::mul(fixed::from_int(u.shielding as i64), dt),
            energy: u.energy,
        }
    }
}

/// The movement-relevant subset of `UnitAction` (fire only turns the sprite
/// and never touches movement, so it stays out of the oracle).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum MoveAction {
    #[default]
    None,
    Dash,
    Shield,
    Sprint {
        on: bool,
    },
    Heel,
}

impl MoveAction {
    /// The movement-relevant projection of an engine action.
    pub fn from_engine(a: Option<&UnitAction>) -> Self {
        match a {
            None | Some(UnitAction::Fire { .. }) => MoveAction::None,
            Some(UnitAction::Dash) => MoveAction::Dash,
            Some(UnitAction::Shield) => MoveAction::Shield,
            Some(UnitAction::Sprint { on }) => MoveAction::Sprint { on: *on },
            Some(UnitAction::Heel) => MoveAction::Heel,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MatchConfig;
    use crate::engine::MatchEngine;
    use crate::types::{BotInput, UnitInput};

    /// Drive a real engine and the oracle through the same input script —
    /// run, dash mid-run, sprint, shield, companion follow + heel — and
    /// require bit-exact agreement every tick. Any divergence here means
    /// the client predicts a different world than the server simulates.
    #[test]
    fn predict_fidelity() {
        let config = MatchConfig::standard();
        let mut engine = MatchEngine::new(
            config.clone(),
            0xC0FFEE,
            &["alpha".to_string(), "beta".to_string()],
        );
        engine.configure_bot(0, 1, false);
        engine.configure_bot(1, 1, false);
        let oracle = MoveOracle::new(
            map::load_map(&config.map_id).expect("map"),
            SimParams::from_config(&config),
        );
        let dt = engine.params.dt;

        // Both units start from the engine's tick-0 state.
        let mut main = MoveState::from_unit(engine.state.main(0), dt);
        let mut comp = MoveState::from_unit(engine.state.companion(0), dt);
        assert_eq!(main.pos, engine.state.main(0).pos);

        // (main move, main action, comp move, comp action) per tick.
        let north = MoveInput { dir: 0, throttle: ONE };
        let script: [(MoveInput, Option<UnitAction>, MoveInput, Option<UnitAction>); 23] = [
            (north, None, MoveInput { dir: 45, throttle: ONE }, None),
            (north, None, MoveInput { dir: 45, throttle: ONE }, None),
            (MoveInput { dir: 90, throttle: ONE }, None, MoveInput::stop(), None),
            (MoveInput { dir: 90, throttle: ONE }, None, MoveInput { dir: 315, throttle: ONE }, None),
            (MoveInput { dir: 90, throttle: ONE }, None, MoveInput { dir: 315, throttle: fixed::from_f64(0.5) }, None),
            (north, None, MoveInput::stop(), None),
            (north, Some(UnitAction::Dash), MoveInput::stop(), None),
            (north, None, MoveInput::stop(), None),
            (north, None, MoveInput { dir: 180, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, None, MoveInput { dir: 180, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, Some(UnitAction::Sprint { on: true }), MoveInput { dir: 200, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, None, MoveInput { dir: 200, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, None, MoveInput { dir: 200, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, None, MoveInput { dir: 270, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, Some(UnitAction::Shield), MoveInput { dir: 270, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, Some(UnitAction::Shield), MoveInput { dir: 270, throttle: ONE }, None),
            (MoveInput { dir: 200, throttle: ONE }, None, MoveInput::stop(), Some(UnitAction::Heel)),
            (MoveInput { dir: 200, throttle: ONE }, None, MoveInput::stop(), Some(UnitAction::Heel)),
            (MoveInput::stop(), Some(UnitAction::Sprint { on: false }), MoveInput { dir: 90, throttle: ONE }, None),
            (MoveInput { dir: 135, throttle: fixed::from_f64(0.75) }, None, MoveInput { dir: 90, throttle: ONE }, None),
            (MoveInput { dir: 135, throttle: fixed::from_f64(0.75) }, Some(UnitAction::Dash), MoveInput { dir: 90, throttle: ONE }, None),
            (MoveInput { dir: 135, throttle: ONE }, None, MoveInput { dir: 90, throttle: ONE }, None),
            (MoveInput::stop(), None, MoveInput::stop(), None),
        ];

        // The scripted ticks, then 4 ticks where the companion charges the
        // standing main — the deterministic way to make the pair overlap and
        // exercise unit separation bit-for-bit.
        for t in 0..script.len() + 4 {
            let (main_mv, main_action, comp_mv, comp_action) = if t < script.len() {
                script[t]
            } else {
                (
                    MoveInput::stop(),
                    None,
                    MoveInput {
                        dir: fixed::norm_deg(comp.pos.bearing_to(main.pos)),
                        throttle: ONE,
                    },
                    None,
                )
            };
            // The main steps first: prev/new main positions for the comp.
            let main_prev = main.pos;
            let main_vel = oracle.step(
                &mut main,
                &main_mv,
                MoveAction::from_engine(main_action.as_ref()),
                dt,
                None,
                None,
            );
            let comp_vel = oracle.step(
                &mut comp,
                &comp_mv,
                MoveAction::from_engine(comp_action.as_ref()),
                dt,
                Some(main_prev),
                Some(main.pos),
            );
            // The engine separates ALL overlapping pairs after movement; the
            // script keeps the two bots far apart, so the own pair is the
            // only overlap the oracle can see.
            oracle.separate(&mut main, &mut comp);

            engine.submit(
                0,
                BotInput {
                    main: UnitInput {
                        r#move: main_mv,
                        action: main_action,
                    },
                    companion: UnitInput {
                        r#move: comp_mv,
                        action: comp_action,
                    },
                    ..Default::default()
                },
                0,
            );
            engine.step_tick();

            let em = engine.state.main(0);
            let ec = engine.state.companion(0);
            assert_eq!(main.pos, em.pos, "tick {} main pos", t + 1);
            assert_eq!(main.facing, em.facing, "tick {} main facing", t + 1);
            assert_eq!(main_vel, em.vel, "tick {} main vel", t + 1);
            assert_eq!(
                main.dashing,
                fixed::mul(fixed::from_int(em.dashing as i64), dt),
                "tick {} main dashing",
                t + 1
            );
            assert_eq!(
                main.shielding,
                fixed::mul(fixed::from_int(em.shielding as i64), dt),
                "tick {} main shielding",
                t + 1
            );
            assert_eq!(main.sprint, em.sprint, "tick {} main sprint", t + 1);
            assert_eq!(main.energy, em.energy, "tick {} main energy", t + 1);
            assert_eq!(comp.pos, ec.pos, "tick {} comp pos", t + 1);
            assert_eq!(comp.facing, ec.facing, "tick {} comp facing", t + 1);
            assert_eq!(comp_vel, ec.vel, "tick {} comp vel", t + 1);
            assert_eq!(
                comp.shielding,
                fixed::mul(fixed::from_int(ec.shielding as i64), dt),
                "tick {} comp shielding",
                t + 1
            );
        }
    }
}
