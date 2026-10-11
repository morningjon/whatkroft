//! Pairing QR code, drawn with cairo so it stays sharp at any size.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use qrcode::{Color, QrCode};

/// Light margin around the code, in modules. Scanners need at least four.
const QUIET_ZONE: usize = 4;

#[derive(Clone)]
pub struct QrView {
    area: gtk::DrawingArea,
    code: Rc<RefCell<Option<QrCode>>>,
}

impl QrView {
    pub fn new(size: i32) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_width(size)
            .content_height(size)
            .halign(gtk::Align::Center)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        area.add_css_class("qr");
        let code: Rc<RefCell<Option<QrCode>>> = Rc::default();

        let shared = Rc::clone(&code);
        area.set_draw_func(move |_, cr, width, height| {
            // Always black on white, whatever the theme: phones scan it.
            cr.set_source_rgb(1.0, 1.0, 1.0);
            let _ = cr.paint();
            let code = shared.borrow();
            let Some(code) = code.as_ref() else { return };

            let modules = code.width();
            let total = (modules + 2 * QUIET_ZONE) as f64;
            let scale = f64::from(width.min(height)) / total;
            let origin_x = (f64::from(width) - total * scale) / 2.0;
            let origin_y = (f64::from(height) - total * scale) / 2.0;

            cr.set_source_rgb(0.0, 0.0, 0.0);
            for (index, color) in code.to_colors().into_iter().enumerate() {
                if color == Color::Dark {
                    let x = (index % modules + QUIET_ZONE) as f64;
                    let y = (index / modules + QUIET_ZONE) as f64;
                    cr.rectangle(origin_x + x * scale, origin_y + y * scale, scale, scale);
                }
            }
            let _ = cr.fill();
        });

        Self { area, code }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn set_payload(&self, payload: &str) {
        match QrCode::new(payload.as_bytes()) {
            Ok(code) => *self.code.borrow_mut() = Some(code),
            Err(e) => {
                log::error!("pairing code does not fit a QR code: {e}");
                *self.code.borrow_mut() = None;
            }
        }
        self.area.queue_draw();
    }
}
