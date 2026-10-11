//! Demo mode: a made-up account served entirely from memory.
//!
//! Lets the UI be developed and tested without linking a real WhatsApp
//! account. Nothing here touches the network; attachments are generated.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use whatkroft_ipc::{ConnState, Message, MessageKind, MessageStatus};

use crate::hub::Hub;
use crate::media::{self, MediaRef};
use crate::store::NewMessage;
use crate::wa::now;

const AVA: &str = "447700900101@s.whatsapp.net";
const BEN: &str = "447700900102@s.whatsapp.net";
const CLUB: &str = "120363000000000042@g.us";
const UNKNOWN: &str = "447700900199@s.whatsapp.net";
const LANDLORD: &str = "447700900150@s.whatsapp.net";
const OLD_GROUP: &str = "120363000000000077@g.us";

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

/// A picture that is recognisably different per `seed`.
fn picture(width: u32, height: u32, seed: u32) -> image::DynamicImage {
    let (r, g, b) = match seed % 3 {
        0 => (235.0, 120.0, 60.0),
        1 => (60.0, 150.0, 220.0),
        _ => (120.0, 190.0, 110.0),
    };
    image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
        let fx = x as f32 / width as f32;
        let fy = y as f32 / height as f32;
        let sun = (((fx - 0.7).powi(2) + (fy - 0.3).powi(2)).sqrt() < 0.12) as u8 as f32;
        let hill = (fy > 0.75 - 0.1 * (fx * 9.0).sin()) as u8 as f32;
        let shade = 1.0 - 0.55 * fy;
        let mix = |c: f32| {
            let sky = c * shade + 40.0 * fy;
            let v = sky * (1.0 - sun) + 255.0 * sun;
            (v * (1.0 - hill) + (c * 0.25 + 20.0) * hill) as u8
        };
        image::Rgb([mix(r), mix(g), mix(b)])
    }))
}

fn jpeg(image: &image::DynamicImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    image
        .to_rgb8()
        .write_with_encoder(encoder)
        .expect("encoding to memory");
    out
}

/// One second of a 440 Hz tone as 16-bit mono WAV.
fn tone_wav(seconds: u32) -> Vec<u8> {
    const RATE: u32 = 16_000;
    let samples = RATE * seconds;
    let data_len = samples * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for n in 0..samples {
        let t = n as f32 / RATE as f32;
        let sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3 * i16::MAX as f32;
        out.extend_from_slice(&(sample as i16).to_le_bytes());
    }
    out
}

struct Row {
    chat: &'static str,
    sender: &'static str,
    age: i64,
    kind: MessageKind,
    text: &'static str,
    status: MessageStatus,
    media: Option<MediaRef>,
    thumbnail: Option<Vec<u8>>,
}

fn row(chat: &'static str, sender: &'static str, age: i64, text: &'static str) -> Row {
    Row {
        chat,
        sender,
        age,
        kind: MessageKind::Text,
        text,
        status: if sender.is_empty() {
            MessageStatus::Read
        } else {
            MessageStatus::Delivered
        },
        media: None,
        thumbnail: None,
    }
}

fn seed(hub: &Hub) -> rusqlite::Result<()> {
    let store = &hub.store;
    store.upsert_contact(AVA, Some("Ava Lindqvist"), Some("ava"))?;
    store.upsert_contact(BEN, None, Some("Ben"))?;
    store.upsert_contact(LANDLORD, Some("Mr Okafor (landlord)"), None)?;
    store.set_chat_name(CLUB, "Sunday Run Club", true)?;
    store.set_chat_name(OLD_GROUP, "Lisbon trip 2024", true)?;

    let photo = |seed: u32, width: u32, height: u32| MediaRef {
        mime: Some("image/jpeg".into()),
        width: Some(width),
        height: Some(height),
        file_length: Some(u64::from(seed)),
        ..MediaRef::default()
    };
    let thumb = |seed: u32, width: u32, height: u32| Some(jpeg(&picture(width, height, seed), 55));

    let mut rows = vec![
        row(AVA, AVA, 3 * 86_400, "Are we still on for Thursday?"),
        row(AVA, "", 3 * 86_400 - 120, "Yes! 7pm at the usual place."),
        Row {
            kind: MessageKind::Image,
            media: Some(photo(0, 1600, 1200)),
            thumbnail: thumb(0, 48, 36),
            ..row(AVA, AVA, 5_400, "the view from the top")
        },
        row(
            AVA,
            AVA,
            5_340,
            "You have to come next time, it was unreal.",
        ),
        row(AVA, "", 5_000, "That looks incredible. Count me in."),
        Row {
            kind: MessageKind::Document,
            media: Some(MediaRef {
                mime: Some("text/plain".into()),
                file_name: Some("Route notes.txt".into()),
                file_length: Some(1_840),
                ..MediaRef::default()
            }),
            ..row(AVA, AVA, 4_800, "")
        },
        row(
            CLUB,
            BEN,
            9_000,
            "Route for Sunday is up. 12k, mostly flat.",
        ),
        Row {
            kind: MessageKind::Image,
            media: Some(photo(1, 900, 1200)),
            thumbnail: thumb(1, 36, 48),
            ..row(CLUB, BEN, 8_950, "")
        },
        row(
            CLUB,
            AVA,
            8_700,
            "Perfect, I need a gentle one after last week 😅",
        ),
        row(CLUB, "", 8_400, "I'll bring coffee for after."),
        Row {
            kind: MessageKind::Sticker,
            media: Some(MediaRef {
                mime: Some("image/webp".into()),
                width: Some(512),
                height: Some(512),
                file_length: Some(2),
                ..MediaRef::default()
            }),
            ..row(CLUB, AVA, 8_300, "")
        },
        Row {
            kind: MessageKind::Poll,
            ..row(CLUB, BEN, 1_200, "Poll: Start time?")
        },
        Row {
            kind: MessageKind::Audio,
            media: Some(MediaRef {
                mime: Some("audio/wav".into()),
                seconds: Some(4),
                voice: true,
                file_length: Some(128_044),
                ..MediaRef::default()
            }),
            ..row(BEN, BEN, 86_400, "")
        },
        Row {
            status: MessageStatus::Sent,
            ..row(BEN, "", 86_000, "Will listen in a bit, on a call.")
        },
        Row {
            kind: MessageKind::Video,
            media: Some(MediaRef {
                mime: Some("video/mp4".into()),
                seconds: Some(23),
                width: Some(1280),
                height: Some(720),
                file_length: Some(4_300_000),
                ..MediaRef::default()
            }),
            thumbnail: thumb(2, 48, 27),
            ..row(BEN, BEN, 85_000, "finish line!")
        },
        row(UNKNOWN, UNKNOWN, 600, "Hi, is this still available?"),
        row(
            LANDLORD,
            LANDLORD,
            20 * 86_400,
            "Boiler service is booked for the 14th.",
        ),
        row(LANDLORD, "", 20 * 86_400 - 300, "Thanks, I'll be in."),
        row(
            OLD_GROUP,
            AVA,
            400 * 86_400,
            "Best trip ever. Same again next year?",
        ),
    ];
    // Enough history in one chat to page through.
    for n in 0..260 {
        rows.push(row(
            OLD_GROUP,
            if n % 3 == 0 {
                ""
            } else if n % 3 == 1 {
                AVA
            } else {
                BEN
            },
            401 * 86_400 + (260 - n) * 600,
            Box::leak(format!("Trip planning message {}", n + 1).into_boxed_str()),
        ));
    }

    let t = now();
    for row in rows {
        // Only the two newest chats start with unread messages.
        let unread = row.age < 2_000;
        store.insert_message(
            &NewMessage {
                chat: row.chat.to_owned(),
                id: next_id(),
                sender: row.sender.to_owned(),
                from_me: row.sender.is_empty(),
                timestamp: t - row.age,
                kind: row.kind,
                text: row.text.to_owned(),
                status: row.status,
                media: row.media,
                thumbnail: row.thumbnail,
                media_path: None,
                needs_receipt: unread,
            },
            unread,
        )?;
    }
    store.set_archived(LANDLORD, true)?;
    store.set_archived(OLD_GROUP, true)?;
    Ok(())
}

/// "Send" a message: it is delivered, read, and answered by an echo.
pub fn send_text(hub: &Arc<Hub>, chat: &str, text: &str) -> Result<Message, String> {
    let message = NewMessage::text(chat, &next_id(), "", true, now(), text);
    let stored = hub
        .ingest(&message, false)
        .map_err(|e| e.to_string())?
        .ok_or("duplicate message id")?;
    simulate_replies(hub, message.chat, message.id, Some(format!("Echo: {text}")));
    Ok(stored)
}

fn simulate_replies(hub: &Arc<Hub>, chat: String, id: String, reply: Option<String>) {
    let hub = Arc::clone(hub);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        hub.raise_status(&chat, &id, MessageStatus::Delivered);
        tokio::time::sleep(Duration::from_millis(900)).await;
        hub.raise_status(&chat, &id, MessageStatus::Read);
        let Some(reply) = reply else { return };
        tokio::time::sleep(Duration::from_millis(700)).await;
        let sender = if chat.ends_with("@g.us") {
            BEN
        } else {
            chat.as_str()
        };
        let mut reply = NewMessage::text(&chat, &next_id(), sender, false, now(), &reply);
        reply.needs_receipt = true;
        if let Err(e) = hub.ingest(&reply, true) {
            log::error!("demo reply: {e}");
        }
    });
}

/// "Send" a file: converted exactly as a real send would be, then stored.
pub async fn send_file(
    hub: &Arc<Hub>,
    chat: &str,
    path: &str,
    caption: Option<&str>,
) -> Result<Message, String> {
    let source = Path::new(path).to_owned();
    let bytes = tokio::fs::read(&source)
        .await
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    let outgoing = tokio::task::spawn_blocking(move || media::prepare_outgoing(&source, bytes))
        .await
        .map_err(|e| e.to_string())?;

    let id = next_id();
    let reference = MediaRef {
        file_length: Some(outgoing.bytes.len() as u64),
        mime: Some(outgoing.mime.clone()),
        file_name: (outgoing.kind == MessageKind::Document).then(|| outgoing.file_name.clone()),
        width: outgoing.width,
        height: outgoing.height,
        ..MediaRef::default()
    };
    let dir = media::chat_dir(&hub.media_dir, chat);
    let name = match outgoing.kind {
        MessageKind::Document => media::local_file_name(&id, outgoing.kind, &reference),
        _ => format!("{id}.{}", outgoing.extension),
    };
    let target = dir.join(name);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(&target, &outgoing.bytes)
        .await
        .map_err(|e| e.to_string())?;

    let caption = caption.map(str::trim).filter(|c| !c.is_empty());
    let message = NewMessage {
        chat: chat.to_owned(),
        id,
        sender: String::new(),
        from_me: true,
        timestamp: now(),
        kind: outgoing.kind,
        text: if outgoing.kind == MessageKind::Audio {
            String::new()
        } else {
            caption.unwrap_or_default().to_owned()
        },
        status: MessageStatus::Sent,
        media: Some(reference),
        thumbnail: outgoing.thumbnail,
        media_path: Some(target.to_string_lossy().into_owned()),
        needs_receipt: false,
    };
    let stored = hub
        .ingest(&message, false)
        .map_err(|e| e.to_string())?
        .ok_or("duplicate message id")?;
    simulate_replies(hub, message.chat, message.id, None);
    Ok(stored)
}

/// "Download" an attachment by generating it. Video has nothing to
/// generate, which doubles as the example of a failed download.
pub async fn download(hub: &Hub, chat: &str, id: &str) -> Result<String, String> {
    tokio::time::sleep(Duration::from_millis(700)).await;
    let stored = hub
        .store
        .get_media(chat, id)
        .map_err(|e| e.to_string())?
        .ok_or("this message has no attachment")?;
    let seed = stored.media.file_length.unwrap_or(0) as u32;
    let (width, height) = (
        stored.media.width.unwrap_or(800).min(1600),
        stored.media.height.unwrap_or(600).min(1600),
    );
    let (bytes, extension): (Vec<u8>, &str) = match stored.kind {
        MessageKind::Image => (jpeg(&picture(width, height, seed), 85), "jpg"),
        MessageKind::Sticker => {
            let sticker = image::RgbaImage::from_fn(256, 256, |x, y| {
                let (dx, dy) = (x as f32 - 128.0, y as f32 - 128.0);
                let inside = (dx * dx + dy * dy).sqrt() < 110.0;
                let eye = ((dx.abs() - 40.0).powi(2) + (dy + 30.0).powi(2)).sqrt() < 14.0;
                let smile = dy > 20.0 && ((dx * dx + dy * dy).sqrt() - 70.0).abs() < 7.0;
                match (inside, eye || smile) {
                    (false, _) => image::Rgba([0, 0, 0, 0]),
                    (true, true) => image::Rgba([60, 40, 20, 255]),
                    (true, false) => image::Rgba([255, 205, 60, 255]),
                }
            });
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(sticker)
                .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
            (png, "png")
        }
        MessageKind::Audio => (tone_wav(stored.media.seconds.unwrap_or(2)), "wav"),
        MessageKind::Document => (
            b"Demo document.\n\nMeet at the car park at 8:45. Bring water.\n".to_vec(),
            "txt",
        ),
        _ => return Err("the demo account has no file for this attachment".to_owned()),
    };
    let dir = media::chat_dir(&hub.media_dir, chat);
    let path = dir.join(format!("{id}.{extension}"));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| e.to_string())?;
    let path = path.to_string_lossy().into_owned();
    hub.store
        .set_media_path(chat, id, &path)
        .map_err(|e| e.to_string())?;
    Ok(path)
}
