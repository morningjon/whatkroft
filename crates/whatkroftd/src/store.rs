//! Local chat store: chats, contacts and messages in SQLite.
//!
//! This is WhatKroft's own database, separate from the session database that
//! `whatsapp-rust` keeps for keys and protocol state. Everything the UI shows
//! is read from here, so the app works offline and starts instantly.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use whatkroft_ipc::{Chat, Message, MessageKind, MessageStatus};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = "
CREATE TABLE chats (
    jid            TEXT PRIMARY KEY,
    name           TEXT,
    is_group       INTEGER NOT NULL DEFAULT 0,
    last_timestamp INTEGER NOT NULL DEFAULT 0,
    last_text      TEXT,
    unread         INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE contacts (
    jid       TEXT PRIMARY KEY,
    full_name TEXT,
    push_name TEXT
) WITHOUT ROWID;

CREATE TABLE messages (
    chat_jid   TEXT    NOT NULL,
    id         TEXT    NOT NULL,
    sender_jid TEXT    NOT NULL,
    from_me    INTEGER NOT NULL,
    timestamp  INTEGER NOT NULL,
    kind       TEXT    NOT NULL,
    text       TEXT    NOT NULL,
    status     INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, id)
);

CREATE INDEX messages_by_chat_time ON messages (chat_jid, timestamp);
";

/// Display name for a chat or contact: the name we know, else the push name,
/// else NULL (the caller falls back to the number).
const NAME_EXPR: &str = "COALESCE(NULLIF(ct.full_name, ''), NULLIF(ct.push_name, ''))";

pub struct Store {
    conn: Mutex<Connection>,
}

/// A message to insert, before the daemon has resolved display names.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub chat: String,
    pub id: String,
    pub sender: String,
    pub from_me: bool,
    pub timestamp: i64,
    pub kind: MessageKind,
    pub text: String,
    pub status: MessageStatus,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> rusqlite::Result<Self> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite half-written
        // (statements are atomic), so a poisoned lock is still usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run several writes as one transaction (used for history import).
    pub fn batch<T>(
        &self,
        f: impl FnOnce(&Batch<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let out = f(&Batch { conn: &tx })?;
        tx.commit()?;
        Ok(out)
    }

    pub fn upsert_contact(
        &self,
        jid: &str,
        full_name: Option<&str>,
        push_name: Option<&str>,
    ) -> rusqlite::Result<()> {
        Batch { conn: &self.lock() }.upsert_contact(jid, full_name, push_name)
    }

    pub fn set_chat_name(&self, jid: &str, name: &str, is_group: bool) -> rusqlite::Result<()> {
        Batch { conn: &self.lock() }.set_chat_name(jid, name, is_group)
    }

    /// Insert a message and bring its chat up to date. Returns `false` when
    /// the message was already stored (redelivery), in which case nothing
    /// changes.
    pub fn insert_message(&self, msg: &NewMessage, count_unread: bool) -> rusqlite::Result<bool> {
        self.batch(|b| b.insert_message(msg, count_unread))
    }

    /// Raise the delivery status of one of our own messages. Returns `true`
    /// when the stored status changed; statuses never move down.
    pub fn raise_status(
        &self,
        chat: &str,
        id: &str,
        status: MessageStatus,
    ) -> rusqlite::Result<bool> {
        let changed = self.lock().execute(
            "UPDATE messages SET status = ?3
             WHERE chat_jid = ?1 AND id = ?2 AND from_me = 1 AND status < ?3",
            params![chat, id, status.as_i64()],
        )?;
        Ok(changed > 0)
    }

    pub fn mark_read(&self, chat: &str) -> rusqlite::Result<bool> {
        let changed = self.lock().execute(
            "UPDATE chats SET unread = 0 WHERE jid = ?1 AND unread <> 0",
            params![chat],
        )?;
        Ok(changed > 0)
    }

    pub fn list_chats(&self) -> rusqlite::Result<Vec<Chat>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT c.jid, COALESCE(NULLIF(c.name, ''), {NAME_EXPR}), c.is_group,
                    c.last_text, c.last_timestamp, c.unread
             FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid
             ORDER BY c.last_timestamp DESC, c.jid"
        ))?;
        let rows = stmt.query_map([], chat_from_row)?;
        rows.collect()
    }

    pub fn get_chat(&self, jid: &str) -> rusqlite::Result<Option<Chat>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT c.jid, COALESCE(NULLIF(c.name, ''), {NAME_EXPR}), c.is_group,
                    c.last_text, c.last_timestamp, c.unread
             FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid
             WHERE c.jid = ?1"
        ))?;
        stmt.query_row(params![jid], chat_from_row).optional()
    }

    pub fn get_message(&self, chat: &str, id: &str) -> rusqlite::Result<Option<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT m.chat_jid, m.id, m.sender_jid, {NAME_EXPR}, m.from_me, m.timestamp,
                    m.kind, m.text, m.status
             FROM messages m LEFT JOIN contacts ct ON ct.jid = m.sender_jid
             WHERE m.chat_jid = ?1 AND m.id = ?2"
        ))?;
        stmt.query_row(params![chat, id], message_from_row)
            .optional()
    }

    /// Up to `limit` messages of `chat` older than `before` (Unix seconds),
    /// returned oldest first.
    pub fn list_messages(
        &self,
        chat: &str,
        before: Option<i64>,
        limit: u32,
    ) -> rusqlite::Result<Vec<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT m.chat_jid, m.id, m.sender_jid, {NAME_EXPR}, m.from_me, m.timestamp,
                    m.kind, m.text, m.status
             FROM messages m LEFT JOIN contacts ct ON ct.jid = m.sender_jid
             WHERE m.chat_jid = ?1 AND m.timestamp < ?2
             ORDER BY m.timestamp DESC, m.rowid DESC
             LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            params![chat, before.unwrap_or(i64::MAX), limit],
            message_from_row,
        )?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }
}

/// Write operations usable inside [`Store::batch`].
pub struct Batch<'a> {
    conn: &'a Connection,
}

impl Batch<'_> {
    /// `None` leaves the stored value alone; names are never blanked.
    pub fn upsert_contact(
        &self,
        jid: &str,
        full_name: Option<&str>,
        push_name: Option<&str>,
    ) -> rusqlite::Result<()> {
        let full_name = full_name.filter(|s| !s.is_empty());
        let push_name = push_name.filter(|s| !s.is_empty());
        if full_name.is_none() && push_name.is_none() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO contacts (jid, full_name, push_name) VALUES (?1, ?2, ?3)
             ON CONFLICT (jid) DO UPDATE SET
                 full_name = COALESCE(excluded.full_name, full_name),
                 push_name = COALESCE(excluded.push_name, push_name)",
            params![jid, full_name, push_name],
        )?;
        Ok(())
    }

    pub fn set_chat_name(&self, jid: &str, name: &str, is_group: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO chats (jid, name, is_group) VALUES (?1, ?2, ?3)
             ON CONFLICT (jid) DO UPDATE SET name = excluded.name, is_group = excluded.is_group",
            params![jid, name, is_group],
        )?;
        Ok(())
    }

    pub fn set_unread(&self, jid: &str, unread: u32) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE chats SET unread = ?2 WHERE jid = ?1",
            params![jid, unread],
        )?;
        Ok(())
    }

    pub fn insert_message(&self, msg: &NewMessage, count_unread: bool) -> rusqlite::Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO messages
                 (chat_jid, id, sender_jid, from_me, timestamp, kind, text, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                msg.chat,
                msg.id,
                msg.sender,
                msg.from_me,
                msg.timestamp,
                msg.kind.as_str(),
                msg.text,
                msg.status.as_i64(),
            ],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        let unread = i64::from(count_unread && !msg.from_me);
        let is_group = msg.chat.ends_with("@g.us");
        // The preview follows the newest message, which is not always the one
        // just inserted: history arrives out of order.
        self.conn.execute(
            "INSERT INTO chats (jid, is_group, last_timestamp, last_text, unread)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (jid) DO UPDATE SET
                 last_text = CASE WHEN excluded.last_timestamp >= last_timestamp
                                  THEN excluded.last_text ELSE last_text END,
                 last_timestamp = MAX(last_timestamp, excluded.last_timestamp),
                 unread = unread + excluded.unread",
            params![msg.chat, is_group, msg.timestamp, msg.text, unread],
        )?;
        Ok(true)
    }
}

fn chat_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chat> {
    let jid: String = row.get(0)?;
    let name: Option<String> = row.get(1)?;
    Ok(Chat {
        name: name.unwrap_or_else(|| fallback_name(&jid)),
        jid,
        is_group: row.get(2)?,
        last_text: row.get(3)?,
        last_timestamp: row.get(4)?,
        unread: row.get(5)?,
    })
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let sender: String = row.get(2)?;
    let from_me: bool = row.get(4)?;
    let name: Option<String> = row.get(3)?;
    let kind: String = row.get(6)?;
    Ok(Message {
        chat: row.get(0)?,
        id: row.get(1)?,
        sender_name: if from_me {
            None
        } else {
            Some(name.unwrap_or_else(|| fallback_name(&sender)))
        },
        sender,
        from_me,
        timestamp: row.get(5)?,
        kind: MessageKind::parse(&kind),
        text: row.get(7)?,
        status: MessageStatus::from_i64(row.get(8)?),
    })
}

/// Name shown when we know nothing about a JID: the phone number for a
/// regular account, the bare user part otherwise.
pub fn fallback_name(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, "s.whatsapp.net")) if !user.is_empty() => format!("+{user}"),
        Some((_, "g.us")) => "Group".to_owned(),
        Some((user, _)) if !user.is_empty() => user.to_owned(),
        _ => jid.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "447700900001@s.whatsapp.net";
    const GROUP: &str = "120363000000000001@g.us";

    fn msg(chat: &str, id: &str, ts: i64, from_me: bool, text: &str) -> NewMessage {
        NewMessage {
            chat: chat.into(),
            id: id.into(),
            sender: if from_me { String::new() } else { ALICE.into() },
            from_me,
            timestamp: ts,
            kind: MessageKind::Text,
            text: text.into(),
            status: if from_me {
                MessageStatus::Sent
            } else {
                MessageStatus::Delivered
            },
        }
    }

    #[test]
    fn insert_creates_chat_and_counts_unread() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .insert_message(&msg(ALICE, "1", 100, false, "hi"), true)
                .unwrap()
        );
        assert!(
            store
                .insert_message(&msg(ALICE, "2", 101, false, "there"), true)
                .unwrap()
        );
        assert!(
            store
                .insert_message(&msg(ALICE, "3", 102, true, "hello"), true)
                .unwrap()
        );

        let chats = store.list_chats().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].unread, 2, "own messages are not unread");
        assert_eq!(chats[0].last_text.as_deref(), Some("hello"));
        assert_eq!(chats[0].last_timestamp, 102);
        assert_eq!(chats[0].name, "+447700900001");
        assert!(!chats[0].is_group);

        assert!(store.mark_read(ALICE).unwrap());
        assert!(!store.mark_read(ALICE).unwrap(), "already read");
        assert_eq!(store.get_chat(ALICE).unwrap().unwrap().unread, 0);
    }

    #[test]
    fn redelivery_is_ignored() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .insert_message(&msg(ALICE, "1", 100, false, "hi"), true)
                .unwrap()
        );
        assert!(
            !store
                .insert_message(&msg(ALICE, "1", 100, false, "hi"), true)
                .unwrap()
        );
        assert_eq!(store.get_chat(ALICE).unwrap().unwrap().unread, 1);
        assert_eq!(store.list_messages(ALICE, None, 10).unwrap().len(), 1);
    }

    #[test]
    fn older_message_does_not_replace_preview() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_message(&msg(ALICE, "new", 200, false, "newest"), false)
            .unwrap();
        store
            .insert_message(&msg(ALICE, "old", 100, false, "older"), false)
            .unwrap();
        let chat = store.get_chat(ALICE).unwrap().unwrap();
        assert_eq!(chat.last_text.as_deref(), Some("newest"));
        assert_eq!(chat.last_timestamp, 200);
        assert_eq!(chat.unread, 0, "history import does not count unread");
    }

    #[test]
    fn messages_page_oldest_first() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..10 {
            store
                .insert_message(&msg(ALICE, &format!("m{i}"), 100 + i, false, "x"), false)
                .unwrap();
        }
        let page = store.list_messages(ALICE, None, 3).unwrap();
        let ids: Vec<_> = page.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m7", "m8", "m9"]);

        let earlier = store
            .list_messages(ALICE, Some(page[0].timestamp), 3)
            .unwrap();
        let ids: Vec<_> = earlier.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m4", "m5", "m6"]);
    }

    #[test]
    fn status_never_moves_down_and_only_for_own_messages() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_message(&msg(ALICE, "out", 100, true, "x"), false)
            .unwrap();
        store
            .insert_message(&msg(ALICE, "in", 101, false, "y"), false)
            .unwrap();

        assert!(
            store
                .raise_status(ALICE, "out", MessageStatus::Read)
                .unwrap()
        );
        assert!(
            !store
                .raise_status(ALICE, "out", MessageStatus::Delivered)
                .unwrap()
        );
        assert!(
            !store
                .raise_status(ALICE, "in", MessageStatus::Read)
                .unwrap()
        );
        assert!(
            !store
                .raise_status(ALICE, "missing", MessageStatus::Read)
                .unwrap()
        );

        let out = store.get_message(ALICE, "out").unwrap().unwrap();
        assert_eq!(out.status, MessageStatus::Read);
        assert_eq!(out.sender_name, None);
    }

    #[test]
    fn names_resolve_through_contacts() {
        let store = Store::open_in_memory().unwrap();
        let mut m = msg(GROUP, "1", 100, false, "hi all");
        m.sender = ALICE.into();
        store.insert_message(&m, true).unwrap();
        assert_eq!(store.get_chat(GROUP).unwrap().unwrap().name, "Group");
        assert!(store.get_chat(GROUP).unwrap().unwrap().is_group);

        store.upsert_contact(ALICE, None, Some("ali")).unwrap();
        let got = store.get_message(GROUP, "1").unwrap().unwrap();
        assert_eq!(got.sender_name.as_deref(), Some("ali"));

        // The address-book name wins over the push name, and an empty update
        // does not erase what we know.
        store
            .upsert_contact(ALICE, Some("Alice Example"), None)
            .unwrap();
        store.upsert_contact(ALICE, Some(""), Some("")).unwrap();
        let got = store.get_message(GROUP, "1").unwrap().unwrap();
        assert_eq!(got.sender_name.as_deref(), Some("Alice Example"));

        store.set_chat_name(GROUP, "Family", true).unwrap();
        assert_eq!(store.get_chat(GROUP).unwrap().unwrap().name, "Family");

        // A direct chat is named after its contact.
        store
            .insert_message(&msg(ALICE, "2", 50, false, "yo"), false)
            .unwrap();
        assert_eq!(
            store.get_chat(ALICE).unwrap().unwrap().name,
            "Alice Example"
        );
    }

    #[test]
    fn chats_sort_newest_first() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_message(&msg(ALICE, "1", 100, false, "a"), false)
            .unwrap();
        store
            .insert_message(&msg(GROUP, "2", 200, false, "b"), false)
            .unwrap();
        let jids: Vec<_> = store
            .list_chats()
            .unwrap()
            .into_iter()
            .map(|c| c.jid)
            .collect();
        assert_eq!(jids, [GROUP, ALICE]);
    }

    #[test]
    fn schema_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("whatkroft-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .insert_message(&msg(ALICE, "1", 100, false, "kept"), true)
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.list_messages(ALICE, None, 10).unwrap()[0].text,
            "kept"
        );
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fallback_names() {
        assert_eq!(fallback_name("123@s.whatsapp.net"), "+123");
        assert_eq!(fallback_name("999@lid"), "999");
        assert_eq!(fallback_name("1-2@g.us"), "Group");
        assert_eq!(fallback_name("weird"), "weird");
    }
}
