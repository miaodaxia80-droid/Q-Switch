//! Small DAO over the existing Q Switch database. Never drops legacy tables.
use crate::{config::get_app_config_dir, error::AppError, provider::Provider};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, MutexGuard},
};

pub struct Database {
    conn: Mutex<Connection>,
}
pub struct ProxyConfig {
    pub listen_port: u16,
}
impl Database {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        Self::from_connection(Connection::open(path)?)
    }
    pub fn new() -> Result<Self, AppError> {
        Self::open(&get_app_config_dir().join("cc-switch.db"))
    }
    pub fn memory() -> Result<Self, AppError> {
        Self::from_connection(Connection::open_in_memory()?)
    }
    fn from_connection(conn: Connection) -> Result<Self, AppError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT);
          CREATE TABLE IF NOT EXISTS providers (id TEXT NOT NULL, app_type TEXT NOT NULL,
          name TEXT NOT NULL, settings_config TEXT NOT NULL,
          website_url TEXT, category TEXT, created_at INTEGER, sort_index INTEGER,
          notes TEXT, icon TEXT, icon_color TEXT, meta TEXT NOT NULL DEFAULT '{}',
          is_current BOOLEAN NOT NULL DEFAULT 0, in_failover_queue BOOLEAN NOT NULL DEFAULT 0,
          PRIMARY KEY(id,app_type));",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    fn connection(&self) -> Result<MutexGuard<'_, Connection>, AppError> {
        Ok(self.conn.lock()?)
    }
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, AppError> {
        Ok(self
            .connection()?
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), AppError> {
        self.connection()?.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        Ok(())
    }
    pub fn save_provider(&self, app: &str, p: &Provider) -> Result<(), AppError> {
        self.connection()?.execute("INSERT INTO providers(id,app_type,name,settings_config) VALUES(?1,?2,?3,?4)
          ON CONFLICT(id,app_type) DO UPDATE SET name=excluded.name,settings_config=excluded.settings_config",
          params![p.id,app,p.name,p.settings_config.to_string()])?;
        Ok(())
    }
    pub fn delete_provider(&self, app: &str, id: &str) -> Result<(), AppError> {
        self.connection()?.execute(
            "DELETE FROM providers WHERE id=?1 AND app_type=?2",
            params![id, app],
        )?;
        Ok(())
    }
    pub fn get_all_providers(&self, app: &str) -> Result<HashMap<String, Provider>, AppError> {
        let conn = self.connection()?;
        let mut stmt =
            conn.prepare("SELECT id,name,settings_config FROM providers WHERE app_type=?1")?;
        let rows = stmt.query_map([app], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, name, raw) = row?;
            let settings_config = serde_json::from_str(&raw)
                .map_err(|_| AppError::Config("Invalid stored Qoder provider JSON".into()))?;
            out.insert(
                id.clone(),
                Provider {
                    id,
                    name,
                    settings_config,
                },
            );
        }
        Ok(out)
    }
    pub fn get_provider_by_id(&self, id: &str, app: &str) -> Result<Option<Provider>, AppError> {
        Ok(self.get_all_providers(app)?.remove(id))
    }
    pub async fn get_proxy_config(&self) -> Result<ProxyConfig, AppError> {
        let configured = self
            .get_setting("qoder_listen_port")?
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|port| *port > 0);
        // Earlier Q Switch releases shared the listener configuration with the
        // inherited 'claude' row. Read that port without changing the row.
        let legacy = self
            .connection()?
            .query_row(
                "SELECT listen_port FROM proxy_config WHERE app_type='claude'",
                [],
                |row| row.get::<_, u16>(0),
            )
            .optional()
            .ok()
            .flatten()
            .filter(|port| *port > 0);
        let port = configured.or(legacy).unwrap_or(15731);
        Ok(ProxyConfig { listen_port: port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_preserve_other_apps_and_legacy_columns() {
        let db = Database::memory().unwrap();
        db.connection()
            .unwrap()
            .execute("ALTER TABLE providers ADD COLUMN legacy_note TEXT", [])
            .unwrap();
        let p = Provider::with_id("p".into(), "Other".into(), serde_json::json!({}), None);
        db.save_provider("claude", &p).unwrap();
        db.save_provider("qoder", &p).unwrap();
        db.connection()
            .unwrap()
            .execute("UPDATE providers SET legacy_note='keep'", [])
            .unwrap();
        db.save_provider(
            "qoder",
            &Provider {
                name: "New".into(),
                ..p
            },
        )
        .unwrap();
        assert_eq!(
            db.get_provider_by_id("p", "claude").unwrap().unwrap().name,
            "Other"
        );
        let notes: String = db
            .connection()
            .unwrap()
            .query_row(
                "SELECT legacy_note FROM providers WHERE app_type='qoder'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(notes, "keep");
    }
}
