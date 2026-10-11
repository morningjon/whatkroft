//! WhatKroft: a native GTK4 client for WhatsApp on Linux.
//!
//! This binary is only the window. The WhatsApp connection and the chat
//! database live in `whatkroftd`, which is started on demand.

mod client;
mod qr;
mod ui;

use adw::prelude::*;
use gtk::glib;

pub const APP_ID: &str = "io.github.morningjon.WhatKroft";

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| ui::load_css());
    app.connect_activate(|app| match app.active_window() {
        // A second launch raises the window that is already open.
        Some(window) => window.present(),
        None => ui::App::start(app),
    });
    app.run()
}
