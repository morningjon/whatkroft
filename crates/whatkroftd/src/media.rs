//! Attachments: what we keep about them, fetching them from WhatsApp's CDN,
//! and preparing local files for sending.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use whatkroft_ipc::{Media, Message, MessageKind, MessageStatus};
use whatsapp_rust::download::{Downloadable, MediaType};
use whatsapp_rust::media::{
    AudioOptions, DocumentOptions, ImageOptions, VideoOptions, audio_message, document_message,
    image_message, video_message,
};
use whatsapp_rust::prelude::*;
use whatsapp_rust::upload::UploadOptions;

use crate::hub::Hub;
use crate::store::NewMessage;
use crate::wa::now;

/// Photos, videos and audio above this go as documents, which WhatsApp does
/// not re-process and accepts at larger sizes.
const MAX_INLINE_MEDIA: u64 = 64 * 1024 * 1024;
/// Uploads are encrypted in memory, so cap what we are willing to read.
const MAX_UPLOAD: u64 = 512 * 1024 * 1024;
/// Longest edge of the preview embedded in an outgoing photo.
const THUMBNAIL_EDGE: u32 = 96;

/// Everything needed to show an attachment's details and to download it
/// later. Stored as JSON next to the message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaRef {
    pub direct_path: Option<String>,
    pub media_key: Option<Vec<u8>>,
    pub file_enc_sha256: Option<Vec<u8>>,
    pub file_sha256: Option<Vec<u8>>,
    pub file_length: Option<u64>,
    pub mime: Option<String>,
    pub file_name: Option<String>,
    pub seconds: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub voice: bool,
}

impl MediaRef {
    /// The client-facing view: no keys, plus the preview and local path.
    pub fn to_ipc(&self, thumbnail: Option<&[u8]>, path: Option<String>) -> Media {
        Media {
            mime: self.mime.clone(),
            file_name: self.file_name.clone(),
            size: self.file_length,
            seconds: self.seconds,
            width: self.width,
            height: self.height,
            voice: self.voice,
            thumbnail: thumbnail.map(|bytes| BASE64.encode(bytes)),
            path,
        }
    }
}

/// A stored reference in the shape the library downloads from.
struct Fetch<'a> {
    media: &'a MediaRef,
    media_type: MediaType,
}

impl Downloadable for Fetch<'_> {
    fn direct_path(&self) -> Option<&str> {
        self.media.direct_path.as_deref()
    }
    fn media_key(&self) -> Option<&[u8]> {
        self.media.media_key.as_deref()
    }
    fn file_enc_sha256(&self) -> Option<&[u8]> {
        self.media.file_enc_sha256.as_deref()
    }
    fn file_sha256(&self) -> Option<&[u8]> {
        self.media.file_sha256.as_deref()
    }
    fn file_length(&self) -> Option<u64> {
        self.media.file_length
    }
    fn app_info(&self) -> MediaType {
        self.media_type
    }
}

fn media_type(kind: MessageKind) -> Option<MediaType> {
    match kind {
        MessageKind::Image => Some(MediaType::Image),
        MessageKind::Video => Some(MediaType::Video),
        MessageKind::Audio => Some(MediaType::Audio),
        MessageKind::Document => Some(MediaType::Document),
        MessageKind::Sticker => Some(MediaType::Sticker),
        MessageKind::Text | MessageKind::Location | MessageKind::Contact | MessageKind::Poll => {
            None
        }
    }
}

// ── File names ──────────────────────────────────────────────────────────

/// Keep only characters that are safe in a file name on any filesystem.
fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            c if c.is_alphanumeric() => c,
            '.' | '-' | '_' | ' ' | '(' | ')' | '+' | ',' => c,
            _ => '_',
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '.' || c == ' ');
    trimmed.chars().take(120).collect()
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    // Parameters such as "; codecs=opus" do not matter here.
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    Some(match essence {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/3gpp" => "3gp",
        "video/quicktime" => "mov",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        "audio/amr" => "amr",
        "application/pdf" => "pdf",
        _ => return None,
    })
}

/// What a file's type is, judged by its extension.
fn mime_for_extension(extension: &str) -> &'static str {
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg; codecs=opus",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        _ => "application/octet-stream",
    }
}

fn lower_extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// File name for a downloaded attachment. Documents keep their own name
/// (prefixed with the message id so two "invoice.pdf" do not collide);
/// everything else is named after the message.
pub fn local_file_name(id: &str, kind: MessageKind, media: &MediaRef) -> String {
    let id = sanitise(id);
    if kind == MessageKind::Document
        && let Some(name) = media.file_name.as_deref().map(sanitise)
        && !name.is_empty()
    {
        return format!("{id}-{name}");
    }
    let extension = media
        .mime
        .as_deref()
        .and_then(extension_for_mime)
        .unwrap_or(match kind {
            MessageKind::Image => "jpg",
            MessageKind::Video => "mp4",
            MessageKind::Audio => "ogg",
            MessageKind::Sticker => "webp",
            _ => "bin",
        });
    format!("{id}.{extension}")
}

/// Per-chat folder under the media directory.
pub fn chat_dir(media_dir: &Path, chat: &str) -> PathBuf {
    media_dir.join(sanitise(chat))
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Write `bytes` so that a reader never sees a half-written file.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path)
}

/// GTK cannot be relied on to decode WebP, so stickers are kept as PNG.
/// Animated stickers keep their first frame. Returns the bytes unchanged
/// (and `false`) when they cannot be decoded.
fn sticker_to_png(bytes: Vec<u8>) -> (Vec<u8>, bool) {
    let Ok(image) = image::load_from_memory(&bytes) else {
        return (bytes, false);
    };
    let mut png = Vec::new();
    match image.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png) {
        Ok(()) => (png, true),
        Err(_) => (bytes, false),
    }
}

// ── Download ────────────────────────────────────────────────────────────

/// Fetch and decrypt a message's attachment into the media directory.
/// Returns the local path; a file already on disk is not fetched again.
pub async fn download(hub: &Hub, client: &Client, chat: &str, id: &str) -> Result<String, String> {
    let stored = hub
        .store
        .get_media(chat, id)
        .map_err(|e| e.to_string())?
        .ok_or("this message has no attachment")?;
    if let Some(path) = &stored.path
        && Path::new(path).is_file()
    {
        return Ok(path.clone());
    }
    let media_type = media_type(stored.kind).ok_or("this message has no attachment")?;
    if stored.media.direct_path.is_none() {
        return Err("the attachment is no longer available".to_owned());
    }

    let fetch = Fetch {
        media: &stored.media,
        media_type,
    };
    let bytes = client.download(&fetch).await.map_err(|e| {
        format!(
            "could not download the attachment (it may have expired on WhatsApp's servers): {e}"
        )
    })?;

    let dir = chat_dir(&hub.media_dir, chat);
    let name = local_file_name(id, stored.kind, &stored.media);
    let is_sticker = stored.kind == MessageKind::Sticker;
    let path = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
        create_private_dir(&dir)?;
        let (bytes, path) = if is_sticker {
            let (bytes, converted) = sticker_to_png(bytes);
            let path = dir.join(&name);
            (
                bytes,
                if converted {
                    path.with_extension("png")
                } else {
                    path
                },
            )
        } else {
            (bytes, dir.join(&name))
        };
        write_atomically(&path, &bytes)?;
        Ok(path)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("could not save the attachment: {e}"))?;

    let path = path.to_string_lossy().into_owned();
    hub.store
        .set_media_path(chat, id, &path)
        .map_err(|e| e.to_string())?;
    Ok(path)
}

// ── Upload ──────────────────────────────────────────────────────────────

/// A local file made ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub kind: MessageKind,
    pub bytes: Vec<u8>,
    pub mime: String,
    pub file_name: String,
    /// Extension of the bytes as they will be sent (a PNG photo becomes JPEG).
    pub extension: String,
    pub thumbnail: Option<Vec<u8>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    image.to_rgb8().write_with_encoder(encoder).ok()?;
    Some(out)
}

/// Decide how a file is sent and convert it if needed. CPU-bound.
///
/// Photos WhatsApp can display are sent as JPEG with a small preview;
/// MP4 video and common audio formats are sent as media; anything else,
/// and anything too large to inline, is sent as a document untouched.
pub fn prepare_outgoing(path: &Path, bytes: Vec<u8>) -> Outgoing {
    let extension = lower_extension(path);
    let mime = mime_for_extension(&extension);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let small = (bytes.len() as u64) <= MAX_INLINE_MEDIA;
    let document = |bytes: Vec<u8>| Outgoing {
        kind: MessageKind::Document,
        bytes,
        mime: mime.to_owned(),
        file_name: file_name.clone(),
        extension: extension.clone(),
        thumbnail: None,
        width: None,
        height: None,
    };

    match mime {
        "image/jpeg" | "image/png" | "image/webp" if small => {
            let Ok(image) = image::load_from_memory(&bytes) else {
                return document(bytes);
            };
            let thumbnail = encode_jpeg(&image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE), 60);
            let (width, height) = (image.width(), image.height());
            // A JPEG goes out as it is; other formats are converted, since
            // that is the only photo format every WhatsApp client shows.
            let jpeg = if mime == "image/jpeg" {
                Some(bytes.clone())
            } else {
                encode_jpeg(&image, 90)
            };
            match jpeg {
                Some(jpeg) => Outgoing {
                    kind: MessageKind::Image,
                    bytes: jpeg,
                    mime: "image/jpeg".to_owned(),
                    file_name,
                    extension: "jpg".to_owned(),
                    thumbnail,
                    width: Some(width),
                    height: Some(height),
                },
                None => document(bytes),
            }
        }
        "video/mp4" if small => Outgoing {
            kind: MessageKind::Video,
            ..document(bytes)
        },
        "audio/mpeg" | "audio/mp4" | "audio/aac" | "audio/ogg; codecs=opus" if small => Outgoing {
            kind: MessageKind::Audio,
            ..document(bytes)
        },
        _ => document(bytes),
    }
}

/// Send a local file to `chat` and record it. The caption is dropped for
/// audio, which cannot carry one.
pub async fn send_file(
    hub: &Hub,
    client: &Client,
    chat: &str,
    path: &str,
    caption: Option<&str>,
) -> Result<Message, String> {
    let to: Jid = chat.parse().map_err(|e| format!("invalid chat id: {e}"))?;
    let source = PathBuf::from(path);
    let size = tokio::fs::metadata(&source)
        .await
        .map_err(|e| format!("cannot read {path}: {e}"))?
        .len();
    if size == 0 {
        return Err("the file is empty".to_owned());
    }
    if size > MAX_UPLOAD {
        return Err(format!(
            "the file is too large to send ({} MB; the limit is {} MB)",
            size / (1024 * 1024),
            MAX_UPLOAD / (1024 * 1024)
        ));
    }
    let bytes = tokio::fs::read(&source)
        .await
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    let outgoing = tokio::task::spawn_blocking(move || prepare_outgoing(&source, bytes))
        .await
        .map_err(|e| e.to_string())?;

    // Keep our own copy before the bytes are handed to the uploader, so the
    // message still shows if the original file is moved or deleted.
    let dir = chat_dir(&hub.media_dir, chat);
    let staged = dir.join(format!(".outgoing-{}-{}", std::process::id(), now_nanos()));
    {
        let (dir, staged, bytes) = (dir.clone(), staged.clone(), outgoing.bytes.clone());
        tokio::task::spawn_blocking(move || {
            create_private_dir(&dir)?;
            std::fs::write(&staged, bytes)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not keep a copy of the file: {e}"))?;
    }

    let result = upload_and_send(hub, client, &to, chat, outgoing, caption, &dir, &staged).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&staged).await;
    }
    result
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn upload_and_send(
    hub: &Hub,
    client: &Client,
    to: &Jid,
    chat: &str,
    outgoing: Outgoing,
    caption: Option<&str>,
    dir: &Path,
    staged: &Path,
) -> Result<Message, String> {
    let Outgoing {
        kind,
        bytes,
        mime,
        file_name,
        extension,
        thumbnail,
        width,
        height,
    } = outgoing;
    let caption = caption.map(str::trim).filter(|c| !c.is_empty());
    let media_type = media_type(kind).expect("outgoing files are always media kinds");

    let upload = client
        .upload(bytes, media_type, UploadOptions::new())
        .await
        .map_err(|e| format!("upload failed: {e}"))?;
    let reference = MediaRef {
        direct_path: Some(upload.direct_path.clone()),
        media_key: Some(upload.media_key.to_vec()),
        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
        file_sha256: Some(upload.file_sha256.to_vec()),
        file_length: Some(upload.file_length),
        mime: Some(mime.clone()),
        file_name: (kind == MessageKind::Document).then(|| file_name.clone()),
        seconds: None,
        width,
        height,
        voice: false,
    };

    let message = match kind {
        MessageKind::Image => {
            let mut message = image_message(
                upload,
                ImageOptions {
                    caption: caption.map(str::to_owned),
                    mimetype: Some(mime),
                    jpeg_thumbnail: thumbnail.clone(),
                    context_info: None,
                },
            );
            if let Some(image) = message.image_message.as_option_mut() {
                image.width = width;
                image.height = height;
            }
            message
        }
        MessageKind::Video => video_message(
            upload,
            VideoOptions {
                caption: caption.map(str::to_owned),
                mimetype: Some(mime),
                ..Default::default()
            },
        ),
        MessageKind::Audio => audio_message(
            upload,
            AudioOptions {
                mimetype: Some(mime),
                ..Default::default()
            },
        ),
        _ => document_message(
            upload,
            DocumentOptions {
                mimetype: Some(mime),
                file_name: Some(file_name.clone()),
                title: Some(file_name),
                caption: caption.map(str::to_owned),
                ..Default::default()
            },
        ),
    };

    let sent = client
        .send_message(to.clone(), message)
        .await
        .map_err(|e| e.to_string())?;

    // Now that the message has an id, give our copy its final name.
    let name = match kind {
        MessageKind::Document => local_file_name(&sent.message_id, kind, &reference),
        _ => format!("{}.{extension}", sanitise(&sent.message_id)),
    };
    let final_path = dir.join(name);
    let media_path = match tokio::fs::rename(staged, &final_path).await {
        Ok(()) => Some(final_path.to_string_lossy().into_owned()),
        Err(e) => {
            log::warn!("keeping sent file copy: {e}");
            None
        }
    };

    let record = NewMessage {
        chat: chat.to_owned(),
        id: sent.message_id,
        sender: String::new(),
        from_me: true,
        timestamp: now(),
        kind,
        text: if kind == MessageKind::Audio {
            String::new()
        } else {
            caption.unwrap_or_default().to_owned()
        },
        status: MessageStatus::Sent,
        media: Some(reference),
        thumbnail,
        media_path,
        needs_receipt: false,
    };
    hub.ingest(&record, false).map_err(|e| e.to_string())?;
    hub.store
        .get_message(&record.chat, &record.id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "sent message missing from the store".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |x, y| {
                image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
            }));
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn png_photo_is_sent_as_jpeg_with_preview() {
        let outgoing =
            prepare_outgoing(Path::new("/home/u/Pictures/Holiday Pic.PNG"), png(400, 200));
        assert_eq!(outgoing.kind, MessageKind::Image);
        assert_eq!(outgoing.mime, "image/jpeg");
        assert_eq!(outgoing.extension, "jpg");
        assert_eq!((outgoing.width, outgoing.height), (Some(400), Some(200)));
        assert_eq!(&outgoing.bytes[..2], &[0xff, 0xd8], "JPEG magic");

        let thumbnail = image::load_from_memory(&outgoing.thumbnail.unwrap()).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (96, 48));
    }

    #[test]
    fn jpeg_photo_is_sent_untouched() {
        let jpeg = encode_jpeg(&image::load_from_memory(&png(64, 64)).unwrap(), 80).unwrap();
        let outgoing = prepare_outgoing(Path::new("a.jpeg"), jpeg.clone());
        assert_eq!(outgoing.kind, MessageKind::Image);
        assert_eq!(outgoing.bytes, jpeg);
    }

    #[test]
    fn broken_image_and_unknown_types_become_documents() {
        let broken = prepare_outgoing(Path::new("photo.jpg"), b"not a jpeg".to_vec());
        assert_eq!(broken.kind, MessageKind::Document);
        assert_eq!(broken.file_name, "photo.jpg");
        assert_eq!(broken.bytes, b"not a jpeg");

        let archive = prepare_outgoing(Path::new("backup.tar.zst"), vec![1, 2, 3]);
        assert_eq!(archive.kind, MessageKind::Document);
        assert_eq!(archive.mime, "application/octet-stream");

        let none = prepare_outgoing(Path::new("README"), vec![1]);
        assert_eq!(
            (none.kind, none.extension.as_str()),
            (MessageKind::Document, "")
        );
    }

    #[test]
    fn video_and_audio_kinds() {
        assert_eq!(
            prepare_outgoing(Path::new("clip.mp4"), vec![0; 8]).kind,
            MessageKind::Video
        );
        assert_eq!(
            prepare_outgoing(Path::new("clip.mkv"), vec![0; 8]).kind,
            MessageKind::Document
        );
        let voice = prepare_outgoing(Path::new("note.opus"), vec![0; 8]);
        assert_eq!(voice.kind, MessageKind::Audio);
        assert_eq!(voice.mime, "audio/ogg; codecs=opus");
        assert_eq!(
            prepare_outgoing(Path::new("song.wav"), vec![0; 8]).kind,
            MessageKind::Document
        );
    }

    #[test]
    fn local_names() {
        let photo = MediaRef {
            mime: Some("image/jpeg".into()),
            ..MediaRef::default()
        };
        assert_eq!(
            local_file_name("3EB0AB", MessageKind::Image, &photo),
            "3EB0AB.jpg"
        );

        let voice = MediaRef {
            mime: Some("audio/ogg; codecs=opus".into()),
            ..MediaRef::default()
        };
        assert_eq!(local_file_name("A1", MessageKind::Audio, &voice), "A1.ogg");

        let unknown = MediaRef::default();
        assert_eq!(
            local_file_name("A1", MessageKind::Video, &unknown),
            "A1.mp4"
        );

        // A hostile file name cannot climb out of the media directory.
        let evil = MediaRef {
            file_name: Some("../../.ssh/authorized_keys".into()),
            ..MediaRef::default()
        };
        let name = local_file_name("A/1", MessageKind::Document, &evil);
        assert_eq!(name, "A_1-_.._.ssh_authorized_keys");
        assert!(!name.contains('/'));

        assert_eq!(
            chat_dir(Path::new("/m"), "447700900001@s.whatsapp.net"),
            Path::new("/m/447700900001_s.whatsapp.net")
        );
        assert_eq!(chat_dir(Path::new("/m"), "../x"), Path::new("/m/_x"));
    }

    #[test]
    fn sticker_conversion() {
        let (bytes, converted) = sticker_to_png(png(8, 8));
        assert!(converted);
        assert_eq!(&bytes[1..4], b"PNG");
        let (bytes, converted) = sticker_to_png(b"garbage".to_vec());
        assert!(!converted);
        assert_eq!(bytes, b"garbage");
    }

    #[test]
    fn ipc_view_hides_keys() {
        let media = MediaRef {
            media_key: Some(vec![9; 32]),
            direct_path: Some("/secret".into()),
            file_length: Some(10),
            ..MediaRef::default()
        };
        let json = serde_json::to_string(&media.to_ipc(Some(&[1, 2, 3]), None)).unwrap();
        assert!(!json.contains("secret") && !json.contains("media_key"));
        assert!(json.contains("\"thumbnail\":\"AQID\""));
    }
}
