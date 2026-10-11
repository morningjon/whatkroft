//! Demo mode: a made-up account served entirely from memory.
//!
//! Lets the UI be developed and tested without linking a real WhatsApp
//! account. Nothing here touches the network.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use whatkroft_ipc::{ConnState, Message, MessageKind, MessageStatus};

use crate::hub::Hub;
use crate::store::NewMessage;
use crate::wa::now;

const AVA: &str = "447700900101@s.whatsapp.net";
const BEN: &str = "447700900102@s.whatsapp.net";
const CLUB: &str = "120363000000000042@g.us";
const UNKNOWN: &str = "447700900199@s.whatsapp.net";

/// A QR payload shaped like a real pairing code, for the pairing screen.
const DEMO_QR: &str = "2@demoDEMOdemoDEMOdemoDEMOdemoDEMOdemoDEMO,ZGVtbw==,ZGVtbw==,ZGVtbw==";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> String {
    format!("DEMO{:08}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

pub fn start(hub: &Arc<Hub>, pairing: bool) -> rusqlite::Result<()> {
    if pairing {
        hub.set_state(ConnState::Pairing {
            qr: DEMO_QR.to_owned(),
        });
        return Ok(());
    }
    seed(hub)?;
    hub.set_state(ConnState::Connected);
    Ok(())
}

fn seed(hub: &Hub) -> rusqlite::Result<()> {
    let store = &hub.store;
    store.upsert_contact(AVA, Some("Ava Lindqvist"), Some("ava"))?;
    store.upsert_contact(BEN, None, Some("Ben"))?;
    store.set_chat_name(CLUB, "Sunday Run Club", true)?;

    let t = now();
    let rows: &[(&str, &str, bool, i64, MessageKind, &str, MessageStatus)] = &[
        (
            AVA,
            AVA,
            false,
            3 * 86_400,
            MessageKind::Text,
            "Are we still on for Thursday?",
            MessageStatus::Delivered,
        ),
        (
            AVA,
            "",
            true,
            3 * 86_400 - 120,
            MessageKind::Text,
            "Yes! 7pm at the usual place.",
            MessageStatus::Read,
        ),
        (
            AVA,
            AVA,
            false,
            5_400,
            MessageKind::Image,
            "Photo: the view from the top",
            MessageStatus::Delivered,
        ),
        (
            AVA,
            AVA,
            false,
            5_340,
            MessageKind::Text,
            "You have to come next time, it was unreal.",
            MessageStatus::Delivered,
        ),
        (
            AVA,
            "",
            true,
            5_000,
            MessageKind::Text,
            "That looks incredible. Count me in.",
            MessageStatus::Delivered,
        ),
        (
            CLUB,
            BEN,
            false,
            9_000,
            MessageKind::Text,
            "Route for Sunday is up. 12k, mostly flat.",
            MessageStatus::Delivered,
        ),
        (
            CLUB,
            AVA,
            false,
            8_700,
            MessageKind::Text,
            "Perfect, I need a gentle one after last week 😅",
            MessageStatus::Delivered,
        ),
        (
            CLUB,
            "",
            true,
            8_400,
            MessageKind::Text,
            "I'll bring coffee for after.",
            MessageStatus::Read,
        ),
        (
            CLUB,
            BEN,
            false,
            1_200,
            MessageKind::Poll,
            "Poll: Start time?",
            MessageStatus::Delivered,
        ),
        (
            BEN,
            BEN,
            false,
            86_400,
            MessageKind::Audio,
            "Voice message",
            MessageStatus::Delivered,
        ),
        (
            BEN,
            "",
            true,
            86_000,
            MessageKind::Text,
            "Will listen in a bit, on a call.",
            MessageStatus::Sent,
        ),
        (
            UNKNOWN,
            UNKNOWN,
            false,
            600,
            MessageKind::Text,
            "Hi, is this still available?",
            MessageStatus::Delivered,
        ),
    ];
    for (chat, sender, from_me, age, kind, text, status) in rows {
        // Only the two newest chats start with unread messages.
        let unread = *age < 2_000;
        store.insert_message(
            &NewMessage {
                chat: (*chat).to_owned(),
                id: next_id(),
                sender: (*sender).to_owned(),
                from_me: *from_me,
                timestamp: t - age,
                kind: *kind,
                text: (*text).to_owned(),
                status: *status,
            },
            unread,
        )?;
    }
    Ok(())
}

/// "Send" a message: it is delivered, read, and answered by an echo.
pub fn send_text(hub: &Arc<Hub>, chat: &str, text: &str) -> Result<Message, String> {
    let message = NewMessage {
        chat: chat.to_owned(),
        id: next_id(),
        sender: String::new(),
        from_me: true,
        timestamp: now(),
        kind: MessageKind::Text,
        text: text.to_owned(),
        status: MessageStatus::Sent,
    };
    let stored = hub
        .ingest(&message, false)
        .map_err(|e| e.to_string())?
        .ok_or("duplicate message id")?;

    let hub = Arc::clone(hub);
    let (chat, id, text) = (message.chat, message.id, message.text);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        hub.raise_status(&chat, &id, MessageStatus::Delivered);
        tokio::time::sleep(Duration::from_millis(900)).await;
        hub.raise_status(&chat, &id, MessageStatus::Read);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let sender = if chat.ends_with("@g.us") {
            BEN
        } else {
            chat.as_str()
        };
        let reply = NewMessage {
            chat: chat.clone(),
            id: next_id(),
            sender: sender.to_owned(),
            from_me: false,
            timestamp: now(),
            kind: MessageKind::Text,
            text: format!("Echo: {text}"),
            status: MessageStatus::Delivered,
        };
        if let Err(e) = hub.ingest(&reply, true) {
            log::error!("demo reply: {e}");
        }
    });
    Ok(stored)
}
