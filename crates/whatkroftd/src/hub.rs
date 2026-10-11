//! Shared daemon state: the store, the current connection state and the
//! event fan-out to every connected client.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use tokio::sync::{Semaphore, broadcast};
use whatkroft_ipc::{ConnState, Event, Message, MessageStatus};

use crate::store::{NewMessage, Store};

/// Attachments fetched at the same time, across all clients.
const PARALLEL_DOWNLOADS: usize = 4;

pub struct Hub {
    pub store: Store,
    /// Where downloaded and sent attachments are kept.
    pub media_dir: PathBuf,
    /// Limits concurrent attachment downloads.
    pub download_slots: Semaphore,
    /// (chat, message id) of downloads in flight, so a second request for
    /// the same attachment does not fetch it twice.
    downloading: Mutex<HashSet<(String, String)>>,
    state: Mutex<ConnState>,
    events: broadcast::Sender<Event>,
}

impl Hub {
    pub fn new(store: Store, media_dir: PathBuf) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            store,
            media_dir,
            download_slots: Semaphore::new(PARALLEL_DOWNLOADS),
            downloading: Mutex::new(HashSet::new()),
            state: Mutex::new(ConnState::Starting),
            events,
        }
    }

    /// Claim a download. `false` means it is already under way.
    pub fn begin_download(&self, chat: &str, id: &str) -> bool {
        self.downloading
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((chat.to_owned(), id.to_owned()))
    }

    pub fn end_download(&self, chat: &str, id: &str) {
        self.downloading
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(chat.to_owned(), id.to_owned()));
    }

    pub fn set_archived(&self, chat: &str, archived: bool) {
        match self.store.set_archived(chat, archived) {
            Ok(true) => self.emit_chat(chat),
            Ok(false) => {}
            Err(e) => log::error!("archiving {chat}: {e}"),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn state(&self) -> ConnState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_state(&self, new: ConnState) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if *state == new {
                return;
            }
            *state = new.clone();
        }
        self.emit(Event::State(new));
    }

    pub fn emit(&self, event: Event) {
        // Err only means no client is connected right now.
        let _ = self.events.send(event);
    }

    /// Tell clients that `jid`'s row in the chat list changed.
    pub fn emit_chat(&self, jid: &str) {
        match self.store.get_chat(jid) {
            Ok(Some(chat)) => self.emit(Event::Chat(chat)),
            Ok(None) => {}
            Err(e) => log::error!("reading chat {jid}: {e}"),
        }
    }

    /// Store a message and announce it. Returns the stored message with names
    /// resolved, or `None` if it was a duplicate.
    pub fn ingest(
        &self,
        msg: &NewMessage,
        count_unread: bool,
    ) -> rusqlite::Result<Option<Message>> {
        if !self.store.insert_message(msg, count_unread)? {
            return Ok(None);
        }
        let stored = self.store.get_message(&msg.chat, &msg.id)?;
        if let Some(stored) = &stored {
            self.emit(Event::Message(stored.clone()));
        }
        self.emit_chat(&msg.chat);
        Ok(stored)
    }

    pub fn raise_status(&self, chat: &str, id: &str, status: MessageStatus) {
        match self.store.raise_status(chat, id, status) {
            Ok(true) => self.emit(Event::Status {
                chat: chat.to_owned(),
                id: id.to_owned(),
                status,
            }),
            Ok(false) => {}
            Err(e) => log::error!("updating status of {id}: {e}"),
        }
    }
}
