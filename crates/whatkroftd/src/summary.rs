//! Reduce a WhatsApp protobuf message to what the chat view shows today.
//!
//! The first milestone renders text only, so media and other rich kinds are
//! stored as a kind plus a short description (and caption when there is one).
//! The full protobuf is not kept yet.

use whatkroft_ipc::MessageKind;
use whatsapp_rust::prelude::*;

/// `None` for messages with nothing to show: protocol traffic, reactions,
/// key distribution, and kinds this milestone does not understand yet.
pub fn summarise(message: &wa::Message) -> Option<(MessageKind, String)> {
    if let Some(text) = message.text_content()
        && !text.is_empty()
    {
        return Some((MessageKind::Text, text.to_owned()));
    }

    let base = message.get_base_message();
    let captioned = |label: &str, caption: Option<&str>| match caption {
        Some(caption) if !caption.is_empty() => format!("{label}: {caption}"),
        _ => label.to_owned(),
    };

    if let Some(m) = base.image_message.as_option() {
        return Some((MessageKind::Image, captioned("Photo", m.caption.as_deref())));
    }
    if let Some(m) = base.video_message.as_option() {
        let label = if m.gif_playback == Some(true) {
            "GIF"
        } else {
            "Video"
        };
        return Some((MessageKind::Video, captioned(label, m.caption.as_deref())));
    }
    if let Some(m) = base.audio_message.as_option() {
        let label = if m.ptt == Some(true) {
            "Voice message"
        } else {
            "Audio"
        };
        return Some((MessageKind::Audio, label.to_owned()));
    }
    if let Some(m) = base.document_message.as_option() {
        let name = m.file_name.as_deref().or(m.title.as_deref());
        return Some((MessageKind::Document, captioned("Document", name)));
    }
    if base.sticker_message.is_set() {
        return Some((MessageKind::Sticker, "Sticker".to_owned()));
    }
    if let Some(m) = base.location_message.as_option() {
        return Some((
            MessageKind::Location,
            captioned("Location", m.name.as_deref()),
        ));
    }
    if base.live_location_message.is_set() {
        return Some((MessageKind::Location, "Live location".to_owned()));
    }
    if let Some(m) = base.contact_message.as_option() {
        return Some((
            MessageKind::Contact,
            captioned("Contact", m.display_name.as_deref()),
        ));
    }
    if base.contacts_array_message.is_set() {
        return Some((MessageKind::Contact, "Contacts".to_owned()));
    }
    let poll = base
        .poll_creation_message
        .as_option()
        .or(base.poll_creation_message_v2.as_option())
        .or(base.poll_creation_message_v3.as_option());
    if let Some(m) = poll {
        return Some((MessageKind::Poll, captioned("Poll", m.name.as_deref())));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_extended_text() {
        let plain = wa::Message::text("hello");
        assert_eq!(summarise(&plain), Some((MessageKind::Text, "hello".into())));

        let extended = wa::Message::text_with_context("quoted reply", wa::ContextInfo::default());
        assert_eq!(
            summarise(&extended),
            Some((MessageKind::Text, "quoted reply".into()))
        );
    }

    #[test]
    fn media_gets_a_description() {
        let photo = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("sunset".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            summarise(&photo),
            Some((MessageKind::Image, "Photo: sunset".into()))
        );

        let voice = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            summarise(&voice),
            Some((MessageKind::Audio, "Voice message".into()))
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
