//! @file preview-probe.rs
//! @brief 私有测试桌面的连续动画与静止帧探针
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11

use gtk4::{self as gtk, glib, prelude::*};
use std::{cell::Cell, rc::Rc};

fn main() {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
    assert!(runtime.starts_with("/tmp/cv-") || runtime.starts_with("/tmp/cp-"));
    gtk::init().unwrap();
    let window = gtk::Window::builder()
        .title("Realtime preview probe")
        .build();
    let area = gtk::DrawingArea::builder()
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    let counter = Rc::new(Cell::new(0u32));
    let animated = Rc::new(Cell::new(true));
    let draw_count = counter.clone();
    area.set_draw_func(move |_, cr, width, height| {
        let count = draw_count.get();
        cr.set_source_rgb(f64::from(count % 255) / 255.0, 0.2, 0.4);
        cr.paint().unwrap();
        cr.set_source_rgb(1.0, 0.0, 0.0);
        cr.rectangle(0.0, 0.0, 128.0, 128.0);
        cr.fill().unwrap();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.rectangle(
            f64::from(count % width.max(1) as u32),
            160.0,
            60.0,
            f64::from(height - 160),
        );
        cr.fill().unwrap();
        cr.set_source_rgb(0.0, 0.0, 0.0);
        cr.set_font_size(24.0);
        cr.move_to(180.0, 80.0);
        cr.show_text(&format!("Native pixels / frame {count} / {width}x{height}"))
            .unwrap();
    });
    let animation = animated.clone();
    area.add_tick_callback(move |area, _| {
        if animation.get() {
            counter.set(counter.get().wrapping_add(1));
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::space {
            animated.set(!animated.get());
        }
        glib::Propagation::Stop
    });
    area.add_controller(keys);
    window.set_child(Some(&area));
    window.fullscreen();
    window.present();
    area.grab_focus();
    glib::MainLoop::new(None, false).run();
}
