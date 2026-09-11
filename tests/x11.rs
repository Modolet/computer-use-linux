//! @file x11.rs
//! @brief 私有 XWayland 的真实输入、剪贴板、缩放及伪造窗口身份验收
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11
use computer_use_linux::{
    backend::{
        Backend, Cancellation,
        applications::DesktopApplication,
        preview::Viewport,
        sway::{Application, Isolated, SavedSession, sway_request},
    },
    model::*,
};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{self, ConnectionExt as _},
    wrapper::ConnectionExt as _,
};

struct Cleanup(SavedSession);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in [&self.0.app, &self.0.sway, &self.0.bus] {
            p.terminate();
        }
    }
}
fn environment() {
    assert_eq!(
        std::env::var("COMPUTER_USE_HEADLESS_TEST").as_deref(),
        Ok("1")
    );
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    assert!(runtime.starts_with("/tmp"));
    for (name, directory) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        assert_eq!(
            PathBuf::from(std::env::var_os(name).unwrap()),
            runtime.join(directory)
        );
    }
}
fn cancel() -> Cancellation {
    Cancellation::new(Arc::new(AtomicU64::new(0)))
}
fn observe(b: &mut Isolated) -> Observation {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match b.observe(None, &cancel()) {
            Ok(o) => return o,
            Err(e) if e.code == ErrorCode::StaleTarget && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => panic!("{e}"),
        }
    }
}
fn act(b: &mut Isolated, a: Action) {
    let o = observe(b);
    b.act(&o, &a, &cancel()).unwrap();
}
fn key(b: &mut Isolated, key: &str, modifiers: Vec<Modifier>) {
    act(
        b,
        Action::Key {
            key: key.into(),
            modifiers,
        },
    );
}
fn clipboard(b: &Isolated) -> String {
    std::thread::sleep(Duration::from_millis(150));
    let output = std::process::Command::new("wl-paste")
        .arg("--no-newline")
        .env("XDG_RUNTIME_DIR", &b.saved.runtime)
        .env("WAYLAND_DISPLAY", &b.saved.wayland)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn editor() -> Isolated {
    Isolated::launch(Application::Installed(DesktopApplication {
        id: "x11-test.desktop".into(),
        name: "X11 测试编辑器".into(),
        program: gtk4::glib::find_program_in_path("env").unwrap(),
        args: vec![
            "GDK_BACKEND=x11".into(),
            "gnome-text-editor".into(),
            "--standalone".into(),
        ],
        directory: None,
        desktop_file: PathBuf::from("/tmp/x11-test.desktop"),
    }))
    .unwrap()
}
#[test]
#[ignore = "需 tests/run-x11.sh 的临时个人环境和本机 WPS 安装"]
fn wps_launch_capture_and_keyboard() {
    environment();
    let app = DesktopApplication::resolve("WPS Spreadsheets").unwrap();
    assert_eq!(app.id, "wps-office-et.desktop");
    let mut b = Isolated::launch(Application::Installed(app)).unwrap();
    let _cleanup = Cleanup(b.saved.clone());
    assert!(b.saved.x11_application);
    println!("WPS session: {}", b.saved.runtime.display());
    let before = observe(&mut b);
    key(&mut b, "Tab", vec![]);
    std::thread::sleep(Duration::from_millis(300));
    let after = observe(&mut b);
    assert_ne!(
        before.png_base64, after.png_base64,
        "键盘应改变 WPS 真实窗口"
    );
    if let Ok(path) = std::env::var("COMPUTER_USE_TEST_PNG") {
        use base64::Engine;
        std::fs::write(
            path,
            base64::engine::general_purpose::STANDARD
                .decode(after.png_base64.unwrap())
                .unwrap(),
        )
        .unwrap();
    }
    assert!(b.saved.app.alive());
}
#[test]
#[ignore = "需 tests/run-x11.sh 的临时个人环境"]
fn x11_input_clipboard_identity_and_native_pixels() {
    environment();
    let mut user = editor();
    let _u = Cleanup(user.saved.clone());
    let mut agent = editor();
    let _a = Cleanup(agent.saved.clone());
    assert!(agent.saved.x11_application && user.saved.x11_application);
    assert_ne!(
        agent.saved.x11.as_ref().unwrap().display,
        user.saved.x11.as_ref().unwrap().display
    );
    for (b, text) in [(&mut user, "用户剪贴板"), (&mut agent, "Hello 中文 X11")] {
        key(b, "n", vec![Modifier::Ctrl]);
        act(b, Action::Text { text: text.into() });
        key(b, "a", vec![Modifier::Ctrl]);
        key(b, "c", vec![Modifier::Ctrl]);
        assert_eq!(clipboard(b), text);
    }
    let focus = sway_request(&user.saved.socket, 4, "").unwrap()["focus"].clone();
    act(
        &mut agent,
        Action::Text {
            text: "AI continues".into(),
        },
    );
    assert_eq!(clipboard(&user), "用户剪贴板");
    assert_eq!(
        focus,
        sway_request(&user.saved.socket, 4, "").unwrap()["focus"]
    );
    let old = observe(&mut agent);
    let mut preview = agent.realtime_preview().unwrap();
    preview
        .resize(Viewport::new(1600, 1000, 1.25).unwrap(), &cancel())
        .unwrap();
    let after = observe(&mut agent);
    assert_eq!(
        (after.target.width, after.target.height, after.target.scale),
        (1600, 1000, 1.0)
    );
    assert_eq!(
        agent
            .act(
                &old,
                &Action::Key {
                    key: "a".into(),
                    modifiers: vec![]
                },
                &cancel()
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    // A foreign X client cannot impersonate the authorized app with _NET_WM_PID,
    // including override-redirect popups absent from Sway's managed window tree.
    for override_redirect in [false, true] {
        let endpoint = agent.saved.x11.as_ref().unwrap();
        let conn = endpoint.connect().unwrap();
        let root = conn.setup().roots[0].root;
        let id = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            root,
            20,
            20,
            100,
            100,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            0,
            &xproto::CreateWindowAux::new().override_redirect(u32::from(override_redirect)),
        )
        .unwrap()
        .check()
        .unwrap();
        let atom = conn
            .intern_atom(false, b"_NET_WM_PID")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        conn.change_property32(
            xproto::PropMode::REPLACE,
            id,
            atom,
            xproto::AtomEnum::CARDINAL,
            &[agent.saved.app.pid],
        )
        .unwrap()
        .check()
        .unwrap();
        conn.map_window(id).unwrap().check().unwrap();
        conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let actual = endpoint.windows(&[]).unwrap();
        assert_eq!(actual[&id].process.pid, std::process::id());
        assert_eq!(
            agent.observe(None, &cancel()).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        conn.destroy_window(id).unwrap().check().unwrap();
        conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        observe(&mut agent);
    }
    let endpoint = agent.saved.x11.clone().unwrap();
    agent.saved.sway.terminate();
    std::thread::sleep(Duration::from_millis(300));
    assert!(endpoint.connect().is_err());
}

#[test]
#[ignore = "需 tests/run-x11.sh 的私有 X11 输入探针"]
fn x11_pointer_drag_scroll_and_key_release() {
    environment();
    let mut b = Isolated::launch(Application::Installed(DesktopApplication {
        id: "input-probe.desktop".into(),
        name: "X11 input probe".into(),
        program: gtk4::glib::find_program_in_path("env").unwrap(),
        args: vec![
            "GDK_BACKEND=x11".into(),
            std::env::var("COMPUTER_USE_INPUT_PROBE").unwrap(),
        ],
        directory: None,
        desktop_file: PathBuf::from("/tmp/input-probe.desktop"),
    }))
    .unwrap();
    let _cleanup = Cleanup(b.saved.clone());
    assert!(b.saved.x11_application);
    let point = Point { x: 200.0, y: 250.0 };
    act(
        &mut b,
        Action::Click {
            at: point,
            button: Button::Left,
        },
    );
    act(
        &mut b,
        Action::Drag {
            from: point,
            to: Point { x: 400.0, y: 350.0 },
        },
    );
    act(
        &mut b,
        Action::Scroll {
            at: point,
            dx: -60.0,
            dy: 120.0,
        },
    );
    key(&mut b, "a", vec![Modifier::Ctrl]);
    key(&mut b, "b", vec![]);
    std::thread::sleep(Duration::from_millis(100));
    let events: Vec<serde_json::Value> =
        std::fs::read_to_string(b.saved.runtime.join("input.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "press" && e["x"] == 200.0 && e["y"] == 250.0)
    );
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "motion" && e["x"] == 400.0 && e["y"] == 350.0)
    );
    assert!(events.iter().any(|e| e["event"] == "release"));
    let scroll: Vec<_> = events.iter().filter(|e| e["event"] == "scroll").collect();
    assert_eq!(
        scroll
            .iter()
            .map(|e| e["dy"].as_f64().unwrap())
            .sum::<f64>(),
        8.0,
        "首次滚动必须完整送达，不能用重复滚动掩盖丢失"
    );
    assert_eq!(
        scroll
            .iter()
            .map(|e| e["dx"].as_f64().unwrap())
            .sum::<f64>(),
        -4.0
    );
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "key_press" && e["key"] == "a")
    );
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "key_press" && e["key"] == "b" && e["modifiers"] == 0)
    );
}
