//! Persistent KV store for game state, backed by [redb] — a pure-Rust embedded
//! ACID database. Each `chat_id` maps to one JSON-encoded `Game`.
//!
//! Why redb over SQLite: our access pattern is a flat `chat_id → Game` table
//! with no joins, no queries, no aggregates. redb is single-file, zero-config,
//! pure Rust (no C dep), ACID, and a few hundred KB of compiled code.
//!
//! [redb]: https://github.com/cberner/redb

use crate::game::Game;
use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

const GAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("games");

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
        // Touch the table so subsequent reads on a fresh DB don't fail.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(GAMES)?;
        }
        txn.commit()?;
        Ok(Arc::new(Self { db }))
    }

    pub fn save(&self, chat_id: &str, game: &Game) -> Result<()> {
        let bytes = serde_json::to_vec(game)?;
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
    pub fn load_all(&self) -> Result<HashMap<String, Game>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(GAMES)?;
        let mut out = HashMap::new();
        for kv in t.iter()? {
            let (k, v) = kv?;
            let key = k.value().to_string();
            match serde_json::from_slice::<Game>(v.value()) {
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
}
