//! SQLite persistence: bot registry, ELO, match history (PLAN §8.1 —
//! "Postgres or SQLite for ladder/ELO and bot registry"). Lives on the node
//! seam: identity claims and standings are matchmaker reads/writes, results
//! and ELO write-back are the game role's only output contract.

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BotRow {
    pub id: i64,
    pub name: String,
    pub elo: i64,
    pub wins: i64,
    pub games: i64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MatchRow {
    pub id: i64,
    pub ended_at: String,
    pub winner: Option<String>,
    pub num_bots: i64,
    pub replay_path: String,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS bots(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                token TEXT NOT NULL DEFAULT '',
                elo INTEGER NOT NULL DEFAULT 1000,
                wins INTEGER NOT NULL DEFAULT 0,
                games INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS matches(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at TEXT NOT NULL DEFAULT (datetime('now')),
                ended_at TEXT,
                winner_bot INTEGER,
                num_bots INTEGER NOT NULL,
                seed INTEGER NOT NULL,
                ticks INTEGER NOT NULL DEFAULT 0,
                replay_path TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS placements(
                match_id INTEGER NOT NULL REFERENCES matches(id),
                bot_id INTEGER NOT NULL REFERENCES bots(id),
                place INTEGER NOT NULL,
                kills INTEGER NOT NULL DEFAULT 0
             );",
        )?;
        // Two-tier identities (issue #42): rated rows are ladder identities
        // protected by their token; tokenless rows are casual — off-ladder
        // and disposable by design. The column defaults to rated so every
        // pre-existing row keeps its ladder standing: nobody falls off at
        // the upgrade, and their next reconnect claims + protects the row.
        // Fresh databases need the ALTER too — the CREATE TABLE above does
        // not carry the column (it predates #42) — so check structurally
        // instead of sniffing the "duplicate column" error text.
        let has_rated: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('bots') WHERE name='rated'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !has_rated {
            conn.execute("ALTER TABLE bots ADD COLUMN rated INTEGER NOT NULL DEFAULT 1", [])?;
        }
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// Register or re-register a bot; returns the db id. A name's token is
    /// claimable only while it is empty (first come): re-registration may
    /// never rotate an existing token, or anyone who knew a bot's name —
    /// and nothing else — could take over its ladder identity. The secret
    /// itself is chosen by the server (issued at enrollment), never by the
    /// client; `rated` enrolls the row on the ladder (issue #42).
    pub fn register_bot(&self, name: &str, token: &str, rated: bool) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO bots(name, token, rated) VALUES(?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET token=excluded.token, rated=excluded.rated
             WHERE bots.token=''",
            rusqlite::params![name, token, rated as i64],
        )?;
        conn.query_row("SELECT id FROM bots WHERE name=?1", [name], |r| r.get(0))
    }

    /// Door rule (issue #42): a claimed name accepts only its token; an
    /// unclaimed (tokenless) row accepts only a tokenless presentation —
    /// the server then issues the secret in the registration ack. Unknown
    /// names likewise pass only tokenless: the first registration creates
    /// the row and receives the issued secret.
    pub fn verify_token(&self, name: &str, token: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        match conn.query_row("SELECT token FROM bots WHERE name=?1", [name], |r| {
            r.get::<_, String>(0)
        }) {
            Ok(stored) => stored == token,
            Err(_) => token.is_empty(), // unknown bot: first connection registers it
        }
    }

    /// Ladder tier of a name (issue #42): false = casual — off-ladder,
    /// tokenless, disposable by design. Unknown names read as unrated; the
    /// tier is chosen at first registration via the `rated` flag, and a
    /// casual row upgrades by re-registering with it.
    pub fn is_rated(&self, name: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT rated FROM bots WHERE name=?1", [name], |r| {
            r.get::<_, i64>(0)
        })
        .map(|v| v != 0)
        .unwrap_or(false)
    }

    /// Is this name in the registry at all? Read-only check so the
    /// matchmaker can rate-limit first-time registrations (issue #37)
    /// without conflating them with reconnects.
    pub fn bot_exists(&self, name: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT 1 FROM bots WHERE name=?1", [name], |_| Ok(()))
            .is_ok()
    }

    pub fn elo_of(&self, name: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT elo FROM bots WHERE name=?1", [name], |r| r.get(0))
            .unwrap_or(1000)
    }

    pub fn standings(&self) -> Vec<BotRow> {
        let conn = self.conn.lock().unwrap();
        // House bots are sparring partners, not competitors — keep them off
        // the ladder (they still carry placements for match history); so
        // are casual identities (issue #42): only rated rows stand on it.
        let mut stmt = match conn.prepare(
            "SELECT id, name, elo, wins, games FROM bots
             WHERE rated=1 AND name NOT LIKE 'house·%' ORDER BY elo DESC, name LIMIT 100",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map([], |r| {
            Ok(BotRow {
                id: r.get(0)?,
                name: r.get(1)?,
                elo: r.get(2)?,
                wins: r.get(3)?,
                games: r.get(4)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn record_match(
        &self,
        seed: u64,
        num_bots: i64,
        ticks: u64,
        replay_path: &str,
        // (bot name, place, kills) in rank order
        results: &[(String, i64, i64)],
        elos: &[(String, i64, i64)], // name, old_elo, new_elo
    ) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        let winner = results.first().map(|(n, _, _)| n.as_str());
        conn.execute(
            "INSERT INTO matches(ended_at, winner_bot, num_bots, seed, ticks, replay_path)
             VALUES(datetime('now'),
                    (SELECT id FROM bots WHERE name=?1),
                    ?2, ?3, ?4, ?5)",
            rusqlite::params![winner, num_bots, seed as i64, ticks as i64, replay_path],
        )
        .ok()?;
        let mid = conn.last_insert_rowid();
        for (name, place, kills) in results {
            conn.execute(
                "INSERT INTO placements(match_id, bot_id, place, kills)
                 VALUES(?1, (SELECT id FROM bots WHERE name=?2), ?3, ?4)",
                rusqlite::params![mid, name, place, kills],
            )
            .ok()?;
            conn.execute(
                "UPDATE bots SET games = games + 1,
                                  wins = wins + (CASE WHEN ?2 = 1 THEN 1 ELSE 0 END),
                                  elo = ?3
                                WHERE name = ?1",
                rusqlite::params![
                    name,
                    place,
                    elos.iter()
                        .find(|(n, _, _)| n == name)
                        .map(|(_, _, e)| e)
                        .unwrap_or(&1000)
                ],
            )
            .ok()?;
        }
        Some(mid)
    }

    pub fn recent_matches(&self, limit: i64) -> Vec<MatchRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT m.id, COALESCE(m.ended_at, m.started_at), b.name, m.num_bots, m.replay_path
             FROM matches m LEFT JOIN bots b ON b.id = m.winner_bot
             WHERE m.ended_at IS NOT NULL
             ORDER BY m.id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map([limit], |r| {
            Ok(MatchRow {
                id: r.get(0)?,
                ended_at: r.get(1)?,
                winner: r.get(2)?,
                num_bots: r.get(3)?,
                replay_path: r.get(4)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }
}

/// Pairwise multi-player ELO (PLAN §8.2): every placement compares against
/// every other entrant; winner takes from everyone it beat.
pub fn elo_update(
    ratings: &[(String, i64)],
    places: &std::collections::HashMap<String, i64>,
    k: f64,
) -> Vec<(String, i64, i64)> {
    let mut out = Vec::new();
    for (name, ra) in ratings {
        let pa = *places.get(name).unwrap_or(&i64::MAX);
        let mut delta = 0.0f64;
        for (other, rb) in ratings {
            if other == name {
                continue;
            }
            let pb = *places.get(other).unwrap_or(&i64::MAX);
            let expected = 1.0 / (1.0 + 10f64.powf((*rb - *ra) as f64 / 400.0));
            let actual = match pa.cmp(&pb) {
                std::cmp::Ordering::Less => 1.0,
                std::cmp::Ordering::Equal => 0.5,
                std::cmp::Ordering::Greater => 0.0,
            };
            delta += k * (actual - expected);
        }
        let new = (*ra as f64 + delta).round() as i64;
        out.push((name.clone(), *ra, new));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elo_winner_gains_loses_drop() {
        let ratings = vec![
            ("a".into(), 1000),
            ("b".into(), 1000),
            ("c".into(), 1000),
            ("d".into(), 1000),
        ];
        let mut places = std::collections::HashMap::new();
        places.insert("a".to_string(), 1);
        places.insert("b".to_string(), 2);
        places.insert("c".to_string(), 3);
        places.insert("d".to_string(), 4);
        let out = elo_update(&ratings, &places, 32.0);
        let (_, _, a) = out.iter().find(|(n, _, _)| n == "a").unwrap();
        let (_, _, d) = out.iter().find(|(n, _, _)| n == "d").unwrap();
        assert!(*a > 1000, "winner gains: {a}");
        assert!(*d < 1000, "last loses: {d}");
        // Zero-sum-ish (symmetric pairs).
        let total: i64 = out.iter().map(|(_, _, e)| *e).sum();
        assert_eq!(total, 4000);
    }

    #[test]
    fn claimed_token_cannot_be_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();

        // Unclaimed name: only a tokenless presentation passes the door —
        // the server issues the secret, it is never client-chosen (issue
        // #42). An attacker presenting any token for a claimable name is
        // refused before they could close the window.
        db.register_bot("bot", "", true).unwrap();
        assert!(db.verify_token("bot", ""));
        assert!(!db.verify_token("bot", "anything"));

        // Enrollment closes the name…
        db.register_bot("bot", "secret", true).unwrap();
        assert!(db.verify_token("bot", "secret"));
        assert!(!db.verify_token("bot", ""));
        assert!(!db.verify_token("bot", "wrong"));

        // …and no later re-registration can rotate it. (The old upsert did
        // `SET token=excluded.token` unconditionally, so a fresh connection
        // with a new token took over the name and locked out its owner.)
        db.register_bot("bot", "evil", true).unwrap();
        assert!(db.verify_token("bot", "secret"));
        assert!(!db.verify_token("bot", "evil"));
        // An unknown name lets only a tokenless presentation through.
        assert!(db.verify_token("ghost", ""));
        assert!(!db.verify_token("ghost", "x"));
    }

    #[test]
    fn casual_identities_are_off_ladder_until_enrolled() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();

        // Casual: tokenless, off-ladder, disposable — nothing worth
        // stealing, so no secret is issued and none is needed.
        db.register_bot("cas", "", false).unwrap();
        assert!(!db.is_rated("cas"));
        assert!(db.verify_token("cas", ""));
        assert!(
            !db.standings().iter().any(|b| b.name == "cas"),
            "casual must not appear on the ladder"
        );

        // Enrolling issues the secret and puts the identity on the ladder —
        // with its accumulated history intact (ELO was written all along).
        db.register_bot("cas", "issued-1", true).unwrap();
        assert!(db.is_rated("cas"));
        assert!(db.verify_token("cas", "issued-1"));
        assert!(
            db.standings().iter().any(|b| b.name == "cas"),
            "rated row stands on the ladder"
        );

        // A pre-existing rated row with no token yet (the upgrade edge:
        // every pre-#42 row and every known name reconnecting) reads as
        // rated so its owner's next reconnect enrolls and protects it.
        db.register_bot("legacy", "", true).unwrap();
        assert!(db.is_rated("legacy"));
        assert!(db.verify_token("legacy", ""));
    }

    #[test]
    fn db_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.register_bot("hunter1", "", true).unwrap();
        assert!(db.verify_token("hunter1", ""));
        db.record_match(
            42,
            2,
            500,
            "/replays/x.json",
            &[("hunter1".into(), 1, 3), ("hunter2".into(), 2, 0)],
            &[
                ("hunter1".into(), 1000, 1016),
                ("hunter2".into(), 1000, 984),
            ],
        );
        let s = db.standings();
        assert_eq!(s[0].name, "hunter1");
        assert_eq!(s[0].wins, 1);
        let m = db.recent_matches(5);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].winner.as_deref(), Some("hunter1"));
    }
}
