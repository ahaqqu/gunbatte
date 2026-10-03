//! M3 acceptance (PLAN §9): bots connect over WebSocket, get drafted into a
//! match, play through the 10Hz gateway loop with momentum on misses, and
//! the finished match writes a replay + updates the ladder + ELO.

use gunbatte_core::config::MatchConfig;
use gunbatte_lobby::{Server, ServerConfig};
use gunbatte_server::GameHost;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Drive one bot socket through `wanted` consecutive matches, replying to
/// every observation and staying connected across match_over. Returns each
/// match's match_over payload in order.
async fn play_matches(
    url: &str,
    name: &str,
    rated: bool,
    // given the observation JSON, produce the action JSON
    act: fn(&serde_json::Value) -> Option<serde_json::Value>,
    wanted: usize,
    max_wait: Duration,
) -> Vec<serde_json::Value> {
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("bot connects");
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::Text(
        json!({"type": "register", "name": name, "decision_rate": 1, "rated": rated})
            .to_string(),
    ))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + max_wait;
    let mut overs = Vec::new();
    while overs.len() < wanted {
        let msg = tokio::time::timeout_at(deadline, rx.next()).await;
        let msg = match msg {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let Message::Text(text) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(json!(null));
        match v["type"].as_str() {
            Some("match_over") => overs.push(v),
            Some("registered") | Some("match_start") | Some("error") => continue,
            _ => {
                // Observation: reply with the scripted action.
                if let Some(action) = act(&v) {
                    tx.send(Message::Text(action.to_string())).await.ok();
                }
            }
        }
    }
    assert_eq!(
        overs.len(),
        wanted,
        "bot {name} saw {} of {wanted} match_overs",
        overs.len()
    );
    overs
}

/// Drive one bot socket until the match ends; returns its match_over payload.
async fn play_as_bot(
    url: &str,
    name: &str,
    rated: bool,
    act: fn(&serde_json::Value) -> Option<serde_json::Value>,
    max_wait: Duration,
) -> serde_json::Value {
    play_matches(url, name, rated, act, 1, max_wait)
        .await
        .pop()
        .unwrap()
}

fn idle_action(_obs: &serde_json::Value) -> Option<serde_json::Value> {
    Some(json!({"tick": _obs["tick"], "main": {"move": {"dir": 0, "throttle": 0.0}}}))
}

fn active_action(obs: &serde_json::Value) -> Option<serde_json::Value> {
    // March north and shield occasionally — legal but not smart.
    Some(json!({
        "tick": obs["tick"],
        "main": {"move": {"dir": 0, "throttle": 1.0}},
        "companion": {"move": {"dir": 180, "throttle": 1.0}}
    }))
}

/// Poll the port until the spawned server actually accepts TCP. A fixed nap
/// loses the race on a cold CI runner, where bind can lag the client's first
/// connect attempt (seen as `Connection refused` in the 0.72s CI failure).
async fn wait_until_bound(port: u16) {
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("server on 127.0.0.1:{port} never started accepting");
}

#[tokio::test(flavor = "multi_thread")]
async fn m3_gateway_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let replay_dir = dir.path().join("replays");
    let port = 8931; // test-local port
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: replay_dir.clone(),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    // Give the server a moment to bind.
    wait_until_bound(port).await;

    let url = format!("ws://127.0.0.1:{port}/ws/bot");
    let (a, b) = tokio::join!(
        play_as_bot(&url, "test-alpha", true, active_action, Duration::from_secs(240)),
        play_as_bot(&url, "test-beta", true, idle_action, Duration::from_secs(240)),
    );

    // Match must have completed and reported placements + a replay link.
    assert!(a["place"].as_i64().is_some(), "alpha got {a}");
    assert!(b["place"].as_i64().is_some(), "beta got {b}");
    assert_ne!(a["place"], b["place"], "placements must be distinct");
    let replay_url = a["replay"].as_str().expect("replay url");
    let replay_path = replay_dir.join(replay_url.trim_start_matches("/replays/"));
    assert!(
        replay_path.exists(),
        "replay file written: {}",
        replay_path.display()
    );

    // Replay must verify byte-identically.
    let bytes = std::fs::read(&replay_path).unwrap();
    let replay: gunbatte_core::replay::Replay = serde_json::from_slice(&bytes).unwrap();
    gunbatte_core::replay::verify_replay(&replay).expect("gateway replay re-simulates byte-identically");

    // Ladder must update: standings + matches list via the HTTP API.
    let http = format!("http://127.0.0.1:{port}");
    let body = reqwest_get(&format!("{http}/api/standings")).await;
    assert!(body.contains("test-alpha"), "standings: {body}");
    assert!(body.contains("test-beta"), "standings: {body}");
    // Rated matches move ELO — the contrast for the unrated house-fill test.
    let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
    for name in ["test-alpha", "test-beta"] {
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .expect("standings row");
        assert_ne!(
            row["elo"].as_i64(),
            Some(1000),
            "rated match must move ELO: {row}"
        );
    }
    let body = reqwest_get(&format!("{http}/api/matches")).await;
    assert!(
        body.contains(replay_url.trim_start_matches("/replays/")),
        "matches: {body}"
    );

    // The ladder page itself renders (at /ladder; / is the viewer).
    let body = reqwest_get(&format!("{http}/ladder")).await;
    assert!(body.contains("GUNBATTE<span>★</span>ROYALE"));
    assert!(body.contains("test-alpha"));
}

async fn reqwest_get(url: &str) -> String {
    // Minimal HTTP GET via raw TCP (avoid another dependency).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (host, path) = {
        let rest = url.split_once("//").unwrap().1;
        match rest.split_once('/') {
            Some((h, p)) => (h.to_string(), format!("/{p}")),
            None => (rest.to_string(), "/".to_string()),
        }
    };
    let mut stream = tokio::net::TcpStream::connect(&host).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);
    // Body only: some callers parse the payload as JSON, so the status line
    // and headers (everything before the first blank line) must go.
    match raw.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => raw.to_string(),
    }
}

/// M4.5 acceptance (solo play): a single human registering `human: true` is
/// topped up to a full 8-entrant match with house bots — no other connected
/// bots, no waiting. House bots fight, the match completes, a replay is
/// written, and the human lands on the ladder while house bots stay off it.
#[tokio::test(flavor = "multi_thread")]
async fn solo_human_gets_house_fill() {
    let dir = tempfile::tempdir().unwrap();
    let replay_dir = dir.path().join("replays");
    let port = 8933;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: replay_dir.clone(),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 8,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    // Short match cap so the test doesn't run a full 5-minute BR.
    let mut match_cfg = MatchConfig::standard();
    match_cfg.match_max_s = 25;
    tokio::spawn(async move {
        Server::start(cfg, match_cfg, Arc::new(GameHost::new(3))).await.expect("server");
    });
    wait_until_bound(port).await;

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/bot"))
        .await
        .expect("human connects");
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::Text(
        // The viewer auto-enrolls humans on the ladder (issue #42); this
        // raw socket does the same by hand.
        json!({"type": "register", "name": "solo-human", "decision_rate": 1, "human": true,
               "rated": true})
            .to_string(),
    ))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut entrants: Vec<String> = vec![];
    let mut over: Option<serde_json::Value> = None;
    while tokio::time::Instant::now() < deadline {
        let msg = tokio::time::timeout_at(deadline, rx.next()).await;
        let msg = match msg {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let Message::Text(text) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(json!(null));
        match v["type"].as_str() {
            Some("match_start") => {
                entrants = v["bots"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|b| b.as_str().map(String::from)).collect())
                    .unwrap_or_default();
            }
            Some("match_over") => {
                over = Some(v);
                break;
            }
            Some("registered") | Some("error") => {}
            _ => {
                // Observation: keep moving so the match stays live.
                tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok();
            }
        }
    }
    let over = over.expect("human saw match_over within 90s");

    // The roster was topped up to 8: the human + 7 house bots.
    assert_eq!(entrants.len(), 8, "entrants: {entrants:?}");
    let house = entrants.iter().filter(|n| n.starts_with("house·")).count();
    assert_eq!(house, 7, "entrants: {entrants:?}");
    assert!(entrants.contains(&"solo-human".to_string()));

    // Match completed with a placement and a verifiable replay.
    assert!(over["place"].as_i64().is_some(), "over: {over}");
    let replay_url = over["replay"].as_str().expect("replay url");
    let replay_path = replay_dir.join(replay_url.trim_start_matches("/replays/"));
    assert!(replay_path.exists(), "replay written: {}", replay_path.display());
    let bytes = std::fs::read(&replay_path).unwrap();
    let replay: gunbatte_core::replay::Replay = serde_json::from_slice(&bytes).unwrap();
    gunbatte_core::replay::verify_replay(&replay).expect("house-fill replay verifies byte-identically");

    // Ladder shows the human; house bots stay off the standings.
    let body = reqwest_get(&format!("http://127.0.0.1:{port}/api/standings")).await;
    assert!(body.contains("solo-human"), "standings: {body}");
    assert!(!body.contains("house·"), "house bots must not appear: {body}");
    // And the house-filled match is unrated (issue #37): sparring against
    // scripted bots must not move anyone's rating.
    let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
    let human = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "solo-human")
        .expect("human standings row");
    assert_eq!(
        human["elo"].as_i64(),
        Some(1000),
        "house-filled match must be unrated: {human}"
    );
}

/// Lobby (PLAN extension): a host creates a room, shares the code, an
/// invitee joins, the host starts by hand — and the match drafts exactly the
/// room's roster (topped up with house bots), never the public queue.
/// Works the same for royale and boss; this is the royale half.
#[tokio::test(flavor = "multi_thread")]
async fn lobby_host_and_invitee_play_a_private_royale() {
    let dir = tempfile::tempdir().unwrap();
    let replay_dir = dir.path().join("replays");
    let port = 8935;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: replay_dir.clone(),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 8,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    let mut match_cfg = MatchConfig::standard();
    match_cfg.match_max_s = 25;
    tokio::spawn(async move {
        Server::start(cfg, match_cfg, Arc::new(GameHost::new(3))).await.expect("server");
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let url = format!("ws://127.0.0.1:{port}/ws/bot");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut host_tx, mut host_rx) = ws.split();
    host_tx
        .send(Message::Text(
            json!({"type":"register","name":"lobby-host","decision_rate":1,"human":true,
                   "lobby_action":"create"})
                .to_string(),
        ))
        .await
        .unwrap();

    // Host learns the share code.
    let code = loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), host_rx.next())
            .await
            .expect("host hears back")
            .unwrap()
            .unwrap();
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "lobby_joined" {
            assert_eq!(v["host"], "lobby-host");
            assert_eq!(v["members"].as_array().unwrap().len(), 1);
            break v["lobby"].as_str().unwrap().to_string();
        }
    };
    assert_eq!(code.len(), 4, "share code looks like K7QP: {code}");

    // The public scheduler must not steal a lobby member: a human-flagged
    // solo queuer would normally be house-filled within one 2s pass, so if the
    // host were visible to the queue a match_start would land here.
    let quiet = tokio::time::Instant::now() + Duration::from_millis(4600);
    while tokio::time::Instant::now() < quiet {
        match tokio::time::timeout_at(quiet, host_rx.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_ne!(
                    v["type"], "match_start",
                    "the public queue drafted a lobby member: {v}"
                );
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    // The invitee joins the room by code.
    let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut guest_tx, mut guest_rx) = ws2.split();
    guest_tx
        .send(Message::Text(
            json!({"type":"register","name":"lobby-guest","decision_rate":1,"human":true,
                   "lobby_action":"join","lobby":code})
                .to_string(),
        ))
        .await
        .unwrap();

    let mut host_started = false;
    let mut guest_started = false;
    let mut host_roster = 0usize;
    let mut started_sent = false;
    // Host sees the roster update, then starts the match by hand.
    let started_at = tokio::time::Instant::now();
    while started_at.elapsed() < Duration::from_secs(30) && !(host_started && guest_started) {
        tokio::select! {
            msg = host_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                match v["type"].as_str() {
                    Some("lobby_roster") => {
                        host_roster = v["members"].as_array().unwrap().len();
                        if host_roster >= 2 && !started_sent {
                            // The whole point of a lobby: the host presses start.
                            started_sent = true;
                            host_tx
                                .send(Message::Text(
                                    json!({"type":"lobby_start","action":"start"}).to_string(),
                                ))
                                .await
                                .unwrap();
                        }
                    }
                    Some("match_start") => {
                        host_started = true;
                        let entrants: Vec<String> = v["bots"].as_array().unwrap().iter()
                            .filter_map(|b| b.as_str().map(String::from)).collect();
                        assert!(entrants.contains(&"lobby-host".to_string()));
                        assert!(entrants.contains(&"lobby-guest".to_string()),
                            "the invitee plays: {entrants:?}");
                        assert_eq!(entrants.len(), 8, "house-filled to 8: {entrants:?}");
                        host_tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok();
                    }
                    Some("error") => panic!("host got error: {v}"),
                    _ => { host_tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok(); }
                }
            }
            msg = guest_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "lobby_joined" {
                    assert_eq!(v["lobby"], code);
                    assert_eq!(v["members"].as_array().unwrap().len(), 2);
                } else if v["type"] == "match_start" {
                    guest_started = true;
                }
                guest_tx.send(Message::Text(active_action(&v).unwrap_or(json!({"tick":0})).to_string())).await.ok();
            }
        }
    }
    assert!(host_started && guest_started, "both lobby members entered the match");
    assert_eq!(host_roster, 2, "host saw the invitee in the roster");

    // Manual start actually plays the match out: both get placements.
    let mut host_over: Option<serde_json::Value> = None;
    let mut guest_over: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while tokio::time::Instant::now() < deadline && (host_over.is_none() || guest_over.is_none()) {
        tokio::select! {
            msg = host_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "match_over" { host_over = Some(v); }
                else if v["type"].is_null() { host_tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok(); }
            }
            msg = guest_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "match_over" { guest_over = Some(v); }
                else if v["type"].is_null() { guest_tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok(); }
            }
        }
    }
    let host_over = host_over.expect("host saw match_over");
    assert!(host_over["place"].as_i64().is_some(), "host: {host_over}");
    let guest_over = guest_over.expect("guest saw match_over");
    assert!(guest_over["place"].as_i64().is_some(), "guest: {guest_over}");

    // The room's match is a real, verifiable replay with mode royale.
    let replay_url = host_over["replay"].as_str().expect("replay url");
    let replay_path = replay_dir.join(replay_url.trim_start_matches("/replays/"));
    let replay: gunbatte_core::replay::Replay =
        serde_json::from_slice(&std::fs::read(&replay_path).unwrap()).unwrap();
    assert_eq!(replay.header.config.mode, gunbatte_core::config::GameMode::Royale);
    gunbatte_core::replay::verify_replay(&replay).expect("lobby replay verifies byte-identically");
}

/// Lobby + Slain the Boss: a boss-lobby casts one member (the host's pick) as
/// the arena boss, fills the raider slots with house bots, and the raid runs
/// to a real, verifiable finish.
#[tokio::test(flavor = "multi_thread")]
async fn boss_lobby_casts_a_member_as_the_boss() {
    let dir = tempfile::tempdir().unwrap();
    let replay_dir = dir.path().join("replays");
    let port = 8936;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: replay_dir.clone(),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 8,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    let mut match_cfg = MatchConfig::standard();
    match_cfg.match_max_s = 40;
    tokio::spawn(async move {
        Server::start(cfg, match_cfg, Arc::new(GameHost::new(3))).await.expect("server");
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let url = format!("ws://127.0.0.1:{port}/ws/bot");
    // Host: a raider who creates the raid room.
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut host_tx, mut host_rx) = ws.split();
    host_tx
        .send(Message::Text(
            json!({"type":"register","name":"raid-leader","decision_rate":1,"human":true,
                   "mode":"boss","lobby_action":"create"})
                .to_string(),
        ))
        .await
        .unwrap();
    let code = loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), host_rx.next())
            .await
            .expect("host hears back")
            .unwrap()
            .unwrap();
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "lobby_joined" {
            assert_eq!(v["mode"], "boss");
            break v["lobby"].as_str().unwrap().to_string();
        }
    };

    // Guest: registers as a raider but claims the boss role in the lobby.
    let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut boss_tx, mut boss_rx) = ws2.split();
    boss_tx
        .send(Message::Text(
            json!({"type":"register","name":"guest-boss","decision_rate":1,"human":true,
                   "mode":"boss","boss":true,"lobby_action":"join","lobby":code})
                .to_string(),
        ))
        .await
        .unwrap();

    // Host starts the raid by hand, casting the guest as the boss.
    let mut waiting = true;
    while waiting {
        let msg = tokio::time::timeout(Duration::from_secs(10), host_rx.next())
            .await
            .expect("host hears back")
            .unwrap()
            .unwrap();
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "lobby_roster" {
            assert_eq!(v["members"].as_array().unwrap().len(), 2);
            host_tx
                .send(Message::Text(json!({"type":"lobby_start","action":"start"}).to_string()))
                .await
                .unwrap();
            waiting = false;
        }
    }

    // Both players see the raid start; the boss role lands on the guest.
    let mut host_role: Option<String> = None;
    let mut boss_role: Option<String> = None;
    let mut host_is_boss = false;
    let mut boss_is_boss = false;
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(20) && (host_role.is_none() || boss_role.is_none()) {
        tokio::select! {
            msg = host_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "match_start" {
                    assert_eq!(v["mode"], "boss");
                    host_is_boss = v["role"] == "boss";
                    host_role = v["role"].as_str().map(String::from);
                    let entrants: Vec<String> = v["bots"].as_array().unwrap().iter()
                        .filter_map(|b| b.as_str().map(String::from)).collect();
                    // The boss is the last entrant (its slot becomes the boss unit).
                    assert_eq!(entrants.last().map(String::as_str), Some("guest-boss"),
                        "cast boss sits in the last slot: {entrants:?}");
                }
                host_tx.send(Message::Text(active_action(&v).unwrap_or(json!({"tick":0})).to_string())).await.ok();
            }
            msg = boss_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "match_start" {
                    boss_is_boss = v["role"] == "boss";
                    boss_role = v["role"].as_str().map(String::from);
                }
                boss_tx.send(Message::Text(active_action(&v).unwrap_or(json!({"tick":0})).to_string())).await.ok();
            }
        }
    }
    assert!(!host_is_boss, "the host raided, not bossed");
    assert!(boss_is_boss, "the guest was cast as the boss");
    assert_eq!(host_role.as_deref(), Some("raider"));
    assert_eq!(boss_role.as_deref(), Some("boss"));

    // Play it out; the boss guest must survive longer than a walkover.
    let mut over: Option<(String, serde_json::Value)> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    while tokio::time::Instant::now() < deadline && over.is_none() {
        tokio::select! {
            msg = host_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "match_over" { over = Some(("raid-leader".into(), v)); }
                else if v["type"].is_null() { host_tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok(); }
            }
            msg = boss_rx.next() => {
                let Some(Ok(m)) = msg else { break };
                // The server keepalive pings: skip non-text frames.
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "match_over" { over = Some(("guest-boss".into(), v)); }
                else if v["type"].is_null() { boss_tx.send(Message::Text(active_action(&v).unwrap().to_string())).await.ok(); }
            }
        }
    }
    let (who, _o) = over.expect("the raid finished");

    // The raid replay verifies and is a boss-mode match.
    let replay_url = _o["replay"].as_str().expect("replay url");
    let replay_path = replay_dir.join(replay_url.trim_start_matches("/replays/"));
    let replay: gunbatte_core::replay::Replay =
        serde_json::from_slice(&std::fs::read(&replay_path).unwrap()).unwrap();
    assert_eq!(replay.header.config.mode, gunbatte_core::config::GameMode::Boss);
    assert!(replay.header.bot_names.last().map(|n| n == "guest-boss").unwrap_or(false),
        "guest-boss was the raid boss: {:?}", replay.header.bot_names);
    let verified = gunbatte_core::replay::verify_replay(&replay).expect("raid replay verifies");
    assert!(verified.ticks > 0, "raid had substance ({who})");
}

/// Keepalive (hardening): a registered bot that goes totally silent — reads
/// nothing, writes nothing — is closed by the server after the idle window
/// instead of lingering forever as a half-open connection. We observe the
/// close through a dup'd raw socket: the WS half would auto-pong (which
/// resets the idle clock), the raw half only reads, so only the server's
/// idle timeout can end the connection.
#[tokio::test(flavor = "multi_thread")]
async fn silent_bot_socket_is_closed_after_idle_window() {
    use tokio::io::AsyncReadExt;

    let dir = tempfile::tempdir().unwrap();
    let port = 8937;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 1,
        ws_idle_timeout_s: 3,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;

    // Connect at the std level so the socket can be dup'd: tokio's TcpStream
    // has no try_clone.
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    sock.connect(&addr.into()).expect("connect");
    let std_stream: std::net::TcpStream = sock.into();
    let probe_std = std_stream.try_clone().expect("dup socket");
    std_stream.set_nonblocking(true).unwrap();
    probe_std.set_nonblocking(true).unwrap();
    let mut probe = tokio::net::TcpStream::from_std(probe_std).expect("probe");
    let tcp = tokio::net::TcpStream::from_std(std_stream).expect("ws stream");

    let (mut ws, _) =
        tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/ws/bot"), tcp)
            .await
            .expect("ws handshake");
    ws.send(Message::Text(
        json!({"type": "register", "name": "silent-bot"}).to_string(),
    ))
    .await
    .unwrap();

    // Consume the `registered` ack (server pings may interleave).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) if t.contains("registered") => break,
            Ok(Some(Ok(_))) => {}
            other => panic!("no registration ack: {other:?}"),
        }
    }
    drop(ws); // release the WS half; the dup'd probe holds the connection

    // Total silence from us: no pong can reach the server, so the idle
    // window (3s, pinged every 1s) is the only way this ends.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 512];
    let mut closed = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, probe.read(&mut buf)).await {
            Err(_) => break,        // ran out of time: never closed
            Ok(Ok(0)) => { closed = true; break; } // EOF — server hung up
            Ok(Err(_)) => { closed = true; break; } // reset — server hung up
            Ok(Ok(_)) => {}          // ping frames, discarded
        }
    }
    assert!(closed, "server never closed the silent socket");
}

/// A bot that registers and then stops reading its socket entirely must not
/// stall the match loop for everyone else: the healthy entrant keeps
/// receiving observations and the match completes. Regression net for the
/// awaited-send lane freeze (the deterministic unit test lives in lib.rs —
/// whether the TCP buffers back up fast enough here is kernel-dependent).
#[tokio::test(flavor = "multi_thread")]
async fn non_reading_bot_cannot_stall_the_match() {
    let dir = tempfile::tempdir().unwrap();
    let replay_dir = dir.path().join("replays");
    let port = 8939;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: replay_dir.clone(),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 1,
        ws_idle_timeout_s: 30,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    let mut match_cfg = MatchConfig::standard();
    match_cfg.match_max_s = 20;
    tokio::spawn(async move {
        Server::start(cfg, match_cfg, Arc::new(GameHost::new(3))).await.expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    // The stalled bot: a 4 KiB receive buffer so its TCP window shuts almost
    // immediately, then it never reads or writes again.
    let stalled = tokio::spawn({
        async move {
            let sock = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )
            .unwrap();
            sock.set_recv_buffer_size(4096).ok();
            let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            sock.connect(&addr.into()).unwrap();
            sock.set_nonblocking(true).unwrap();
            let tcp = tokio::net::TcpStream::from_std(sock.into()).unwrap();
            let (mut ws, _) =
                tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/ws/bot"), tcp)
                    .await
                    .unwrap();
            ws.send(Message::Text(
                json!({"type": "register", "name": "stalled-bot"}).to_string(),
            ))
            .await
            .unwrap();
            // Hold the socket open without ever reading it again.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    // The healthy bot must still get a full match.
    let over = play_as_bot(&url, "healthy-bot", false, idle_action, Duration::from_secs(90)).await;
    assert!(
        over["place"].as_i64().is_some(),
        "healthy bot finished despite the stalled reader: {over}"
    );
    stalled.abort();
}

/// A match consumes its roster; connected bots must land back in the public
/// queue and be drafted again. (The requeue is matchmaking work that moved
/// out of the game loop in the lobby/game-server split — this test is its
/// regression net.)
#[tokio::test(flavor = "multi_thread")]
async fn bots_requeue_after_a_match_and_get_drafted_again() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8941;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    // Short match cap so two full matches don't take a full BR's worth of time.
    let mut match_cfg = MatchConfig::standard();
    match_cfg.match_max_s = 25;
    tokio::spawn(async move {
        Server::start(cfg, match_cfg, Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;

    let url = format!("ws://127.0.0.1:{port}/ws/bot");
    // Both sockets stay open across the first match_over; a second match_over
    // on each is only possible if matchmaking requeued and re-drafted them.
    let (a, b) = tokio::join!(
        play_matches(&url, "requeue-alpha", true, idle_action, 2, Duration::from_secs(180)),
        play_matches(&url, "requeue-beta", true, active_action, 2, Duration::from_secs(180)),
    );
    assert_eq!(a.len(), 2, "alpha matches: {a:?}");
    assert_eq!(b.len(), 2, "beta matches: {b:?}");
    assert!(
        a[1]["place"].as_i64().is_some() && b[1]["place"].as_i64().is_some(),
        "second match must report placements: {a:?} {b:?}"
    );
}

// --- issue #37: identity & resource ceilings ---------------------------------

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;
type WsTx = futures_util::stream::SplitSink<WsStream, Message>;
type WsRx = futures_util::stream::SplitStream<WsStream>;

/// Open a bot socket and register with `payload`. The reply is read via
/// [`reply_of_type`].
async fn connect_and_register(url: &str, payload: serde_json::Value) -> (WsTx, WsRx) {
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connects");
    let (mut tx, rx) = ws.split();
    tx.send(Message::Text(payload.to_string())).await.unwrap();
    (tx, rx)
}

/// Read text frames until one parses to a message whose `type` is in `want`
/// (skipping keepalive pings), or panic.
async fn reply_of_type(rx: &mut WsRx, wait: Duration, want: &[&str]) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = match tokio::time::timeout_at(deadline, rx.next()).await {
            Ok(Some(Ok(m))) => m,
            other => panic!("waiting for {want:?} got {other:?}"),
        };
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap_or(json!(null));
        if v["type"].as_str().is_some_and(|ty| want.contains(&ty)) {
            return v;
        }
    }
}

/// Connection ceiling (issue #37): the socket pool is bounded — a full pool
/// refuses the upgrade outright, and a released permit is reusable.
#[tokio::test(flavor = "multi_thread")]
async fn connection_cap_refuses_overflow_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8943;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 2,
        max_lobbies: 64,
        join_attempts_per_min: 30,
        new_names_per_min: 60,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    // Two sockets fill the pool. No register needed: the permit is taken at
    // upgrade time, which is exactly what bounds task spawn.
    let (ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert!(
        tokio_tungstenite::connect_async(&url).await.is_err(),
        "third socket must be refused while the pool is full"
    );

    // Releasing one socket frees its permit for the next taker.
    drop(ws1);
    let mut reconnected = false;
    for _ in 0..40 {
        if tokio_tungstenite::connect_async(&url).await.is_ok() {
            reconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(reconnected, "released permit must admit a new socket");
    drop(ws2);
}

/// Lobby ceiling (issue #37): live rooms are capped.
#[tokio::test(flavor = "multi_thread")]
async fn lobby_cap_rejects_creation_beyond_max() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8945;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 1,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    let (mut _host_tx, mut host_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"cap-host","decision_rate":1,"lobby_action":"create"}),
    )
    .await;
    let joined = reply_of_type(&mut host_rx, Duration::from_secs(10), &["lobby_joined"]).await;
    assert_eq!(joined["host"], "cap-host");

    // The one live room is taken: a second creation is refused.
    let (_, mut second_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"cap-second","decision_rate":1,"lobby_action":"create"}),
    )
    .await;
    let err = reply_of_type(&mut second_rx, Duration::from_secs(10), &["error"]).await;
    assert!(
        err["error"].as_str().unwrap().contains("lobby limit"),
        "second room must hit the ceiling: {err}"
    );
}

/// One live connection per name (issue #42): a second concurrent socket
/// with an already-connected name is refused at registration — a name can
/// no longer sit in someone's room or double-draft into a match. Host
/// authorization stays handle identity (issue #37): an ordinary member can
/// join and is refused at start, and the room survives both.
#[tokio::test(flavor = "multi_thread")]
async fn same_name_socket_cannot_start_anothers_lobby() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8947;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    let (mut host_tx, mut host_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"spoof-host","decision_rate":1,"lobby_action":"create"}),
    )
    .await;
    let joined = reply_of_type(&mut host_rx, Duration::from_secs(10), &["lobby_joined"]).await;
    let code = joined["lobby"].as_str().unwrap().to_string();

    // The spoofer: same name, knows the code — refused at the door because
    // the name already holds a live connection.
    let (_, mut spoofer_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"spoof-host","decision_rate":1,
               "lobby_action":"join","lobby":code}),
    )
    .await;
    let err = reply_of_type(&mut spoofer_rx, Duration::from_secs(10), &["error"]).await;
    assert_eq!(err["error"], "already connected", "spoofer: {err}");

    // An ordinary member joins and tries to start it: handle identity must
    // refuse and keep the room.
    let (mut guest_tx, mut guest_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"innocent-guest","decision_rate":1,
               "lobby_action":"join","lobby":code}),
    )
    .await;
    let rejoined = reply_of_type(&mut guest_rx, Duration::from_secs(10), &["lobby_joined"]).await;
    assert_eq!(rejoined["lobby"], code, "room must survive the spoof attempt");
    guest_tx
        .send(Message::Text(json!({"type":"lobby_start","action":"start"}).to_string()))
        .await
        .unwrap();
    let err = reply_of_type(&mut guest_rx, Duration::from_secs(10), &["error"]).await;
    assert_eq!(err["error"], "only the host can start", "guest: {err}");

    // The room is healthy: the host starts it and both are drafted.
    host_tx
        .send(Message::Text(json!({"type":"lobby_start","action":"start"}).to_string()))
        .await
        .unwrap();
    let _ = reply_of_type(&mut host_rx, Duration::from_secs(20), &["match_start"]).await;
    let _ = reply_of_type(&mut guest_rx, Duration::from_secs(20), &["match_start"]).await;
}

/// Join throttle (issue #37): wrong codes draw from a global bucket, so
/// reconnect churn cannot brute-force the room-code space.
#[tokio::test(flavor = "multi_thread")]
async fn join_brute_force_is_throttled() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8949;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 2,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    for attempt in 1..=3 {
        let payload = json!({"type":"register","name":"joiner","decision_rate":1,
                             "lobby_action":"join","lobby":"ZZZZ"});
        let (_, mut rx) = connect_and_register(&url, payload).await;
        let err = reply_of_type(&mut rx, Duration::from_secs(10), &["error"]).await;
        let msg = err["error"].as_str().unwrap();
        if attempt <= 2 {
            assert_eq!(msg, "no such lobby", "attempt {attempt}: {err}");
        } else {
            assert_eq!(
                msg,
                "too many join attempts, slow down",
                "attempt {attempt} must hit the bucket: {err}"
            );
        }
    }
}

/// Registration ceiling (issue #37): first-time names draw from a global
/// bucket (cycling unique names cannot mint unbounded ladder rows); a known
/// name reconnecting bypasses it.
#[tokio::test(flavor = "multi_thread")]
async fn new_name_registration_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8951;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 1,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    // The first fresh name takes the bucket's one token and registers.
    let (_, mut rx1) = connect_and_register(
        &url,
        json!({"type":"register","name":"fresh-a","decision_rate":1}),
    )
    .await;
    let ok = reply_of_type(&mut rx1, Duration::from_secs(10), &["registered", "error"]).await;
    assert_eq!(ok["type"], "registered", "{ok}");

    // A second fresh name finds the bucket empty.
    let (_, mut rx2) = connect_and_register(
        &url,
        json!({"type":"register","name":"fresh-b","decision_rate":1}),
    )
    .await;
    let err = reply_of_type(&mut rx2, Duration::from_secs(10), &["registered", "error"]).await;
    assert_eq!(
        err["error"],
        "too many new bots, slow down",
        "fresh name must hit the bucket: {err}"
    );

    // A known name reconnecting bypasses the bucket entirely. Its first
    // socket must be gone first: a name holds one live connection
    // (issue #42), so the old sockets are closed and teardown polled.
    drop(rx1);
    drop(rx2);
    let mut ok = None;
    for _ in 0..40 {
        let (tx3, mut rx3) = connect_and_register(
            &url,
            json!({"type":"register","name":"fresh-a","decision_rate":1}),
        )
        .await;
        let msg =
            reply_of_type(&mut rx3, Duration::from_secs(5), &["registered", "error"]).await;
        if msg["type"] == "registered" {
            ok = Some(msg);
            break;
        }
        drop(tx3);
        drop(rx3);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        ok.expect("known name must bypass the bucket")["type"],
        "registered"
    );
}

/// Per-message size cap (issue #41): a >64 KiB frame closes the socket — the
/// entrant rides the normal disconnect path — while the largest legal
/// message (a ~17 KiB mind-cam submission) passes and the bot keeps playing.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_frame_closes_socket_and_mindcam_passes() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8953;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    let (mut survivor_tx, mut survivor_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"cap-survivor","decision_rate":1}),
    )
    .await;
    let (mut flooder_tx, mut flooder_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"cap-flooder","decision_rate":1}),
    )
    .await;
    let _ = reply_of_type(&mut survivor_rx, Duration::from_secs(10), &["match_start"]).await;
    let _ = reply_of_type(&mut flooder_rx, Duration::from_secs(10), &["match_start"]).await;

    // The largest legal message: a full 4096-byte mind-cam belief — roughly
    // 17 KiB as a JSON number array — carrying a full-throttle north move.
    // Wire throttle is Q16.16 i64, so it must be an integer (65536 = 1.0):
    // a fractional number fails parsing and the reader drops the WHOLE
    // message silently (docs/AI-BOTS.md).
    let belief: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    survivor_tx
        .send(Message::Text(
            json!({"tick": 0, "belief": belief,
                   "main": {"move": {"dir": 0, "throttle": 65536}}})
                .to_string(),
        ))
        .await
        .unwrap();

    // The message must be consumed, not just tolerated: full throttle north
    // shows up as motion in the survivor's own observation within a few
    // ticks (a dropped message would leave the main standing — no momentum
    // source exists yet at tick 0).
    let moved = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut moved = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), survivor_rx.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if v.get("you").is_some() {
                            let vel = &v["you"]["main"]["vel"];
                            if vel[0] != 0 || vel[1] != 0 {
                                moved = true;
                                break;
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                _ => break,
            }
        }
        moved
    };
    assert!(
        moved,
        "the 17 KiB mind-cam + action message must be consumed: the main must move"
    );

    // 70 KiB in one frame: over the cap. The server errors the stream and
    // closes; the entrant falls into the normal disconnect path.
    flooder_tx
        .send(Message::Text("x".repeat(70 * 1024)))
        .await
        .unwrap();
    let closed = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut closed = false;
        loop {
            match tokio::time::timeout_at(deadline, flooder_rx.next()).await {
                // The cap must END the socket: a Close frame, a transport
                // error, or EOF all count. A timeout (socket still alive,
                // observations streaming) does not — it fails the assert.
                Ok(Some(Ok(Message::Close(_)))) => {
                    closed = true;
                    break;
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) => {
                    closed = true;
                    break;
                }
                Ok(None) => {
                    closed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        closed
    };
    assert!(
        closed,
        "the oversized frame must close the flooder's socket (cap removed?)"
    );

    // The survivor is unaffected: the server keeps streaming to it after
    // the flood (observations at the tick rate).
    let got = tokio::time::timeout(Duration::from_secs(5), survivor_rx.next()).await;
    assert!(
        matches!(got, Ok(Some(Ok(_)))),
        "survivor still receives traffic after the flood: {got:?}"
    );
}

/// Ladder enrollment (issue #42): a rated first registration receives a
/// server-issued secret in the ack; a third party presenting any token for
/// the now-claimed name is refused; the owner reconnects with the issued
/// secret once the old socket is gone.
#[tokio::test(flavor = "multi_thread")]
async fn issued_token_protects_ladder_identity() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8955;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    // First tokenless registration of a rated name: the ack carries the
    // server-issued secret.
    let (owner_tx, mut owner_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"tok-owner","decision_rate":1,"rated":true}),
    )
    .await;
    let ack = reply_of_type(&mut owner_rx, Duration::from_secs(10), &["registered"]).await;
    assert_eq!(ack["rated"], true, "{ack}");
    let secret = ack["token"].as_str().expect("issued token in ack").to_string();
    assert!(!secret.is_empty());

    // An attacker presenting a self-chosen token for the claimed name is
    // refused — the window from issue #42 is closed.
    let (_, mut atk_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"tok-owner","decision_rate":1,"token":"x"}),
    )
    .await;
    let err = reply_of_type(&mut atk_rx, Duration::from_secs(10), &["error"]).await;
    assert_eq!(err["error"], "bad token", "attacker: {err}");

    // The owner's own secret frees the name after the old socket closes.
    drop(owner_tx);
    drop(owner_rx);
    let mut reowned = None;
    for _ in 0..40 {
        let (tx, mut rx) = connect_and_register(
            &url,
            json!({"type":"register","name":"tok-owner","decision_rate":1,"token":secret}),
        )
        .await;
        let msg = reply_of_type(&mut rx, Duration::from_secs(5), &["registered", "error"]).await;
        if msg["type"] == "registered" {
            reowned = Some(msg);
            break;
        }
        drop(tx);
        drop(rx);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        reowned.expect("owner reconnects with the issued secret")["type"],
        "registered"
    );
}

/// Verified-owner eviction (reliability doctrine): while the old socket is
/// STILL LIVE, the owner re-registering with the correct secret evicts it
/// (latest connection wins) instead of being refused — a hung tab can never
/// lock the owner out of their own name. The evicted socket gets a last
/// error and is closed.
#[tokio::test(flavor = "multi_thread")]
async fn verified_owner_evicts_a_hung_connection() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8961;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    let (hung_tx, mut hung_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"evict-owner","decision_rate":1,"rated":true}),
    )
    .await;
    let ack = reply_of_type(&mut hung_rx, Duration::from_secs(10), &["registered"]).await;
    let secret = ack["token"].as_str().expect("issued token in ack").to_string();

    // The owner dials again (new tab, same name + secret) while the old
    // socket is still open: admitted, not refused.
    let (_, mut fresh_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"evict-owner","decision_rate":1,"token":secret}),
    )
    .await;
    let ack2 = reply_of_type(&mut fresh_rx, Duration::from_secs(10), &["registered"]).await;
    assert_eq!(ack2["you"], "evict-owner", "owner takes over: {ack2}");

    // The hung socket learns why it died, then actually dies.
    let err = reply_of_type(&mut hung_rx, Duration::from_secs(10), &["error"]).await;
    assert!(
        err["error"].as_str().unwrap().contains("newer connection"),
        "evicted socket told why: {err}"
    );
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match hung_rx.next().await {
                // Stream end, transport error, or a Close frame — all dead.
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "evicted socket closed: {closed:?}");
    drop(hung_tx);
}

/// Casual tier (issue #42): a tokenless registration of a brand-new name is
/// off-ladder and gets no secret; enrolling later puts the same identity on
/// the ladder with a server-issued secret.
#[tokio::test(flavor = "multi_thread")]
async fn casual_name_is_hidden_until_enrolled() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8957;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");
    let http = format!("http://127.0.0.1:{port}");

    // Casual: no secret in the ack, and nothing on the ladder.
    let (cas_tx, mut cas_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"cas-bot","decision_rate":1}),
    )
    .await;
    let ack = reply_of_type(&mut cas_rx, Duration::from_secs(10), &["registered"]).await;
    assert_eq!(ack["rated"], false, "{ack}");
    assert!(ack["token"].is_null(), "casual gets no secret: {ack}");
    let body = reqwest_get(&format!("{http}/api/standings")).await;
    assert!(!body.contains("cas-bot"), "casual hidden: {body}");

    // Enroll: the same identity lands on the ladder with a secret.
    drop(cas_tx);
    drop(cas_rx);
    let mut secret = None;
    for _ in 0..40 {
        let (tx, mut rx) = connect_and_register(
            &url,
            json!({"type":"register","name":"cas-bot","decision_rate":1,"rated":true}),
        )
        .await;
        let msg = reply_of_type(&mut rx, Duration::from_secs(5), &["registered", "error"]).await;
        if msg["type"] == "registered" {
            secret = Some(msg["token"].as_str().unwrap_or("").to_string());
            break;
        }
        drop(tx);
        drop(rx);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let secret = secret.expect("enrolled ack carries the issued secret");
    assert!(!secret.is_empty());
    let body = reqwest_get(&format!("{http}/api/standings")).await;
    assert!(body.contains("cas-bot"), "enrolled on ladder: {body}");
}

/// A name holds one live connection (issue #42): a second concurrent
/// socket with the same name is refused while the first keeps playing, and
/// the name frees once that socket closes.
#[tokio::test(flavor = "multi_thread")]
async fn second_concurrent_socket_with_same_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let port = 8959;
    let cfg = ServerConfig {
        port,
        bind: "127.0.0.1".to_string(),
        db_path: dir.path().join("ladder.db"),
        replay_dir: dir.path().join("replays"),
        viewer_dir: None,
        lanes: 1,
        min_bots: 2,
        house_bots: 0,
        spectate_delay_s: 0,
        ws_ping_every_s: 10,
        ws_idle_timeout_s: 45,
        max_connections: 256,
        max_lobbies: 64,
        join_attempts_per_min: 0,
        new_names_per_min: 0,
        max_replays: 100,
    };
    tokio::spawn(async move {
        Server::start(cfg, MatchConfig::standard(), Arc::new(GameHost::new(3)))
            .await
            .expect("server");
    });
    wait_until_bound(port).await;
    let url = format!("ws://127.0.0.1:{port}/ws/bot");

    let (first_tx, mut first_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"dup-bot","decision_rate":1}),
    )
    .await;
    let _ = reply_of_type(&mut first_rx, Duration::from_secs(10), &["registered"]).await;

    let (_, mut second_rx) = connect_and_register(
        &url,
        json!({"type":"register","name":"dup-bot","decision_rate":1}),
    )
    .await;
    let err = reply_of_type(&mut second_rx, Duration::from_secs(10), &["error"]).await;
    assert_eq!(err["error"], "already connected", "second socket: {err}");

    // The first socket is unaffected: the server still streams to it.
    let got = tokio::time::timeout(Duration::from_secs(5), first_rx.next()).await;
    assert!(
        matches!(got, Ok(Some(Ok(_)))),
        "first socket still connected: {got:?}"
    );

    // Closing it frees the name for the next registration.
    drop(first_tx);
    drop(first_rx);
    let mut freed = false;
    for _ in 0..40 {
        let (tx, mut rx) = connect_and_register(
            &url,
            json!({"type":"register","name":"dup-bot","decision_rate":1}),
        )
        .await;
        let msg = reply_of_type(&mut rx, Duration::from_secs(5), &["registered", "error"]).await;
        if msg["type"] == "registered" {
            freed = true;
            break;
        }
        drop(tx);
        drop(rx);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(freed, "name frees after the socket closes");
}
