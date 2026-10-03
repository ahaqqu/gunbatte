//! Matchmaker role (AGENTS.md): owns people — bots and browsers connect
//! here over WebSocket, register against the ladder DB, and wait in the
//! public queue or a private lobby. The scheduler drafts matches between
//! idle entrants and hands each roster to the [`MatchHost`] (the game-server
//! role, injected by whoever wires the node); when a match ends, connected
//! survivors return to the public queue. Matchmaking never runs a match
//! itself: one match, one owner.

pub mod house;
pub mod page;

use gunbatte_core::config::{GameMode, MatchConfig};
use gunbatte_core::types::BotInput;
use gunbatte_node::{BotMsg, MatchContext, MatchEntrant};
/// String → WS text message (axum 0.8 uses Utf8Bytes).
fn tmsg(s: String) -> Message {
    Message::Text(s.into())
}

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex};

/// The handoff to the game-server role: the lobby drafts a roster, the host
/// runs the match to completion. Implementations live outside this crate —
/// in-process wiring today, an assignment protocol when the roles split
/// across processes. The lobby must learn nothing about how matches run
/// beyond this signature and [`MatchContext`].
pub trait MatchHost: Send + Sync + 'static {
    /// Run the match to completion. Returns only after results and the
    /// replay are persisted; the lobby then requeues connected survivors.
    fn host_match(
        &self,
        ctx: MatchContext,
        entrants: Vec<MatchEntrant>,
        config: MatchConfig,
    ) -> BoxFuture<'static, ()>;
}

/// One connected bot. The WS task owns the socket; the match task talks to
/// it through `out_tx` (server → bot) and `in_rx` (bot → server). The
/// game-role view of this handle is [`BotHandle::entrant`] — the only thing
/// a match ever sees of it.
pub struct BotHandle {
    pub name: String,
    pub db_id: i64,
    pub decision_rate: u64,
    pub auto_heel: bool,
    /// A human at the viewer (queued by the play client) — triggers house fill.
    pub human: bool,
    /// An in-process house bot (never re-queued, hidden from standings).
    pub house: bool,
    /// Which match mode this entrant queued for (royale unless it asked).
    pub mode: GameMode,
    /// This entrant wants to BE the boss in a Slain-the-Boss raid.
    pub wants_boss: bool,
    /// Private lobby this entrant belongs to, if any: members are never
    /// drafted by the public queue — their host starts the match by hand.
    /// Cleared when their lobby match starts (afterwards they requeue publicly).
    pub lobby: std::sync::Mutex<Option<String>>,
    pub connected: Arc<AtomicBool>,
    pub out_tx: mpsc::Sender<String>,
    pub in_rx: Arc<Mutex<mpsc::Receiver<BotMsg>>>,
}

impl BotHandle {
    /// The room code this entrant is waiting in, if any.
    pub fn lobby_code(&self) -> Option<String> {
        self.lobby.lock().expect("lobby lock").clone()
    }

    pub fn set_lobby(&self, code: Option<String>) {
        *self.lobby.lock().expect("lobby lock") = code;
    }

    /// The game-role view of this entrant — the whole seam handoff
    /// (gunbatte-node). Everything else on the handle is matchmaking-private.
    pub fn entrant(&self) -> MatchEntrant {
        MatchEntrant {
            name: self.name.clone(),
            db_id: self.db_id,
            decision_rate: self.decision_rate,
            auto_heel: self.auto_heel,
            connected: self.connected.clone(),
            out_tx: self.out_tx.clone(),
            in_rx: self.in_rx.clone(),
        }
    }
}

/// A private room created over the bot socket: a shareable code, a roster of
/// invited/joined players, and a host who starts the match when everyone is in.
/// Works for both modes — a royale lobby is just a match you picked the roster
/// for, and a boss lobby casts its last member (or the built-in brain) as boss.
pub struct Lobby {
    pub code: String,
    pub host: Arc<BotHandle>,
    pub mode: GameMode,
    /// Boss-cannon raid (`raid_size` entrants, boss last) vs royale.
    pub boss_raid: bool,
    pub members: Vec<Arc<BotHandle>>,
}

/// What a registering socket asked to do with lobbies.
enum LobbyIntent {
    None,
    Create { boss: bool },
    Join { code: String },
}

#[derive(Clone)]
pub struct ServerConfig {
    pub port: u16,
    /// Address to listen on: "0.0.0.0" (default) or "127.0.0.1" when a reverse
    /// proxy on the same host fronts the server.
    pub bind: String,
    pub db_path: PathBuf,
    pub replay_dir: PathBuf,
    pub viewer_dir: Option<PathBuf>,
    /// Concurrent match lanes (PLAN §8.2: 1–2).
    pub lanes: usize,
    /// Minimum connected bots before the queue drafts a match.
    pub min_bots: usize,
    /// When a human is queued, top the match up to 8 entrants with at most
    /// this many in-process house bots (0 disables solo play entirely).
    pub house_bots: usize,
    /// Spectate delay in seconds (anti-cheat, PLAN §6.2).
    pub spectate_delay_s: u64,
    /// Seconds between server → bot keepalive pings (0 disables).
    pub ws_ping_every_s: u64,
    /// Seconds of total silence (no inputs, no pongs) before a bot socket is
    /// closed, so a vanished peer's half-open connection cannot linger. Must
    /// exceed the ping interval; 0 disables the idle check.
    pub ws_idle_timeout_s: u64,
    /// Ceiling on concurrent WebSocket sockets (bot gateway + spectators
    /// share one pool); each spawns tasks and channels. 0 = unlimited.
    pub max_connections: usize,
    /// Ceiling on live private rooms. 0 = unlimited.
    pub max_lobbies: usize,
    /// Global token-bucket rate for failed `join` attempts per minute: a
    /// wrong code draws from one shared bucket, so brute-forcing the ~1M
    /// room-code space cannot run at connection speed. 0 = unlimited.
    pub join_attempts_per_min: u32,
    /// Global token-bucket rate for first-time bot-name registrations per
    /// minute: cycling unique names cannot mint unbounded ladder rows.
    /// Repeat connections and server-side house bots bypass it. 0 = unlimited.
    pub new_names_per_min: u32,
    /// Replay retention: startup deletes the oldest `match-*.json` beyond
    /// this many. 0 = keep everything.
    pub max_replays: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            port: 8321,
            bind: "0.0.0.0".to_string(),
            db_path: PathBuf::from("ladder.db"),
            replay_dir: PathBuf::from("replays"),
            viewer_dir: None,
            lanes: 2,
            min_bots: 2,
            house_bots: 8,
            spectate_delay_s: 30,
            ws_ping_every_s: 10,
            ws_idle_timeout_s: 45,
            max_connections: 256,
            max_lobbies: 64,
            join_attempts_per_min: 30,
            new_names_per_min: 60,
            max_replays: 100,
        }
    }
}

/// Global token bucket for the per-minute abuse ceilings (issue #37): starts
/// full, refills continuously at `capacity` per minute, and `take` draws one
/// token. A `capacity` of 0 means unlimited (every take succeeds) so a zero
/// knob disables the ceiling instead of bricking the door.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(per_minute: u32) -> Self {
        let capacity = f64::from(per_minute);
        TokenBucket {
            tokens: capacity,
            capacity,
            per_sec: capacity / 60.0,
            last: Instant::now(),
        }
    }

    fn take(&mut self) -> bool {
        if self.capacity == 0.0 {
            return true;
        }
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.per_sec)
            .min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Startup replay retention (issue #37): delete the oldest `match-*.json`
/// beyond `keep`. Only files whose name parses as `match-<millis>.json` are
/// touched, so hand-placed fixtures (demo8.json, m1-acceptance.json) and any
/// unrecognized file survive; 0 keeps everything.
pub fn sweep_replays(dir: &std::path::Path, keep: usize) {
    if keep == 0 {
        return;
    }
    let mut seqs: Vec<(u64, PathBuf)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let stem = p.file_stem()?.to_str()?;
                let seq = stem.strip_prefix("match-")?.parse::<u64>().ok()?;
                (p.extension()?.to_str()? == "json").then_some((seq, p))
            })
            .collect(),
        Err(_) => return,
    };
    seqs.sort_by_key(|(seq, _)| *seq);
    while seqs.len() > keep {
        let (_, path) = seqs.remove(0);
        if std::fs::remove_file(&path).is_ok() {
            println!("🗑 replay retention: removed {}", path.display());
        }
    }
}

pub struct Server {
    pub cfg: ServerConfig,
    pub db: Arc<gunbatte_node::db::Db>,
    pub config: MatchConfig,
    /// Who actually runs matches: the game-server role, injected at start.
    pub host: Arc<dyn MatchHost>,
    /// Issue #38: browser origins allowed to open /ws/bot and /ws/spectate.
    /// Empty = the check is off (local dev, tests); see `origin_allowed`.
    pub allowed_origins: Vec<String>,
    pub lobby: Arc<Mutex<Vec<Arc<BotHandle>>>>,
    /// Private rooms by share code (uppercase alphanumeric, e.g. "K7QP").
    pub lobbies: Arc<Mutex<HashMap<String, Lobby>>>,
    pub lobby_seq: Arc<AtomicU64>,
    pub spectate_tx: broadcast::Sender<String>,
    pub lane_count: Arc<tokio::sync::Semaphore>,
    /// Concurrent-socket ceiling shared by the bot gateway and spectators:
    /// a permit is held for the life of each upgraded connection.
    conn_permits: Arc<tokio::sync::Semaphore>,
    /// Abuse ceilings drawn once per offense (wrong join code / brand-new
    /// bot name) — global on purpose, since per-IP state behind the reverse
    /// proxy would trust spoofable headers.
    join_bucket: std::sync::Mutex<TokenBucket>,
    new_name_bucket: std::sync::Mutex<TokenBucket>,
    /// Names with a live connection (issue #42): a second concurrent
    /// registration with an already-connected name is refused, closing the
    /// duplicate-draft ELO double-attribution and the same-name room
    /// confusion. Inserted after the door checks, removed at teardown.
    ///
    /// Reliability amendment: a registration presenting a tokened name's
    /// correct secret is its owner — it evicts the old connection (latest
    /// wins) instead of being refused, so a hung tab or a vanished socket
    /// can never lock the owner out of their own name. Tokenless casual
    /// rows keep the strict refusal (no secret exists to prove ownership
    /// with), and wrong tokens are refused exactly as before.
    live_names:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<LiveSlot>>>,
}

/// One live connection's kill switch: its writer's output queue (for a
/// last-word notice) and its reader→writer closed flag (to break the
/// writer, which releases the socket and runs the teardown). The slot arc
/// doubles as the teardown's ownership proof: a teardown may only remove
/// the map entry that is still its own, so an evicted connection dying
/// late cannot erase its successor's registration.
struct LiveSlot {
    out: mpsc::Sender<String>,
    close: tokio::sync::watch::Sender<bool>,
}

#[derive(Deserialize)]
struct RegisterMsg {
    #[serde(rename = "type")]
    _type: String,
    name: String,
    #[serde(default)]
    token: String,
    #[serde(default = "default_rate")]
    decision_rate: u64,
    #[serde(default)]
    auto_heel: bool,
    #[serde(default)]
    human: bool,
    /// "royale" (default) or "boss" — queue for a Slain-the-Boss raid.
    #[serde(default)]
    mode: String,
    /// Register AS the raid boss: the server casts this entrant as the boss
    /// of the next raid instead of spawning the built-in boss brain.
    #[serde(default)]
    boss: bool,
    /// Lobby intent: "create" the room (you become host) or "join" `lobby`.
    #[serde(default)]
    lobby_action: String,
    /// Room code for `lobby_action: "join"`.
    #[serde(default)]
    lobby: String,
    /// Ladder enrollment (issue #42): rated identities stand on the ladder
    /// protected by a server-issued secret; without the flag a brand-new
    /// name plays casual — off-ladder, tokenless, disposable. Known names
    /// keep whatever tier their row already has.
    #[serde(default)]
    rated: bool,
}

/// A message from a lobby host (or the viewer UI) on an established socket.
#[derive(Deserialize)]
struct LobbyCmd {
    #[serde(rename = "type")]
    _type: String,
    /// Only "start" for now; any other value is ignored.
    #[serde(default)]
    action: String,
    /// Entrants to fill the match up to (solo host); server clamps.
    #[serde(default)]
    fill: Option<usize>,
    /// In a boss lobby, start this member as the boss ("boss" or the name);
    /// absent = the built-in boss brain.
    #[serde(default)]
    boss: Option<String>,
}

fn default_rate() -> u64 {
    1
}

/// House-fill ceiling for a royale lobby (same 8 as the public queue).
const HOUSE_MATCH_MAX: usize = 8;

/// "Disabled" for the keepalive timers: one year out — never fires in
/// practice, still overflow-safe on the timer wheel (Duration::MAX is not).
const KEEPALIVE_OFF: Duration = Duration::from_secs(365 * 24 * 3600);

/// Global in-flight HTTP request ceiling (issue #37): per-IP limiting is the
/// reverse proxy's job (the socket address here is the proxy's), so this is
/// one blunt, correctly-total cap.
const HTTP_CONCURRENCY: usize = 256;

/// Bot names are operator-controlled wire data that render into the viewer
/// HUD and the ladder page. Pin them to a markup-free charset so a crafted
/// name can never carry HTML into a spectator's browser.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
}

/// Journal-safe rendering of unvalidated wire data (a refused registration's
/// claimed name, a join code): keep the name-like charset, drop everything a
/// newline escape or ANSI shim would need, truncate to 32 chars.
fn log_safe(s: &str) -> String {
    let clean: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
        .take(32)
        .collect();
    clean
}

/// 128 bits of OS randomness, hex — the secret issued to an enrolling
/// ladder identity (issue #42). Unguessable credentials are the point, so
/// this is a real CSPRNG: the lobby-code trick (counter + clock) would not
/// do. Clients present it on every later connection.
fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS RNG is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize)]
struct ClientAction {
    #[serde(default)]
    tick: u64,
    #[serde(flatten)]
    input: BotInput,
}

impl Server {
    pub async fn start(
        cfg: ServerConfig,
        config: MatchConfig,
        host: Arc<dyn MatchHost>,
    ) -> anyhow::Result<()> {
        let db = Arc::new(gunbatte_node::db::Db::open(&cfg.db_path)?);
        std::fs::create_dir_all(&cfg.replay_dir).ok();
        sweep_replays(&cfg.replay_dir, cfg.max_replays);
        // Issue #38: cross-site WebSocket hijacking. Browsers announce which
        // page opened the socket; a hostile page in another tab must not be
        // able to open the bot or spectate sockets in a player's name. The
        // allowlist comes from the environment (apply.sh renders the game
        // host into the unit); unset = check disabled (local dev, tests).
        // Clients with NO Origin header — homemade bots and scripts — are
        // never browsers and always allowed.
        let allowed_origins = std::env::var("GUNBATTE_ALLOWED_ORIGINS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let (spectate_tx, _) = broadcast::channel(1024);
        let permits = if cfg.max_connections == 0 {
            usize::MAX
        } else {
            cfg.max_connections
        };
        let server = Arc::new(Server {
            lane_count: Arc::new(tokio::sync::Semaphore::new(cfg.lanes)),
            conn_permits: Arc::new(tokio::sync::Semaphore::new(permits)),
            join_bucket: std::sync::Mutex::new(TokenBucket::new(cfg.join_attempts_per_min)),
            new_name_bucket: std::sync::Mutex::new(TokenBucket::new(cfg.new_names_per_min)),
            live_names: std::sync::Mutex::new(std::collections::HashMap::new()),
            cfg,
            db,
            config,
            host,
            allowed_origins,
            lobby: Arc::new(Mutex::new(Vec::new())),
            lobbies: Arc::new(Mutex::new(HashMap::new())),
            lobby_seq: Arc::new(AtomicU64::new(0)),
            spectate_tx,
        });

        // Scheduler: draft matches between idle bots.
        {
            let server = server.clone();
            tokio::spawn(async move {
                loop {
                    server.schedule_tick().await;
                    tokio::time::sleep(Duration::from_millis(2000)).await;
                }
            });
        }

        let net = server.clone();
        start_axum(net).await
    }

    /// Called every 2s: draft a match between idle bots when a lane is free.
    /// Slain-the-Boss raids are drafted first: any queued boss-mode entrant
    /// (or a registered boss AI) starts a raid, topped up with house bots.
    /// Members of a private lobby are never drafted here — their host starts
    /// the match by hand (`start_lobby`).
    async fn schedule_tick(self: &Arc<Self>) {
        if self.lane_count.available_permits() == 0 {
            return;
        }
        let mut lobby = self.lobby.lock().await;
        // Private rooms are invisible to matchmaking: the host decides when
        // (and with whom) the match starts.
        lobby.retain(|h| h.lobby_code().is_none());
        lobby.retain(|h| h.entrant().connected.load(Ordering::Relaxed));
        let raid_queued = lobby.iter().any(|h| h.mode == GameMode::Boss || h.wants_boss);
        let raid = if raid_queued {
            self.draft_boss_raid(&mut lobby)
        } else {
            None
        };
        if let Some((drafted, config)) = raid {
            drop(lobby);
            self.spawn_match(drafted, config).await;
            return;
        }
        // A queued human wants a match NOW — house bots make up the numbers,
        // so solo play never waits for other bots to connect (README "play live").
        let has_human = lobby.iter().any(|h| h.human);
        if lobby.is_empty() || (lobby.len() < self.cfg.min_bots && !has_human) {
            return;
        }
        // ELO-proximity drafting (PLAN §8.2): sort by elo, take up to 8.
        lobby.sort_by_key(|h| self.db.elo_of(&h.name));
        let take = lobby.len().min(8);
        let mut drafted: Vec<Arc<BotHandle>> = lobby.drain(..take).collect();
        drop(lobby);
        if has_human {
            let want = 8usize.min(drafted.len() + self.cfg.house_bots);
            while drafted.len() < want {
                let brain = house::HOUSE_ROSTER[drafted.len() % house::HOUSE_ROSTER.len()];
                drafted.push(house::spawn(&self.db, brain));
            }
        }

        let config = self.config.clone();
        self.spawn_match(drafted, config).await;
    }

    /// Take a lane permit, hand the roster to the match host, and when the
    /// match is over, return connected non-house members to the public queue.
    /// (The requeue used to sit at the end of the game loop; it is matchmaking
    /// work and moved here in the lobby/game-server split.)
    async fn spawn_match(self: &Arc<Self>, drafted: Vec<Arc<BotHandle>>, config: MatchConfig) {
        let permit = self.lane_count.clone().acquire_owned().await.unwrap();
        // House-filled matches are sparring (issue #37): matchmaking knows it
        // topped the roster up, the game role only honors the verdict.
        let rated = !drafted.iter().any(|h| h.house);
        let ctx = MatchContext {
            db: self.db.clone(),
            replay_dir: self.cfg.replay_dir.clone(),
            spectate: self.spectate_tx.clone(),
            rated,
        };
        let entrants: Vec<MatchEntrant> = drafted.iter().map(|h| h.entrant()).collect();
        let fut = self.host.host_match(ctx, entrants, config);
        let server = self.clone();
        tokio::spawn(async move {
            fut.await;
            for h in &drafted {
                if h.entrant().connected.load(Ordering::Relaxed) && !h.house {
                    // The room is consumed by its match: members go back to
                    // the public queue (a fresh lobby is a fresh code).
                    h.set_lobby(None);
                    server.lobby.lock().await.push(h.clone());
                }
            }
            drop(permit);
        });
    }

    /// Generate a share code nobody is using: 4 chars from a no-lookalike
    /// alphabet ("K7QP"), retried on the (vanishingly rare) collision.
    async fn new_lobby_code(&self) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        loop {
            let n = self.lobby_seq.fetch_add(1, Ordering::Relaxed);
            let mut seed = n
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(7);
            seed |= 1;
            let code: String = (0..4)
                .map(|_| {
                    // xorshift: cheap, deterministic given the seed, no rand dep.
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ALPHABET[(seed % ALPHABET.len() as u64) as usize] as char
                })
                .collect();
            if !self.lobbies.lock().await.contains_key(&code) {
                return code;
            }
        }
    }

    /// Register a socket into either the public queue or a private lobby.
    /// Lobby membership is decided here, atomically with the queue push, so a
    /// lobby-bound entrant is never visible to the public scheduler.
    async fn admit(
        self: &Arc<Self>,
        handle: Arc<BotHandle>,
        intent: LobbyIntent,
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ) -> anyhow::Result<()> {
        let mode = handle.mode;
        let roster_msg = |lobby: &Lobby, you: &str| {
            json!({
                "type": "lobby_joined",
                "lobby": lobby.code,
                "host": lobby.host.name,
                "you": you,
                "mode": if lobby.mode == GameMode::Boss { "boss" } else { "royale" },
                "members": lobby.members.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
            })
            .to_string()
        };
        match intent {
            LobbyIntent::None => {
                if handle.wants_boss {
                    let _ = ws_tx
                        .send(tmsg(
                            json!({"type":"queued_as_boss","hint":"waiting for a raid host"})
                                .to_string(),
                        ))
                        .await;
                }
                self.lobby.lock().await.push(handle);
                Ok(())
            }
            LobbyIntent::Create { boss } => {
                // Live-room ceiling (issue #37): rooms are consumed by their
                // match, so this bounds matchmaking bookkeeping, not play.
                if self.cfg.max_lobbies > 0 && self.lobbies.lock().await.len() >= self.cfg.max_lobbies
                {
                    let _ = ws_tx
                        .send(tmsg(
                            json!({"type":"error","error":"lobby limit reached, try later"})
                                .to_string(),
                        ))
                        .await;
                    return Ok(());
                }
                let code = self.new_lobby_code().await;
                handle.set_lobby(Some(code.clone()));
                let lobby = Lobby {
                    code: code.clone(),
                    host: handle.clone(),
                    mode,
                    boss_raid: boss,
                    members: vec![handle.clone()],
                };
                let msg = roster_msg(&lobby, &handle.name);
                self.lobbies.lock().await.insert(code.clone(), lobby);
                println!("⇄ lobby {code} created by {}", handle.name);
                let _ = ws_tx.send(tmsg(msg)).await;
                Ok(())
            }
            LobbyIntent::Join { code } => {
                let code = code.trim().to_uppercase();
                let mut lobbies = self.lobbies.lock().await;
                let Some(lobby) = lobbies.get_mut(&code) else {
                    drop(lobbies);
                    // Wrong codes draw from a global bucket (issue #37): one
                    // join per connection otherwise lets reconnect churn
                    // brute-force the ~1M room-code space at wire speed.
                    if !self.join_bucket.lock().unwrap().take() {
                        println!(
                            "lobby_join_failed name={} code=\"{}\" reason=join_bucket",
                            log_safe(&handle.name),
                            log_safe(&code)
                        );
                        let _ = ws_tx
                            .send(tmsg(
                                json!({"type":"error","error":"too many join attempts, slow down"})
                                    .to_string(),
                            ))
                            .await;
                    } else {
                        println!(
                            "lobby_join_failed name={} code=\"{}\" reason=no_such_lobby",
                            log_safe(&handle.name),
                            log_safe(&code)
                        );
                        let _ = ws_tx
                            .send(tmsg(
                                json!({"type":"error","error":"no such lobby"}).to_string(),
                            ))
                            .await;
                    }
                    return Ok(());
                };
                let cap = if lobby.boss_raid {
                    self.config.boss.raid_size.max(2) as usize
                } else {
                    HOUSE_MATCH_MAX
                };
                if lobby.members.len() >= cap {
                    let n = lobby.members.len();
                    drop(lobbies);
                    println!(
                        "lobby_join_failed name={} code={} reason=lobby_full",
                        handle.name, code
                    );
                    let _ = ws_tx
                        .send(tmsg(
                            json!({"type":"error","error":format!("lobby is full ({n}/{cap})")})
                                .to_string(),
                        ))
                        .await;
                    return Ok(());
                }
                handle.set_lobby(Some(code.clone()));
                let names: Vec<String> = lobby
                    .members
                    .iter()
                    .map(|m| m.name.clone())
                    .chain(std::iter::once(handle.name.clone()))
                    .collect();
                lobby.members.push(handle.clone());
                let msg = json!({
                    "type": "lobby_joined",
                    "lobby": code,
                    "host": lobby.host.name,
                    "you": handle.name,
                    "mode": if lobby.mode == GameMode::Boss { "boss" } else { "royale" },
                    "members": names,
                })
                .to_string();
                let host_msg = json!({
                    "type": "lobby_roster",
                    "lobby": code,
                    "host": lobby.host.name,
                    "mode": if lobby.mode == GameMode::Boss { "boss" } else { "royale" },
                    "members": names,
                })
                .to_string();
                // Everyone already in the room hears the new roster; the joiner
                // has `lobby_joined` (which carries the same roster).
                let others: Vec<Arc<BotHandle>> = lobby.members[..lobby.members.len() - 1].to_vec();
                drop(lobbies);
                let _ = ws_tx.send(tmsg(msg)).await;
                for m in others {
                    let _ = m.out_tx.send(host_msg.clone()).await;
                }
                println!("⇄ lobby {code}: {} joined ({names:?})", handle.name);
                Ok(())
            }
        }
    }

    /// The lobby host hits start: the room's members become the match roster —
    /// house bots fill it up to `fill` (solo testing) and, in a boss lobby, the
    /// boss is the member the host named (`boss: "boss"`/a name) or the
    /// built-in brain. The room is consumed: one lobby = one match.
    async fn start_lobby(
        self: &Arc<Self>,
        host: &Arc<BotHandle>,
        fill: Option<usize>,
        boss_name: Option<String>,
    ) -> Result<(), String> {
        let code = host.lobby_code().ok_or("you are not in a lobby")?;
        let mut lobbies = self.lobbies.lock().await;
        let Some(lobby) = lobbies.remove(&code) else {
            return Err("lobby is gone".into());
        };
        // Host authorization is handle identity, not a name string (issue
        // #37): a second socket registering the same name shares one ladder
        // row (name-UNIQUE upsert), so only ptr_eq — the same primitive the
        // disconnect path uses — can tell the creator from a spoofer.
        if !Arc::ptr_eq(&lobby.host, host) {
            // Put the room back — only its creator starts the match.
            lobbies.insert(code, lobby);
            return Err("only the host can start".into());
        }
        if lobby.members.len() < 2 && self.cfg.house_bots == 0 {
            // The engine needs two entrants; with house fill off, one is not a match.
            lobbies.insert(code, lobby);
            return Err("need at least 2 players".into());
        }
        drop(lobbies);

        let mut drafted: Vec<Arc<BotHandle>> = lobby
            .members
            .iter()
            .filter(|m| m.entrant().connected.load(Ordering::Relaxed))
            .cloned()
            .collect();
        if !drafted.iter().any(|m| Arc::ptr_eq(m, host)) {
            // Put the room back: a failed start must never strand the
            // members outside the public queue (their lobby binding is
            // what the scheduler filters on).
            self.lobbies.lock().await.insert(code, lobby);
            return Err("host is disconnected".into());
        }
        if drafted.len() < 2 && self.cfg.house_bots == 0 {
            self.lobbies.lock().await.insert(code, lobby);
            return Err("need at least 2 players".into());
        }

        let config = MatchConfig {
            mode: lobby.mode,
            ..self.config.clone()
        };

        if lobby.mode == GameMode::Boss {
            let raid_size = config.boss.raid_size.max(2) as usize;
            // Who leads the raid: the host's pick, else a boss-mode member,
            // else the built-in brain. `"ai"` forces the built-in brain.
            let boss_name = boss_name.unwrap_or_default();
            let boss_idx = if boss_name.eq_ignore_ascii_case("ai") {
                None
            } else if boss_name.is_empty() || boss_name.eq_ignore_ascii_case("boss") {
                // No pick (or "boss"): a member who claimed the role, else brain.
                drafted.iter().position(|m| m.wants_boss)
            } else {
                drafted
                    .iter()
                    .position(|m| m.name.eq_ignore_ascii_case(&boss_name))
            };
            let boss = match boss_idx {
                Some(i) => drafted.remove(i),
                None => house::spawn_boss(&self.db),
            };
            while drafted.len() < raid_size - 1 {
                let brain = house::HOUSE_ROSTER[drafted.len() % house::HOUSE_ROSTER.len()];
                drafted.push(house::spawn(&self.db, brain));
            }
            drafted.truncate(raid_size - 1);
            drafted.push(boss);
            let msg = json!({"type":"lobby_started","lobby":code}).to_string();
            for m in lobby.members.iter() {
                let _ = m.out_tx.send(msg.clone()).await;
            }
            println!(
                "▶ lobby {code} starting raid: {} (boss: {})",
                drafted.len(),
                drafted.last().map(|h| h.name.clone()).unwrap_or_default()
            );
            self.spawn_match(drafted, config).await;
            return Ok(());
        }

        // Royale lobby: fill to `fill` (default: house-filled 8, 0 disables).
        let want = fill
            .unwrap_or(8)
            .clamp(drafted.len(), HOUSE_MATCH_MAX)
            .min(drafted.len() + self.cfg.house_bots);
        while drafted.len() < want {
            let brain = house::HOUSE_ROSTER[drafted.len() % house::HOUSE_ROSTER.len()];
            drafted.push(house::spawn(&self.db, brain));
        }
        let msg = json!({"type":"lobby_started","lobby":code}).to_string();
        for m in lobby.members.iter() {
            let _ = m.out_tx.send(msg.clone()).await;
        }
        println!("▶ lobby {code} starting royale: {} entrants", drafted.len());
        self.spawn_match(drafted, config).await;
        Ok(())
    }

    /// Draft one Slain-the-Boss raid from the queue: raiders = boss-mode
    /// entrants topped up to `raid_size` with house bots, boss slot = a
    /// registered boss AI if one queued, else the built-in boss brain.
    fn draft_boss_raid(
        &self,
        lobby: &mut Vec<Arc<BotHandle>>,
    ) -> Option<(Vec<Arc<BotHandle>>, MatchConfig)> {
        let raid_size = self.config.boss.raid_size.max(2) as usize;
        // The raiders (boss-mode queuers; a registered boss takes no raider slot).
        let mut drafted: Vec<Arc<BotHandle>> = lobby
            .iter()
            .position(|h| h.wants_boss)
            .map(|bi| {
                lobby
                    .iter()
                    .enumerate()
                    .filter(|(i, h)| *i != bi && h.mode == GameMode::Boss)
                    .map(|(_, h)| h.clone())
                    .collect()
            })
            .unwrap_or_else(|| {
                lobby
                    .iter()
                    .filter(|h| h.mode == GameMode::Boss)
                    .cloned()
                    .collect()
            });
        drafted.truncate(raid_size - 1);
        // The boss: a registered boss AI, else the built-in brain.
        let boss_handle = match lobby.iter().position(|h| h.wants_boss) {
            Some(bi) => lobby.remove(bi),
            None => house::spawn_boss(&self.db),
        };
        lobby.retain(|h| h.mode != GameMode::Boss);

        while drafted.len() < raid_size - 1 {
            let brain = house::HOUSE_ROSTER[drafted.len() % house::HOUSE_ROSTER.len()];
            drafted.push(house::spawn(&self.db, brain));
        }
        // The boss entrant is always last (that slot becomes UnitKind::Boss).
        drafted.push(boss_handle);

        let config = MatchConfig {
            mode: GameMode::Boss,
            ..self.config.clone()
        };
        Some((drafted, config))
    }
}

async fn start_axum(server: Arc<Server>) -> anyhow::Result<()> {
    use axum::routing::get;
    use tower_http::services::{ServeDir, ServeFile};

    let replays_dir = server.cfg.replay_dir.clone();
    let replays_list = replays_dir.clone();

    let mut app = axum::Router::new()
        .route("/ladder", get(ladder_page))
        .route("/ws/bot", get(ws_bot_handler))
        .route("/ws/spectate", get(ws_spectate_handler))
        .route(
            "/api/standings",
            get(|State(s): State<Arc<Server>>| async move {
                axum::Json(s.db.standings()).into_response()
            }),
        )
        .route(
            "/api/map/{id}",
            get(|AxPath(id): AxPath<String>| async move {
                match gunbatte_core::map::load_map(&id) {
                    Some(m) => axum::Json(m.to_wire()).into_response(),
                    None => "not found".into_response(),
                }
            }),
        )
        .route(
            "/api/matches",
            get(|State(s): State<Arc<Server>>| async move {
                axum::Json(s.db.recent_matches(50)).into_response()
            }),
        )
        .route(
            "/api/replays",
            get(move || async move {
                let mut items = vec![];
                if let Ok(rd) = std::fs::read_dir(&replays_list) {
                    for entry in rd.flatten() {
                        let p = entry.path();
                        if p.extension().is_some_and(|e| e == "json") {
                            let name = p
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                            items.push(json!({
                                "name": name,
                                "url": format!("/replays/{name}"),
                                "size_kb": size as f64 / 1024.0,
                            }));
                        }
                    }
                }
                items.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
                axum::Json(items).into_response()
            }),
        )
        .nest_service(
            "/replays",
            ServeDir::new(&replays_dir).append_index_html_on_directories(false),
        )
        // Slowloris backstop (issue #37): a 30s ceiling on every dynamic
        // request. Deliberately applied BEFORE /replays is nested so replay
        // downloads — tens of MB over a slow link is legitimate — stay
        // exempt; WS upgrades are safe under it (the response completes at
        // the 101, the upgraded socket outlives the request). tower-http's
        // TimeoutLayer answers 408 instead of erroring, as axum requires.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ));

    if let Some(viewer) = &server.cfg.viewer_dir {
        let index = viewer.join("index.html");
        if index.exists() {
            app = app
                .fallback_service(ServeDir::new(viewer).not_found_service(ServeFile::new(index)));
        }
    }

    let app = app
        .with_state(server.clone())
        // One global in-flight ceiling (issue #37) — cheap insurance if the
        // server is ever exposed without the nginx limits; replay downloads
        // are included here on purpose (disk IO is exactly what needs a cap).
        .layer(tower::limit::ConcurrencyLimitLayer::new(HTTP_CONCURRENCY));

    let listener = tokio::net::TcpListener::bind((server.cfg.bind.as_str(), server.cfg.port)).await?;
    println!(
        "▶ GUNBATTE ROYALE ladder server on http://{}:{} (bots: /ws/bot, spectate: /ws/spectate)",
        server.cfg.bind, server.cfg.port
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ladder_page(State(s): State<Arc<Server>>) -> impl IntoResponse {
    page::ladder_html(&s.db)
}

/// Issue #38: cross-site WebSocket hijacking guard. A browser always sends
/// `Origin` on WebSocket handshakes, so when the allowlist is configured, a
/// connection claiming a foreign page is refused. No Origin header means
/// the client is not a browser (homemade bots, scripts) and is allowed; an
/// empty allowlist disables the check entirely (local dev, tests).
fn origin_allowed(allowed: &[String], headers: &HeaderMap) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    allowed
        .iter()
        .any(|a| a.eq_ignore_ascii_case(origin.trim_matches('/')))
}

/// Per-message inbound cap on both upgrades (issue #41): the largest legal
/// message is a mind-cam submission (~4 KiB belief, ~17 KiB as JSON), so 64
/// KiB leaves headroom without letting a bot stream 64 MiB frames at axum's
/// default. An oversize frame errors the stream → the reader breaks → the
/// normal disconnect path (momentum, then forfeit) takes over.
const WS_MAX_MESSAGE_BYTES: usize = 64 * 1024;

async fn ws_bot_handler(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if !origin_allowed(&server.allowed_origins, &headers) {
        println!("▶ ws: rejected /ws/bot handshake with foreign Origin");
        return (axum::http::StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    // Connection ceiling (issue #37): hold a permit for the life of the
    // socket — bots and spectators draw from the same pool. A full pool
    // refuses the upgrade outright instead of spawning more tasks.
    let Ok(permit) = server.conn_permits.clone().try_acquire_owned() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "connection limit reached",
        )
            .into_response();
    };
    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .max_frame_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| on_bot_socket(server, socket, permit))
}

async fn ws_spectate_handler(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if !origin_allowed(&server.allowed_origins, &headers) {
        println!("▶ ws: rejected /ws/spectate handshake with foreign Origin");
        return (axum::http::StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let Ok(permit) = server.conn_permits.clone().try_acquire_owned() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "connection limit reached",
        )
            .into_response();
    };
    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .max_frame_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| on_spectate_socket(server, socket, permit))
}

async fn on_bot_socket(
    server: Arc<Server>,
    ws: WebSocket,
    _conn_permit: tokio::sync::OwnedSemaphorePermit,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    // Handshake: first message must be a register.
    let first = tokio::time::timeout(Duration::from_secs(10), ws_rx.next()).await;
    let reg: RegisterMsg = match first {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str(&t),
        _ => return,
    }
    .unwrap_or_else(|_| RegisterMsg {
        _type: String::new(),
        name: String::new(),
        token: String::new(),
        decision_rate: 1,
        auto_heel: false,
        human: false,
        mode: String::new(),
        boss: false,
        lobby_action: String::new(),
        lobby: String::new(),
        rated: false,
    });
    if !valid_name(&reg.name) {
        println!(
            "register_refused name=\"{}\" reason=invalid_name",
            log_safe(&reg.name)
        );
        let _ = ws_tx
            .send(tmsg(
                json!({"type":"error","error":"invalid name"}).to_string(),
            ))
            .await;
        return;
    }
    // First-sight names draw from a global bucket (issue #37): cycling
    // unique names at connection speed otherwise inserts unbounded ladder
    // rows before any queue admission. Known names — every reconnect, and
    // house bots (they register server-side) — bypass the bucket.
    let known = server.db.bot_exists(&reg.name);
    if !known && !server.new_name_bucket.lock().unwrap().take() {
        println!("register_refused name={} reason=name_bucket", reg.name);
        let _ = ws_tx
            .send(tmsg(
                json!({"type":"error","error":"too many new bots, slow down"}).to_string(),
            ))
            .await;
        return;
    }
    if !server.db.verify_token(&reg.name, &reg.token) {
        println!("register_refused name={} reason=bad_token", reg.name);
        let _ = ws_tx
            .send(tmsg(
                json!({"type":"error","error":"bad token"}).to_string(),
            ))
            .await;
        return;
    }
    // One live connection per name (issue #42): two same-name entrants in
    // one match would double-count the name's result. But a registration
    // that got here has already presented the name's correct token — and a
    // tokened name has an owner: it evicts the old connection (latest
    // wins) instead of being refused, so a hung tab or a half-open socket
    // can never lock the owner out of their own name. A tokenless (casual)
    // row has no secret to verify, so it keeps the strict refusal —
    // first-come is all the protection a disposable name has. The atomic
    // map insert is the check; the name frees when this socket tears down.
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let (in_tx, in_rx) = mpsc::channel::<BotMsg>(64);
    let connected = Arc::new(AtomicBool::new(true));
    let (closed_tx, mut closed_rx) = tokio::sync::watch::channel(false);
    let slot = Arc::new(LiveSlot {
        out: out_tx.clone(),
        close: closed_tx.clone(),
    });
    // Decide under one lock (get + insert atomic against rival
    // registrations); the notice is sent after the lock is released.
    enum Dup {
        None,
        Owner(Arc<LiveSlot>),
        Locked,
    }
    let dup: Dup = {
        let mut live = server.live_names.lock().unwrap();
        match live.get(&reg.name) {
            Some(existing) if server.db.has_token(&reg.name) => {
                let ev = Dup::Owner(existing.clone());
                live.insert(reg.name.clone(), slot.clone());
                ev
            }
            Some(_) => Dup::Locked,
            None => {
                live.insert(reg.name.clone(), slot.clone());
                Dup::None
            }
        }
    };
    match dup {
        Dup::Owner(existing) => {
            println!("register_evicted name={} (newer connection)", reg.name);
            let _ = existing
                .out
                .send(
                    json!({"type":"error","error":"this name was just opened in a newer connection"})
                        .to_string(),
                )
                .await;
            let _ = existing.close.send(true);
        }
        Dup::Locked => {
            // A live tokenless duplicate: the strict refusal stands.
            println!("register_refused name={} reason=already_connected", reg.name);
            let _ = ws_tx
                .send(tmsg(
                    json!({"type":"error","error":"already connected"}).to_string(),
                ))
                .await;
            return;
        }
        Dup::None => {}
    }
    // Tier and secret (issue #42): known names keep their row's tier — a
    // rated row enrolls even on a tokenless reconnect, which is how every
    // pre-existing ladder identity gets claimed and protected. Brand-new
    // names are casual unless the register asked for the ladder; rated
    // enrollment gets a server-issued secret in the ack, never a
    // client-chosen one.
    let rated = reg.rated || server.db.is_rated(&reg.name);
    let issued = if rated && reg.token.is_empty() {
        Some(new_token())
    } else {
        None
    };
    let eff_token = issued.as_deref().unwrap_or(&reg.token);
    let db_id = server.db.register_bot(&reg.name, eff_token, rated).unwrap_or(0);

    let connected2 = connected.clone();
    let mode = if reg.mode.eq_ignore_ascii_case("boss") {
        GameMode::Boss
    } else {
        GameMode::Royale
    };
    // Lobby intent decides queue vs private room: "boss" as a *lobby* action
    // means "make this a raid" (the host is its first raider, not the boss).
    let intent = match reg.lobby_action.as_str() {
        "create" => LobbyIntent::Create {
            boss: mode == GameMode::Boss,
        },
        "join" => LobbyIntent::Join {
            code: reg.lobby.clone(),
        },
        _ => LobbyIntent::None,
    };
    let handle = Arc::new(BotHandle {
        name: reg.name.clone(),
        db_id,
        decision_rate: reg.decision_rate.clamp(1, 10),
        // Auto-heel is opt-in per bot at registration (the companion AI
        // would otherwise overwrite the bot's own companion commands).
        auto_heel: reg.auto_heel,
        human: reg.human,
        house: false,
        mode,
        wants_boss: reg.boss,
        lobby: std::sync::Mutex::new(None),
        connected: connected.clone(),
        out_tx: out_tx.clone(),
        in_rx: Arc::new(Mutex::new(in_rx)),
    });

    let mut ack = json!({
        "type":"registered",
        "you": reg.name,
        "deadline_ms": server.config.deadline_ms,
        "rated": rated,
    });
    if let Some(t) = issued {
        ack["token"] = json!(t);
    }
    let _ = ws_tx.send(tmsg(ack.to_string())).await;
    if let Err(e) = server.admit(handle.clone(), intent, &mut ws_tx).await {
        println!("⚠ admit failed for {}: {e}", reg.name);
    }
    println!(
        "⇄ bot connected: {} (queue: {})",
        reg.name,
        server.lobby.lock().await.len()
    );

    // The host's lobby commands arrive on the same socket as its inputs.
    let (lobby_tx, mut lobby_rx) = mpsc::channel::<LobbyCmd>(8);

    // Keepalive: the writer pings on an interval and the reader closes the
    // socket after `ws_idle_timeout_s` of total silence, so a half-open TCP
    // connection (peer vanished without FIN) cannot linger forever. Any
    // traffic — inputs or pongs — resets the clock. The closed watch channel
    // itself is created up at registration, where the eviction path shares
    // it to break a replaced connection's writer.
    let idle_window = if server.cfg.ws_idle_timeout_s == 0 {
        KEEPALIVE_OFF
    } else {
        Duration::from_secs(server.cfg.ws_idle_timeout_s)
    };

    // Reader: bot → server inputs (+ lobby_* control messages).
    let closed_reader = closed_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            let next = tokio::time::timeout(idle_window, ws_rx.next()).await;
            let msg = match next {
                Ok(Some(Ok(m))) => m,
                // Idle past the keepalive window, transport error, or EOF.
                _ => break,
            };
            match msg {
                Message::Text(t) => {
                    // One parse per ordinary action (issue #41): the byte
                    // probe skips the control check for everything that
                    // can't be a lobby command. Only texts whose parsed
                    // `type` really is lobby_start route to the lobby —
                    // trying ClientAction first is not an option, every
                    // BotInput field defaults, so a control message would
                    // also parse as a legal all-zero action.
                    if t.contains("\"lobby_start\"") {
                        let is_control = serde_json::from_str::<serde_json::Value>(&t)
                            .ok()
                            .and_then(|v| v["type"].as_str().map(str::to_string))
                            .as_deref()
                            == Some("lobby_start");
                        if is_control {
                            if let Ok(cmd) = serde_json::from_str::<LobbyCmd>(&t) {
                                if lobby_tx.send(cmd).await.is_err() {
                                    break;
                                }
                            }
                            continue;
                        }
                    }
                    if let Ok(action) = serde_json::from_str::<ClientAction>(&t) {
                        if in_tx
                            .send(BotMsg {
                                client_tick: action.tick,
                                input: action.input,
                                arrived: Instant::now(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {} // Ping/Pong/Binary — pongs prove liveness
            }
        }
        let _ = closed_reader.send(true);
        connected2.store(false, Ordering::Relaxed);
    });

    let host = handle.clone();
    let server2 = server.clone();
    let out_tx2 = out_tx.clone();
    tokio::spawn(async move {
        while let Some(cmd) = lobby_rx.recv().await {
            if cmd.action != "start" {
                continue;
            }
            if let Err(e) = server2
                .start_lobby(&host, cmd.fill, cmd.boss.clone())
                .await
            {
                let _ = out_tx2
                    .send(json!({"type":"error","error":e}).to_string())
                    .await;
            }
        }
    });

    // Writer: server → bot (observations + lifecycle events) + keepalive
    // pings. Exits as soon as the reader side dies, so the socket is fully
    // released and the post-loop cleanup (queue/lobby leave) runs.
    let mut ping = tokio::time::interval(if server.cfg.ws_ping_every_s == 0 {
        KEEPALIVE_OFF
    } else {
        Duration::from_secs(server.cfg.ws_ping_every_s)
    });
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            text = out_rx.recv() => {
                let Some(text) = text else { break };
                if ws_tx.send(tmsg(text)).await.is_err() {
                    break;
                }
            }
            _ = ping.tick() => {
                if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            _ = closed_rx.changed() => {
                // Evicted (or the reader died): flush whatever is already
                // queued before the socket drops — an evicted connection's
                // last word ("replaced by a newer connection") must reach
                // the wire, not lose the select! race against its own
                // close signal.
                while let Ok(text) = out_rx.try_recv() {
                    let _ = ws_tx.send(tmsg(text)).await;
                }
                let _ = ws_tx.send(Message::Close(None)).await;
                break;
            }
        }
    }
    connected.store(false, Ordering::Relaxed);
    reader.abort();
    // Free the name only if this connection still owns it: an evicted
    // connection tears down after its successor registered, and its late
    // cleanup must not erase the successor's slot.
    {
        let mut live = server.live_names.lock().unwrap();
        if live
            .get(&handle.name)
            .map(|s| Arc::ptr_eq(s, &slot))
            .unwrap_or(false)
        {
            live.remove(&handle.name);
        }
    }
    let code = handle.lobby_code();
    server.lobby.lock().await.retain(|h| !Arc::ptr_eq(h, &handle));
    // A disconnecting member leaves the room; an empty room (or the host
    // leaving) closes it so nobody waits on a start that can never come.
    if let Some(code) = code {
        let mut lobbies = server.lobbies.lock().await;
        if let Some(l) = lobbies.get_mut(&code) {
            l.members.retain(|m| !Arc::ptr_eq(m, &handle));
            if Arc::ptr_eq(&l.host, &handle) {
                let msg = json!({"type":"lobby_closed","lobby":code,"reason":"host left"}).to_string();
                let members = l.members.clone();
                for m in &members {
                    m.set_lobby(None);
                    let _ = m.out_tx.send(msg.clone()).await;
                }
                lobbies.remove(&code);
                println!("⇄ lobby {code} closed (host left)");
            } else if let Some(l) = lobbies.get(&code) {
                if l.members.is_empty() {
                    lobbies.remove(&code);
                    println!("⇄ lobby {code} closed (empty)");
                } else {
                    println!("⇄ lobby {code}: {} left", handle.name);
                }
            }
        }
        drop(lobbies);
    }
}

async fn on_spectate_socket(
    server: Arc<Server>,
    ws: WebSocket,
    _conn_permit: tokio::sync::OwnedSemaphorePermit,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut sub = server.spectate_tx.subscribe();
    let delay_ticks = server.cfg.spectate_delay_s * server.config.tick_rate_hz as u64;
    let mut buffer: VecDeque<(u64, String)> = VecDeque::new();

    // Anti-cheat delay (PLAN §6.2): frames stream out `delay` behind live.
    let sender = tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(json) => {
                    let tick = serde_json::from_str::<serde_json::Value>(&json)
                        .ok()
                        .and_then(|v| v["frame"]["tick"].as_u64())
                        .unwrap_or(0);
                    buffer.push_back((tick, json));
                    if let Some(&(front_tick, _)) = buffer.front() {
                        if tick.saturating_sub(front_tick) >= delay_ticks {
                            if let Some((_, out)) = buffer.pop_front() {
                                if ws_tx.send(tmsg(out)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    // Keep the socket's read side alive until the client leaves.
    while let Some(Ok(msg)) = ws_rx.next().await {
        if matches!(msg, Message::Close(_)) {
            break;
        }
    }
    sender.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gunbatte_core::config::MatchConfig;
    use gunbatte_core::engine::MatchEngine;
    use proptest::prelude::*;

    /// Arbitrary JSON: scalars with extreme values, multibyte strings and
    /// keys, arrays and objects nested to a bounded depth — wrong types,
    /// missing fields and junk shapes all fall out naturally.
    fn json_garbage() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            any::<i64>().prop_map(serde_json::Value::from),
            any::<u64>().prop_map(serde_json::Value::from),
            ".*".prop_map(serde_json::Value::from),
            any::<bool>().prop_map(serde_json::Value::from),
        ];
        leaf.prop_recursive(
            5,   // nesting depth
            64,  // total nodes
            10,  // elements per collection
            |inner| {
                prop_oneof![
                    proptest::collection::vec(inner.clone(), 0..10)
                        .prop_map(serde_json::Value::Array),
                    proptest::collection::vec((proptest::string::string_regex(".{0,24}").unwrap(), inner), 0..10).prop_map(
                        |pairs| serde_json::Value::Object(pairs.into_iter().collect())
                    ),
                ]
            },
        )
    }

    /// BotInput-shaped but extreme: full-range `dir` (u16), full-range
    /// `throttle` (Fix = i64), multi-byte strings, missing fields, junk
    /// action objects. Mirrors what a buggy or hostile bot might send.
    fn shaped_action() -> impl Strategy<Value = String> {
        (
            proptest::option::of(any::<u64>()),  // tick
            proptest::option::of(any::<u16>()),  // main.dir
            proptest::option::of(any::<i64>()),  // main throttle (Fix)
            proptest::option::of(json_garbage()), // main.action
            proptest::option::of(".{0,64}"),     // intent shout, any chars
            proptest::option::of(proptest::collection::vec(any::<u8>(), 0..300)), // belief
            any::<bool>(),                       // add a companion too
        )
            .prop_map(|(tick, dir, throttle, action, intent, belief, companion)| {
                let mut mv = serde_json::Map::new();
                if let Some(d) = dir {
                    mv.insert("dir".into(), serde_json::json!(d));
                }
                if let Some(t) = throttle {
                    mv.insert("throttle".into(), serde_json::json!(t));
                }
                let mut main = serde_json::Map::new();
                main.insert("move".into(), serde_json::Value::Object(mv));
                if let Some(a) = action {
                    main.insert("action".into(), a);
                }
                let mut root = serde_json::Map::new();
                if let Some(t) = tick {
                    root.insert("tick".into(), serde_json::json!(t));
                }
                root.insert("main".into(), serde_json::Value::Object(main));
                if let Some(s) = intent {
                    root.insert("intent".into(), serde_json::json!(s));
                }
                if let Some(b) = belief {
                    root.insert("belief".into(), serde_json::json!(b));
                }
                if companion {
                    root.insert(
                        "companion".into(),
                        serde_json::json!({"move": {"dir": u16::MAX, "throttle": i64::MIN}}),
                    );
                }
                serde_json::Value::Object(root).to_string()
            })
    }

    // Issue #38: the P0 truncate panic (#36) was findable by throwing
    // arbitrary input at the gateway — nothing did that systematically.
    // This property keeps that class closed: garbage on the action wire may
    // be dropped at parse or clamped by the engine, but it may never panic
    // and never push a unit outside the world.
    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(256))]
        #[test]
        fn garbage_action_never_panics_or_escapes_the_world(
            payload in prop_oneof![3 => shaped_action(), 7 => json_garbage().prop_map(|v| v.to_string())],
            seed in 0u64..8,
            bot in 0u32..2,
        ) {
            let names = vec!["alpha".to_string(), "beta".to_string()];
            let mut engine = MatchEngine::new(MatchConfig::standard(), seed, &names);
            engine.configure_bot(0, 1, false);
            engine.configure_bot(1, 1, false);
            // Exactly the socket's path (on_bot_socket): parse → submit → tick.
            if let Ok(action) = serde_json::from_str::<ClientAction>(&payload) {
                engine.submit(bot, action.input, 0);
            }
            engine.step_tick();
            for u in &engine.state.units {
                prop_assert!(u.pos.x >= 0 && u.pos.x <= engine.map.size);
                prop_assert!(u.pos.y >= 0 && u.pos.y <= engine.map.size);
            }
        }
    }

    /// Deep nesting must come back as a parse error, never a stack overflow:
    /// serde_json's parser caps recursion at 128 levels. The shape that
    /// reaches the cap is a deep value inside the `#[serde(flatten)]` buffer
    /// — unknown fields are fully parsed before being discarded — or inside
    /// a known field. A bare `{"{" × depth}0{...}` stack errors shallowly
    /// ("key must be a string") and would pass even with no cap at all.
    #[test]
    fn deeply_nested_action_json_is_rejected_not_crashed() {
        for depth in [1usize, 126, 127, 128, 129, 400] {
            let payload = format!(
                r#"{{"tick":0,"junk":{}0{}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            );
            if depth < 127 {
                // Under the cap: buffered, then ignored. Either outcome is
                // fine — the contract is just no stack overflow.
                let _ = serde_json::from_str::<ClientAction>(&payload);
            } else {
                let err = match serde_json::from_str::<ClientAction>(&payload) {
                    Ok(_) => panic!("deep nesting must not parse (depth {depth})"),
                    Err(err) => err,
                };
                assert!(
                    err.to_string().contains("recursion limit"),
                    "depth {depth} errored some other way: {err}"
                );
            }
        }
    }

    /// Issue #38: browsers always announce their page's origin; homemade
    /// bots send no Origin at all. The guard refuses only foreign-page
    /// browsers, and is fully disabled while no allowlist is configured.
    #[test]
    fn origin_guard_rejects_only_foreign_browsers() {
        let mut headers = HeaderMap::new();
        // No allowlist configured: everything passes (dev, tests).
        assert!(origin_allowed(&[], &headers));
        // No Origin header: not a browser — always allowed.
        let allowlist = vec!["https://play.gunbatte.ahaqqu.com".to_string()];
        assert!(origin_allowed(&allowlist, &headers));
        // The configured origin itself passes.
        headers.insert(
            axum::http::header::ORIGIN,
            "https://play.gunbatte.ahaqqu.com".parse().unwrap(),
        );
        assert!(origin_allowed(&allowlist, &headers));
        // Host case is irrelevant, a stray trailing slash is tolerated.
        let mut lax = HeaderMap::new();
        lax.insert(
            axum::http::header::ORIGIN,
            "https://PLAY.GUNBATTE.AHAQQU.COM/".parse().unwrap(),
        );
        assert!(origin_allowed(&allowlist, &lax));
        // A foreign page is refused.
        let mut evil = HeaderMap::new();
        evil.insert(
            axum::http::header::ORIGIN,
            "https://evil.example.net".parse().unwrap(),
        );
        assert!(!origin_allowed(&allowlist, &evil));
    }

    #[test]
    fn names_reject_markup_and_junk() {
        assert!(valid_name("hunter-1"));
        assert!(valid_name("Sleepy Tarsius"));
        assert!(valid_name("my.bot"));
        assert!(valid_name(&"x".repeat(32)));
        assert!(!valid_name(""));
        assert!(!valid_name("<img src=x onerror=alert(1)>"));
        assert!(!valid_name("a<b"));
        assert!(!valid_name("a&b"));
        assert!(!valid_name("\u{1f600}")); // emoji — multi-byte
        assert!(!valid_name(&"x".repeat(33)));
    }

    #[test]
    fn token_bucket_drains_then_refills() {
        let mut b = TokenBucket::new(3);
        assert!(b.take());
        assert!(b.take());
        assert!(b.take());
        assert!(!b.take(), "empty bucket refuses");
        // Refill is continuous: ~3/min means a token after 20s.
        b.last -= Duration::from_secs(20);
        assert!(b.take(), "20s at 3/min accrues one token");
    }

    #[test]
    fn zero_capacity_bucket_is_unlimited() {
        let mut b = TokenBucket::new(0);
        for _ in 0..1000 {
            assert!(b.take(), "0 = no ceiling, never refuses");
        }
    }

    #[test]
    fn replay_sweep_keeps_newest_and_leaves_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let touch = |name: &str| std::fs::write(dir.path().join(name), b"{}").unwrap();
        touch("match-3.json");
        touch("match-9.json");
        touch("match-5.json");
        touch("match-broken.json"); // unparseable sequence: never deleted
        touch("demo8.json"); // hand-placed fixture: never deleted
        touch("notes.txt"); // not a replay: never deleted

        sweep_replays(dir.path(), 2);
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert!(!left.iter().any(|n| n == "match-3.json"), "oldest swept: {left:?}");
        assert!(left.contains(&"match-5.json".to_string()), "{left:?}");
        assert!(left.contains(&"match-9.json".to_string()), "{left:?}");
        assert!(left.contains(&"match-broken.json".to_string()), "{left:?}");
        assert!(left.contains(&"demo8.json".to_string()), "{left:?}");
        assert!(left.contains(&"notes.txt".to_string()), "{left:?}");
    }

    #[test]
    fn replay_sweep_zero_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("match-{i}.json")), b"{}").unwrap();
        }
        sweep_replays(dir.path(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 5);
    }
}
