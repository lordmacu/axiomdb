//! Connection registry + RAII guard for `SHOW [FULL] PROCESSLIST` (GAP-B.7).
//!
//! Each authenticated connection registers itself on the shared registry held
//! by [`super::shared_db::SharedDatabase`]. The [`ProcesslistGuard`] removes
//! the entry on drop so disconnects — clean or abrupt — are reflected in
//! subsequent `SHOW PROCESSLIST` queries.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::shared_db::ConnectionInfo;

pub type Registry = Arc<RwLock<HashMap<u32, ConnectionInfo>>>;

/// RAII guard: inserts the connection on construction and removes it on drop.
pub struct ProcesslistGuard {
    registry: Registry,
    conn_id: u32,
}

impl ProcesslistGuard {
    pub fn register(registry: Registry, info: ConnectionInfo) -> Self {
        let conn_id = info.id;
        if let Ok(mut guard) = registry.write() {
            guard.insert(conn_id, info);
        }
        Self { registry, conn_id }
    }

    /// Updates this connection's current command + optional SQL text.
    /// Called at the start of each query; reset to `"Sleep"` when idle.
    pub fn set_command(&self, command: &str, info: Option<String>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Ok(mut guard) = self.registry.write() {
            if let Some(entry) = guard.get_mut(&self.conn_id) {
                entry.command = command.to_string();
                entry.info = info;
                entry.command_started_at = now;
            }
        }
    }

    /// Updates the current database column shown by PROCESSLIST — called
    /// after `USE <db>` or when authentication supplies an initial database.
    pub fn set_database(&self, db: Option<String>) {
        if let Ok(mut guard) = self.registry.write() {
            if let Some(entry) = guard.get_mut(&self.conn_id) {
                entry.db = db;
            }
        }
    }
}

impl Drop for ProcesslistGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.registry.write() {
            guard.remove(&self.conn_id);
        }
    }
}

/// Returns a snapshot of every registered connection, ordered by id.
pub fn snapshot(registry: &Registry) -> Vec<ConnectionInfo> {
    let guard = match registry.read() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let mut entries: Vec<ConnectionInfo> = guard.values().cloned().collect();
    entries.sort_by_key(|e| e.id);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u32, user: &str) -> ConnectionInfo {
        ConnectionInfo {
            id,
            user: user.into(),
            host: "localhost".into(),
            db: None,
            command: "Sleep".into(),
            command_started_at: 0,
            state: None,
            info: None,
        }
    }

    #[test]
    fn register_and_snapshot_sorted_by_id() {
        let reg: Registry = Arc::new(RwLock::new(HashMap::new()));
        let g1 = ProcesslistGuard::register(Arc::clone(&reg), info(3, "a"));
        let g2 = ProcesslistGuard::register(Arc::clone(&reg), info(1, "b"));
        let snap = snapshot(&reg);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].id, 1);
        assert_eq!(snap[1].id, 3);
        drop(g1);
        drop(g2);
    }

    #[test]
    fn guard_drop_removes_entry() {
        let reg: Registry = Arc::new(RwLock::new(HashMap::new()));
        {
            let _g = ProcesslistGuard::register(Arc::clone(&reg), info(7, "u"));
            assert_eq!(snapshot(&reg).len(), 1);
        }
        assert_eq!(snapshot(&reg).len(), 0, "drop must remove registry entry");
    }

    #[test]
    fn set_command_updates_in_place() {
        let reg: Registry = Arc::new(RwLock::new(HashMap::new()));
        let g = ProcesslistGuard::register(Arc::clone(&reg), info(1, "u"));
        g.set_command("Query", Some("SELECT 1".into()));
        let snap = snapshot(&reg);
        assert_eq!(snap[0].command, "Query");
        assert_eq!(snap[0].info.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn set_database_updates_in_place() {
        let reg: Registry = Arc::new(RwLock::new(HashMap::new()));
        let g = ProcesslistGuard::register(Arc::clone(&reg), info(1, "u"));
        g.set_database(Some("mydb".into()));
        assert_eq!(snapshot(&reg)[0].db.as_deref(), Some("mydb"));
    }
}
