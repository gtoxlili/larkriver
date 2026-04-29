//! Persistent KV store for game state, backed by [redb] — a pure-Rust embedded
//! ACID database. Each `chat_id` maps to one schema-versioned, JSON-encoded
//! envelope wrapping the actual `Game` / `WolfGame` payload.
//!
//! ## Wire format
//!
//! ```text
//! { "v": <u32>, "data": <Game|WolfGame> }
//! ```
//!
//! `v` is the schema version of `data`. Old records without the envelope
//! (legacy bare-Game JSON) are detected by absence of the top-level `v` field
//! and fall through to a v0 → v1 migration path on read.
//!
//! Why this matters: without versioning, **adding** a serde field is safe
//! (`#[serde(default)]`) but **renaming / removing / type-changing** silently
//! breaks deserialise on old files at next startup. Recording an explicit
//! version per record lets us write `match v { 1 => ..., 2 => migrate(..), .. }`
//! when the time comes — no big-bang database migrations.
//!
//! Hot serialise / deserialise still goes through [sonic-rs] (SIMD JSON).
//!
//! [redb]: https://github.com/cberner/redb

use crate::game::Game;
use crate::util::FoldHashMap;
use crate::werewolf::WolfGame;
use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

const GAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("games");
const WOLF_GAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("wolf_games");

/// Latest schema version we write. Bump on incompatible struct changes,
/// add a corresponding arm to [`decode`].
const POKER_SCHEMA_VERSION: u32 = 1;
const WOLF_SCHEMA_VERSION: u32 = 1;

/// Versioned envelope written to redb. Field order is fixed so sonic-rs lays
/// `v` out before `data` — readers can short-circuit on the version without
/// allocating the full payload (rejected non-current versions).
///
/// Write-only (no `Deserialize`): the read path uses [`EnvelopeOwned`] so we
/// can borrow `&T` here without colliding with serde's lifetime requirements
/// for `Deserialize<'de>`.
#[derive(Debug, Serialize)]
struct Envelope<'a, T: Serialize> {
    /// Schema version of `data`.
    v: u32,
    /// The actual game state.
    data: &'a T,
}

/// Owned counterpart of [`Envelope`] used on the read path.
#[derive(Debug, Deserialize)]
struct EnvelopeOwned<T> {
    /// Optional so we can detect legacy unversioned records and route them
    /// through the v0-fallback.
    #[serde(default)]
    v: Option<u32>,
    data: Option<T>,
}

pub struct Store {
    db: Database,
}

impl Store {
    /// Open (creating if missing) the database file at `path`.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let db = Database::create(path)
            .with_context(|| format!("opening redb at {}", path.display()))?;
        // Touch the tables so subsequent reads on a fresh DB don't fail.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(GAMES)?;
            let _ = txn.open_table(WOLF_GAMES)?;
        }
        txn.commit()?;
        Ok(Arc::new(Self { db }))
    }

    pub fn save(&self, chat_id: &str, game: &Game) -> Result<()> {
        let bytes = encode(POKER_SCHEMA_VERSION, game)?;
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(GAMES)?;
            t.insert(chat_id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn delete(&self, chat_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(GAMES)?;
            t.remove(chat_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Replay all persisted games. Bad / partially-written entries are
    /// dropped with a warning rather than aborting startup.
    pub fn load_all(&self) -> Result<FoldHashMap<String, Game>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(GAMES)?;
        let mut out = FoldHashMap::default();
        for kv in t.iter()? {
            let (k, v) = kv?;
            let key = k.value().to_string();
            match decode_poker(v.value()) {
                Ok(game) => {
                    out.insert(key, game);
                }
                Err(e) => {
                    warn!(?e, chat_id = %key, "skipping unparseable game record");
                }
            }
        }
        Ok(out)
    }

    pub fn save_wolf(&self, chat_id: &str, game: &WolfGame) -> Result<()> {
        let bytes = encode(WOLF_SCHEMA_VERSION, game)?;
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(WOLF_GAMES)?;
            t.insert(chat_id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn delete_wolf(&self, chat_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(WOLF_GAMES)?;
            t.remove(chat_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn load_all_wolf(&self) -> Result<FoldHashMap<String, WolfGame>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(WOLF_GAMES)?;
        let mut out = FoldHashMap::default();
        for kv in t.iter()? {
            let (k, v) = kv?;
            let key = k.value().to_string();
            match decode_wolf(v.value()) {
                Ok(game) => {
                    out.insert(key, game);
                }
                Err(e) => {
                    warn!(?e, chat_id = %key, "skipping unparseable wolf record");
                }
            }
        }
        Ok(out)
    }
}

/// Serialise `data` inside a `{v, data}` envelope.
fn encode<T: Serialize>(version: u32, data: &T) -> Result<Vec<u8>> {
    let env = Envelope { v: version, data };
    Ok(sonic_rs::to_vec(&env)?)
}

/// Read a Game from a stored byte blob. Routes through migrations:
/// - **v1** (current): plain envelope deser
/// - **legacy** (no `v` field): try as bare `Game`, that was the v0 wire format
fn decode_poker(bytes: &[u8]) -> Result<Game> {
    if let Ok(env) = sonic_rs::from_slice::<EnvelopeOwned<Game>>(bytes) {
        if let Some(version) = env.v {
            return migrate_poker(version, env.data);
        }
    }
    // Legacy (pre-envelope) record: bare Game JSON
    let game: Game = sonic_rs::from_slice(bytes)
        .context("failed both envelope and legacy deserialise for poker record")?;
    Ok(game)
}

fn decode_wolf(bytes: &[u8]) -> Result<WolfGame> {
    if let Ok(env) = sonic_rs::from_slice::<EnvelopeOwned<WolfGame>>(bytes) {
        if let Some(version) = env.v {
            return migrate_wolf(version, env.data);
        }
    }
    let game: WolfGame = sonic_rs::from_slice(bytes)
        .context("failed both envelope and legacy deserialise for wolf record")?;
    Ok(game)
}

/// Run any necessary migrations to bring a stored Game up to the current
/// in-memory representation. Today there's only v1; future versions add
/// a match arm here that mutates `data` accordingly.
fn migrate_poker(version: u32, data: Option<Game>) -> Result<Game> {
    let g = data.context("envelope missing data field for poker record")?;
    match version {
        v if v == POKER_SCHEMA_VERSION => Ok(g),
        v if v < POKER_SCHEMA_VERSION => {
            // No older versions yet. When we add v2, branch like:
            //   1 => g = migrate_v1_to_v2(g),
            //   2 => g = migrate_v2_to_v3(g),
            //   ...
            warn!(version = v, "poker record older than current schema, accepting as-is");
            Ok(g)
        }
        v => Err(anyhow::anyhow!(
            "poker record schema v{v} is newer than this binary's v{POKER_SCHEMA_VERSION} — \
             refuse to read forward-incompatible state"
        )),
    }
}

fn migrate_wolf(version: u32, data: Option<WolfGame>) -> Result<WolfGame> {
    let g = data.context("envelope missing data field for wolf record")?;
    match version {
        v if v == WOLF_SCHEMA_VERSION => Ok(g),
        v if v < WOLF_SCHEMA_VERSION => {
            warn!(version = v, "wolf record older than current schema, accepting as-is");
            Ok(g)
        }
        v => Err(anyhow::anyhow!(
            "wolf record schema v{v} is newer than this binary's v{WOLF_SCHEMA_VERSION}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Game, PlayerAction};
    use crate::poker::DeckMode;

    #[test]
    fn round_trip_preserves_chips_and_action_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");

        // Seed
        {
            let store = Store::open(&path).unwrap();
            let mut g = Game::new("oc_test".into());
            g.add_player("ou_a".into(), "Alice".into()).unwrap();
            g.add_player("ou_b".into(), "Bob".into()).unwrap();
            g.start_hand(DeckMode::Standard).unwrap();
            g.act("ou_a", PlayerAction::RaiseTo(40)).unwrap();
            store.save(&g.chat_id, &g).unwrap();
        }

        // Reopen and verify
        {
            let store = Store::open(&path).unwrap();
            let games = store.load_all().unwrap();
            assert_eq!(games.len(), 1);
            let g = games.get("oc_test").unwrap();
            assert_eq!(g.players.len(), 2);
            assert_eq!(g.players[0].name, "Alice");
            assert!(g.action_log.iter().any(|l| l.amount == 40));
            assert_eq!(g.hand_count, 1);
        }
    }

    #[test]
    fn delete_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let store = Store::open(&path).unwrap();
        let g = Game::new("oc_x".into());
        store.save("oc_x", &g).unwrap();
        assert_eq!(store.load_all().unwrap().len(), 1);
        store.delete("oc_x").unwrap();
        assert_eq!(store.load_all().unwrap().len(), 0);
    }

    #[test]
    fn wolf_round_trip_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        {
            let store = Store::open(&path).unwrap();
            let mut g = WolfGame::new("oc_w".into());
            for i in 0..9 {
                g.add_player(format!("p{i}"), format!("P{i}")).unwrap();
            }
            g.start_game().unwrap();
            store.save_wolf(&g.chat_id, &g).unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            let games = store.load_all_wolf().unwrap();
            assert_eq!(games.len(), 1);
            let g = games.get("oc_w").unwrap();
            assert_eq!(g.players.len(), 9);
            assert_eq!(g.day, 1);
            assert!(g.players.iter().all(|p| p.role.is_some()));
        }
    }

    #[test]
    fn legacy_bare_game_record_still_loads() {
        // Verify the v0 fall-through: a bare `Game` JSON (no envelope) written
        // by an older version of this binary should still deser cleanly when
        // the new code reads it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.redb");
        let g = Game::new("oc_legacy".into());
        let legacy_bytes = sonic_rs::to_vec(&g).unwrap(); // no envelope

        // Drop the bytes straight into the table the way the old code did.
        {
            let db = redb::Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut t = txn.open_table(GAMES).unwrap();
                t.insert("oc_legacy", legacy_bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        // Reopen with the new envelope-aware Store and confirm the read path
        // unwraps the legacy form.
        let store = Store::open(&path).unwrap();
        let games = store.load_all().unwrap();
        assert_eq!(games.len(), 1, "legacy record must still load");
        assert!(games.contains_key("oc_legacy"));
    }

    #[test]
    fn forward_incompatible_record_is_rejected() {
        // Version higher than what this binary knows → the record is dropped
        // (with warn!). Better than panicking on a downgraded deploy that
        // sees future-version records.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.redb");

        // Hand-craft a future-version envelope.
        let g = Game::new("oc_future".into());
        let payload = sonic_rs::json!({
            "v": POKER_SCHEMA_VERSION + 99,
            "data": sonic_rs::to_string(&g).unwrap(),
        });
        let bytes = sonic_rs::to_vec(&payload).unwrap();
        {
            let db = redb::Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut t = txn.open_table(GAMES).unwrap();
                t.insert("oc_future", bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&path).unwrap();
        let games = store.load_all().unwrap();
        assert_eq!(games.len(), 0, "forward-incompat record should be skipped");
    }
}
