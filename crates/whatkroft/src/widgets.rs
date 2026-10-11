//! Stateless widget builders and text formatting for the main window.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use whatkroft_ipc::{Chat, Media, MessageKind, MessageStatus};

/// `widget_name` of the row that opens the archive, where chat rows carry
/// their JID.
pub const ARCHIVE_ROW: &str = "::archived";

/// Largest edge of a photo or video preview in a bubble.
const MEDIA_EDGE: f64 = 300.0;
const STICKER_EDGE: f64 = 140.0;

/// What a media bubble is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    Idle,
    Downloading,
    Failed,
    Playing,
}

pub fn chat_row(chat: &Chat) -> gtk::ListBoxRow {
    let avatar = adw::Avatar::new(40, Some(&chat.name), true);

    let name = gtk::Label::new(Some(&chat.name));
    name.add_css_class("name");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(pango::EllipsizeMode::End);

    let time = gtk::Label::new(Some(&list_time(chat.last_timestamp)));
    time.add_css_class("caption");
    time.add_css_class("dim-label");

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    top.append(&name);
    top.append(&time);

    // Previews are one line; a message with line breaks shows its first.
    let preview_text = chat
        .last_text
        .as_deref()
        .and_then(|t| t.lines().next())
        .unwrap_or("");
    let preview = gtk::Label::new(Some(preview_text));
    preview.add_css_class("dim-label");
    preview.set_xalign(0.0);
    preview.set_hexpand(true);
    preview.set_ellipsize(pango::EllipsizeMode::End);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bottom.append(&preview);
    if chat.unread > 0 {
        bottom.append(&badge(chat.unread));
    }

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.append(&top);
    text.append(&bottom);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.add_css_class("chat-row");
    content.append(&avatar);
    content.append(&text);

    let row = gtk::ListBoxRow::new();
    row.set_widget_name(&chat.jid);
    row.set_child(Some(&content));
    row
}

/// The entry at the top of the chat list that opens archived chats.
pub fn archived_row(chats: usize, unread: u32) -> gtk::ListBoxRow {
    let icon = gtk::Image::from_icon_name("folder-symbolic");
    icon.set_pixel_size(20);
    icon.set_margin_start(10);
    icon.set_margin_end(10);

    let label = gtk::Label::new(Some("Archived"));
    label.add_css_class("name");
    label.set_xalign(0.0);
    label.set_hexpand(true);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.add_css_class("archived-row");
    content.append(&icon);
    content.append(&label);
    if unread > 0 {
        content.append(&badge(unread));
    } else {
        let count = gtk::Label::new(Some(&chats.to_string()));
        count.add_css_class("dim-label");
        count.add_css_class("caption");
        content.append(&count);
    }

    let row = gtk::ListBoxRow::new();
    row.set_widget_name(ARCHIVE_ROW);
    // It opens a view rather than a chat, so it never shows as selected.
    row.set_selectable(false);
    row.set_child(Some(&content));
    row.set_tooltip_text(Some("Archived chats"));
    row
}

fn badge(count: u32) -> gtk::Label {
    let badge = gtk::Label::new(Some(&count.to_string()));
    badge.add_css_class("badge");
    badge.set_valign(gtk::Align::Center);
    badge
}

/// Time and delivery state under a bubble. `status` is `None` for incoming
/// messages, which show only the time.
pub fn set_meta(label: &gtk::Label, time: &str, status: Option<MessageStatus>) {
    let (mark, tooltip) = match status {
        None => ("", None),
        Some(MessageStatus::Pending) => (" ◷", Some("Sending")),
        Some(MessageStatus::Sent) => (" ✓", Some("Sent")),
        Some(MessageStatus::Delivered) => (" ✓✓", Some("Delivered")),
        Some(MessageStatus::Read) => (" ✓✓", Some("Read")),
    };
    label.set_text(&format!("{time}{mark}"));
    label.set_tooltip_text(tooltip);
    if status == Some(MessageStatus::Read) {
        label.add_css_class("read");
    } else {
        label.remove_css_class("read");
    }
}

// ── Media ───────────────────────────────────────────────────────────────

/// Size on screen for a picture of `width` x `height`, keeping its shape.
/// Extreme panoramas are clamped so a bubble is never a sliver.
pub fn display_size(width: Option<u32>, height: Option<u32>, edge: f64) -> (i32, i32) {
    let (w, h) = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (f64::from(w), f64::from(h)),
        _ => (4.0, 3.0),
    };
    let ratio = (w / h).clamp(0.45, 2.2);
    let (dw, dh) = if ratio >= 1.0 {
        (edge, edge / ratio)
    } else {
        (edge * ratio, edge)
    };
    (dw.round() as i32, dh.round() as i32)
}

fn thumbnail_texture(media: &Media) -> Option<gdk::Texture> {
    let bytes = BASE64.decode(media.thumbnail.as_deref()?).ok()?;
    gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
}

fn overlay_badge(text: &str, halign: gtk::Align, valign: gtk::Align) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("media-badge");
    label.set_halign(halign);
    label.set_valign(valign);
    label
}

/// A photo, sticker or video preview at a fixed size: the downloaded file
/// when there is one (never for video, which opens in a player), else the
/// blurry preview that came with the message, else a placeholder.
pub fn picture(kind: MessageKind, media: &Media, state: MediaState) -> gtk::Widget {
    let is_sticker = kind == MessageKind::Sticker;
    let edge = if is_sticker { STICKER_EDGE } else { MEDIA_EDGE };
    let (width, height) = display_size(media.width, media.height, edge);

    // The sizing child fixes the overlay's size; everything else is laid
    // over it, so a 4000-pixel photo cannot stretch the bubble.
    let sizer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sizer.set_size_request(width, height);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&sizer));
    overlay.set_overflow(gtk::Overflow::Hidden);
    overlay.add_css_class("media");
    overlay.set_halign(gtk::Align::Start);

    let full = match (kind, &media.path) {
        (MessageKind::Video, _) | (_, None) => None,
        (_, Some(path)) => gdk::Texture::from_filename(path).ok(),
    };
    let have_full = full.is_some();
    match full.or_else(|| thumbnail_texture(media)) {
        Some(texture) => {
            let picture = gtk::Picture::for_paintable(&texture);
            picture.set_can_shrink(true);
            picture.set_content_fit(if is_sticker {
                gtk::ContentFit::Contain
            } else {
                gtk::ContentFit::Cover
            });
            overlay.add_overlay(&picture);
        }
        None => sizer.add_css_class("media-placeholder"),
    }
    if is_sticker {
        overlay.add_css_class("sticker");
        sizer.remove_css_class("media-placeholder");
    }

    match state {
        MediaState::Downloading => {
            let spinner = gtk::Spinner::new();
            spinner.set_spinning(true);
            spinner.set_halign(gtk::Align::Center);
            spinner.set_valign(gtk::Align::Center);
            spinner.set_size_request(28, 28);
            overlay.add_overlay(&spinner);
        }
        MediaState::Failed => overlay.add_overlay(&overlay_badge(
            "Could not load. Click to retry",
            gtk::Align::Center,
            gtk::Align::Center,
        )),
        MediaState::Idle | MediaState::Playing => {
            if kind == MessageKind::Video {
                let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
                play.set_pixel_size(28);
                play.add_css_class("media-play");
                play.set_halign(gtk::Align::Center);
                play.set_valign(gtk::Align::Center);
                overlay.add_overlay(&play);
            } else if !have_full && !is_sticker {
                let size = media.size.map(format_size).unwrap_or_default();
                overlay.add_overlay(&overlay_badge(
                    format!("↓ {size}").trim_end(),
                    gtk::Align::Center,
                    gtk::Align::Center,
                ));
            }
        }
    }
    if kind == MessageKind::Video
        && let Some(seconds) = media.seconds
    {
        overlay.add_overlay(&overlay_badge(
            &format_duration(seconds),
            gtk::Align::Start,
            gtk::Align::End,
        ));
    }

    overlay.set_tooltip_text(Some(match kind {
        MessageKind::Video => "Open video",
        MessageKind::Sticker => "Sticker",
        _ => "Open photo",
    }));
    overlay.upcast()
}

/// A voice note, audio file or document: an icon button with its details.
/// Returns the row and the button that acts on it.
pub fn attachment(
    kind: MessageKind,
    media: &Media,
    state: MediaState,
) -> (gtk::Widget, gtk::Button) {
    let is_audio = kind == MessageKind::Audio;
    let button = gtk::Button::new();
    button.add_css_class("circular");
    button.set_valign(gtk::Align::Center);
    match state {
        MediaState::Downloading => {
            let spinner = gtk::Spinner::new();
            spinner.set_spinning(true);
            button.set_child(Some(&spinner));
            button.set_sensitive(false);
        }
        MediaState::Playing => button.set_icon_name("media-playback-pause-symbolic"),
        MediaState::Failed => button.set_icon_name("view-refresh-symbolic"),
        MediaState::Idle if is_audio => button.set_icon_name("media-playback-start-symbolic"),
        MediaState::Idle if media.path.is_some() => button.set_icon_name("document-open-symbolic"),
        MediaState::Idle => button.set_icon_name("folder-download-symbolic"),
    }
    button.set_tooltip_text(Some(match (state, is_audio) {
        (MediaState::Failed, _) => "Try again",
        (MediaState::Playing, _) => "Pause",
        (_, true) => "Play",
        (_, false) if media.path.is_some() => "Open",
        (_, false) => "Download and open",
    }));

    let title_text = match (kind, media.voice) {
        (MessageKind::Audio, true) => "Voice message".to_owned(),
        (MessageKind::Audio, false) => "Audio".to_owned(),
        _ => media
            .file_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Document".to_owned()),
    };
    let title = gtk::Label::new(Some(&title_text));
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::Middle);
    title.set_max_width_chars(32);

    let mut details = Vec::new();
    if state == MediaState::Failed {
        details.push("Could not load".to_owned());
    }
    if let Some(seconds) = media.seconds.filter(|_| is_audio) {
        details.push(format_duration(seconds));
    }
    if let Some(size) = media.size {
        details.push(format_size(size));
    }
    let detail = gtk::Label::new(Some(&details.join(" · ")));
    detail.add_css_class("detail");
    detail.set_xalign(0.0);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_valign(gtk::Align::Center);
    text.append(&title);
    text.append(&detail);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("attachment");
    row.append(&button);
    row.append(&text);
    (row.upcast(), button)
}

// ── Formatting ──────────────────────────────────────────────────────────

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes:.0} B")
    } else if bytes < KB * KB {
        format!("{:.0} kB", bytes / KB)
    } else if bytes < KB * KB * KB {
        format!("{:.1} MB", bytes / (KB * KB))
    } else {
        format!("{:.1} GB", bytes / (KB * KB * KB))
    }
}

pub fn format_duration(seconds: u32) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn same_day(a: &glib::DateTime, b: &glib::DateTime) -> bool {
    a.year() == b.year() && a.day_of_year() == b.day_of_year()
}

/// Separator text between days in a conversation.
pub fn day_label(when: &glib::DateTime) -> String {
    if let Ok(now) = glib::DateTime::now_local() {
        if same_day(when, &now) {
            return "Today".to_owned();
        }
        if let Ok(yesterday) = now.add_days(-1)
            && same_day(when, &yesterday)
        {
            return "Yesterday".to_owned();
        }
    }
    when.format("%-e %B %Y")
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Compact time for the chat list: the clock time today, the date before,
/// with the year once it is not this one.
pub fn list_time(timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    let Ok(when) = glib::DateTime::from_unix_local(timestamp) else {
        return String::new();
    };
    let now = glib::DateTime::now_local().ok();
    let format = match &now {
        Some(now) if same_day(&when, now) => "%H:%M",
        Some(now) if now.year() == when.year() => "%-e %b",
        _ => "%-e %b %Y",
    };
    when.format(format)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(900), "900 B");
        assert_eq!(format_size(1_840), "2 kB");
        assert_eq!(format_size(4_300_000), "4.1 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(4), "0:04");
        assert_eq!(format_duration(83), "1:23");
        assert_eq!(format_duration(3_725), "1:02:05");
    }

    #[test]
    fn picture_sizes_keep_shape_within_the_edge() {
        assert_eq!(display_size(Some(1600), Some(1200), 300.0), (300, 225));
        assert_eq!(display_size(Some(900), Some(1200), 300.0), (225, 300));
        assert_eq!(display_size(Some(512), Some(512), 140.0), (140, 140));
        // Unknown or nonsense dimensions fall back to 4:3.
        assert_eq!(display_size(None, None, 300.0), (300, 225));
        assert_eq!(display_size(Some(0), Some(10), 300.0), (300, 225));
        // A 10:1 panorama and a 1:10 screenshot are clamped, not slivers.
        assert_eq!(display_size(Some(5000), Some(500), 300.0), (300, 136));
        assert_eq!(display_size(Some(500), Some(5000), 300.0), (135, 300));
    }
}
