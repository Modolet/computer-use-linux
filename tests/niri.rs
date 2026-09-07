//! @file niri.rs
//! @brief 真实嵌套 niri 整机后端验收
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use computer_use_linux::{
    backend::{
        Backend, Cancellation,
        desktop::{Desktop, niri_request},
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
