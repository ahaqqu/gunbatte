//! gunbatte-server CLI: the one binary that runs the whole box (PLAN §8.1) —
//! the matchmaker and the game-server role wired together in-process.

use gunbatte_lobby::ServerConfig;
use gunbatte_server::GameHost;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "gunbatte-server", about = "GUNBATTE ROYALE ladder server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the ladder server: bot gateway + queue + spectate + ladder page.
    Serve {
        #[arg(long, default_value_t = 8321)]
        port: u16,
        /// Address to listen on; use 127.0.0.1 behind a same-host reverse proxy.
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value = "ladder.db")]
        db: PathBuf,
        #[arg(long, default_value = "replays")]
        replays: PathBuf,
        /// Built viewer directory to serve at /viewer/.
        #[arg(long, default_value = "viewer/dist")]
        viewer: PathBuf,
        /// Concurrent match lanes.
        #[arg(long, default_value_t = 2)]
        lanes: usize,
        /// Minimum connected bots to draft a match.
        #[arg(long, default_value_t = 2)]
        min_bots: usize,
        /// Max house bots used to top up a match when a human is queued
        /// (solo play); 0 disables.
        #[arg(long, default_value_t = 8)]
        house_bots: usize,
        /// Live spectate delay in seconds (anti-cheat; PLAN §6.2).
        #[arg(long, default_value_t = 0)]
        spectate_delay_s: u64,
        /// Seconds between keepalive pings to bot sockets (0 disables).
        #[arg(long, default_value_t = 10)]
        ws_ping_every_s: u64,
        /// Seconds of total silence before a bot socket is closed (0 disables).
        #[arg(long, default_value_t = 45)]
        ws_idle_timeout_s: u64,
        /// Ceiling on concurrent WebSocket sockets, bots + spectators (0 = no cap).
        #[arg(long, default_value_t = 256)]
        max_connections: usize,
        /// Ceiling on live private rooms (0 = no cap).
        #[arg(long, default_value_t = 64)]
        max_lobbies: usize,
        /// Global failed-join attempts per minute (0 = no limit) — room-code
        /// brute-forcing throttle.
        #[arg(long, default_value_t = 30)]
        join_attempts_per_min: u32,
        /// Global first-time bot registrations per minute (0 = no limit) —
        /// ladder-row spam throttle; reconnects bypass it.
        #[arg(long, default_value_t = 60)]
        new_names_per_min: u32,
        /// Replay retention: delete the oldest match-*.json beyond this many
        /// at startup (0 = keep everything).
        #[arg(long, default_value_t = 100)]
        max_replays: usize,
        /// Input acceptance window in ticks: replies stamped up to this many
        /// ticks older than the one being decided are still applied, so a
        /// long-haul human (RTT ≫ the 50ms deadline) stays playable. 0
        /// restores the strict #53 gate. The default covers ~1s of one-way
        /// latency — roughly 2s of round-trip — because reliability on a
        /// slow link outranks strictness (AGENTS.md).
        #[arg(long, default_value_t = 10)]
        input_window_ticks: u32,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            port,
            bind,
            db,
            replays,
            viewer,
            lanes,
            min_bots,
            house_bots,
            spectate_delay_s,
            ws_ping_every_s,
            ws_idle_timeout_s,
            max_connections,
            max_lobbies,
            join_attempts_per_min,
            new_names_per_min,
            max_replays,
            input_window_ticks,
        } => {
            let cfg = ServerConfig {
                port,
                bind,
                db_path: db,
                replay_dir: replays,
                viewer_dir: Some(viewer),
                lanes,
                min_bots,
                house_bots,
                spectate_delay_s,
                ws_ping_every_s,
                ws_idle_timeout_s,
                max_connections,
                max_lobbies,
                join_attempts_per_min,
                new_names_per_min,
                max_replays,
            };
            gunbatte_lobby::Server::start(
                cfg,
                gunbatte_core::config::MatchConfig::standard(),
                Arc::new(GameHost::new(input_window_ticks)),
            )
            .await
            .expect("server");
        }
    }
}
