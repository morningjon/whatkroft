//! The main window: pairing screen, chat list and conversation view.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib, pango};
use whatkroft_ipc::{
    Chat, ConnState, Event, Message, MessageKind, MessageStatus, Reply, Request, ServerMessage,
};

use crate::client::{self, Conn};
use crate::qr::QrView;

/// Messages loaded when a chat is opened.
const PAGE_SIZE: u32 = 200;
/// How close to the end (in pixels) still counts as "reading the latest".
const BOTTOM_SLACK: f64 = 48.0;

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub struct App {
    app: adw::Application,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    /// Top-level pages: "status", "pair" and "main".
    pages: gtk::Stack,
    status: adw::StatusPage,
    qr: QrView,
    banner: adw::Banner,
    split: adw::NavigationSplitView,
    chat_list: gtk::ListBox,
    content_page: adw::NavigationPage,
    /// Right-hand side: "empty" until a chat is opened, then "chat".
    content: gtk::Stack,
    messages: gtk::Box,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    conn: Option<Rc<Conn>>,
    chats: Vec<Chat>,
    current: Option<String>,
    /// Status line of each of our own messages on screen, with its time text.
    meta: HashMap<String, (gtk::Label, String)>,
    /// (year, day of year) of the last message drawn, for day separators.
    last_day: Option<(i32, i32)>,
    /// Keep the view pinned to the newest message as more arrive.
    stick_to_bottom: bool,
    /// The daemon process, if this window had to start one.
    daemon: Option<std::process::Child>,
}

impl App {
    pub fn start(app: &adw::Application) {
        let this = Rc::new(Self::build(app));
        this.connect_signals();
        this.install_actions();
        this.show_status(
            "Starting WhatKroft",
            "Connecting to the background service…",
        );
        this.window.present();
        glib::spawn_future_local(Rc::clone(&this).run());
    }

    fn build(app: &adw::Application) -> Self {
        // ── Status page ────────────────────────────────────────────────
        let status = adw::StatusPage::new();
        let status_view = adw::ToolbarView::new();
        status_view.add_top_bar(&adw::HeaderBar::new());
        status_view.set_content(Some(&status));

        // ── Pairing page ───────────────────────────────────────────────
        let qr = QrView::new(280);
        let pair_title = gtk::Label::new(Some("Link WhatKroft to your phone"));
        pair_title.add_css_class("title-1");
        let pair_steps = gtk::Label::new(Some(
            "1. Open WhatsApp on your phone\n\
             2. Go to Settings, then Linked devices\n\
             3. Tap Link a device and scan this code",
        ));
        pair_steps.set_justify(gtk::Justification::Left);
        pair_steps.add_css_class("body");
        let pair_note = gtk::Label::new(Some(
            "WhatKroft is an unofficial client and is not affiliated with WhatsApp or Meta.",
        ));
        pair_note.set_wrap(true);
        pair_note.set_justify(gtk::Justification::Center);
        pair_note.add_css_class("dim-label");
        pair_note.add_css_class("caption");
        let pair_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
        pair_box.set_valign(gtk::Align::Center);
        pair_box.set_margin_top(24);
        pair_box.set_margin_bottom(24);
        pair_box.set_margin_start(24);
        pair_box.set_margin_end(24);
        pair_box.append(&pair_title);
        pair_box.append(qr.widget());
        pair_box.append(&pair_steps);
        pair_box.append(&pair_note);
        pair_steps.set_halign(gtk::Align::Center);
        let pair_view = adw::ToolbarView::new();
        pair_view.add_top_bar(&adw::HeaderBar::new());
        pair_view.set_content(Some(
            &adw::Clamp::builder()
                .maximum_size(480)
                .child(&pair_box)
                .build(),
        ));

        // ── Chat list (sidebar) ────────────────────────────────────────
        let chat_list = gtk::ListBox::new();
        chat_list.set_selection_mode(gtk::SelectionMode::Single);
        chat_list.add_css_class("navigation-sidebar");
        let no_chats = adw::StatusPage::builder()
            .title("No chats yet")
            .description("Your conversations appear here once they have synced from your phone.")
            .build();
        no_chats.add_css_class("compact");
        chat_list.set_placeholder(Some(&no_chats));

        let menu = gio::Menu::new();
        menu.append(Some("Unlink This Device…"), Some("win.unlink"));
        menu.append(Some("About WhatKroft"), Some("win.about"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main Menu")
            .primary(true)
            .build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.pack_end(&menu_button);

        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&sidebar_header);
        sidebar_view.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&chat_list)
                .build(),
        ));
        let sidebar_page = adw::NavigationPage::new(&sidebar_view, "WhatKroft");

        // ── Conversation (content) ─────────────────────────────────────
        let messages = gtk::Box::new(gtk::Orientation::Vertical, 0);
        messages.add_css_class("messages");
        messages.set_valign(gtk::Align::End);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .maximum_size(860)
                    .child(&messages)
                    .build(),
            )
            .build();

        let entry = gtk::Entry::builder()
            .placeholder_text("Message")
            .hexpand(true)
            .build();
        let send = gtk::Button::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Send")
            .action_name("win.send")
            .build();
        send.add_css_class("suggested-action");
        send.add_css_class("circular");
        let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        composer.add_css_class("composer");
        composer.append(&entry);
        composer.append(&send);

        let chat_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat_view.append(&scroller);
        chat_view.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        chat_view.append(
            &adw::Clamp::builder()
                .maximum_size(860)
                .child(&composer)
                .build(),
        );

        let empty = adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("Select a chat")
            .description("Pick a conversation from the list to read and reply.")
            .build();

        let content = gtk::Stack::new();
        content.add_named(&empty, Some("empty"));
        content.add_named(&chat_view, Some("chat"));

        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&adw::HeaderBar::new());
        content_view.set_content(Some(&content));
        let content_page = adw::NavigationPage::new(&content_view, "WhatKroft");

        let split = adw::NavigationSplitView::new();
        split.set_min_sidebar_width(260.0);
        split.set_max_sidebar_width(360.0);
        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        split.set_vexpand(true);

        let banner = adw::Banner::new("");
        let main_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main_box.append(&banner);
        main_box.append(&split);

        // ── Window ─────────────────────────────────────────────────────
        let pages = gtk::Stack::new();
        pages.set_transition_type(gtk::StackTransitionType::Crossfade);
        pages.add_named(&status_view, Some("status"));
        pages.add_named(&pair_view, Some("pair"));
        pages.add_named(&main_box, Some("main"));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&pages));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("WhatKroft")
            .default_width(1000)
            .default_height(700)
            .width_request(360)
            .height_request(420)
            .content(&toasts)
            .build();

        // Below this width the list and the conversation take turns.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            640.0,
            adw::LengthUnit::Sp,
        ));
        narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(narrow);

        Self {
            app: app.clone(),
            window,
            toasts,
            pages,
            status,
            qr,
            banner,
            split,
            chat_list,
            content_page,
            content,
            messages,
            scroller,
            entry,
            state: RefCell::new(State {
                stick_to_bottom: true,
                ..State::default()
            }),
        }
    }

    fn connect_signals(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.chat_list.connect_row_activated(move |_, row| {
            let jid = row.widget_name().to_string();
            this.open_chat(&jid);
        });

        let this = Rc::clone(self);
        self.entry.connect_activate(move |_| this.send_current());

        // Follow new messages only while the reader is at the end.
        let adjustment = self.scroller.vadjustment();
        let this = Rc::clone(self);
        adjustment.connect_value_changed(move |adj| {
            let at_end = adj.value() + adj.page_size() >= adj.upper() - BOTTOM_SLACK;
            this.state.borrow_mut().stick_to_bottom = at_end;
        });
        let this = Rc::clone(self);
        adjustment.connect_upper_notify(move |adj| {
            if this.state.borrow().stick_to_bottom {
                adj.set_value(adj.upper() - adj.page_size());
            }
        });

        // Coming back to the window counts as reading the open chat.
        let this = Rc::clone(self);
        self.window.connect_is_active_notify(move |window| {
            if window.is_active() {
                this.mark_current_read();
            }
        });
    }

    fn install_actions(self: &Rc<Self>) {
        let send = gio::SimpleAction::new("send", None);
        let this = Rc::clone(self);
        send.connect_activate(move |_, _| this.send_current());
        self.window.add_action(&send);

        let unlink = gio::SimpleAction::new("unlink", None);
        let this = Rc::clone(self);
        unlink.connect_activate(move |_, _| this.confirm_unlink());
        self.window.add_action(&unlink);

        let about = gio::SimpleAction::new("about", None);
        let this = Rc::clone(self);
        about.connect_activate(move |_, _| this.show_about());
        self.window.add_action(&about);
    }

    // ── Daemon connection ──────────────────────────────────────────────

    /// Stay connected to the daemon for the life of the window, starting it
    /// if needed and reconnecting if it goes away.
    async fn run(self: Rc<Self>) {
        loop {
            let Some((conn, incoming)) = self.connect().await else {
                self.show_status(
                    "Background service unavailable",
                    "WhatKroft could not start or reach whatkroftd. Trying again…",
                );
                glib::timeout_future(Duration::from_secs(3)).await;
                continue;
            };
            let conn = Rc::new(conn);
            self.state.borrow_mut().conn = Some(Rc::clone(&conn));
            conn.request(Request::ListChats);

            while let Ok(message) = incoming.recv().await {
                self.handle(message);
            }

            self.state.borrow_mut().conn = None;
            self.show_status(
                "Reconnecting",
                "Lost contact with the background service. Reconnecting…",
            );
            glib::timeout_future(Duration::from_secs(1)).await;
        }
    }

    async fn connect(&self) -> Option<(Conn, async_channel::Receiver<ServerMessage>)> {
        if let Ok(connected) = Conn::connect() {
            return Some(connected);
        }
        self.ensure_daemon();
        for _ in 0..40 {
            glib::timeout_future(Duration::from_millis(250)).await;
            if let Ok(connected) = Conn::connect() {
                return Some(connected);
            }
        }
        None
    }

    /// Start the daemon unless the one this window started is still running.
    fn ensure_daemon(&self) {
        let mut state = self.state.borrow_mut();
        // `try_wait` also reaps a daemon that has exited.
        let running = state
            .daemon
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if running {
            return;
        }
        match client::spawn_daemon() {
            Ok(child) => state.daemon = Some(child),
            Err(e) => {
                state.daemon = None;
                log::error!("could not start whatkroftd: {e}");
            }
        }
    }

    fn request(&self, request: Request) {
        let conn = self.state.borrow().conn.clone();
        if let Some(conn) = conn {
            conn.request(request);
        }
    }

    fn handle(&self, message: ServerMessage) {
        match message {
            ServerMessage::Event { event } => self.handle_event(event),
            ServerMessage::Reply {
                ok: Some(reply), ..
            } => self.handle_reply(reply),
            ServerMessage::Reply { error, .. } => {
                let text = error.unwrap_or_else(|| "The request failed".to_owned());
                self.toasts.add_toast(adw::Toast::new(&text));
            }
        }
    }

    fn handle_event(&self, event: Event) {
        match event {
            Event::State(state) => self.apply_state(state),
            Event::Chat(chat) => self.upsert_chat(chat),
            Event::ChatsChanged => {
                self.request(Request::ListChats);
                self.reload_current();
            }
            Event::Message(message) => self.on_message(message),
            Event::Status { chat, id, status } => {
                if self.state.borrow().current.as_deref() == Some(chat.as_str())
                    && let Some((label, time)) = self.state.borrow().meta.get(&id)
                {
                    set_meta(label, time, Some(status));
                }
            }
        }
    }

    fn handle_reply(&self, reply: Reply) {
        match reply {
            Reply::Chats(chats) => {
                self.state.borrow_mut().chats = chats;
                self.rebuild_chat_list();
            }
            Reply::Messages { chat, messages } => {
                if self.state.borrow().current.as_deref() == Some(chat.as_str()) {
                    self.show_messages(&messages);
                }
            }
            Reply::State(state) => self.apply_state(state),
            // The message itself already arrived as an event.
            Reply::Sent(_) | Reply::Done => {}
        }
    }

    fn apply_state(&self, state: ConnState) {
        match &state {
            ConnState::Pairing { qr } => {
                self.qr.set_payload(qr);
                self.pages.set_visible_child_name("pair");
            }
            ConnState::LoggedOut => self.show_status(
                "Device unlinked",
                "This device is no longer linked to your account. Preparing a new code…",
            ),
            ConnState::Connected => {
                self.banner.set_revealed(false);
                self.pages.set_visible_child_name("main");
                self.request(Request::ListChats);
            }
            ConnState::Starting | ConnState::Connecting => {
                self.banner.set_title("Connecting to WhatsApp…");
                self.banner.set_revealed(true);
                self.pages.set_visible_child_name("main");
            }
            ConnState::Disconnected => {
                self.banner.set_title("Offline. Reconnecting…");
                self.banner.set_revealed(true);
                self.pages.set_visible_child_name("main");
            }
        }
    }

    fn show_status(&self, title: &str, description: &str) {
        self.status.set_title(title);
        self.status.set_description(Some(description));
        self.pages.set_visible_child_name("status");
    }

    // ── Chat list ──────────────────────────────────────────────────────

    fn upsert_chat(&self, chat: Chat) {
        let is_current_unread = {
            let mut state = self.state.borrow_mut();
            let is_current = state.current.as_deref() == Some(chat.jid.as_str());
            let unread = chat.unread > 0;
            match state.chats.iter_mut().find(|c| c.jid == chat.jid) {
                Some(existing) => *existing = chat,
                None => state.chats.push(chat),
            }
            state.chats.sort_by(|a, b| {
                b.last_timestamp
                    .cmp(&a.last_timestamp)
                    .then(a.jid.cmp(&b.jid))
            });
            is_current && unread
        };
        self.rebuild_chat_list();
        if is_current_unread && self.window.is_active() {
            self.mark_current_read();
        }
    }

    fn rebuild_chat_list(&self) {
        while let Some(row) = self.chat_list.first_child() {
            // The placeholder is a child too, but not one we may remove.
            if row.downcast_ref::<gtk::ListBoxRow>().is_none() {
                break;
            }
            self.chat_list.remove(&row);
        }
        let state = self.state.borrow();
        for chat in &state.chats {
            let row = chat_row(chat);
            self.chat_list.append(&row);
            if state.current.as_deref() == Some(chat.jid.as_str()) {
                self.chat_list.select_row(Some(&row));
                self.content_page.set_title(&chat.name);
            }
        }
    }

    fn open_chat(&self, jid: &str) {
        let name = {
            let mut state = self.state.borrow_mut();
            state.current = Some(jid.to_owned());
            state.stick_to_bottom = true;
            state
                .chats
                .iter()
                .find(|c| c.jid == jid)
                .map(|c| c.name.clone())
        };
        self.content_page
            .set_title(name.as_deref().unwrap_or("Chat"));
        self.show_messages(&[]);
        self.content.set_visible_child_name("chat");
        self.split.set_show_content(true);
        self.reload_current();
        self.request(Request::MarkRead {
            chat: jid.to_owned(),
        });
        self.entry.grab_focus();
    }

    fn reload_current(&self) {
        let current = self.state.borrow().current.clone();
        if let Some(chat) = current {
            self.request(Request::ListMessages {
                chat,
                before: None,
                limit: PAGE_SIZE,
            });
        }
    }

    fn mark_current_read(&self) {
        let unread_chat = {
            let state = self.state.borrow();
            state.current.as_ref().and_then(|jid| {
                state
                    .chats
                    .iter()
                    .find(|c| &c.jid == jid && c.unread > 0)
                    .map(|c| c.jid.clone())
            })
        };
        if let Some(chat) = unread_chat {
            self.request(Request::MarkRead { chat });
        }
    }

    // ── Conversation ───────────────────────────────────────────────────

    fn show_messages(&self, messages: &[Message]) {
        while let Some(child) = self.messages.first_child() {
            self.messages.remove(&child);
        }
        {
            let mut state = self.state.borrow_mut();
            state.meta.clear();
            state.last_day = None;
            state.stick_to_bottom = true;
        }
        for message in messages {
            self.append_message(message);
        }
    }

    fn on_message(&self, message: Message) {
        let is_current = self.state.borrow().current.as_deref() == Some(message.chat.as_str());
        if is_current {
            // Sending always jumps to the end; reading back does not get
            // interrupted by incoming messages.
            if message.from_me {
                self.state.borrow_mut().stick_to_bottom = true;
            }
            self.append_message(&message);
        }
        if !message.from_me && !(is_current && self.window.is_active()) {
            self.notify(&message);
        }
    }

    fn append_message(&self, message: &Message) {
        if self.state.borrow().meta.contains_key(&message.id) {
            return;
        }
        let when = glib::DateTime::from_unix_local(message.timestamp).ok();
        if let Some(when) = &when {
            let day = (when.year(), when.day_of_year());
            if self.state.borrow().last_day != Some(day) {
                self.state.borrow_mut().last_day = Some(day);
                let label = gtk::Label::new(Some(&day_label(when)));
                label.add_css_class("day");
                label.set_halign(gtk::Align::Center);
                self.messages.append(&label);
            }
        }

        let in_group = {
            let state = self.state.borrow();
            state
                .chats
                .iter()
                .any(|c| c.jid == message.chat && c.is_group)
        };

        let bubble = gtk::Box::new(gtk::Orientation::Vertical, 2);
        bubble.add_css_class("bubble");
        bubble.add_css_class(if message.from_me { "out" } else { "in" });
        bubble.set_halign(if message.from_me {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });

        if in_group
            && !message.from_me
            && let Some(name) = &message.sender_name
        {
            let sender = gtk::Label::new(Some(name));
            sender.add_css_class("sender");
            sender.set_xalign(0.0);
            sender.set_ellipsize(pango::EllipsizeMode::End);
            bubble.append(&sender);
        }

        let body = gtk::Label::new(Some(&message.text));
        body.set_wrap(true);
        body.set_wrap_mode(pango::WrapMode::WordChar);
        body.set_xalign(0.0);
        body.set_max_width_chars(52);
        body.set_selectable(true);
        if message.kind != MessageKind::Text {
            body.add_css_class("media");
        }
        bubble.append(&body);

        let time = when
            .and_then(|w| w.format("%H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let meta = gtk::Label::new(None);
        meta.add_css_class("meta");
        meta.set_halign(gtk::Align::End);
        set_meta(&meta, &time, message.from_me.then_some(message.status));
        bubble.append(&meta);

        self.messages.append(&bubble);
        self.state
            .borrow_mut()
            .meta
            .insert(message.id.clone(), (meta, time));
    }

    fn send_current(&self) {
        let text = self.entry.text().trim().to_owned();
        let Some(chat) = self.state.borrow().current.clone() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.request(Request::SendText { chat, text });
        self.entry.set_text("");
    }

    fn notify(&self, message: &Message) {
        let chat_name = {
            let state = self.state.borrow();
            state
                .chats
                .iter()
                .find(|c| c.jid == message.chat)
                .map(|c| (c.name.clone(), c.is_group))
        };
        let sender = message.sender_name.as_deref().unwrap_or("New message");
        let (title, body) = match chat_name {
            Some((name, true)) => (name, format!("{sender}: {}", message.text)),
            Some((name, false)) => (name, message.text.clone()),
            None => (sender.to_owned(), message.text.clone()),
        };
        let notification = gio::Notification::new(&title);
        notification.set_body(Some(&body));
        // One notification per chat: a newer message replaces the older one.
        self.app
            .send_notification(Some(&message.chat), &notification);
    }

    // ── Dialogs ────────────────────────────────────────────────────────

    fn confirm_unlink(self: &Rc<Self>) {
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("Unlink this device?"),
            Some(
                "WhatKroft will be removed from your linked devices. Messages already \
                 stored on this computer are kept. You can link again at any time.",
            ),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("unlink", "Unlink");
        dialog.set_response_appearance("unlink", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let this = Rc::clone(self);
        dialog.connect_response(None, move |_, response| {
            if response == "unlink" {
                this.request(Request::Logout);
            }
        });
        dialog.present();
    }

    fn show_about(&self) {
        adw::AboutWindow::builder()
            .transient_for(&self.window)
            .application_name("WhatKroft")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(
                "A native WhatsApp client for Linux.\n\nUnofficial: not affiliated with, \
                 endorsed by, or supported by WhatsApp or Meta.",
            )
            .website(env!("CARGO_PKG_REPOSITORY"))
            .license_type(gtk::License::Gpl30)
            .build()
            .present();
    }
}

fn chat_row(chat: &Chat) -> gtk::ListBoxRow {
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
        let badge = gtk::Label::new(Some(&chat.unread.to_string()));
        badge.add_css_class("badge");
        badge.set_valign(gtk::Align::Center);
        bottom.append(&badge);
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

/// Time and delivery state under a bubble. `status` is `None` for incoming
/// messages, which show only the time.
fn set_meta(label: &gtk::Label, time: &str, status: Option<MessageStatus>) {
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

fn same_day(a: &glib::DateTime, b: &glib::DateTime) -> bool {
    a.year() == b.year() && a.day_of_year() == b.day_of_year()
}

/// Separator text between days in a conversation.
fn day_label(when: &glib::DateTime) -> String {
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

/// Compact time for the chat list: the clock time today, the date before.
fn list_time(timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    let Ok(when) = glib::DateTime::from_unix_local(timestamp) else {
        return String::new();
    };
    let today = glib::DateTime::now_local().is_ok_and(|now| same_day(&when, &now));
    let format = if today { "%H:%M" } else { "%-e %b" };
    when.format(format)
        .map(|s| s.to_string())
        .unwrap_or_default()
}
