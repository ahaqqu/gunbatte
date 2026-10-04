//! gunbatte-core: the deterministic simulation core for AI Battle Royale
//! (PLAN §5). One authoritative implementation, compiled twice: native
//! (match server / runner) and WASM (web viewer, bot SDK).
//!
//! Public contract:
//! - [`engine::MatchEngine`] — inputs in, tick out; momentum + timeouts.
//! - [`observe::observe`] — strict fog-of-war observation (PLAN §3).
//! - [`replay`] — thin replay format + byte-identical verification.
//! - [`fixed`] — Q16.16 math; no floats in game state, ever (PLAN §5.1).

pub mod bots;
pub mod config;
pub mod engine;
pub mod events;
pub mod fixed;
pub mod loot;
pub mod map;
pub mod observe;
pub mod params;
pub mod predict;
pub mod replay;
pub mod rng;
pub mod state;
pub mod step;
pub mod timeout;
pub mod trig_tables;
pub mod types;
pub mod weapons;
pub mod zone;

pub use config::MatchConfig;
pub use engine::MatchEngine;
pub use events::Event;
pub use types::{BotInput, UnitAction, UnitInput};
