use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};

use super::manifest::PluginManifest;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS plugins (
  id              TEXT PRIMARY KEY,
  manifest        TEXT NOT NULL,
  wasm_sha256     TEXT NOT NULL,
  enabled         INTEGER NOT NULL,
  dev_path        TEXT,
  position        INTEGER NOT NULL,
  installed_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS grants (
  plugin_id TEXT NOT NULL,
  grant_key TEXT NOT NULL,
  is_on     INTEGER NOT NULL,
  PRIMARY KEY (plugin_id, grant_key)
);
CREATE TABLE IF NOT EXISTS kv (
  plugin_id TEXT NOT NULL,
  key       TEXT NOT NULL,
  value     BLOB NOT NULL,
  PRIMARY KEY (plugin_id, key)
);
CREATE TABLE IF NOT EXISTS keys (
  plugin_id TEXT NOT NULL,
  target    TEXT NOT NULL,
  key       TEXT NOT NULL,
  PRIMARY KEY (plugin_id, target)
);
CREATE TABLE IF NOT EXISTS controls (
  plugin_id  TEXT NOT NULL,
  control_id TEXT NOT NULL,
  hidden     INTEGER NOT NULL,
  position   INTEGER NOT NULL,
  PRIMARY KEY (plugin_id, control_id)
);";

pub const FILE_NAME: &str = "plugins.db";
pub const KV_TOTAL_BYTES: usize = 256 * 1024 * 1024;
pub const KV_VALUE_BYTES: usize = 64 * 1024 * 1024;
pub const KV_KEY_BYTES: usize = 256;
pub const SETTING_PREFIX: &str = "settings.";

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id: String,
    pub manifest: PluginManifest,
    pub wasm_sha256: String,
    pub enabled: bool,
    pub dev_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControlPref {
    pub plugin_id: String,
    pub control_id: String,
    pub hidden: bool,
    pub position: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
    kv_total_bytes: usize,
}

type StoreResult<T> = Result<T, String>;

fn sql(err: rusqlite::Error) -> String {
    format!("plugin database error: {err}")
}

impl Store {
    pub fn open(dir: &Path) -> StoreResult<Self> {
        std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
        Self::from_conn(Connection::open(dir.join(FILE_NAME)).map_err(sql)?)
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::from_conn(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn from_conn(conn: Connection) -> StoreResult<Self> {
        conn.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Self { conn: Mutex::new(conn), kv_total_bytes: KV_TOTAL_BYTES })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn list(&self) -> StoreResult<Vec<PluginRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id, manifest, wasm_sha256, enabled, dev_path FROM plugins ORDER BY position, id")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(sql)?;
        let mut plugins = Vec::new();
        for row in rows {
            let (id, manifest, wasm_sha256, enabled, dev_path) = row.map_err(sql)?;
            match serde_json::from_str::<PluginManifest>(&manifest) {
                Ok(manifest) => {
                    plugins.push(PluginRow { id, manifest, wasm_sha256, enabled, dev_path: dev_path.map(PathBuf::from) })
                }
                Err(err) => crate::logging::warn!("stored manifest of plugin {id} can't be read: {err}"),
            }
        }
        Ok(plugins)
    }

    /// Inserts a new plugin at the end of the list, or replaces an installed one in place, keeping its
    /// place, its on or off state and its data. `grants` are the user's switches for this version.
    pub fn upsert(
        &self,
        manifest: &PluginManifest,
        wasm_sha256: &str,
        dev_path: Option<&Path>,
        grants: &BTreeMap<String, bool>,
        now_ms: i64,
    ) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(sql)?;
        let manifest_json = serde_json::to_string(manifest).map_err(|err| err.to_string())?;
        let dev_path = dev_path.map(|path| path.to_string_lossy().into_owned());
        tx.execute(
            "INSERT INTO plugins (id, manifest, wasm_sha256, enabled, dev_path, position, installed_at_ms) \
             VALUES (?1, ?2, ?3, 1, ?4, (SELECT COALESCE(MAX(position), 0) + 1 FROM plugins), ?5) \
             ON CONFLICT(id) DO UPDATE SET manifest = ?2, wasm_sha256 = ?3, dev_path = ?4",
            params![manifest.id, manifest_json, wasm_sha256, dev_path, now_ms],
        )
        .map_err(sql)?;
        tx.execute("DELETE FROM grants WHERE plugin_id = ?1", [&manifest.id]).map_err(sql)?;
        for (key, on) in grants {
            tx.execute(
                "INSERT INTO grants (plugin_id, grant_key, is_on) VALUES (?1, ?2, ?3)",
                params![manifest.id, key, on],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    pub fn grants(&self, plugin_id: &str) -> StoreResult<BTreeMap<String, bool>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT grant_key, is_on FROM grants WHERE plugin_id = ?1").map_err(sql)?;
        let rows = stmt
            .query_map([plugin_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)))
            .map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn set_grant(&self, plugin_id: &str, key: &str, on: bool) -> StoreResult<()> {
        self.lock()
            .execute(
                "INSERT INTO grants (plugin_id, grant_key, is_on) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(plugin_id, grant_key) DO UPDATE SET is_on = ?3",
                params![plugin_id, key, on],
            )
            .map_err(sql)?;
        Ok(())
    }

    /// Refreshes a plugin loaded from a folder, whose manifest may have changed, keeping known switches.
    pub fn refresh_dev(&self, manifest: &PluginManifest, wasm_sha256: &str) -> StoreResult<()> {
        let manifest_json = serde_json::to_string(manifest).map_err(|err| err.to_string())?;
        let conn = self.lock();
        conn.execute(
            "UPDATE plugins SET manifest = ?2, wasm_sha256 = ?3 WHERE id = ?1",
            params![manifest.id, manifest_json, wasm_sha256],
        )
        .map_err(sql)?;
        for key in manifest.grant_keys() {
            conn.execute(
                "INSERT OR IGNORE INTO grants (plugin_id, grant_key, is_on) VALUES (?1, ?2, 1)",
                params![manifest.id, key],
            )
            .map_err(sql)?;
        }
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> StoreResult<()> {
        self.lock().execute("UPDATE plugins SET enabled = ?1 WHERE id = ?2", params![enabled, id]).map_err(sql)?;
        Ok(())
    }

    pub fn delete(&self, id: &str, delete_data: bool) -> StoreResult<()> {
        let conn = self.lock();
        for table in ["plugins", "grants", "keys", "controls"] {
            let column = if table == "plugins" { "id" } else { "plugin_id" };
            conn.execute(&format!("DELETE FROM {table} WHERE {column} = ?1"), [id]).map_err(sql)?;
        }
        if delete_data {
            conn.execute("DELETE FROM kv WHERE plugin_id = ?1", [id]).map_err(sql)?;
        }
        Ok(())
    }

    pub fn kv_get(&self, plugin_id: &str, key: &str) -> StoreResult<Option<Vec<u8>>> {
        self.lock()
            .query_row("SELECT value FROM kv WHERE plugin_id = ?1 AND key = ?2", params![plugin_id, key], |row| row.get(0))
            .optional()
            .map_err(sql)
    }

    pub fn kv_set(&self, plugin_id: &str, key: &str, value: &[u8]) -> StoreResult<()> {
        if key.is_empty() || key.len() > KV_KEY_BYTES {
            return Err(format!("Storage keys must be 1 to {KV_KEY_BYTES} bytes"));
        }
        if value.len() > KV_VALUE_BYTES {
            return Err(format!("A stored value can't be larger than {} MB", KV_VALUE_BYTES / (1024 * 1024)));
        }
        let conn = self.lock();
        let others: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(key) + LENGTH(value)), 0) FROM kv WHERE plugin_id = ?1 AND key != ?2",
                params![plugin_id, key],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if others as usize + key.len() + value.len() > self.kv_total_bytes {
            return Err(format!("Plugin storage is full ({} MB)", self.kv_total_bytes / (1024 * 1024)));
        }
        conn.execute(
            "INSERT INTO kv (plugin_id, key, value) VALUES (?1, ?2, ?3) ON CONFLICT(plugin_id, key) DO UPDATE SET value = ?3",
            params![plugin_id, key, value],
        )
        .map_err(sql)?;
        Ok(())
    }

    pub fn kv_delete(&self, plugin_id: &str, key: &str) -> StoreResult<()> {
        self.lock().execute("DELETE FROM kv WHERE plugin_id = ?1 AND key = ?2", params![plugin_id, key]).map_err(sql)?;
        Ok(())
    }

    pub fn kv_keys(&self, plugin_id: &str, prefix: &str) -> StoreResult<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT key FROM kv WHERE plugin_id = ?1 AND substr(key, 1, ?2) = ?3 ORDER BY key")
            .map_err(sql)?;
        let keys = stmt
            .query_map(params![plugin_id, prefix.chars().count() as i64, prefix], |row| row.get(0))
            .map_err(sql)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(sql)?;
        Ok(keys)
    }

    /// Keys bound to plugin actions and controls, as `(plugin id, target, key)`.
    pub fn key_bindings(&self) -> StoreResult<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT plugin_id, target, key FROM keys").map_err(sql)?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    /// An empty key clears the binding. Any other binding of the same key is cleared too.
    pub fn bind_key(&self, plugin_id: &str, target: &str, key: &str) -> StoreResult<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM keys WHERE plugin_id = ?1 AND target = ?2", params![plugin_id, target]).map_err(sql)?;
        if !key.is_empty() {
            conn.execute("DELETE FROM keys WHERE key = ?1", [key]).map_err(sql)?;
            conn.execute(
                "INSERT INTO keys (plugin_id, target, key) VALUES (?1, ?2, ?3)",
                params![plugin_id, target, key],
            )
            .map_err(sql)?;
        }
        Ok(())
    }

    pub fn clear_key(&self, key: &str) -> StoreResult<()> {
        self.lock().execute("DELETE FROM keys WHERE key = ?1", [key]).map_err(sql)?;
        Ok(())
    }

    pub fn control_prefs(&self) -> StoreResult<Vec<ControlPref>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT plugin_id, control_id, hidden, position FROM controls ORDER BY position").map_err(sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ControlPref { plugin_id: row.get(0)?, control_id: row.get(1)?, hidden: row.get(2)?, position: row.get(3)? })
            })
            .map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn set_control_prefs(&self, prefs: &[ControlPref]) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(sql)?;
        tx.execute("DELETE FROM controls", []).map_err(sql)?;
        for pref in prefs {
            tx.execute(
                "INSERT INTO controls (plugin_id, control_id, hidden, position) VALUES (?1, ?2, ?3, ?4)",
                params![pref.plugin_id, pref.control_id, pref.hidden, pref.position],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, permissions: serde_json::Value) -> PluginManifest {
        serde_json::from_value(serde_json::json!({
            "apiVersion": 1, "id": id, "name": "Test", "version": "1.0.0", "permissions": permissions,
        }))
        .unwrap()
    }

    #[test]
    fn an_update_keeps_the_place_the_switch_and_the_data() {
        let store = Store::in_memory();
        let first = manifest("com.example.a", serde_json::json!({ "playback.read": "" }));
        store.upsert(&first, "hash1", None, &BTreeMap::from([("playback.read".into(), true)]), 1).unwrap();
        store.upsert(&manifest("com.example.b", serde_json::json!({})), "hash", None, &BTreeMap::new(), 2).unwrap();
        store.set_enabled("com.example.a", false).unwrap();
        store.kv_set("com.example.a", "token", b"secret").unwrap();

        let grants = BTreeMap::from([("playback.read".into(), true), ("ui.osd".into(), false)]);
        store.upsert(&first, "hash2", None, &grants, 3).unwrap();

        let rows = store.list().unwrap();
        assert_eq!(rows[0].id, "com.example.a");
        assert!(!rows[0].enabled);
        assert_eq!(rows[0].wasm_sha256, "hash2");
        assert_eq!(store.grants("com.example.a").unwrap(), grants);
        assert_eq!(store.kv_get("com.example.a", "token").unwrap().unwrap(), b"secret");
    }

    #[test]
    fn uninstalling_without_deleting_data_keeps_storage_for_a_reinstall() {
        let store = Store::in_memory();
        store.upsert(&manifest("com.example.a", serde_json::json!({})), "h", None, &BTreeMap::new(), 1).unwrap();
        store.kv_set("com.example.a", "history", b"1").unwrap();
        store.delete("com.example.a", false).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.kv_get("com.example.a", "history").unwrap().is_some());
        store.delete("com.example.a", true).unwrap();
        assert!(store.kv_get("com.example.a", "history").unwrap().is_none());
    }

    #[test]
    fn a_key_bound_to_a_new_target_leaves_the_old_one() {
        let store = Store::in_memory();
        store.bind_key("com.example.a", "action:loop", "l").unwrap();
        store.bind_key("com.example.b", "control:lyrics", "l").unwrap();
        assert_eq!(store.key_bindings().unwrap(), vec![("com.example.b".into(), "control:lyrics".into(), "l".into())]);
    }

    #[test]
    fn storage_is_separate_per_plugin_and_has_a_quota() {
        let store = Store { kv_total_bytes: 4 * 1024, ..Store::in_memory() };
        store.kv_set("a", "token", b"secret-a").unwrap();
        store.kv_set("b", "token", b"secret-b").unwrap();
        assert_eq!(store.kv_get("a", "token").unwrap().unwrap(), b"secret-a");
        assert!(store.kv_set("a", "big", &vec![0; 4 * 1024]).is_err());
        assert!(store.kv_set("b", "other", &vec![0; 1024]).is_ok());
    }
}
