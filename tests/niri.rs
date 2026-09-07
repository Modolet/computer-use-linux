//! @file niri.rs
//! @brief 真实嵌套 niri 与 AT-SPI 后端验收
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use computer_use_linux::{
    backend::{
        Backend, Cancellation, accessibility,
        desktop::{Desktop, niri_request},
        existing::Existing,
        process::ProcessIdentity,
    },
    model::*,
};
use std::{
    process::{Child, Command},
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(p) = ProcessIdentity::read(self.0.id()) {
            p.terminate();
        }
        let _ = self.0.wait();
    }
}
fn cancel() -> Cancellation {
    Cancellation::new(Arc::new(AtomicU64::new(0)))
}
#[test]
#[ignore = "通过 tests/run-niri.sh 在专用桌面运行"]
fn desktop_and_background_application() {
    assert_eq!(std::env::var("COMPUTER_USE_NIRI_TEST").as_deref(), Ok("1"));
    assert!(
        std::env::var("XDG_RUNTIME_DIR")
            .unwrap()
            .starts_with("/tmp/cn-")
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, "后台文本测试").unwrap();
    let _app = ChildGuard(
        Command::new("gnome-text-editor")
            .arg("--standalone")
            .arg(file)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(2));
    let mut desktop = Desktop::connect().unwrap();
    let o = desktop.observe(None, &cancel()).unwrap();
    assert!(o.png_base64.is_some());
    println!("desktop: {:?}", desktop.capabilities());
    let user_file = dir.path().join("user.txt");
    std::fs::write(&user_file, "用户前台窗口").unwrap();
    let _user = ChildGuard(
        Command::new("gnome-text-editor")
            .arg("--standalone")
            .arg(user_file)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(1));
    let list = accessibility::candidates().unwrap();
    let candidate = list
        .iter()
        .find(|c| c.label.contains("target.txt"))
        .unwrap()
        .clone();
    let user = list
        .iter()
        .find(|c| c.label.contains("user.txt"))
        .unwrap()
        .clone();
    let mut background = Existing::bind(candidate.clone(), None).unwrap();
    assert!(background.capabilities().contains(&"set_text".into()));
    let o = background.observe(None, &cancel()).unwrap();
    let node = find_text(&o.nodes, "后台文本测试").unwrap();
    assert!(node.actions.contains(&"set_text".into()));
    let node_id = node.id.clone();
    for primary in [false, true] {
        let mut copy = Command::new("wl-copy");
        if primary {
            copy.arg("--primary");
        }
        assert!(copy.arg("用户剪贴板哨兵").status().unwrap().success());
    }
    let niri = std::path::PathBuf::from(std::env::var_os("NIRI_SOCKET").unwrap());
    let before = focused(&niri);
    assert_eq!(before, user.window_id);
    background
        .act(
            &o,
            &Action::SetText {
                node: node_id,
                text: "后台编辑成功：你好，世界。".into(),
            },
            &cancel(),
        )
        .unwrap();
    assert_eq!(focused(&niri), before);
    for primary in [false, true] {
        let mut paste = Command::new("wl-paste");
        if primary {
            paste.arg("--primary");
        }
        let output = paste.arg("--no-newline").output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "用户剪贴板哨兵");
    }
    let changed = background.observe(None, &cancel()).unwrap();
    assert!(find_text(&changed.nodes, "后台编辑成功：你好，世界。").is_some());
    let node = find_text(&changed.nodes, "后台编辑成功：你好，世界。")
        .unwrap()
        .id
        .clone();
    niri_request(
        &niri,
        niri_ipc::Request::Action(niri_ipc::Action::SetWindowWidth {
            id: Some(candidate.window_id),
            change: niri_ipc::SizeChange::SetFixed(700),
        }),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        background
            .act(
                &changed,
                &Action::SetText {
                    node,
                    text: "旧布局不能继续输入".into()
                },
                &cancel()
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );

    desktop_action(
        &mut desktop,
        Action::Text {
            text: "前台输入ABC你好".into(),
        },
    );
    let mut user_access = accessibility::Accessibility::bind(user).unwrap();
    let user_nodes = user_access.read(&cancel()).unwrap();
    assert!(
        has_text(&user_nodes, "前台输入ABC你好"),
        "前台文本输入失败：{user_nodes:?}"
    );
    let target_window = desktop
        .observe(None, &cancel())
        .unwrap()
        .windows
        .iter()
        .find(|w| w.label.contains("target.txt"))
        .unwrap()
        .id
        .clone();
    desktop_action(
        &mut desktop,
        Action::FocusWindow {
            window: target_window,
        },
    );
    let o = background.observe(None, &cancel()).unwrap();
    let node = find_text(&o.nodes, "后台编辑成功：你好，世界。")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        background
            .act(
                &o,
                &Action::SetText {
                    node,
                    text: "不能写入".into()
                },
                &cancel()
            )
            .unwrap_err()
            .code,
        ErrorCode::Paused
    );
    desktop_action(
        &mut desktop,
        Action::Key {
            key: "a".into(),
            modifiers: vec![Modifier::Ctrl],
        },
    );
    niri_request(
        &niri,
        niri_ipc::Request::Action(niri_ipc::Action::FocusWindow {
            id: user_access.candidate.window_id,
        }),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let o = background.observe(None, &cancel()).unwrap();
    let node = find_text(&o.nodes, "后台编辑成功：你好，世界。")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        background
            .act(
                &o,
                &Action::SetText {
                    node,
                    text: "不能覆盖用户选区".into()
                },
                &cancel()
            )
            .unwrap_err()
            .code,
        ErrorCode::Unsupported
    );
    assert_eq!(focused(&niri), user_access.candidate.window_id);
    let mut unverified = candidate.clone();
    unverified.version = "unverified".into();
    assert!(
        !Existing::bind(unverified, None)
            .unwrap()
            .capabilities()
            .contains(&"set_text".into())
    );
    candidate.process.terminate();
    std::thread::sleep(Duration::from_millis(200));
    let _replacement = ChildGuard(
        Command::new("gnome-text-editor")
            .arg("--standalone")
            .arg(dir.path().join("target.txt"))
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        background.observe(None, &cancel()).unwrap_err().code,
        ErrorCode::StaleTarget
    );
}
fn focused(path: &std::path::Path) -> u64 {
    let niri_ipc::Response::Windows(windows) =
        niri_request(path, niri_ipc::Request::Windows).unwrap()
    else {
        panic!()
    };
    windows.into_iter().find(|w| w.is_focused).unwrap().id
}
fn find_text<'a>(nodes: &'a [Node], text: &str) -> Option<&'a Node> {
    nodes.iter().find_map(|n| {
        if n.text.as_deref() == Some(text) {
            Some(n)
        } else {
            find_text(&n.children, text)
        }
    })
}
fn has_text(nodes: &[Node], text: &str) -> bool {
    nodes
        .iter()
        .any(|n| n.text.as_ref().is_some_and(|t| t.contains(text)) || has_text(&n.children, text))
}
fn desktop_action(desktop: &mut Desktop, action: Action) {
    let o = desktop.observe(None, &cancel()).unwrap();
    desktop.act(&o, &action, &cancel()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
#[ignore = "通过 tests/run-niri.sh 在专用桌面运行"]
fn pointer_events_reach_application() {
    assert_eq!(std::env::var("COMPUTER_USE_NIRI_TEST").as_deref(), Ok("1"));
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
    assert!(runtime.starts_with("/tmp/cn-"));
    let file = std::path::PathBuf::from(&runtime).join("input.jsonl");
    let _probe = ChildGuard(
        Command::new("target/debug/examples/input-probe")
            .arg(&file)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(1));
    let mut desktop = Desktop::connect().unwrap();
    let niri = std::path::PathBuf::from(std::env::var_os("NIRI_SOCKET").unwrap());
    let output_name = desktop.targets().unwrap()[0].label.clone();
    // Winit uses a vertically flipped buffer; real Sway rotation is tested separately.
    {
        let transform = niri_ipc::Transform::Flipped180;
        let old = desktop.observe(None, &cancel()).unwrap();
        let o = desktop.observe(None, &cancel()).unwrap();
        let point = Point {
            x: f64::from(o.target.width) * 0.25,
            y: f64::from(o.target.height) * 0.35,
        };
        desktop
            .act(
                &o,
                &Action::Click {
                    at: point,
                    button: Button::Left,
                },
                &cancel(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let events = input_events(&file);
        let event = events
            .iter()
            .rev()
            .find(|v| v["event"] == "press")
            .expect("应用必须收到鼠标按下");
        assert!(
            (event["x"].as_f64().unwrap() - point.x / o.target.scale).abs() < 1.0,
            "{transform:?}: {event}, {:?}",
            o.target
        );
        assert!(
            (event["y"].as_f64().unwrap() - point.y / o.target.scale).abs() < 1.0,
            "{transform:?}: {event}, {:?}",
            o.target
        );
        assert!(
            desktop
                .act(
                    &old,
                    &Action::Text {
                        text: "stale".into()
                    },
                    &cancel()
                )
                .is_err()
        );
    }
    niri_request(
        &niri,
        niri_ipc::Request::Output {
            output: output_name.clone(),
            action: niri_ipc::OutputAction::Scale {
                scale: niri_ipc::ScaleToSet::Specific(1.25),
            },
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let scaled = desktop.observe(None, &cancel()).unwrap();
    assert!((scaled.target.scale - 1.25).abs() < 0.01);
    let point = Point { x: 300.0, y: 300.0 };
    desktop_action(
        &mut desktop,
        Action::Click {
            at: point,
            button: Button::Left,
        },
    );
    let events = input_events(&file);
    let click = events.iter().rev().find(|v| v["event"] == "press").unwrap();
    assert!(
        (click["x"].as_f64().unwrap() - 240.0).abs() < 1.0,
        "{click}"
    );
    assert!(
        (click["y"].as_f64().unwrap() - 240.0).abs() < 1.0,
        "{click}"
    );
    desktop_action(
        &mut desktop,
        Action::Scroll {
            at: point,
            dx: 15.0,
            dy: 30.0,
        },
    );
    assert!(
        input_events(&file)
            .iter()
            .any(|v| v["event"] == "scroll" && v["dy"].as_f64().unwrap() > 0.0)
    );
    desktop_action(
        &mut desktop,
        Action::Drag {
            from: point,
            to: Point { x: 450.0, y: 400.0 },
        },
    );
    let events = input_events(&file);
    let motion = events
        .iter()
        .rev()
        .find(|v| v["event"] == "motion")
        .unwrap();
    assert!(
        (motion["x"].as_f64().unwrap() - 360.0).abs() < 1.0,
        "{motion}"
    );
    let generation = Arc::new(AtomicU64::new(0));
    let cancellation = Cancellation::new(generation.clone());
    let o = desktop.observe(None, &cancellation).unwrap();
    let stop = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    assert!(
        desktop
            .act(
                &o,
                &Action::Drag {
                    from: point,
                    to: Point { x: 450.0, y: 400.0 }
                },
                &cancellation
            )
            .is_err()
    );
    stop.join().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    desktop_action(
        &mut desktop,
        Action::Key {
            key: "a".into(),
            modifiers: vec![],
        },
    );
    let events = input_events(&file);
    let key = events
        .iter()
        .rev()
        .find(|v| v["event"] == "key_press")
        .unwrap();
    assert_eq!(key["key"], "a");
    assert_eq!(key["modifiers"], 0);
    niri_request(
        &niri,
        niri_ipc::Request::Output {
            output: output_name,
            action: niri_ipc::OutputAction::Scale {
                scale: niri_ipc::ScaleToSet::Specific(1.0),
            },
        },
    )
    .unwrap();
}

fn input_events(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

#[test]
#[ignore = "在私有 niri 中持续模拟用户输入，同时编辑后台窗口"]
fn background_writes_do_not_disturb_continuous_user_input() {
    assert_eq!(std::env::var("COMPUTER_USE_NIRI_TEST").as_deref(), Ok("1"));
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
    assert!(runtime.starts_with("/tmp/cn-"));
    let file = std::path::PathBuf::from(&runtime).join("concurrent-target.txt");
    std::fs::write(&file, "背景 0").unwrap();
    let _app = ChildGuard(
        Command::new("gnome-text-editor")
            .arg("--standalone")
            .arg(&file)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(1));
    let log = std::path::PathBuf::from(&runtime).join("concurrent-input.jsonl");
    let _probe = ChildGuard(
        Command::new("target/debug/examples/input-probe")
            .arg(&log)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(1));
    let candidate = accessibility::candidates()
        .unwrap()
        .into_iter()
        .find(|c| c.label.contains("concurrent-target.txt"))
        .unwrap();
    let mut background = Existing::bind(candidate, None).unwrap();
    let niri = std::path::PathBuf::from(std::env::var_os("NIRI_SOCKET").unwrap());
    let expected_focus = focused(&niri);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = barrier.clone();
    let worker = std::thread::spawn(move || {
        other.wait();
        for i in 0..8 {
            let o = background.observe(None, &cancel()).unwrap();
            let node = find_text(&o.nodes, &format!("背景 {i}"))
                .unwrap()
                .id
                .clone();
            background
                .act(
                    &o,
                    &Action::SetText {
                        node,
                        text: format!("背景 {}", i + 1),
                    },
                    &cancel(),
                )
                .unwrap();
        }
        assert!(
            find_text(
                &background.observe(None, &cancel()).unwrap().nodes,
                "背景 8"
            )
            .is_some()
        );
    });
    let mut desktop = Desktop::connect().unwrap();
    barrier.wait();
    for i in 0..8 {
        desktop_action(
            &mut desktop,
            Action::Click {
                at: Point {
                    x: 250.0 + f64::from(i),
                    y: 300.0,
                },
                button: Button::Left,
            },
        );
        desktop_action(
            &mut desktop,
            Action::Text {
                text: format!("用户{i}ABC"),
            },
        );
        for primary in [false, true] {
            let mut copy = Command::new("wl-copy");
            if primary {
                copy.arg("--primary");
            }
            assert!(
                copy.arg(format!("用户复制 {i}"))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert_eq!(focused(&niri), expected_focus);
    }
    worker.join().unwrap();
    assert_eq!(focused(&niri), expected_focus);
    for primary in [false, true] {
        let mut paste = Command::new("wl-paste");
        if primary {
            paste.arg("--primary");
        }
        let output = paste.arg("--no-newline").output().unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "用户复制 7");
    }
    let events = input_events(&log);
    assert_eq!(events.iter().filter(|e| e["event"] == "press").count(), 8);
    assert_eq!(
        events.iter().filter(|e| e["event"] == "key_press").count(),
        48
    );
    let last = events
        .iter()
        .rev()
        .find(|e| e["event"] == "motion")
        .unwrap();
    assert!((last["x"].as_f64().unwrap() - 257.0).abs() < 1.0);
    assert!((last["y"].as_f64().unwrap() - 300.0).abs() < 1.0);
}
