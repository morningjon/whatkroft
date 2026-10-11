//! The main window: pairing screen, chat list and conversation view.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use whatkroft_ipc::{Chat, ConnState, Event, Message, MessageKind, Reply, Request, ServerMessage};

use crate::client::{self, Conn};
use crate::qr::QrView;
use crate::widgets::{self, ARCHIVE_ROW, MediaState};

/// Messages loaded when a chat is opened, and per step when scrolling back.
const PAGE_SIZE: u32 = 100;
/// Distance from the top (in pixels) at which earlier messages are fetched.
const LOAD_OLDER_WITHIN: f64 = 400.0;
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
    sidebar_page: adw::NavigationPage,
    /// Leaves the archive; only shown while it is open.
    back_button: gtk::Button,
    content_page: adw::NavigationPage,
    /// Context menu for chat rows. One popover for the whole list, owned by
    /// a widget that outlives the rows, which are rebuilt on every change.
    row_menu: gtk::PopoverMenu,
    /// Per-chat menu in the conversation header.
    chat_menu: gio::Menu,
    chat_menu_button: gtk::MenuButton,
    /// Right-hand side: "empty" until a chat is opened, then "chat".
    content: gtk::Stack,
    messages: gtk::Box,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    state: RefCell<State>,
}

/// The widgets of one message on screen.
struct Row {
    bubble: gtk::Box,
    meta: gtk::Label,
    /// Clock time shown under the bubble, kept to rebuild the status line.
    time: String,
}

#[derive(Default)]
struct State {
    conn: Option<Rc<Conn>>,
    chats: Vec<Chat>,
    current: Option<String>,
    /// The sidebar lists archived chats instead of the main list.
    showing_archived: bool,
    /// Messages of the open chat, oldest first.
    loaded: Vec<Message>,
    rows: HashMap<String, Row>,
    /// (year, day of year) of the last message drawn, for day separators.
    last_day: Option<(i32, i32)>,
    /// Keep the view pinned to the newest message as more arrive.
    stick_to_bottom: bool,
    /// Earlier messages may exist beyond the oldest one loaded.
    has_more: bool,
    loading_older: bool,
    /// Distance from the end to restore after earlier messages are inserted
    /// above, so the text being read does not jump.
    anchor_from_end: Option<f64>,
    /// Attachments being fetched, by message id.
    downloading: HashSet<String>,
    /// Attachments whose last fetch failed; not retried automatically.
    failed: HashSet<String>,
    /// Attachments the user asked to open, to act on once they arrive.
    open_when_ready: HashSet<String>,
    /// The voice note or audio file being played, by message id.
    player: Option<(String, gtk::MediaFile)>,
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
        let back_button = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back to chats")
            .action_name("win.show-chats")
            .visible(false)
            .build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.pack_start(&back_button);
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
        let attach = gtk::Button::builder()
            .icon_name("mail-attachment-symbolic")
            .tooltip_text("Send a file")
            .action_name("win.attach")
            .build();
        attach.add_css_class("flat");
        attach.add_css_class("circular");
        let send = gtk::Button::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Send")
            .action_name("win.send")
            .build();
        send.add_css_class("suggested-action");
        send.add_css_class("circular");
        let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        composer.add_css_class("composer");
        composer.append(&attach);
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

        let chat_menu = gio::Menu::new();
        let chat_menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&chat_menu)
            .tooltip_text("Chat Menu")
            .visible(false)
            .build();
        let content_header = adw::HeaderBar::new();
        content_header.pack_end(&chat_menu_button);
        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&content_header);
        content_view.set_content(Some(&content));
        let content_page = adw::NavigationPage::new(&content_view, "WhatKroft");

        let split = adw::NavigationSplitView::new();
        split.set_min_sidebar_width(260.0);
        split.set_max_sidebar_width(360.0);
        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        split.set_vexpand(true);

        let row_menu = gtk::PopoverMenu::from_model(None::<&gio::Menu>);
        row_menu.set_parent(&split);
        row_menu.set_has_arrow(false);
        row_menu.set_halign(gtk::Align::Start);

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
            sidebar_page,
            back_button,
            content_page,
            row_menu,
            chat_menu,
            chat_menu_button,
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
            let name = row.widget_name();
            if name == ARCHIVE_ROW {
                this.show_archive(true);
            } else {
                this.open_chat(&name);
            }
        });

        // Right-click on a chat offers to archive it.
        let this = Rc::clone(self);
        let secondary = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        secondary.connect_pressed(move |gesture, _, x, y| {
            if let Some(row) = this.chat_list.row_at_y(y as i32) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                this.show_row_menu(&row, x, y);
            }
        });
        self.chat_list.add_controller(secondary);

        let this = Rc::clone(self);
        self.entry.connect_activate(move |_| this.send_current());

        let adjustment = self.scroller.vadjustment();
        let this = Rc::clone(self);
        adjustment.connect_value_changed(move |adj| {
            // Follow new messages only while the reader is at the end.
            let at_end = adj.value() + adj.page_size() >= adj.upper() - BOTTOM_SLACK;
            this.state.borrow_mut().stick_to_bottom = at_end;
            if !at_end && adj.value() < LOAD_OLDER_WITHIN {
                this.load_older();
            }
        });
        let this = Rc::clone(self);
        adjustment.connect_upper_notify(move |adj| {
            let (stick, anchor) = {
                let state = this.state.borrow();
                (state.stick_to_bottom, state.anchor_from_end)
            };
            if let Some(anchor) = anchor {
                adj.set_value(adj.upper() - anchor);
            } else if stick {
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

        // Dropping files on the conversation sends them.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let this = Rc::clone(self);
        drop.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            for file in files.files() {
                if let Some(path) = file.path() {
                    this.send_file(&path);
                }
            }
            true
        });
        self.scroller.add_controller(drop);
    }

    fn install_actions(self: &Rc<Self>) {
        let simple = |name: &str, run: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            let this = Rc::clone(self);
            action.connect_activate(move |_, _| run(&this));
            self.window.add_action(&action);
        };
        simple("send", |this| this.send_current());
        simple("attach", |this| this.choose_file());
        simple("unlink", |this| this.confirm_unlink());
        simple("about", |this| this.show_about());
        simple("show-chats", |this| this.show_archive(false));

        // Archive or unarchive the chat whose JID is the parameter.
        for (name, archived) in [("archive-chat", true), ("unarchive-chat", false)] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let this = Rc::clone(self);
            action.connect_activate(move |_, parameter| {
                if let Some(chat) = parameter.and_then(|p| p.str()) {
                    this.request(Request::SetArchived {
                        chat: chat.to_owned(),
                        archived,
                    });
                }
            });
            self.window.add_action(&action);
        }
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

    fn handle(self: &Rc<Self>, message: ServerMessage) {
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

    fn handle_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::State(state) => self.apply_state(state),
            Event::Chat(chat) => self.upsert_chat(chat),
            Event::ChatsChanged => {
                self.request(Request::ListChats);
                self.reload_current();
            }
            Event::Message(message) => self.on_message(message),
            Event::Status { chat, id, status } => {
                let mut state = self.state.borrow_mut();
                if state.current.as_deref() != Some(chat.as_str()) {
                    return;
                }
                if let Some(message) = state.loaded.iter_mut().find(|m| m.id == id) {
                    message.status = status;
                }
                if let Some(row) = state.rows.get(&id) {
                    widgets::set_meta(&row.meta, &row.time, Some(status));
                }
            }
            Event::Media { chat, id, path } => {
                let open = {
                    let mut state = self.state.borrow_mut();
                    state.downloading.remove(&id);
                    state.failed.remove(&id);
                    if state.current.as_deref() != Some(chat.as_str()) {
                        return;
                    }
                    if let Some(media) = state
                        .loaded
                        .iter_mut()
                        .find(|m| m.id == id)
                        .and_then(|m| m.media.as_mut())
                    {
                        media.path = Some(path);
                    }
                    state.open_when_ready.remove(&id)
                };
                self.refresh_bubble(&id);
                if open {
                    self.open_media(&id);
                }
            }
            Event::MediaFailed { chat, id, error } => {
                let asked = {
                    let mut state = self.state.borrow_mut();
                    state.downloading.remove(&id);
                    state.failed.insert(id.clone());
                    state.open_when_ready.remove(&id)
                        && state.current.as_deref() == Some(chat.as_str())
                };
                self.refresh_bubble(&id);
                // Only interrupt for something the user clicked; a photo that
                // failed to load in the background just shows as failed.
                if asked {
                    self.toasts.add_toast(adw::Toast::new(&error));
                }
            }
        }
    }

    fn handle_reply(self: &Rc<Self>, reply: Reply) {
        match reply {
            Reply::Chats(chats) => {
                self.state.borrow_mut().chats = chats;
                self.rebuild_chat_list();
            }
            Reply::Messages {
                chat,
                before_id,
                messages,
            } => {
                if self.state.borrow().current.as_deref() != Some(chat.as_str()) {
                    return;
                }
                match before_id {
                    None => self.show_messages(messages),
                    Some(_) => self.prepend_messages(messages),
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
        let archived: Vec<&Chat> = state.chats.iter().filter(|c| c.archived).collect();
        // An emptied archive has nothing to show: fall back to the chats.
        let in_archive = state.showing_archived && !archived.is_empty();

        self.back_button.set_visible(in_archive);
        self.sidebar_page
            .set_title(if in_archive { "Archived" } else { "WhatKroft" });

        if !in_archive && !archived.is_empty() {
            let unread = archived.iter().map(|c| c.unread).sum();
            self.chat_list
                .append(&widgets::archived_row(archived.len(), unread));
        }
        let mut selected = false;
        for chat in state.chats.iter().filter(|c| c.archived == in_archive) {
            let row = widgets::chat_row(chat);
            self.chat_list.append(&row);
            if state.current.as_deref() == Some(chat.jid.as_str()) {
                self.chat_list.select_row(Some(&row));
                selected = true;
            }
        }
        if !selected {
            self.chat_list.unselect_all();
        }
        if let Some(chat) = state
            .current
            .as_ref()
            .and_then(|jid| state.chats.iter().find(|c| &c.jid == jid))
        {
            self.content_page.set_title(&chat.name);
            self.update_chat_menu(chat);
        }
    }

    fn show_archive(&self, show: bool) {
        self.state.borrow_mut().showing_archived = show;
        self.rebuild_chat_list();
    }

    /// The conversation header's menu for the open chat.
    fn update_chat_menu(&self, chat: &Chat) {
        self.chat_menu.remove_all();
        self.chat_menu.append_item(&archive_item(chat));
        self.chat_menu_button.set_visible(true);
    }

    /// Pop up the context menu for `row` at (x, y) in chat list coordinates.
    fn show_row_menu(&self, row: &gtk::ListBoxRow, x: f64, y: f64) {
        let jid = row.widget_name();
        let menu = gio::Menu::new();
        {
            let state = self.state.borrow();
            let Some(chat) = state.chats.iter().find(|c| c.jid == jid.as_str()) else {
                return;
            };
            menu.append_item(&archive_item(chat));
        }
        let Some(point) = self
            .chat_list
            .compute_point(&self.split, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        self.row_menu.set_menu_model(Some(&menu));
        self.row_menu.set_pointing_to(Some(&gdk::Rectangle::new(
            point.x() as i32,
            point.y() as i32,
            1,
            1,
        )));
        self.row_menu.popup();
    }

    fn open_chat(self: &Rc<Self>, jid: &str) {
        let chat = {
            let mut state = self.state.borrow_mut();
            state.current = Some(jid.to_owned());
            state.chats.iter().find(|c| c.jid == jid).cloned()
        };
        self.stop_player();
        match &chat {
            Some(chat) => {
                self.content_page.set_title(&chat.name);
                self.update_chat_menu(chat);
            }
            None => self.content_page.set_title("Chat"),
        }
        self.show_messages(Vec::new());
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
                before_id: None,
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

    /// Fetch the page before the oldest message on screen.
    fn load_older(&self) {
        let request = {
            let mut state = self.state.borrow_mut();
            if !state.has_more || state.loading_older {
                return;
            }
            let (Some(chat), Some(oldest)) = (state.current.clone(), state.loaded.first()) else {
                return;
            };
            let before_id = Some(oldest.id.clone());
            state.loading_older = true;
            Request::ListMessages {
                chat,
                before_id,
                limit: PAGE_SIZE,
            }
        };
        self.request(request);
    }

    /// Replace the conversation with the newest page of messages.
    fn show_messages(self: &Rc<Self>, messages: Vec<Message>) {
        {
            let mut state = self.state.borrow_mut();
            state.has_more = messages.len() as u32 >= PAGE_SIZE;
            state.loading_older = false;
            state.anchor_from_end = None;
            state.stick_to_bottom = true;
            state.loaded = messages;
        }
        self.render_all();
    }

    /// Insert an earlier page above what is shown, keeping the reader's place.
    fn prepend_messages(self: &Rc<Self>, mut older: Vec<Message>) {
        let adjustment = self.scroller.vadjustment();
        {
            let mut state = self.state.borrow_mut();
            state.loading_older = false;
            state.has_more = older.len() as u32 >= PAGE_SIZE;
            let known: HashSet<&str> = state.loaded.iter().map(|m| m.id.as_str()).collect();
            older.retain(|m| !known.contains(m.id.as_str()));
            if older.is_empty() {
                return;
            }
            older.append(&mut state.loaded);
            state.loaded = older;
            state.stick_to_bottom = false;
            state.anchor_from_end = Some(adjustment.upper() - adjustment.value());
        }
        self.render_all();
        // Layout settles over a few frames; hold the anchor until it has.
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            this.state.borrow_mut().anchor_from_end = None;
        });
    }

    fn render_all(self: &Rc<Self>) {
        while let Some(child) = self.messages.first_child() {
            self.messages.remove(&child);
        }
        let messages = {
            let mut state = self.state.borrow_mut();
            state.rows.clear();
            state.last_day = None;
            state.loaded.clone()
        };
        if self.state.borrow().has_more {
            let hint = gtk::Label::new(Some("Scroll up for earlier messages"));
            hint.add_css_class("loading-older");
            self.messages.append(&hint);
        }
        for message in &messages {
            self.append_row(message);
        }
    }

    fn on_message(self: &Rc<Self>, message: Message) {
        let is_current = self.state.borrow().current.as_deref() == Some(message.chat.as_str());
        if is_current {
            let fresh = {
                let mut state = self.state.borrow_mut();
                // Sending always jumps to the end; reading back does not get
                // interrupted by incoming messages.
                if message.from_me {
                    state.stick_to_bottom = true;
                    state.anchor_from_end = None;
                }
                let fresh = !state.loaded.iter().any(|m| m.id == message.id);
                if fresh {
                    state.loaded.push(message.clone());
                }
                fresh
            };
            if fresh {
                self.append_row(&message);
            }
        }
        if !message.from_me && !(is_current && self.window.is_active()) {
            self.notify(&message);
        }
    }

    /// Add one message at the end of the conversation.
    fn append_row(self: &Rc<Self>, message: &Message) {
        let when = glib::DateTime::from_unix_local(message.timestamp).ok();
        if let Some(when) = &when {
            let day = (when.year(), when.day_of_year());
            if self.state.borrow().last_day != Some(day) {
                self.state.borrow_mut().last_day = Some(day);
                let label = gtk::Label::new(Some(&widgets::day_label(when)));
                label.add_css_class("day");
                label.set_halign(gtk::Align::Center);
                self.messages.append(&label);
            }
        }

        let bubble = gtk::Box::new(gtk::Orientation::Vertical, 2);
        bubble.add_css_class("bubble");
        bubble.add_css_class(if message.from_me { "out" } else { "in" });
        bubble.set_halign(if message.from_me {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });
        let meta = gtk::Label::new(None);
        meta.add_css_class("meta");
        meta.set_halign(gtk::Align::End);
        let time = when
            .and_then(|w| w.format("%H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();

        self.messages.append(&bubble);
        self.state
            .borrow_mut()
            .rows
            .insert(message.id.clone(), Row { bubble, meta, time });
        self.fill_bubble(message);
    }

    /// Rebuild one message's bubble after its attachment changed state.
    fn refresh_bubble(self: &Rc<Self>, id: &str) {
        let message = self
            .state
            .borrow()
            .loaded
            .iter()
            .find(|m| m.id == id)
            .cloned();
        let Some(message) = message else { return };
        // Rebuilding a bubble that holds the keyboard focus would hand the
        // focus to its first label, which selects all its text.
        let had_focus = self
            .state
            .borrow()
            .rows
            .get(id)
            .is_some_and(|row| row.bubble.focus_child().is_some());
        self.fill_bubble(&message);
        if had_focus {
            self.entry.grab_focus_without_selecting();
        }
    }

    fn media_state(&self, message: &Message) -> MediaState {
        let state = self.state.borrow();
        if state.downloading.contains(&message.id) {
            MediaState::Downloading
        } else if state.failed.contains(&message.id) {
            MediaState::Failed
        } else if state
            .player
            .as_ref()
            .is_some_and(|(id, player)| id == &message.id && player.is_playing())
        {
            MediaState::Playing
        } else {
            MediaState::Idle
        }
    }

    fn fill_bubble(self: &Rc<Self>, message: &Message) {
        let (bubble, meta, time, in_group) = {
            let state = self.state.borrow();
            let Some(row) = state.rows.get(&message.id) else {
                return;
            };
            let in_group = state
                .chats
                .iter()
                .any(|c| c.jid == message.chat && c.is_group);
            (
                row.bubble.clone(),
                row.meta.clone(),
                row.time.clone(),
                in_group,
            )
        };
        while let Some(child) = bubble.first_child() {
            bubble.remove(&child);
        }
        if message.kind == MessageKind::Sticker {
            bubble.add_css_class("sticker");
        }

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

        if let Some(media) = &message.media {
            // Photos and stickers load by themselves; larger kinds wait for
            // a click.
            let auto = matches!(message.kind, MessageKind::Image | MessageKind::Sticker);
            if auto && media.path.is_none() && self.media_state(message) == MediaState::Idle {
                self.start_download(message, false);
            }
            let state = self.media_state(message);
            match message.kind {
                MessageKind::Image | MessageKind::Sticker | MessageKind::Video => {
                    let picture = widgets::picture(message.kind, media, state);
                    let click = gtk::GestureClick::new();
                    let (this, id) = (Rc::clone(self), message.id.clone());
                    click.connect_released(move |_, _, _, _| this.activate_media_later(&id));
                    picture.add_controller(click);
                    picture.set_cursor_from_name(Some("pointer"));
                    bubble.append(&picture);
                }
                _ => {
                    let (row, button) = widgets::attachment(message.kind, media, state);
                    let (this, id) = (Rc::clone(self), message.id.clone());
                    button.connect_clicked(move |_| this.activate_media_later(&id));
                    bubble.append(&row);
                }
            }
        }

        if !message.text.is_empty() {
            let body = gtk::Label::new(Some(&message.text));
            body.set_wrap(true);
            body.set_wrap_mode(pango::WrapMode::WordChar);
            body.set_xalign(0.0);
            body.set_max_width_chars(52);
            body.set_selectable(true);
            if message.media.is_none() && message.kind != MessageKind::Text {
                body.add_css_class("described");
            }
            bubble.append(&body);
        }

        widgets::set_meta(&meta, &time, message.from_me.then_some(message.status));
        bubble.append(&meta);
    }

    // ── Attachments ────────────────────────────────────────────────────

    fn start_download(&self, message: &Message, open_after: bool) {
        {
            let mut state = self.state.borrow_mut();
            state.failed.remove(&message.id);
            if open_after {
                state.open_when_ready.insert(message.id.clone());
            }
            if !state.downloading.insert(message.id.clone()) {
                return;
            }
        }
        self.request(Request::DownloadMedia {
            chat: message.chat.clone(),
            id: message.id.clone(),
        });
    }

    /// Acting on a click rebuilds the bubble, which destroys the widget that
    /// was clicked; do it once the click has finished being delivered.
    fn activate_media_later(self: &Rc<Self>, id: &str) {
        let (this, id) = (Rc::clone(self), id.to_owned());
        glib::idle_add_local_once(move || this.activate_media(&id));
    }

    /// The user clicked an attachment: open it, fetching it first if needed.
    fn activate_media(self: &Rc<Self>, id: &str) {
        let message = self
            .state
            .borrow()
            .loaded
            .iter()
            .find(|m| m.id == id)
            .cloned();
        let Some(message) = message else { return };
        let on_disk = message
            .media
            .as_ref()
            .and_then(|m| m.path.as_deref())
            .is_some_and(|path| std::path::Path::new(path).is_file());
        if on_disk {
            self.open_media(id);
        } else {
            self.start_download(&message, true);
            self.refresh_bubble(id);
        }
    }

    fn open_media(self: &Rc<Self>, id: &str) {
        let found = {
            let state = self.state.borrow();
            state.loaded.iter().find(|m| m.id == id).and_then(|m| {
                let path = m.media.as_ref()?.path.clone()?;
                Some((m.kind, path))
            })
        };
        let Some((kind, path)) = found else { return };
        if kind == MessageKind::Audio {
            self.toggle_playback(id, &path);
        } else {
            self.open_externally(&path);
        }
    }

    /// Hand a file to the desktop's default application for its type.
    fn open_externally(&self, path: &str) {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
        let toasts = self.toasts.clone();
        launcher.launch(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Err(e) = result {
                log::warn!("opening attachment: {e}");
                toasts.add_toast(adw::Toast::new("No application could open this file"));
            }
        });
    }

    fn stop_player(self: &Rc<Self>) {
        let previous = self.state.borrow_mut().player.take();
        if let Some((id, player)) = previous {
            player.pause();
            self.refresh_bubble(&id);
        }
    }

    /// Play or pause a voice note in place. If GTK has no way to play the
    /// format here, fall back to the desktop's audio player.
    fn toggle_playback(self: &Rc<Self>, id: &str, path: &str) {
        let current = self.state.borrow().player.clone();
        if let Some((playing_id, player)) = current
            && playing_id == id
        {
            if player.is_playing() {
                player.pause();
            } else {
                player.play();
            }
            self.refresh_bubble(id);
            return;
        }
        self.stop_player();

        let player = gtk::MediaFile::for_filename(path);
        let (this, owned_id) = (Rc::clone(self), id.to_owned());
        player.connect_playing_notify(move |_| this.refresh_bubble(&owned_id));
        let (this, owned_id, owned_path) = (Rc::clone(self), id.to_owned(), path.to_owned());
        player.connect_error_notify(move |player| {
            if let Some(error) = player.error() {
                log::warn!("in-app playback unavailable ({error}); opening externally");
                let ours = this
                    .state
                    .borrow()
                    .player
                    .as_ref()
                    .is_some_and(|(id, _)| id == &owned_id);
                if ours {
                    this.state.borrow_mut().player = None;
                    this.refresh_bubble(&owned_id);
                    this.open_externally(&owned_path);
                }
            }
        });
        player.play();
        self.state.borrow_mut().player = Some((id.to_owned(), player.clone()));
        self.refresh_bubble(id);

        // Without a media backend GTK hands out a player that never starts
        // and never reports an error. Give it a moment, then fall back.
        let (this, owned_id, owned_path) = (Rc::clone(self), id.to_owned(), path.to_owned());
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            let stuck = this
                .state
                .borrow()
                .player
                .as_ref()
                .is_some_and(|(id, current)| id == &owned_id && current == &player)
                && !player.is_prepared()
                && player.error().is_none();
            if stuck {
                log::warn!("no in-app audio playback available; opening externally");
                this.state.borrow_mut().player = None;
                this.refresh_bubble(&owned_id);
                this.open_externally(&owned_path);
            }
        });
    }

    // ── Sending ────────────────────────────────────────────────────────

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

    fn choose_file(self: &Rc<Self>) {
        if self.state.borrow().current.is_none() {
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Send a File")
            .accept_label("Send")
            .modal(true)
            .build();
        let this = Rc::clone(self);
        dialog.open_multiple(Some(&self.window), gio::Cancellable::NONE, move |result| {
            // Err here is the user closing the dialog.
            let Ok(files) = result else { return };
            for file in files.iter::<gio::File>().flatten() {
                if let Some(path) = file.path() {
                    this.send_file(&path);
                }
            }
        });
    }

    /// Send a file to the open chat. Whatever is typed in the message box
    /// goes along as the caption of the first file.
    fn send_file(&self, path: &std::path::Path) {
        let Some(chat) = self.state.borrow().current.clone() else {
            return;
        };
        let Some(path_text) = path.to_str() else {
            self.toasts
                .add_toast(adw::Toast::new("That file name cannot be sent"));
            return;
        };
        let caption = self.entry.text().trim().to_owned();
        self.entry.set_text("");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.toasts
            .add_toast(adw::Toast::new(&format!("Sending {name}…")));
        self.request(Request::SendFile {
            chat,
            path: path_text.to_owned(),
            caption: (!caption.is_empty()).then_some(caption),
        });
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
        let text = whatkroft_ipc::preview(message.kind, &message.text, message.media.as_ref());
        let sender = message.sender_name.as_deref().unwrap_or("New message");
        let (title, body) = match chat_name {
            Some((name, true)) => (name, format!("{sender}: {text}")),
            Some((name, false)) => (name, text),
            None => (sender.to_owned(), text),
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

/// Menu entry that archives `chat`, or takes it back out of the archive.
fn archive_item(chat: &Chat) -> gio::MenuItem {
    let (label, action) = if chat.archived {
        ("Unarchive Chat", "win.unarchive-chat")
    } else {
        ("Archive Chat", "win.archive-chat")
    };
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(action), Some(&chat.jid.to_variant()));
    item
}
