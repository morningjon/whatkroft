//! Reduce a WhatsApp protobuf message to what the chat view shows.

use whatkroft_ipc::MessageKind;
use whatsapp_rust::prelude::*;

use crate::media::MediaRef;

/// The displayable part of a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub kind: MessageKind,
    /// Body or caption; a short description for location, contact and poll.
    pub text: String,
    pub media: Option<MediaRef>,
    pub thumbnail: Option<Vec<u8>>,
}

impl Summary {
    fn text(kind: MessageKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            media: None,
            thumbnail: None,
        }
    }
}

/// `None` for messages with nothing to show: protocol traffic, reactions,
/// key distribution, and kinds not understood yet.
pub fn summarise(message: &wa::Message) -> Option<Summary> {
    if let Some(text) = message.text_content()
        && !text.is_empty()
    {
        return Some(Summary::text(MessageKind::Text, text));
    }

    let base = message.get_base_message();
    let labelled = |label: &str, detail: Option<&str>| match detail {
        Some(detail) if !detail.is_empty() => format!("{label}: {detail}"),
        _ => label.to_owned(),
    };
    let thumb = |bytes: &Option<Vec<u8>>| bytes.clone().filter(|b| !b.is_empty());

    if let Some(m) = base.image_message.as_option() {
        return Some(Summary {
            kind: MessageKind::Image,
            text: m.caption.clone().unwrap_or_default(),
            media: Some(MediaRef {
                direct_path: m.direct_path.clone(),
                media_key: m.media_key.clone(),
                file_enc_sha256: m.file_enc_sha256.clone(),
                file_sha256: m.file_sha256.clone(),
                file_length: m.file_length,
                mime: m.mimetype.clone(),
                width: m.width,
                height: m.height,
                ..MediaRef::default()
            }),
            thumbnail: thumb(&m.jpeg_thumbnail),
        });
    }
    if let Some(m) = base.video_message.as_option() {
        return Some(Summary {
            kind: MessageKind::Video,
            text: m.caption.clone().unwrap_or_default(),
            media: Some(MediaRef {
                direct_path: m.direct_path.clone(),
                media_key: m.media_key.clone(),
                file_enc_sha256: m.file_enc_sha256.clone(),
                file_sha256: m.file_sha256.clone(),
                file_length: m.file_length,
                mime: m.mimetype.clone(),
                seconds: m.seconds,
                width: m.width,
                height: m.height,
                ..MediaRef::default()
            }),
            thumbnail: thumb(&m.jpeg_thumbnail),
        });
    }
    if let Some(m) = base.audio_message.as_option() {
        return Some(Summary {
            kind: MessageKind::Audio,
            text: String::new(),
            media: Some(MediaRef {
                direct_path: m.direct_path.clone(),
                media_key: m.media_key.clone(),
                file_enc_sha256: m.file_enc_sha256.clone(),
                file_sha256: m.file_sha256.clone(),
                file_length: m.file_length,
                mime: m.mimetype.clone(),
                seconds: m.seconds,
                voice: m.ptt == Some(true),
                ..MediaRef::default()
            }),
            thumbnail: None,
        });
    }
    if let Some(m) = base.document_message.as_option() {
        return Some(Summary {
            kind: MessageKind::Document,
            text: m.caption.clone().unwrap_or_default(),
            media: Some(MediaRef {
                direct_path: m.direct_path.clone(),
                media_key: m.media_key.clone(),
                file_enc_sha256: m.file_enc_sha256.clone(),
                file_sha256: m.file_sha256.clone(),
                file_length: m.file_length,
                mime: m.mimetype.clone(),
                file_name: m.file_name.clone().or_else(|| m.title.clone()),
                ..MediaRef::default()
            }),
            thumbnail: thumb(&m.jpeg_thumbnail),
        });
    }
    if let Some(m) = base.sticker_message.as_option() {
        return Some(Summary {
            kind: MessageKind::Sticker,
            text: String::new(),
            media: Some(MediaRef {
                direct_path: m.direct_path.clone(),
                media_key: m.media_key.clone(),
                file_enc_sha256: m.file_enc_sha256.clone(),
                file_sha256: m.file_sha256.clone(),
                file_length: m.file_length,
                mime: m.mimetype.clone(),
                width: m.width,
                height: m.height,
                ..MediaRef::default()
            }),
            thumbnail: None,
        });
    }
    if let Some(m) = base.location_message.as_option() {
        return Some(Summary::text(
            MessageKind::Location,
            labelled("Location", m.name.as_deref()),
        ));
    }
    if base.live_location_message.is_set() {
        return Some(Summary::text(MessageKind::Location, "Live location"));
    }
    if let Some(m) = base.contact_message.as_option() {
        return Some(Summary::text(
            MessageKind::Contact,
            labelled("Contact", m.display_name.as_deref()),
        ));
    }
    if base.contacts_array_message.is_set() {
        return Some(Summary::text(MessageKind::Contact, "Contacts"));
    }
    let poll = base
        .poll_creation_message
        .as_option()
        .or(base.poll_creation_message_v2.as_option())
        .or(base.poll_creation_message_v3.as_option());
    if let Some(m) = poll {
        return Some(Summary::text(
            MessageKind::Poll,
            labelled("Poll", m.name.as_deref()),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_extended_text() {
        let plain = summarise(&wa::Message::text("hello")).unwrap();
        assert_eq!(
            (plain.kind, plain.text.as_str()),
            (MessageKind::Text, "hello")
        );
        assert!(plain.media.is_none());

        let extended = summarise(&wa::Message::text_with_context(
            "quoted reply",
            wa::ContextInfo::default(),
        ))
        .unwrap();
        assert_eq!(extended.text, "quoted reply");
    }

    #[test]
    fn photo_keeps_caption_keys_and_thumbnail() {
        let photo = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("sunset".into()),
                direct_path: Some("/v/t62/abc".into()),
                media_key: Some(vec![7; 32]),
                file_length: Some(1234),
                mimetype: Some("image/jpeg".into()),
                width: Some(1600),
                height: Some(1200),
                jpeg_thumbnail: Some(vec![0xff, 0xd8]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let summary = summarise(&photo).unwrap();
        assert_eq!(summary.kind, MessageKind::Image);
        assert_eq!(summary.text, "sunset", "the caption, not a description");
        assert_eq!(summary.thumbnail.as_deref(), Some(&[0xff, 0xd8][..]));
        let media = summary.media.unwrap();
        assert_eq!(media.direct_path.as_deref(), Some("/v/t62/abc"));
        assert_eq!(media.media_key, Some(vec![7; 32]));
        assert_eq!((media.width, media.height), (Some(1600), Some(1200)));
    }

    #[test]
    fn voice_note_and_document() {
        let voice = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                seconds: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        };
        let media = summarise(&voice).unwrap().media.unwrap();
        assert!(media.voice);
        assert_eq!(media.seconds, Some(12));

        let doc = wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                file_name: Some("report.pdf".into()),
                mimetype: Some("application/pdf".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let summary = summarise(&doc).unwrap();
        assert_eq!(summary.kind, MessageKind::Document);
        assert_eq!(
            summary.media.unwrap().file_name.as_deref(),
            Some("report.pdf")
        );
    }

    #[test]
    fn nothing_to_show() {
        assert_eq!(summarise(&wa::Message::default()), None);
        let reaction = wa::Message {
            reaction_message: MessageField::some(wa::message::ReactionMessage {
                text: Some("👍".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(summarise(&reaction), None);
    }
}
