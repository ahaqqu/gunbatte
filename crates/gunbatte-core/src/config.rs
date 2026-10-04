//! Match configuration — every number in PLAN §11 as a serde-serializable
//! knob. Stored as f64 on the wire, converted to Fix once at engine
//! construction (deterministic conversion, see `fixed::from_f64`).

use serde::{Deserialize, Serialize};

/// Which game mode a match runs. Serde default keeps old replays and old
/// clients parsing: an absent `mode` field means the classic royale.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GameMode {
    /// Classic last-one-standing (PLAN §2).
    #[default]
    Royale,
    /// Slain the Boss: every entrant except the last is a raider (Tarsius +
    /// Jalak), the last entrant is the arena boss with its own AI. Raiders
    /// are one team and cannot hurt each other; the match ends when the
    /// boss or every raider dies.
    Boss,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MatchConfig {
    pub apiversion: u32,
    pub map_id: String,
    pub tick_rate_hz: u32,
    /// Bot reply deadline in ms (PLAN §4.2).
    pub deadline_ms: u64,
    /// Hard cap on match length in seconds (safety net; the zone ends matches).
    pub match_max_s: u64,
    pub main: MainConfig,
    pub companion: CompanionConfig,
    pub audio: AudioConfig,
    pub zone: ZoneConfig,
    pub loot: LootConfig,
    /// Bots that never send companion commands get this server-side AI.
    pub auto_heel: bool,
    /// Timeout ladder (PLAN §4.3).
    pub timeouts: TimeoutConfig,
    /// Game mode (default royale; `boss` = slain-the-boss raid).
    pub mode: GameMode,
    /// Boss tuning — only read when `mode` is Boss.
    pub boss: BossConfig,
}

impl MatchConfig {
    /// The PLAN §2/§11 starting values.
    pub fn standard() -> Self {
        MatchConfig {
            apiversion: 1,
            map_id: "arena-1".into(),
            tick_rate_hz: 10,
            deadline_ms: 50,
            match_max_s: 600,
            main: MainConfig::default(),
            companion: CompanionConfig::default(),
            audio: AudioConfig::default(),
            zone: ZoneConfig::default(),
            loot: LootConfig::default(),
            auto_heel: true,
            timeouts: TimeoutConfig::default(),
            mode: GameMode::default(),
            boss: BossConfig::default(),
        }
    }

    /// A slain-the-boss raid: same arena rules, boss slot enabled.
    pub fn boss_raid() -> Self {
        MatchConfig {
            mode: GameMode::Boss,
            ..MatchConfig::standard()
        }
    }
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MainConfig {
    pub radius: f64,
    pub hp: f64,
    pub speed: f64,
    pub fire_cooldown_s: f64,
    pub projectile_speed: f64,
    pub projectile_damage: f64,
    pub projectile_range: f64,
    pub vision: f64,
    /// Inside this distance from the sensing unit: full detail (PLAN §3.2).
    pub full_detail_range: f64,
    pub dash: DashConfig,
    pub shield: ShieldConfig,
    pub sprint: SprintConfig,
    pub energy_max: f64,
    pub energy_regen: f64,
    /// Weapon-mod caps for stacking.
    pub mod_cooldown_pct_max: f64,
    pub mod_speed_pct_max: f64,
}

impl Default for MainConfig {
    fn default() -> Self {
        MainConfig {
            radius: 14.0,
            hp: 100.0,
            speed: 140.0,
            fire_cooldown_s: 0.5,
            projectile_speed: 420.0,
            projectile_damage: 12.0,
            projectile_range: 1000.0,
            vision: 450.0,
            full_detail_range: 300.0,
            dash: DashConfig::default(),
            shield: ShieldConfig::default(),
            sprint: SprintConfig::default(),
            energy_max: 100.0,
            energy_regen: 10.0,
            mod_cooldown_pct_max: 40.0,
            mod_speed_pct_max: 30.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DashConfig {
    pub cost: f64,
    pub duration_s: f64,
    pub speed_mult: f64,
}

impl Default for DashConfig {
    fn default() -> Self {
        DashConfig {
            cost: 20.0,
            duration_s: 0.3,
            speed_mult: 2.2,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShieldConfig {
    pub cost: f64,
    pub duration_s: f64,
    /// Fraction of damage blocked, e.g. 0.7 = 70% reduction.
    pub reduction: f64,
    pub speed_mult: f64,
}

impl Default for ShieldConfig {
    fn default() -> Self {
        ShieldConfig {
            cost: 15.0,
            duration_s: 1.0,
            reduction: 0.7,
            speed_mult: 0.7,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SprintConfig {
    pub speed_mult: f64,
    pub footstep_radius: f64,
}

impl Default for SprintConfig {
    fn default() -> Self {
        SprintConfig {
            speed_mult: 1.4,
            footstep_radius: 200.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompanionConfig {
    pub radius: f64,
    pub hp: f64,
    pub speed: f64,
    pub vision: f64,
    pub leash: f64,
    pub energy_max: f64,
    pub respawn_s: f64,
}

impl Default for CompanionConfig {
    fn default() -> Self {
        CompanionConfig {
            radius: 10.0,
            hp: 30.0,
            speed: 170.0,
            vision: 250.0,
            leash: 350.0,
            energy_max: 50.0,
            respawn_s: 20.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    pub gunshot: f64,
    pub dash: f64,
    pub footstep: f64,
    /// Bearing quantization in degrees (PLAN §3.2).
    pub bearing_quantization: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            gunshot: 900.0,
            dash: 500.0,
            footstep: 200.0,
            bearing_quantization: 15,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ZoneConfig {
    /// Radii per phase; last entry is the endgame collapse.
    pub radii: Vec<f64>,
    pub hold_s_min: f64,
    pub hold_s_max: f64,
    pub shrink_s: f64,
    /// Damage per second outside the circle, per phase.
    pub damage_per_phase: Vec<f64>,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        ZoneConfig {
            radii: vec![1600.0, 1200.0, 850.0, 550.0, 300.0, 0.0],
            hold_s_min: 45.0,
            hold_s_max: 60.0,
            shrink_s: 10.0,
            damage_per_phase: vec![2.0, 2.0, 4.0, 4.0, 6.0, 8.0],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LootConfig {
    pub count: u32,
    /// Pickups spawn over the first N seconds.
    pub spawn_window_s: f64,
    pub weight_hp: u32,
    pub weight_energy: u32,
    pub weight_mod: u32,
    /// Weight of gun-swap pickups (one of the six special guns).
    pub weight_weapon: u32,
    pub hp_kit_amount: f64,
    pub energy_pack_amount: f64,
    pub mod_cooldown_pct: f64,
    pub mod_speed_pct: f64,
    pub pickup_radius: f64,
}

impl Default for LootConfig {
    fn default() -> Self {
        LootConfig {
            count: 28,
            spawn_window_s: 180.0,
            weight_hp: 35,
            weight_energy: 30,
            weight_mod: 15,
            // Roughly half the loot rolls a gun (~13 of 28 at the default
            // count) — weapon swaps are a core loop, not a rare treat.
            weight_weapon: 70,
            hp_kit_amount: 35.0,
            energy_pack_amount: 40.0,
            mod_cooldown_pct: 20.0,
            mod_speed_pct: 15.0,
            pickup_radius: 25.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TimeoutConfig {
    /// A reply slower than this (ms) is counted as a slow reply — a stat
    /// only, never a forfeit: distance is not misbehavior, and the fatal
    /// deadline below already retires a client that truly froze.
    pub slow_ms: u64,
    /// A reply slower than this (ms) forfeits immediately.
    pub fatal_ms: u64,
    /// Missing this fraction of deadlines (percent) forfeits.
    pub max_missed_pct: u64,
    /// Ticks a bot may stay disconnected (momentum) before forfeit: 30s —
    /// a dropped connection on a slow link should ride out the outage.
    pub disconnect_grace_ticks: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        TimeoutConfig {
            slow_ms: 200,
            fatal_ms: 1000,
            max_missed_pct: 20,
            disconnect_grace_ticks: 300,
        }
    }
}

/// The raid boss (Slain the Boss mode). The boss is one entrant: its main
/// unit gets these stats and the `boss_cannon` gun; its companion is the
/// minion that respawns per the normal companion rules. The boss ignores
/// the zone — it IS the endgame — and never swaps its cannon for loot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BossConfig {
    pub radius: f64,
    pub hp: f64,
    /// Boss ground speed — kiting is the counterplay, so it is well under
    /// the mains' 140.
    pub speed: f64,
    /// The boss watches the whole plaza.
    pub vision: f64,
    pub energy_max: f64,
    pub energy_regen: f64,
    /// The boss cannon: a slow heavy shell with a splash burst.
    pub cannon: BossCannonConfig,
    /// Raiders a boss match is filled up to (including the boss).
    pub raid_size: u32,
}

impl Default for BossConfig {
    fn default() -> Self {
        BossConfig {
            radius: 46.0,
            hp: 3200.0,
            speed: 92.0,
            vision: 1000.0,
            energy_max: 100.0,
            energy_regen: 12.0,
            cannon: BossCannonConfig::default(),
            raid_size: 8,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BossCannonConfig {
    pub damage: f64,
    pub speed: f64,
    pub range: f64,
    pub cooldown_s: f64,
    pub splash_radius: f64,
    pub splash_damage: f64,
}

impl Default for BossCannonConfig {
    fn default() -> Self {
        BossCannonConfig {
            damage: 35.0,
            speed: 380.0,
            range: 900.0,
            cooldown_s: 1.1,
            splash_radius: 110.0,
            splash_damage: 22.0,
        }
    }
}
