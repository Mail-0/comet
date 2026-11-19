use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) use zeron_harness::copilot::BOUND_PREFIX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub account: String,
    pub org: String,
    // Request lifetime only; never part of the persisted binding key.
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Binding {
    pub chat_id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub draft: bool,
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
    conn: Arc<Mutex<Option<Connection>>>,
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("keiki-conversations.sqlite3"),
            conn: Arc::default(),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let mut slot = self.conn.lock().map_err(|_| "Keiki store is unavailable")?;
        if slot.is_none() {
            std::fs::create_dir_all(self.path.parent().unwrap()).map_err(|e| e.to_string())?;
            let conn = Connection::open(&self.path).map_err(|e| e.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|e| e.to_string())?;
            conn.execute_batch(
                "PRAGMA foreign_keys=ON;
                CREATE TABLE IF NOT EXISTS bindings (
                    account TEXT NOT NULL, org TEXT NOT NULL, agent TEXT NOT NULL,
                    conversation TEXT NOT NULL, chat TEXT NOT NULL UNIQUE, draft INTEGER NOT NULL,
                    PRIMARY KEY(account, org, agent, conversation));
                CREATE TABLE IF NOT EXISTS history (
                    chat TEXT NOT NULL REFERENCES bindings(chat), message TEXT NOT NULL,
                    created INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(chat, message));
                CREATE TABLE IF NOT EXISTS questions (
                    chat TEXT PRIMARY KEY REFERENCES bindings(chat), body TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS preferences (
                    chat TEXT PRIMARY KEY REFERENCES bindings(chat), pinned INTEGER NOT NULL);",
            )
            .map_err(|e| e.to_string())?;
            *slot = Some(conn);
        }
        f(slot.as_mut().unwrap()).map_err(|e| e.to_string())
    }

    pub fn bind(
        &self,
        owner: &Owner,
        agent: &str,
        conversation: &str,
        draft: bool,
    ) -> Result<Binding, String> {
        self.with(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO bindings VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    owner.account,
                    owner.org,
                    agent,
                    conversation,
                    format!("{BOUND_PREFIX}{}", uuid::Uuid::new_v4()),
                    draft
                ],
            )?;
            conn.query_row(
                "SELECT chat,agent,conversation,draft FROM bindings
                WHERE account=?1 AND org=?2 AND agent=?3 AND conversation=?4",
                params![owner.account, owner.org, agent, conversation],
                binding,
            )
        })
    }

    pub fn get(&self, owner: &Owner, chat: &str) -> Result<Binding, String> {
        self.with(|conn| {
            conn.query_row(
                "SELECT chat,agent,conversation,draft FROM bindings
            WHERE account=?1 AND org=?2 AND chat=?3",
                params![owner.account, owner.org, chat],
                binding,
            )
            .optional()
        })?
        .ok_or_else(|| "Conversation is not available in this account and organization".into())
    }

    pub fn history(&self, owner: &Owner, chat: &str) -> Result<Vec<Value>, String> {
        self.get(owner, chat)?;
        self.with(|conn| {
            let mut stmt =
                conn.prepare("SELECT body FROM history WHERE chat=?1 ORDER BY created,message")?;
            let rows = stmt.query_map([chat], |r| r.get::<_, String>(0))?;
            rows.map(|r| {
                serde_json::from_str(&r?)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })
            .collect()
        })
    }

    pub fn lookup(&self, owner: &Owner, agent: &str, conversation: &str) -> Result<Option<Binding>, String> {
        self.with(|conn| conn.query_row(
            "SELECT chat,agent,conversation,draft FROM bindings WHERE account=?1 AND org=?2 AND agent=?3 AND conversation=?4",
            params![owner.account, owner.org, agent, conversation], binding,
        ).optional())
    }

    pub fn pinned(&self, owner: &Owner, chat: &str) -> Result<bool, String> {
        self.get(owner, chat)?;
        self.with(|conn| conn.query_row("SELECT pinned FROM preferences WHERE chat=?1", [chat], |row| row.get(0)).optional())
            .map(|value| value.unwrap_or(false))
    }

    pub fn pin(&self, owner: &Owner, chat: &str, pinned: bool) -> Result<(), String> {
        self.get(owner, chat)?;
        self.with(|conn| conn.execute("INSERT INTO preferences(chat,pinned) VALUES(?1,?2) ON CONFLICT(chat) DO UPDATE SET pinned=excluded.pinned", params![chat, pinned]).map(|_| ()))
    }

    pub fn forget(&self, owner: &Owner, chat: &str) -> Result<(), String> {
        self.get(owner, chat)?;
        self.with(|conn| {
            let tx = conn.transaction()?;
            for table in ["preferences", "questions", "history"] {
                tx.execute(&format!("DELETE FROM {table} WHERE chat=?1"), [chat])?;
            }
            tx.execute("DELETE FROM bindings WHERE account=?1 AND org=?2 AND chat=?3", params![owner.account, owner.org, chat])?;
            tx.commit()
        })
    }

    pub fn cache(
        &self,
        owner: &Owner,
        chat: &str,
        messages: &[Value],
        complete: bool,
    ) -> Result<(), String> {
        self.get(owner, chat)?;
        let messages = messages
            .iter()
            .map(|m| {
                let id = m["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("Message has no id")?;
                let created = chrono::DateTime::parse_from_rfc3339(
                    m["createdAt"].as_str().ok_or("Message has no timestamp")?,
                )
                .map_err(|_| "Message has an invalid timestamp")?
                .timestamp_millis();
                Ok((id, created, m))
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.with(|conn| {
            let tx = conn.transaction()?;
            if complete { tx.execute("DELETE FROM history WHERE chat=?1", [chat])?; }
            for (id, created, m) in messages {
                tx.execute("INSERT INTO history VALUES (?1,?2,?3,?4)
                    ON CONFLICT(chat,message) DO UPDATE SET created=excluded.created,body=excluded.body",
                    params![chat, id, created, m.to_string()])?;
            }
            tx.execute("UPDATE bindings SET draft=0 WHERE chat=?1", [chat])?;
            tx.commit()
        })
    }

    pub fn questions(&self, owner: &Owner, chat: &str) -> Result<Value, String> {
        self.get(owner, chat)?;
        let body: Option<String> = self.with(|conn| {
            conn.query_row("SELECT body FROM questions WHERE chat=?1", [chat], |r| {
                r.get(0)
            })
            .optional()
        })?;
        body.map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
            .unwrap_or_else(|| Ok(Value::Array(Vec::new())))
    }

    pub fn set_questions(
        &self,
        owner: &Owner,
        chat: &str,
        questions: &Value,
    ) -> Result<(), String> {
        self.get(owner, chat)?;
        self.with(|conn| conn.execute("INSERT INTO questions VALUES (?1,?2) ON CONFLICT(chat) DO UPDATE SET body=excluded.body",
            params![chat, questions.to_string()]).map(|_| ()))
    }
}

fn binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<Binding> {
    Ok(Binding {
        chat_id: row.get(0)?,
        agent_id: row.get(1)?,
        conversation_id: row.get(2)?,
        draft: row.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase4_binding_preferences_are_scoped_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner { account: "one@example.test".into(), org: "one".into(), revision: 0 };
        let store = Store::new(dir.path());
        let binding = store.bind(&owner, "agent", "conversation", false).unwrap();
        assert!(!store.pinned(&owner, &binding.chat_id).unwrap());
        store.pin(&owner, &binding.chat_id, true).unwrap();
        for other in [Owner { account: "other@example.test".into(), ..owner.clone() }, Owner { org: "other".into(), ..owner.clone() }] {
            assert!(store.lookup(&other, "agent", "conversation").unwrap().is_none());
            assert!(store.pin(&other, &binding.chat_id, false).is_err());
            assert!(store.forget(&other, &binding.chat_id).is_err());
        }
        drop(store);
        let store = Store::new(dir.path());
        assert!(store.pinned(&owner, &binding.chat_id).unwrap());
        store.forget(&owner, &binding.chat_id).unwrap();
        assert!(store.get(&owner, &binding.chat_id).is_err());
        assert!(store.lookup(&owner, "agent", "conversation").unwrap().is_none());
        assert_ne!(store.bind(&owner, "agent", "conversation", false).unwrap().chat_id, binding.chat_id);
    }
}
