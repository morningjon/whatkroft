//! The local IPC server: newline-delimited JSON over a Unix socket.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};
use whatkroft_ipc::{
    Event, Reply, Request, RequestEnvelope, ServerMessage, decode_line, encode_line,
};

use crate::demo;
use crate::hub::Hub;
use crate::wa;

/// Largest page a client may ask for in one `list_messages`.
const MAX_PAGE: u32 = 500;
/// Lines queued per client before it is considered stuck and dropped.
const CLIENT_QUEUE: usize = 4096;

pub enum Backend {
    WhatsApp(Arc<wa::ClientSlot>),
    Demo,
}

pub struct Daemon {
    pub hub: Arc<Hub>,
    pub backend: Backend,
}

pub async fn serve(
    listener: UnixListener,
    daemon: Arc<Daemon>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(connection(stream, Arc::clone(&daemon)));
                }
                Err(e) => {
                    log::error!("accepting a client: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            },
            _ = shutdown.changed() => return,
        }
    }
}

async fn connection(stream: UnixStream, daemon: Arc<Daemon>) {
    log::debug!("client connected");
    let (read, mut write) = stream.into_split();
    let (out, mut queue) = mpsc::channel::<String>(CLIENT_QUEUE);

    let writer = tokio::spawn(async move {
        while let Some(line) = queue.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Subscribe before reading the state so no change falls in between.
    let mut events = daemon.hub.subscribe();
    let hello = ServerMessage::Event {
        event: Event::State(daemon.hub.state()),
    };
    let _ = out.try_send(encode_line(&hello));

    let pusher = {
        let out = out.clone();
        let hub = Arc::clone(&daemon.hub);
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    // The client fell behind and missed events: have it
                    // reload instead of showing stale data.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let state = ServerMessage::Event {
                            event: Event::State(hub.state()),
                        };
                        if out.try_send(encode_line(&state)).is_err() {
                            break;
                        }
                        Event::ChatsChanged
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if out
                    .try_send(encode_line(&ServerMessage::Event { event }))
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let envelope: RequestEnvelope = match decode_line(&line) {
            Ok(envelope) => envelope,
            Err(e) => {
                log::warn!("ignoring malformed request: {e}");
                continue;
            }
        };
        // Requests run concurrently: a send waiting on the network must not
        // hold up scrolling through history.
        let out = out.clone();
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let result = dispatch(&daemon, envelope.request).await;
            let _ = out
                .send(encode_line(&ServerMessage::reply(envelope.id, result)))
                .await;
        });
    }

    pusher.abort();
    drop(out);
    let _ = writer.await;
    log::debug!("client disconnected");
}

async fn dispatch(daemon: &Daemon, request: Request) -> Result<Reply, String> {
    let hub = &daemon.hub;
    let db = |e: rusqlite::Error| format!("database error: {e}");
    match request {
        Request::GetState => Ok(Reply::State(hub.state())),
        Request::ListChats => hub.store.list_chats().map(Reply::Chats).map_err(db),
        Request::ListMessages {
            chat,
            before,
            limit,
        } => {
            let messages = hub
                .store
                .list_messages(&chat, before, limit.clamp(1, MAX_PAGE))
                .map_err(db)?;
            Ok(Reply::Messages { chat, messages })
        }
        Request::MarkRead { chat } => {
            if hub.store.mark_read(&chat).map_err(db)? {
                hub.emit_chat(&chat);
            }
            Ok(Reply::Done)
        }
        Request::SendText { chat, text } => {
            if text.trim().is_empty() {
                return Err("message is empty".to_owned());
            }
            let sent = match &daemon.backend {
                Backend::WhatsApp(slot) => {
                    let client = slot.get().ok_or("WhatsApp client is not running")?;
                    wa::send_text(hub, &client, &chat, &text).await?
                }
                Backend::Demo => demo::send_text(hub, &chat, &text)?,
            };
            Ok(Reply::Sent(sent))
        }
        Request::Logout => {
            match &daemon.backend {
                Backend::WhatsApp(slot) => {
                    let client = slot.get().ok_or("WhatsApp client is not running")?;
                    client.logout().await;
                }
                Backend::Demo => return Err("nothing to unlink in demo mode".to_owned()),
            }
            Ok(Reply::Done)
        }
    }
}
