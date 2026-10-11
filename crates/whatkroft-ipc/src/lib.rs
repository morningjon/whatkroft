//! Wire protocol between `whatkroftd` and its clients.
//!
//! Transport is a Unix domain socket carrying newline-delimited JSON. A client
//! sends [`RequestEnvelope`] lines; the daemon answers each with a
//! [`ServerMessage::Reply`] carrying the same `id`, and pushes
//! [`ServerMessage::Event`] lines whenever something changes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where the daemon listens. `$XDG_RUNTIME_DIR/whatkroft/daemon.sock`, with a
/// per-user fallback under the temp dir when no runtime dir is set.
pub fn socket_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("WHATKROFT_SOCKET") {
        return PathBuf::from(explicit);
    }
    runtime_dir().join("daemon.sock")
}

/// Directory holding the socket. Created by the daemon with mode 0700.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("whatkroft"),
        _ => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("whatkroft-{user}"))
        }
    }
}

/// Connection state of the daemon's WhatsApp session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    /// Daemon is up but has not heard from WhatsApp yet.
    Starting,
    /// Not linked. `qr` is the payload to render as a QR code; it rotates.
    Pairing {
        qr: String,
    },
    /// Linked, session being established.
    Connecting,
    Connected,
    /// Linked but the transport dropped; the daemon reconnects on its own.
    Disconnected,
    /// The phone unlinked this device.
    LoggedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub jid: String,
    pub name: String,
    pub is_group: bool,
    pub last_text: Option<String>,
    /// Unix seconds of the newest message, 0 when the chat is empty.
    pub last_timestamp: i64,
    pub unread: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    Image,
    Video,
    Audio,
    Document,
    Sticker,
    Location,
    Contact,
    Poll,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
            Self::Location => "location",
            Self::Contact => "contact",
            Self::Poll => "poll",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "document" => Self::Document,
            "sticker" => Self::Sticker,
            "location" => Self::Location,
            "contact" => Self::Contact,
            "poll" => Self::Poll,
            _ => Self::Text,
        }
    }
}

/// Delivery state of an outgoing message. Ordered: a status only ever moves up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Pending,
    Sent,
    Delivered,
    Read,
}

impl MessageStatus {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::Pending,
            1 => Self::Sent,
            2 => Self::Delivered,
            _ => Self::Read,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub chat: String,
    pub id: String,
    /// JID of the author; empty for our own messages.
    pub sender: String,
    /// Display name of the author, resolved by the daemon. Only set for
    /// incoming messages.
    pub sender_name: Option<String>,
    pub from_me: bool,
    /// Unix seconds.
    pub timestamp: i64,
    pub kind: MessageKind,
    /// Body text, caption, or a short description for non-text kinds.
    pub text: String,
    pub status: MessageStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    GetState,
    ListChats,
    /// Messages of `chat`, oldest first. `before` pages backwards from a
    /// Unix timestamp; `limit` caps the page size.
    ListMessages {
        chat: String,
        before: Option<i64>,
        limit: u32,
    },
    SendText {
        chat: String,
        text: String,
    },
    /// Clear the local unread counter. Does not send read receipts.
    MarkRead {
        chat: String,
    },
    /// Unlink this device from the WhatsApp account.
    Logout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Reply {
    State(ConnState),
    Chats(Vec<Chat>),
    Messages {
        chat: String,
        messages: Vec<Message>,
    },
    Sent(Message),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    State(ConnState),
    /// One chat was created or changed (new message, name, unread count).
    Chat(Chat),
    /// Many chats changed at once (history import); re-request the list.
    ChatsChanged,
    Message(Message),
    Status {
        chat: String,
        id: String,
        status: MessageStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Reply {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<Reply>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Event {
        #[serde(flatten)]
        event: Event,
    },
}

impl ServerMessage {
    pub fn reply(id: u64, result: Result<Reply, String>) -> Self {
        match result {
            Ok(reply) => Self::Reply {
                id,
                ok: Some(reply),
                error: None,
            },
            Err(error) => Self::Reply {
                id,
                ok: None,
                error: Some(error),
            },
        }
    }
}

/// Serialise one protocol value as a single newline-terminated JSON line.
pub fn encode_line<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).expect("protocol types always serialise");
    line.push('\n');
    line
}

pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: T)
    where
        T: Serialize + for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug,
    {
        let line = encode_line(&value);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "one line per value: {line}");
        assert_eq!(decode_line::<T>(&line).unwrap(), value);
    }

    #[test]
    fn requests_roundtrip() {
        for request in [
            Request::GetState,
            Request::ListChats,
            Request::ListMessages {
                chat: "1@s.whatsapp.net".into(),
                before: Some(42),
                limit: 50,
            },
            Request::SendText {
                chat: "1@s.whatsapp.net".into(),
                text: "hello\nworld".into(),
            },
            Request::MarkRead {
                chat: "g@g.us".into(),
            },
            Request::Logout,
        ] {
            roundtrip(RequestEnvelope { id: 7, request });
        }
    }

    #[test]
    fn request_wire_shape_is_stable() {
        let line = encode_line(&RequestEnvelope {
            id: 1,
            request: Request::MarkRead { chat: "a".into() },
        });
        assert_eq!(
            line.trim_end(),
            r#"{"id":1,"method":"mark_read","params":{"chat":"a"}}"#
        );
        let parsed: RequestEnvelope = decode_line(r#"{"id":2,"method":"list_chats"}"#).unwrap();
        assert_eq!(parsed.request, Request::ListChats);
    }

    #[test]
    fn server_messages_roundtrip() {
        let message = Message {
            chat: "g@g.us".into(),
            id: "ABC".into(),
            sender: "2@s.whatsapp.net".into(),
            sender_name: Some("Sam".into()),
            from_me: false,
            timestamp: 1_760_000_000,
            kind: MessageKind::Image,
            text: "Photo".into(),
            status: MessageStatus::Delivered,
        };
        roundtrip(ServerMessage::reply(1, Ok(Reply::Done)));
        roundtrip(ServerMessage::reply(2, Err("nope".into())));
        roundtrip(ServerMessage::reply(
            3,
            Ok(Reply::State(ConnState::Pairing { qr: "2@abc".into() })),
        ));
        roundtrip(ServerMessage::reply(
            4,
            Ok(Reply::Messages {
                chat: "g@g.us".into(),
                messages: vec![message.clone()],
            }),
        ));
        roundtrip(ServerMessage::Event {
            event: Event::Message(message),
        });
        roundtrip(ServerMessage::Event {
            event: Event::ChatsChanged,
        });
        roundtrip(ServerMessage::Event {
            event: Event::State(ConnState::Connected),
        });
        roundtrip(ServerMessage::Event {
            event: Event::Status {
                chat: "a".into(),
                id: "b".into(),
                status: MessageStatus::Read,
            },
        });
    }

    #[test]
    fn status_only_moves_up() {
        assert!(MessageStatus::Read > MessageStatus::Delivered);
        assert!(MessageStatus::Delivered > MessageStatus::Sent);
        assert!(MessageStatus::Sent > MessageStatus::Pending);
        for status in [
            MessageStatus::Pending,
            MessageStatus::Sent,
            MessageStatus::Delivered,
            MessageStatus::Read,
        ] {
            assert_eq!(MessageStatus::from_i64(status.as_i64()), status);
        }
    }
}
