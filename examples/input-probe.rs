//! @file input-probe.rs
//! @brief 私有测试桌面内记录真实 GTK 输入事件
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use gtk4::{self as gtk, glib, prelude::*};
use std::{cell::RefCell, io::Write, rc::Rc};

fn main() {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
    assert!(
        runtime.starts_with("/tmp/cn-")
            || runtime.starts_with("/tmp/cp-")
            || runtime.starts_with("/tmp/cm-")
    );
    let path = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    assert!(path.starts_with(&runtime));
    let file = Rc::new(RefCell::new(std::fs::File::create(path).unwrap()));
    let log = move |value: serde_json::Value| {
        let mut file = file.borrow_mut();
        writeln!(file, "{value}").unwrap();
        file.flush().unwrap();
    };
    gtk::init().unwrap();
    let window = gtk::Window::builder().title("MCP input probe").build();
    let area = gtk::DrawingArea::builder().focusable(true).build();
    let second = std::env::args().nth(2).as_deref() == Some("2");
    let size_log = log.clone();
    area.set_draw_func(move |_, context, width, height| {
        if second {
            context.set_source_rgb(0.4, 0.25, 0.1);
        } else {
            context.set_source_rgb(0.1, 0.25, 0.4);
        }
        let _ = context.paint();
        size_log(serde_json::json!({"event":"size","width":width,"height":height}));
    });
    let click = gtk::GestureClick::new();
    click.set_button(0);
    let press_log = log.clone();
    click.connect_pressed(move |gesture, _, x, y| {
        press_log(
            serde_json::json!({"event":"press","button":gesture.current_button(),"x":x,"y":y}),
        );
    });
    let release_log = log.clone();
    click.connect_released(move |gesture, _, x, y| {
        release_log(
            serde_json::json!({"event":"release","button":gesture.current_button(),"x":x,"y":y}),
        );
    });
    area.add_controller(click);
    let motion = gtk::EventControllerMotion::new();
    let motion_log = log.clone();
    motion.connect_motion(move |_, x, y| {
        motion_log(serde_json::json!({"event":"motion","x":x,"y":y}))
    });
    area.add_controller(motion);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let scroll_log = log.clone();
    scroll.connect_scroll(move |_, dx, dy| {
        scroll_log(serde_json::json!({"event":"scroll","dx":dx,"dy":dy}));
        glib::Propagation::Stop
    });
    area.add_controller(scroll);
    let keyboard = gtk::EventControllerKey::new();
    let press_log = log.clone();
    keyboard.connect_key_pressed(move |_, key, _, modifiers| {
        press_log(serde_json::json!({"event":"key_press","key":key.name().map(|v|v.to_string()),"modifiers":modifiers.bits()}));
        glib::Propagation::Stop
    });
    keyboard.connect_key_released(move |_, key, _, modifiers| {
        log(serde_json::json!({"event":"key_release","key":key.name().map(|v|v.to_string()),"modifiers":modifiers.bits()}));
    });
    area.add_controller(keyboard);
    window.set_child(Some(&area));
    window.fullscreen();
    window.present();
    area.grab_focus();
    let loop_ = glib::MainLoop::new(None, false);
    loop_.run();
}
