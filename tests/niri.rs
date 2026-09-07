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
