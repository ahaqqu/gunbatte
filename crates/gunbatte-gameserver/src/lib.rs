//! Game-server role (AGENTS.md): everything inside one match. The matchmaker
//! hands over a roster of entrants plus a [`MatchContext`]; this side runs
//! the 10Hz loop, pushes one strict-fog observation per tick, collects one
//! action reply per entrant (50ms reply deadline with a bounded acceptance
//! window for slow links, momentum on misses), records the replay, and
//! writes results + ELO back through the database. It never touches the
//! queue, lobbies, or identity state — one match, one owner.

use gunbatte_core::config::{GameMode, MatchConfig};
use gunbatte_core::engine::MatchEngine;
use gunbatte_core::replay::{build_summary, ReplayRecorder};
use gunbatte_node::db;
use gunbatte_node::{BotMsg, MatchContext, MatchEntrant};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Consecutive dropped observations before a non-reading bot is moved onto
/// the disconnect path (a healthy socket drains its channel continuously).
const STALL_LIMIT: u32 = 10;

/// What a finished match reports back to whoever hosted it.
pub struct MatchOutcome {
    pub replay_url: String,
    pub winner: Option<String>,
    pub ticks: u64,
}

/// The match seed is the one secret of a match — it reproduces the zone
/// schedule and every loot spawn (PLAN §5.1) — so it comes from the OS
/// CSPRNG, not from wall-clock time, which every participant knows.
fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    match getrandom::getrandom(&mut buf) {
        Ok(()) => u64::from_le_bytes(buf) | 1,
        Err(_) => {
            // No OS RNG available: degrade to the old time-based seed.
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            (d.subsec_nanos() as u64 ^ d.as_secs()) | 1
        }
    }
}

/// Per-entrant input-path counters, shown once per entrant at match end
/// (`input_summary` in the event catalog, LIMITS.md) so slow links are
/// visible in aggregate without flooding the journal at 10 Hz.
#[derive(Default, Clone, Copy)]
struct InputStats {
    accepted: u64,
    late: u64,
    staleness_sum: u64,
    max_staleness: u64,
    latency_sum: u64,
    dropped_stale: u64,
    late_logged: bool,
}

/// Run one match to completion: the 10Hz match loop (PLAN §4.2) with obs at
/// t=0, 50ms reply deadline, ~50ms resolution window, simultaneous
/// resolution. `config` decides the mode — royale drafts pass the server
/// default, raids pass a Boss-mode clone. `input_window_ticks` is the reply
/// stamp acceptance window (0 = strict #53 behavior; see `accepts_tick`).
/// Never awaits a bot's channel: a bot that stopped reading would otherwise
/// stall the whole lane once its bounded channel fills, so a full slot drops
/// the frame instead, and consecutive drops move the bot onto the disconnect
/// path (grace, then forfeit).
///
/// Returns when the match is over and persisted; requeueing survivors is
/// the matchmaker's job, not this function's.
pub async fn run_match(
    ctx: MatchContext,
    entrants: Vec<MatchEntrant>,
    config: MatchConfig,
    input_window_ticks: u32,
) -> MatchOutcome {
    let n = entrants.len();
    let names: Vec<String> = entrants.iter().map(|h| h.name.clone()).collect();
    let seed = random_seed();
    let mut engine = MatchEngine::new(config.clone(), seed, &names);
    for (b, h) in entrants.iter().enumerate() {
        engine.configure_bot(b as u32, h.decision_rate, h.auto_heel);
    }
    let mut recorder = ReplayRecorder::new(&engine, &names);
    let boss_bot = if config.mode == GameMode::Boss {
        Some(n - 1)
    } else {
        None
    };

    // Tell the bots what they're in for. Best effort: a stalled socket must
    // not wedge the match before it begins.
    for (b, h) in entrants.iter().enumerate() {
        let _ = h
            .out_tx
            .try_send(
                serde_json::json!({
                    "type": "match_start",
                    "bot": h.name,
                    "bots": names,
                    "map_id": engine.config.map_id,
                    "deadline_ms": engine.config.deadline_ms,
                    "tick_rate": engine.config.tick_rate_hz,
                    "seedless": true,
                    "mode": if config.mode == GameMode::Boss { "boss" } else { "royale" },
                    "role": if Some(b) == boss_bot { "boss" } else { "raider" },
                })
                .to_string(),
            );
    }
    println!(
        "match_started entrants={} mode={} rated={} names=\"{}\"",
        n,
        if boss_bot.is_some() { "boss" } else { "royale" },
        ctx.rated,
        names.join(","),
    );

    let tick_ms: u64 = 1000 / config.tick_rate_hz.max(1) as u64;
    let deadline = Duration::from_millis(config.deadline_ms);
    let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut disconnected = vec![false; n];
    let mut send_stalls = vec![0u32; n];
    let mut stats = vec![InputStats::default(); n];
    let mut forfeit_logged = vec![false; n];

    while !engine.state.finished {
        interval.tick().await;
        let tick_start = Instant::now();

        // Push this tick's observation to every bot simultaneously.
        push_observations(&mut engine, &entrants, &mut send_stalls);

        // Reply deadline window (PLAN §4.2): anything arriving within the
        // deadline is on time; the tail of the window catches stragglers,
        // whose true latency feeds the timeout ladder (§4.3).
        tokio::time::sleep(deadline).await;
        drain_inputs(
            &mut engine,
            &mut recorder,
            &entrants,
            tick_start,
            &mut disconnected,
            &mut stats,
            input_window_ticks,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(
            tick_ms
                .saturating_sub(config.deadline_ms)
                .max(1)
                .saturating_sub(2),
        ))
        .await;
        drain_inputs(
            &mut engine,
            &mut recorder,
            &entrants,
            tick_start,
            &mut disconnected,
            &mut stats,
            input_window_ticks,
        )
        .await;

        let events = engine.step_tick();
        recorder.record_tick(engine.state.digest());

        // The ladder's forfeit decisions happen inside the engine, which
        // knows nothing about names — surface them here, once each, with the
        // entrant name attached.
        for (b, h) in entrants.iter().enumerate() {
            if let Some((reason, tick)) = engine.forfeit_info(b as u32) {
                if !forfeit_logged[b] {
                    forfeit_logged[b] = true;
                    println!(
                        "ladder_forfeit entrant={} bot={} tick={} reason={}",
                        h.name, b, tick, reason
                    );
                }
            }
        }

        // Spectate bus: full state, everything the bots don't get. The sink
        // comes from the matchmaker via the context; nobody here knows how
        // (or whether) spectators are being served.
        let frame = engine.spectator_frame(&events);
        if let Ok(json) = serde_json::to_string(&serde_json::json!({
            "type": "frame",
            "frame": frame,
        })) {
            let _ = ctx.spectate.send(json);
        }
    }

    // Persist: replay file + ladder + ELO — the game role's output contract.
    recorder.finish(&engine);
    let summary = build_summary(&engine, &names);
    let match_seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let file_name = format!("match-{match_seq}.json");
    let path = ctx.replay_dir.join(&file_name);
    std::fs::write(&path, recorder.to_json()).ok();
    let replay_url = format!("/replays/{file_name}");

    let places: std::collections::HashMap<String, i64> = summary
        .placements
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.clone(), i as i64 + 1))
        .collect();
    let ratings: Vec<(String, i64)> = names
        .iter()
        .map(|nm| (nm.clone(), ctx.db.elo_of(nm)))
        .collect();
    // House-filled matches are sparring, not competition (issue #37): the
    // matchmaking side says so via the context. K=0 moves nobody's rating,
    // while the match row, placements, games and wins still record — the
    // ladder ranks by elo, so nothing farmable remains.
    let k = if ctx.rated { 32.0 } else { 0.0 };
    let elos = db::elo_update(&ratings, &places, k);
    let results: Vec<(String, i64, i64)> = summary
        .placements
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.clone(), i as i64 + 1, p.kills as i64))
        .collect();
    ctx.db
        .record_match(seed, n as i64, summary.ticks, &replay_url, &results, &elos);
    for (b, h) in entrants.iter().enumerate() {
        let s = &stats[b];
        println!(
            "input_summary entrant={} accepted={} late={} avg_staleness={} avg_latency_ms={} max_staleness={} dropped_stale={}",
            h.name,
            s.accepted,
            s.late,
            s.staleness_sum.checked_div(s.late).unwrap_or(0),
            s.latency_sum.checked_div(s.accepted).unwrap_or(0),
            s.max_staleness,
            s.dropped_stale,
        );
    }
    println!(
        "match_over ticks={} winner={} replay={} mode={} rated={}",
        summary.ticks,
        summary
            .winner
            .map(|w| names[w as usize].clone())
            .unwrap_or_else(|| "none".into()),
        replay_url,
        if boss_bot.is_some() { "boss" } else { "royale" },
        ctx.rated,
    );

    // Notify the bots. Best effort: the lane must free up even if a bot
    // stopped reading. (Requeueing connected survivors back into the public
    // queue is matchmaking work and happens on the other side of the seam.)
    for (b, h) in entrants.iter().enumerate() {
        let place = summary
            .placements
            .iter()
            .position(|p| p.bot == b as u32)
            .map(|i| i + 1)
            .unwrap_or(0);
        let _ = h
            .out_tx
            .try_send(
                serde_json::json!({
                    "type": "match_over",
                    "place": place,
                    "replay": replay_url,
                    "new_elo": elos.iter().find(|(nm, _, _)| nm == &h.name).map(|(_, _, e)| *e),
                })
                .to_string(),
            );
    }

    MatchOutcome {
        replay_url,
        // The summary names the winner by bot index; report the name.
        winner: summary.winner.map(|w| names[w as usize].clone()),
        ticks: summary.ticks,
    }
}

/// Push this tick's observation to every entrant simultaneously.
fn push_observations(engine: &mut MatchEngine, entrants: &[MatchEntrant], stalls: &mut [u32]) {
    for (b, h) in entrants.iter().enumerate() {
        let obs = engine.observe(b as u32);
        let json = serde_json::to_string(&obs).unwrap_or_default();
        match h.out_tx.try_send(json) {
            Ok(()) => {
                if stalls[b] >= STALL_LIMIT {
                    // The link caught up after a stall: take the entrant
                    // back off the disconnect path. A slow moment must not
                    // become a forfeit — only a link that stays choked for
                    // the whole grace window does that.
                    println!("obs_recovered entrant={}", h.name);
                    engine.reconnect(b as u32);
                }
                stalls[b] = 0;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                stalls[b] += 1;
                if stalls[b] == STALL_LIMIT {
                    println!("obs_stall entrant={}", h.name);
                    engine.disconnect(b as u32, engine.state.tick);
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                engine.disconnect(b as u32, engine.state.tick);
            }
        }
    }
}

/// The reply window's tick discipline (issue #37, windowed per the 2026-10
/// grill): a message stamped with the tick being decided is on time; one
/// stamped up to `window` ticks older is still a usable answer — distant
/// humans cannot beat the speed of light, and momentum already degrades
/// misses — while anything older (or from the future) answers a question
/// nobody is asking anymore and is dropped. 0 (a client that omits the
/// stamp) asserts nothing and is accepted. The window bounds both the
/// staleness of applied inputs and the replay surface #53 closed.
fn accepts_tick(client_tick: u64, current: u64, window: u32) -> bool {
    client_tick == 0
        || (client_tick <= current && current - client_tick <= window as u64)
}

/// Collect each entrant's newest message in the window and submit it.
#[allow(clippy::too_many_arguments)]
async fn drain_inputs(
    engine: &mut MatchEngine,
    recorder: &mut ReplayRecorder,
    entrants: &[MatchEntrant],
    tick_start: Instant,
    disconnected: &mut [bool],
    stats: &mut [InputStats],
    window: u32,
) {
    for (b, h) in entrants.iter().enumerate() {
        let mut rx = h.in_rx.lock().await;
        // Coalesce: each submit overwrites the bot's pending slot, so only
        // the newest message in the window can matter — a flooding bot costs
        // one parse, not a burst. Stale-stamped replies are dropped here, at
        // the same gate, so they never reach the engine or the replay.
        let current_tick = engine.state.tick;
        let mut newest: Option<BotMsg> = None;
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if accepts_tick(msg.client_tick, current_tick, window) {
                        newest = Some(msg);
                    } else {
                        stats[b].dropped_stale += 1;
                        if stats[b].dropped_stale == 1 {
                            println!(
                                "input_dropped_stale entrant={} client_tick={} current_tick={}",
                                h.name, msg.client_tick, current_tick
                            );
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Socket died: 10s of momentum, then forfeit (PLAN §4.3.4).
                    if !disconnected[b] {
                        disconnected[b] = true;
                        println!("disconnect entrant={} cause=socket_closed", h.name);
                        engine.disconnect(b as u32, engine.state.tick);
                    }
                    break;
                }
            }
        }
        if let Some(msg) = newest {
            let latency = msg.arrived.duration_since(tick_start).as_millis() as u64;
            let staleness = if msg.client_tick == 0 {
                0
            } else {
                current_tick.saturating_sub(msg.client_tick)
            };
            let s = &mut stats[b];
            s.accepted += 1;
            s.latency_sum += latency;
            if staleness > 0 {
                // Late but within the window: responsive, so the ladder's
                // miss counters stay untouched (engine.submit records only
                // the latency) — the ladder forfeits the gone, not the
                // distant. First late acceptance is logged; the steady
                // state of a slow link would otherwise flood the journal.
                s.late += 1;
                s.staleness_sum += staleness;
                s.max_staleness = s.max_staleness.max(staleness);
                if !s.late_logged {
                    s.late_logged = true;
                    println!(
                        "input_late_accepted entrant={} staleness={} latency_ms={}",
                        h.name, staleness, latency
                    );
                }
            }
            engine.submit(b as u32, msg.input.clone(), latency);
            if msg.input.intent.is_some() || msg.input.belief.is_some() {
                engine.submit_mind(
                    b as u32,
                    msg.input.intent.clone(),
                    msg.input.belief.clone(),
                );
            }
            recorder.record_submit(b as u32, msg.input);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Mutex;
    use std::sync::Arc;

    #[test]
    fn seeds_come_from_os_entropy() {
        let a = random_seed();
        let b = random_seed();
        assert_ne!(a, b, "two OS-random u64s colliding is ~2^-64");
        assert_eq!(a & 1, 1, "seed stays odd (match-start invariant)");
        assert_eq!(b & 1, 1);
    }

    #[test]
    fn tick_stamp_discipline() {
        const WINDOW: u32 = 3;
        // The current tick's stamp is on time…
        assert!(accepts_tick(42, 42, WINDOW));
        // …an omitted stamp (serde default) asserts nothing…
        assert!(accepts_tick(0, 42, WINDOW));
        // …recently stale stamps are the point of the window: a distant
        // human's answer to tick 42 lands during tick 44's window at best.
        assert!(accepts_tick(41, 42, WINDOW), "one tick late");
        assert!(accepts_tick(39, 42, WINDOW), "exactly at the window edge");
        // …and anything older (or future-stamped) answers a question nobody
        // is asking anymore: replays and time-shifted claims both drop.
        assert!(!accepts_tick(38, 42, WINDOW), "beyond the window");
        assert!(!accepts_tick(1, 42, WINDOW), "ancient replay");
        assert!(!accepts_tick(43, 42, WINDOW), "future claim");
        // Window 0 restores the strict #53 gate exactly.
        assert!(accepts_tick(42, 42, 0));
        assert!(!accepts_tick(41, 42, 0), "strict mode: previous tick");
    }

    #[test]
    fn stalled_reader_is_disconnected_without_blocking_the_lane() {
        let mk = |name: &str, out_tx: mpsc::Sender<String>, in_rx: mpsc::Receiver<BotMsg>| {
            MatchEntrant {
                name: name.into(),
                db_id: 0,
                decision_rate: 1,
                auto_heel: false,
                connected: Arc::new(AtomicBool::new(true)),
                out_tx,
                in_rx: Arc::new(Mutex::new(in_rx)),
            }
        };

        let (tx_ok, mut rx_ok) = mpsc::channel::<String>(64);
        let (tx_stall, _rx_stall) = mpsc::channel::<String>(64); // never drained
        let (_in_tx_a, in_rx_a) = mpsc::channel::<BotMsg>(8);
        let (_in_tx_b, in_rx_b) = mpsc::channel::<BotMsg>(8);
        let entrants = vec![mk("ok", tx_ok, in_rx_a), mk("stalled", tx_stall, in_rx_b)];

        let mut engine =
            MatchEngine::new(MatchConfig::standard(), 1, &["ok".into(), "stalled".into()]);
        let mut stalls = vec![0u32; 2];

        // Fill the stalled bot's 64-slot channel, then keep pushing: the
        // STALL_LIMIT-th consecutive drop must move it onto the disconnect
        // path while the healthy bot keeps receiving every observation.
        for _ in 0..(64 + STALL_LIMIT) {
            while rx_ok.try_recv().is_ok() {}
            push_observations(&mut engine, &entrants, &mut stalls);
        }
        assert!(
            engine.timeouts[1].stats.disconnected_since_tick.is_some(),
            "stalled reader must enter the disconnect path"
        );
        assert!(
            engine.timeouts[0].stats.disconnected_since_tick.is_none(),
            "healthy bot must be untouched"
        );

        // And the healthy channel still works afterwards.
        while rx_ok.try_recv().is_ok() {}
        push_observations(&mut engine, &entrants, &mut stalls);
        assert!(
            rx_ok.try_recv().is_ok(),
            "healthy bot still receives observations"
        );
    }
}
