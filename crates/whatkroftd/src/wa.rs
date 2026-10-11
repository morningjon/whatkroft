//! The WhatsApp side: runs the `whatsapp-rust` client as a linked device and
//! turns its events into store writes and hub events.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::sync::{mpsc, oneshot, watch};
use whatkroft_ipc::{ConnState, Event as IpcEvent, Message, MessageKind, MessageStatus};
use whatsapp_rust::buffa::Enumeration;
use whatsapp_rust::prelude::*;
use whatsapp_rust::types::events::LazyHistorySync;
use whatsapp_rust::types::presence::ReceiptType;
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::wacore_binary::JidExt;

use crate::hub::Hub;
use crate::store::NewMessage;
use crate::summary::summarise;

/// Name shown under Linked Devices on the phone.
const DEVICE_NAME: &str = "WhatKroft";

/// The live client, shared with the IPC side so it can send. Empty while the
/// bot is being (re)built.
#[derive(Default)]
pub struct ClientSlot(Mutex<Option<Arc<Client>>>);

impl ClientSlot {
    pub fn get(&self) -> Option<Arc<Client>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, client: Option<Arc<Client>>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = client;
    }
}

/// Forwards every library event into an unbounded queue. The library calls
/// this inline on its dispatch path, so it must not block; one task drains
/// the queue in arrival order, which keeps messages ordered and drops none.
struct Forward(mpsc::UnboundedSender<Arc<Event>>);

impl EventHandler for Forward {
    fn handle_event(&self, event: Arc<Event>) {
        let _ = self.0.send(event);
    }
}

/// Run the WhatsApp session until `shutdown` fires. After the phone unlinks
/// us the stale session is deleted and a fresh pairing starts.
pub async fn run(
    hub: Arc<Hub>,
    session_db: PathBuf,
    slot: Arc<ClientSlot>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    loop {
        let path = session_db
            .to_str()
            .context("session database path is not valid UTF-8")?;
        let backend = SqliteStore::new(path)
            .await
            .map_err(|e| anyhow::anyhow!("opening session database: {e}"))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let bot = Bot::builder()
            .with_backend(backend)
            .with_event_handler(Forward(tx))
            .with_device_props(DevicePropsOverride {
                os: Some(DEVICE_NAME.to_owned()),
                ..Default::default()
            })
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("starting WhatsApp client: {e}"))?;

        let client = bot.client();
        slot.set(Some(Arc::clone(&client)));
        let (done_tx, done_rx) = oneshot::channel();
        let pump = tokio::spawn(pump(Arc::clone(&hub), client, rx, done_rx));

        let mut handle = bot.spawn();
        let stopping = tokio::select! {
            _ = &mut handle => false,
            _ = shutdown.changed() => {
                log::info!("shutting down WhatsApp client");
                handle.shutdown().await;
                true
            }
        };
        slot.set(None);
        let _ = done_tx.send(());
        let _ = pump.await;

        if stopping {
            return Ok(());
        }
        if hub.state() != ConnState::LoggedOut {
            anyhow::bail!("WhatsApp client stopped unexpectedly");
        }
        log::info!("device was unlinked; clearing the session to pair again");
        remove_session(&session_db);
    }
}

fn remove_session(session_db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = session_db.as_os_str().to_owned();
        name.push(suffix);
        match std::fs::remove_file(PathBuf::from(&name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("removing {}: {e}", Path::new(&name).display()),
        }
    }
}

/// Drain library events in order until the bot has stopped, then finish
/// whatever is still queued (the logout event arrives right at the end).
async fn pump(
    hub: Arc<Hub>,
    client: Arc<Client>,
    mut rx: mpsc::UnboundedReceiver<Arc<Event>>,
    mut done: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            event = rx.recv() => match event {
                Some(event) => handle(&hub, &client, &event).await,
                None => return,
            },
            _ = &mut done => break,
        }
    }
    while let Ok(event) = rx.try_recv() {
        handle(&hub, &client, &event).await;
    }
}

async fn handle(hub: &Hub, client: &Client, event: &Event) {
    match event {
        Event::PairingQrCode(qr) => hub.set_state(ConnState::Pairing {
            qr: qr.code.clone(),
        }),
        Event::PairSuccess(_) => hub.set_state(ConnState::Connecting),
        Event::Connected(_) => hub.set_state(ConnState::Connected),
        Event::Disconnected(_) => {
            // The socket also cycles while a QR code is on screen and right
            // after logout; neither is a dropped session.
            if matches!(hub.state(), ConnState::Connected | ConnState::Connecting) {
                hub.set_state(ConnState::Disconnected);
            }
        }
        Event::LoggedOut(_) => hub.set_state(ConnState::LoggedOut),
        Event::Messages(batch) => {
            for inbound in batch.iter() {
                on_message(hub, client, inbound).await;
            }
        }
        Event::Receipt(receipt) => {
            let chat = canonical(client, &receipt.source.chat).await;
            match receipt.r#type {
                ReceiptType::Delivered => {
                    raise_all(hub, &chat, &receipt.message_ids, MessageStatus::Delivered)
                }
                ReceiptType::Read | ReceiptType::Played => {
                    raise_all(hub, &chat, &receipt.message_ids, MessageStatus::Read)
                }
                // We read this chat on another of our devices.
                ReceiptType::ReadSelf => mark_read(hub, &chat),
                _ => {}
            }
        }
        Event::HistorySync(history) => on_history(hub, client, history).await,
        Event::PushNameUpdate(update) => {
            let jid = canonical(client, &update.jid).await;
            save_contact(hub, &jid, None, Some(&update.new_push_name));
        }
        Event::ContactUpdate(update) => {
            let jid = canonical(client, &update.jid).await;
            let name = update
                .action
                .full_name
                .as_deref()
                .or(update.action.first_name.as_deref());
            save_contact(hub, &jid, name, None);
        }
        Event::ClientOutdated(_) => {
            log::error!("WhatsApp rejected this client version as outdated; update WhatKroft")
        }
        Event::TemporaryBan(_) => log::error!("WhatsApp temporarily banned this account"),
        Event::StreamReplaced(_) => {
            log::warn!("this session was opened somewhere else and replaced")
        }
        _ => {}
    }
}

fn raise_all(hub: &Hub, chat: &str, ids: &[String], status: MessageStatus) {
    for id in ids {
        hub.raise_status(chat, id, status);
    }
}

fn mark_read(hub: &Hub, chat: &str) {
    match hub.store.mark_read(chat) {
        Ok(true) => hub.emit_chat(chat),
        Ok(false) => {}
        Err(e) => log::error!("marking {chat} read: {e}"),
    }
}

fn save_contact(hub: &Hub, jid: &str, full_name: Option<&str>, push_name: Option<&str>) {
    match hub.store.upsert_contact(jid, full_name, push_name) {
        Ok(()) => hub.emit_chat(jid),
        Err(e) => log::error!("saving contact {jid}: {e}"),
    }
}

/// One stable key per conversation partner. WhatsApp addresses the same
/// person by phone number JID or by LID depending on the path a message
/// took; without this a contact would show up as two chats. Falls back to
/// the LID when no mapping is known yet.
async fn canonical(client: &Client, jid: &Jid) -> String {
    let jid = jid.to_non_ad();
    if jid.is_lid()
        && let Ok(Some(entry)) = client.get_lid_pn_entry(&jid).await
    {
        return format!("{}@s.whatsapp.net", entry.phone_number);
    }
    jid.to_string()
}

async fn canonical_str(client: &Client, jid: &str) -> String {
    match jid.parse::<Jid>() {
        Ok(jid) => canonical(client, &jid).await,
        Err(_) => jid.to_owned(),
    }
}

/// Chats this milestone does not show: status updates and channels.
fn is_hidden(chat: &Jid) -> bool {
    chat.is_status_broadcast() || chat.is_newsletter()
}

async fn on_message(hub: &Hub, client: &Client, inbound: &InboundMessage) {
    let info = &inbound.info;
    let source = &info.source;
    if is_hidden(&source.chat) {
        return;
    }
    let Some((kind, text)) = summarise(&inbound.message) else {
        return;
    };
    let chat = canonical(client, &source.chat).await;
    let sender = if source.is_from_me {
        String::new()
    } else {
        let sender = canonical(client, &source.sender).await;
        if let Err(e) = hub
            .store
            .upsert_contact(&sender, None, Some(&info.push_name))
        {
            log::error!("saving contact {sender}: {e}");
        }
        sender
    };
    let message = NewMessage {
        chat,
        id: info.id.clone(),
        sender,
        from_me: source.is_from_me,
        timestamp: info.timestamp.timestamp(),
        kind,
        text,
        status: if source.is_from_me {
            MessageStatus::Sent
        } else {
            MessageStatus::Delivered
        },
    };
    if let Err(e) = hub.ingest(&message, true) {
        log::error!("storing message {}: {e}", message.id);
    }
}

/// Send a text message and record it locally.
pub async fn send_text(
    hub: &Hub,
    client: &Client,
    chat: &str,
    text: &str,
) -> Result<Message, String> {
    let to: Jid = chat.parse().map_err(|e| format!("invalid chat id: {e}"))?;
    let sent = client
        .send_message(to, wa::Message::text(text))
        .await
        .map_err(|e| e.to_string())?;
    let message = NewMessage {
        chat: chat.to_owned(),
        id: sent.message_id,
        sender: String::new(),
        from_me: true,
        timestamp: now(),
        kind: MessageKind::Text,
        text: text.to_owned(),
        status: MessageStatus::Sent,
    };
    hub.ingest(&message, false).map_err(|e| e.to_string())?;
    hub.store
        .get_message(&message.chat, &message.id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "sent message missing from the store".to_owned())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── History sync ────────────────────────────────────────────────────────

struct HistoryChat {
    jid: String,
    name: Option<String>,
    unread: u32,
    messages: Vec<HistoryMessage>,
}

struct HistoryMessage {
    id: String,
    from_me: bool,
    /// Raw author JID for incoming group messages.
    participant: Option<String>,
    push_name: Option<String>,
    timestamp: i64,
    kind: MessageKind,
    text: String,
    status: MessageStatus,
}

#[derive(Default)]
struct History {
    chats: Vec<HistoryChat>,
    push_names: Vec<(String, String)>,
}

/// Inflate and decode a history chunk, keeping only what the store needs.
/// CPU-bound, so it runs off the async threads.
fn extract_history(lazy: &LazyHistorySync) -> Option<History> {
    let sync = lazy.get()?;
    let mut history = History::default();

    for name in &sync.pushnames {
        if let (Some(id), Some(name)) = (&name.id, &name.pushname) {
            history.push_names.push((id.clone(), name.clone()));
        }
    }

    for conversation in &sync.conversations {
        let mut chat = HistoryChat {
            jid: conversation.id.clone(),
            name: conversation.name.clone().filter(|n| !n.is_empty()),
            unread: conversation.unread_count.unwrap_or(0),
            messages: Vec::with_capacity(conversation.messages.len()),
        };
        for entry in &conversation.messages {
            let Some(info) = entry.message.as_option() else {
                continue;
            };
            let (Some(key), Some(body)) = (info.key.as_option(), info.message.as_option()) else {
                continue;
            };
            let (Some(id), Some((kind, text))) = (key.id.clone(), summarise(body)) else {
                continue;
            };
            let from_me = key.from_me.unwrap_or(false);
            chat.messages.push(HistoryMessage {
                id,
                from_me,
                participant: info.participant.clone().or_else(|| key.participant.clone()),
                push_name: info.push_name.clone(),
                timestamp: info.message_timestamp.unwrap_or(0) as i64,
                kind,
                text,
                status: history_status(from_me, info.status.map(|s| s.to_i32())),
            });
        }
        history.chats.push(chat);
    }
    Some(history)
}

/// `WebMessageInfo.Status`: 0 error, 1 pending, 2 server ack, 3 delivered,
/// 4 read, 5 played.
fn history_status(from_me: bool, status: Option<i32>) -> MessageStatus {
    if !from_me {
        return MessageStatus::Delivered;
    }
    match status {
        Some(4 | 5) => MessageStatus::Read,
        Some(3) => MessageStatus::Delivered,
        Some(2) => MessageStatus::Sent,
        _ => MessageStatus::Pending,
    }
}

async fn on_history(hub: &Hub, client: &Client, lazy: &LazyHistorySync) {
    let owned = lazy.clone();
    let history = match tokio::task::spawn_blocking(move || extract_history(&owned)).await {
        Ok(Some(history)) => history,
        Ok(None) => {
            log::warn!("could not decode a history sync chunk");
            return;
        }
        Err(e) => {
            log::error!("history decode task failed: {e}");
            return;
        }
    };

    // Resolve every JID before touching the database: lookups are async and
    // the write below is one synchronous transaction.
    let mut push_names = Vec::with_capacity(history.push_names.len());
    for (jid, name) in history.push_names {
        push_names.push((canonical_str(client, &jid).await, name));
    }
    let mut chats = Vec::with_capacity(history.chats.len());
    for mut chat in history.chats {
        let Ok(jid) = chat.jid.parse::<Jid>() else {
            continue;
        };
        if is_hidden(&jid) {
            continue;
        }
        let is_group = jid.is_group();
        chat.jid = canonical(client, &jid).await;
        let mut senders = Vec::with_capacity(chat.messages.len());
        for message in &chat.messages {
            senders.push(if message.from_me {
                String::new()
            } else if is_group {
                match &message.participant {
                    Some(participant) => canonical_str(client, participant).await,
                    None => String::new(),
                }
            } else {
                chat.jid.clone()
            });
        }
        chats.push((chat, is_group, senders));
    }

    let (chat_count, message_count) = (
        chats.len(),
        chats
            .iter()
            .map(|(c, _, _)| c.messages.len())
            .sum::<usize>(),
    );
    let result = hub.store.batch(|batch| {
        for (jid, name) in &push_names {
            batch.upsert_contact(jid, None, Some(name))?;
        }
        for (chat, is_group, senders) in &chats {
            match (&chat.name, *is_group) {
                (Some(name), true) => batch.set_chat_name(&chat.jid, name, true)?,
                // For a direct chat the conversation name is the contact's.
                (Some(name), false) => batch.upsert_contact(&chat.jid, Some(name), None)?,
                (None, _) => {}
            }
            for (message, sender) in chat.messages.iter().zip(senders) {
                if !sender.is_empty() {
                    batch.upsert_contact(sender, None, message.push_name.as_deref())?;
                }
                batch.insert_message(
                    &NewMessage {
                        chat: chat.jid.clone(),
                        id: message.id.clone(),
                        sender: sender.clone(),
                        from_me: message.from_me,
                        timestamp: message.timestamp,
                        kind: message.kind,
                        text: message.text.clone(),
                        status: message.status,
                    },
                    false,
                )?;
            }
            // The phone's unread count is authoritative for imported chats.
            batch.set_unread(&chat.jid, chat.unread)?;
        }
        Ok(())
    });
    match result {
        Ok(()) => {
            log::info!("imported history: {chat_count} chats, {message_count} messages");
            hub.emit(IpcEvent::ChatsChanged);
        }
        Err(e) => log::error!("importing history: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_status_mapping() {
        assert_eq!(history_status(false, Some(1)), MessageStatus::Delivered);
        assert_eq!(history_status(true, None), MessageStatus::Pending);
        assert_eq!(history_status(true, Some(2)), MessageStatus::Sent);
        assert_eq!(history_status(true, Some(3)), MessageStatus::Delivered);
        assert_eq!(history_status(true, Some(4)), MessageStatus::Read);
        assert_eq!(history_status(true, Some(5)), MessageStatus::Read);
    }
}
