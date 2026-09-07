//! @file headless.rs
//! @brief 独立真实 Wayland 会话集成测试；不操作用户桌面
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use computer_use_linux::{
    backend::{
        Backend, Cancellation,
        sway::{Application, Isolated, sway_request},
    },
    model::*,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

struct Cleanup(computer_use_linux::backend::sway::SavedSession);
fn act(backend: &mut Isolated, action: Action) {
    let c = Cancellation::new(Arc::new(AtomicU64::new(0)));
    let mut attempts = 0;
    let o = loop {
        match backend.observe(None, &c) {
            Ok(o) => break o,
            Err(e) if e.code == ErrorCode::StaleTarget && attempts < 10 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("观察失败：{e}"),
        }
    };
    backend.act(&o, &action, &c).unwrap();
}
fn key(backend: &mut Isolated, key: &str) {
    act(
        backend,
        Action::Key {
            key: key.into(),
            modifiers: vec![Modifier::Ctrl],
        },
    );
}
fn clipboard(backend: &Isolated) -> String {
    std::thread::sleep(std::time::Duration::from_millis(100));
    let output = std::process::Command::new("wl-paste")
        .arg("--no-newline")
        .env("XDG_RUNTIME_DIR", &backend.saved.runtime)
        .env("WAYLAND_DISPLAY", &backend.saved.wayland)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[ignore = "启动两个独立真实应用会话，显式运行隔离集成测试"]
fn independent_sessions_preserve_input_and_clipboard() {
    assert_eq!(
        std::env::var("COMPUTER_USE_HEADLESS_TEST").as_deref(),
        Ok("1")
    );
    let mut user = Isolated::launch(Application::TextEditor).unwrap();
    let _user = Cleanup(user.saved.clone());
    let mut agent = Isolated::launch(Application::TextEditor).unwrap();
    let _agent = Cleanup(agent.saved.clone());
    assert_ne!(user.saved.wayland, agent.saved.wayland);
    key(&mut user, "n");
    act(
        &mut user,
        Action::Text {
            text: "用户的文字和剪贴板".into(),
        },
    );
    key(&mut user, "a");
    key(&mut user, "c");
    let expected = clipboard(&user);
    let user_tree = sway_request(&user.saved.socket, 4, "").unwrap();
    let task = std::thread::spawn(move || {
        key(&mut agent, "n");
        act(
            &mut agent,
            Action::Text {
                text: "Agent private input".into(),
            },
        );
        key(&mut agent, "a");
        key(&mut agent, "c");
        assert_eq!(clipboard(&agent), "Agent private input");
    });
    act(
        &mut user,
        Action::Key {
            key: "End".into(),
            modifiers: vec![],
        },
    );
    act(
        &mut user,
        Action::Text {
            text: "，继续输入".into(),
        },
    );
    task.join().unwrap();
    assert_eq!(clipboard(&user), expected, "AI 不应替换用户剪贴板");
    assert_eq!(
        user_tree["focus"],
        sway_request(&user.saved.socket, 4, "").unwrap()["focus"],
        "AI 不应改变用户焦点"
    );
    key(&mut user, "a");
    key(&mut user, "c");
    assert_eq!(clipboard(&user), "用户的文字和剪贴板，继续输入");
}

#[test]
#[ignore = "启动独立 Firefox，显式运行隔离集成测试"]
fn firefox_address_input_and_capture() {
    assert_eq!(
        std::env::var("COMPUTER_USE_HEADLESS_TEST").as_deref(),
        Ok("1")
    );
    let mut browser = Isolated::launch(Application::Firefox).unwrap();
    let _cleanup = Cleanup(browser.saved.clone());
    key(&mut browser, "l");
    let url = "data:text/html,<title>MCP test</title><h1>Visible application</h1><input placeholder='Type here'>";
    act(&mut browser, Action::Text { text: url.into() });
    key(&mut browser, "a");
    key(&mut browser, "c");
    assert_eq!(clipboard(&browser), url);
    act(
        &mut browser,
        Action::Key {
            key: "Enter".into(),
            modifiers: vec![],
        },
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    let c = Cancellation::new(Arc::new(AtomicU64::new(0)));
    let screenshot = browser.observe(None, &c).unwrap();
    assert!(screenshot.png_base64.is_some());
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for identity in [&self.0.app, &self.0.sway, &self.0.bus] {
            identity.terminate();
        }
    }
}

#[test]
#[ignore = "启动独立的 Sway 和测试编辑器；需 nix develop 及独立 XDG_RUNTIME_DIR/XDG_STATE_HOME"]
fn isolated_text_capture_resize_and_cancellation() {
    assert!(
        std::env::var("COMPUTER_USE_HEADLESS_TEST").is_ok_and(|v| v == "1"),
        "必须显式选择隔离集成测试环境"
    );
    let generation = Arc::new(AtomicU64::new(0));
    let cancel = Cancellation::new(generation.clone());
    let mut backend = Isolated::launch(Application::TextEditor).expect("启动测试编辑器");
    let _cleanup = Cleanup(backend.saved.clone());
    let observation = backend.observe(None, &cancel).expect("初始截图");
    assert_eq!(
        (observation.target.width, observation.target.height),
        (1280, 800)
    );
    backend
        .act(
            &observation,
            &Action::Key {
                key: "n".into(),
                modifiers: vec![Modifier::Ctrl],
            },
            &cancel,
        )
        .expect("新建文档快捷键");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let observation = backend.observe(None, &cancel).expect("新文档截图");
    backend
        .act(
            &observation,
            &Action::Text {
                text: "Hello from MCP!\n你好，猫娘。".into(),
            },
            &cancel,
        )
        .expect("中英文输入");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let after = backend.observe(None, &cancel).expect("输入后截图");
    assert_ne!(
        after.png_base64, observation.png_base64,
        "输入应改变真实图像"
    );
    if let Ok(path) = std::env::var("COMPUTER_USE_TEST_PNG") {
        use base64::Engine;
        std::fs::write(
            path,
            base64::engine::general_purpose::STANDARD
                .decode(after.png_base64.as_ref().unwrap())
                .unwrap(),
        )
        .unwrap();
    }
    for key in ["a", "c"] {
        let o = backend.observe(None, &cancel).unwrap();
        backend
            .act(
                &o,
                &Action::Key {
                    key: key.into(),
                    modifiers: vec![Modifier::Ctrl],
                },
                &cancel,
            )
            .unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    let copied = std::process::Command::new("wl-paste")
        .arg("--no-newline")
        .env("XDG_RUNTIME_DIR", &backend.saved.runtime)
        .env("WAYLAND_DISPLAY", &backend.saved.wayland)
        .output()
        .expect("读取测试会话自身剪贴板");
    assert!(
        copied.status.success(),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );
    assert_eq!(
        String::from_utf8(copied.stdout).unwrap(),
        "Hello from MCP!\n你好，猫娘。",
        "中英文和首字符必须完全一致"
    );
    sway_request(&backend.saved.socket, 0, "output HEADLESS-1 mode 1024x768").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        backend
            .act(
                &after,
                &Action::Click {
                    at: Point { x: 100.0, y: 100.0 },
                    button: Button::Left
                },
                &cancel
            )
            .is_err(),
        "缩放或布局变化必须拒绝旧观察"
    );
    generation.fetch_add(1, Ordering::SeqCst);
    assert!(
        backend.observe(None, &cancel).is_err(),
        "已撤销操作不可继续"
    );
    let fresh = Cancellation::new(generation.clone());
    let before_scale = backend.observe(None, &fresh).unwrap();
    sway_request(&backend.saved.socket, 0, "output HEADLESS-1 scale 1.25").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        backend
            .act(
                &before_scale,
                &Action::Click {
                    at: Point { x: 100.0, y: 100.0 },
                    button: Button::Left
                },
                &fresh
            )
            .is_err()
    );
    let scaled = backend.observe(None, &fresh).unwrap();
    assert_eq!(scaled.target.scale, 1.25);
    assert_eq!((scaled.target.width, scaled.target.height), (1024, 768));
}

#[test]
#[ignore = "独立 Firefox 中按真实像素验证旋转、缩放与鼠标输入"]
fn pointer_coordinates_follow_visible_pixels() {
    assert_eq!(
        std::env::var("COMPUTER_USE_HEADLESS_TEST").as_deref(),
        Ok("1")
    );
    let mut browser = Isolated::launch(Application::Firefox).unwrap();
    let _cleanup = Cleanup(browser.saved.clone());
    key(&mut browser, "l");
    act(
        &mut browser,
        Action::Text {
            text: format!(
                "file://{}/tests/fixtures/pointer.html",
                env!("CARGO_MANIFEST_DIR")
            ),
        },
    );
    act(
        &mut browser,
        Action::Key {
            key: "Enter".into(),
            modifiers: vec![],
        },
    );
    std::thread::sleep(std::time::Duration::from_secs(1));
    for transform in [
        "normal",
        "90",
        "180",
        "270",
        "flipped",
        "flipped-90",
        "flipped-180",
        "flipped-270",
    ] {
        sway_request(
            &browser.saved.socket,
            0,
            &format!("output HEADLESS-1 transform {transform}"),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        key(&mut browser, "r");
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            sway_request(&browser.saved.socket, 4, "")
                .unwrap()
                .to_string()
                .contains("pointer ready")
        );
        let c = Cancellation::new(Arc::new(AtomicU64::new(0)));
        let o = browser.observe(None, &c).unwrap();
        let at = red_center(&o);
        browser
            .act(
                &o,
                &Action::Click {
                    at,
                    button: Button::Left,
                },
                &c,
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let tree = sway_request(&browser.saved.socket, 4, "").unwrap();
        assert!(
            tree.to_string().contains("released 200 130"),
            "{transform}: {tree}"
        );
    }
    sway_request(
        &browser.saved.socket,
        0,
        "output HEADLESS-1 transform normal scale 1.25",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    let c = Cancellation::new(Arc::new(AtomicU64::new(0)));
    let o = browser.observe(None, &c).unwrap();
    let at = red_center(&o);
    browser
        .act(
            &o,
            &Action::Click {
                at,
                button: Button::Left,
            },
            &c,
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        sway_request(&browser.saved.socket, 4, "")
            .unwrap()
            .to_string()
            .contains("released 200 130")
    );
    act(
        &mut browser,
        Action::Scroll {
            at,
            dx: 0.0,
            dy: 150.0,
        },
    );
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert!(
        sway_request(&browser.saved.socket, 4, "")
            .unwrap()
            .to_string()
            .contains("scroll ")
    );
}
fn red_center(o: &Observation) -> Point {
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(o.png_base64.as_ref().unwrap())
        .unwrap();
    let image = image::load_from_memory(&png).unwrap().to_rgb8();
    let mut bounds = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0 == [255, 0, 0] {
            bounds.0 = bounds.0.min(x);
            bounds.1 = bounds.1.min(y);
            bounds.2 = bounds.2.max(x);
            bounds.3 = bounds.3.max(y);
        }
    }
    assert!(bounds.0 < bounds.2, "测试页面红色目标没有显示");
    Point {
        x: f64::from(bounds.0 + bounds.2 + 1) / 2.0,
        y: f64::from(bounds.1 + bounds.3 + 1) / 2.0,
    }
}

#[test]
#[ignore = "长输入期间出现其他进程窗口时必须停止"]
fn foreign_window_interrupts_inflight_isolated_input() {
    assert_eq!(
        std::env::var("COMPUTER_USE_HEADLESS_TEST").as_deref(),
        Ok("1")
    );
    let mut app = Isolated::launch(Application::TextEditor).unwrap();
    let _cleanup = Cleanup(app.saved.clone());
    key(&mut app, "n");
    let saved = app.saved.clone();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let c = Cancellation::new(Arc::new(AtomicU64::new(0)));
    let o = app.observe(None, &c).unwrap();
    let worker = std::thread::spawn(move || {
        let result = app.act(
            &o,
            &Action::Text {
                text: "x".repeat(1000),
            },
            &c,
        );
        (app, result)
    });
    std::thread::sleep(std::time::Duration::from_millis(150));
    let mut other = std::process::Command::new("gnome-text-editor")
        .arg("--standalone")
        .env("XDG_RUNTIME_DIR", &saved.runtime)
        .env("WAYLAND_DISPLAY", &saved.wayland)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/bus", saved.runtime.display()),
        )
        .env("GDK_BACKEND", "wayland")
        .env("GTK_A11Y", "none")
        .env("GSK_RENDERER", "cairo")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (mut app, result) = worker.join().unwrap();
    assert_eq!(result.unwrap_err().code, ErrorCode::PermissionDenied);
    assert_eq!(
        app.observe(None, &Cancellation::new(Arc::new(AtomicU64::new(0))))
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    let c = Cancellation::new(Arc::new(AtomicU64::new(0)));
    assert!(
        app.local_view().unwrap().unwrap().observe(None, &c).is_ok(),
        "本地用户仍可看见独立会话中的窗口"
    );
    app.human_act(
        &Action::Key {
            key: "n".into(),
            modifiers: vec![Modifier::Ctrl],
        },
        &c,
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    app.human_act(
        &Action::Text {
            text: "本地接管".into(),
        },
        &c,
    )
    .unwrap();
    assert_eq!(
        app.observe(None, &c).unwrap_err().code,
        ErrorCode::PermissionDenied,
        "本地接管不能提升 AI 权限"
    );
    computer_use_linux::backend::process::ProcessIdentity::read(other.id())
        .unwrap()
        .terminate();
    other.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        app.observe(None, &Cancellation::new(Arc::new(AtomicU64::new(0))))
            .is_ok()
    );
}
