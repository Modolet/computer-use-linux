//! @file multi_output.rs
//! @brief 真实双输出 Wayland 验收，niri IPC 元数据由测试夹具提供
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use computer_use_linux::{
    backend::{
        Backend, Cancellation, desktop::Desktop, process::ProcessIdentity, sway::sway_request,
    },
    model::*,
};
use std::{
    io::{BufRead, Write},
    os::unix::net::UnixListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

struct Probe(std::process::Child);
impl Drop for Probe {
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
#[ignore = "通过 tests/run-multi-output.sh 使用两个私有输出"]
fn target_routes_only_to_selected_output_and_replug_invalidates_references() {
    assert_eq!(std::env::var("COMPUTER_USE_MULTI_TEST").as_deref(), Ok("1"));
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
    assert!(runtime.starts_with("/tmp/cm-"));
    let sway = std::path::PathBuf::from(std::env::var_os("SWAYSOCK").unwrap());
    let niri = std::path::PathBuf::from(std::env::var_os("NIRI_SOCKET").unwrap());
    assert!(sway.starts_with(&runtime) && niri.starts_with(&runtime));
    let mut probes = vec![];
    let logs: Vec<_> = (1..=2)
        .map(|i| std::path::PathBuf::from(&runtime).join(format!("probe-{i}.jsonl")))
        .collect();
    for (i, log) in logs.iter().enumerate() {
        sway_request(
            &sway,
            0,
            &format!(
                "workspace {}; move workspace to output HEADLESS-{}",
                i + 1,
                i + 1
            ),
        )
        .unwrap();
        probes.push(Probe(
            Command::new("target/debug/examples/input-probe")
                .arg(log)
                .arg((i + 1).to_string())
                .spawn()
                .unwrap(),
        ));
        std::thread::sleep(Duration::from_millis(700));
    }
    let listener = UnixListener::bind(&niri).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let ipc = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            let mut line = String::new();
            if std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap()
                == 0
            {
                continue;
            }
            let request: niri_ipc::Request = serde_json::from_str(&line).unwrap();
            let response = match request {
                niri_ipc::Request::Outputs => niri_ipc::Response::Outputs(
                    (1..=2)
                        .map(|i| {
                            let (width, height, scale, x) = if i == 1 {
                                (1280, 800, 1.0, 0)
                            } else {
                                (1000, 900, 1.25, 1280)
                            };
                            let name = format!("HEADLESS-{i}");
                            (
                                name.clone(),
                                niri_ipc::Output {
                                    name,
                                    make: "fixture".into(),
                                    model: "virtual".into(),
                                    serial: None,
                                    physical_size: None,
                                    modes: vec![niri_ipc::Mode {
                                        width,
                                        height,
                                        refresh_rate: 60000,
                                        is_preferred: true,
                                    }],
                                    current_mode: Some(0),
                                    is_custom_mode: false,
                                    vrr_supported: false,
                                    vrr_enabled: false,
                                    logical: Some(niri_ipc::LogicalOutput {
                                        x,
                                        y: 0,
                                        width: (f64::from(width) / scale) as u32,
                                        height: (f64::from(height) / scale) as u32,
                                        scale,
                                        transform: niri_ipc::Transform::Normal,
                                    }),
                                },
                            )
                        })
                        .collect(),
                ),
                niri_ipc::Request::Windows => niri_ipc::Response::Windows(vec![]),
                _ => panic!("测试后端不接受其他命令"),
            };
            let reply: niri_ipc::Reply = Ok(response);
            serde_json::to_writer(&mut stream, &reply).unwrap();
            stream.write_all(b"\n").unwrap();
        }
    });
    let mut desktop = Desktop::connect().unwrap();
    let mut targets = desktop.targets().unwrap();
    targets.sort_by(|a, b| a.label.cmp(&b.label));
    assert_eq!(targets.len(), 2);
    for (index, target) in targets.iter().enumerate().rev() {
        let o = desktop.observe(Some(&target.id), &cancel()).unwrap();
        assert_eq!(
            (o.target.width, o.target.height),
            if index == 0 { (1280, 800) } else { (1000, 900) }
        );
        use base64::Engine;
        let png = base64::engine::general_purpose::STANDARD
            .decode(o.png_base64.as_ref().unwrap())
            .unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgb8();
        let color = image.get_pixel(400, 300).0;
        let expected = if index == 0 {
            [26u8, 64, 102]
        } else {
            [102u8, 64, 26]
        };
        assert!(
            color
                .into_iter()
                .zip(expected)
                .all(|(a, b)| a.abs_diff(b) <= 1),
            "{color:?}"
        );
        desktop
            .act(
                &o,
                &Action::Click {
                    at: Point { x: 400.0, y: 300.0 },
                    button: Button::Left,
                },
                &cancel(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let contents = std::fs::read_to_string(&logs[index]).unwrap();
        let events: Vec<serde_json::Value> = contents
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let click = events.iter().rev().find(|e| e["event"] == "press").unwrap();
        assert!((click["x"].as_f64().unwrap() - 400.0 / target.scale).abs() < 1.0);
        assert!((click["y"].as_f64().unwrap() - 300.0 / target.scale).abs() < 1.0);
    }
    for log in &logs {
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .filter(|s| s.contains("\"event\":\"press\""))
                .count(),
            1
        );
    }
    let old = desktop.observe(Some(&targets[1].id), &cancel()).unwrap();
    sway_request(&sway, 0, "output HEADLESS-2 disable").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        desktop
            .act(
                &old,
                &Action::Click {
                    at: Point { x: 400.0, y: 300.0 },
                    button: Button::Left
                },
                &cancel()
            )
            .is_err()
    );
    assert_eq!(desktop.targets().unwrap().len(), 1);
    sway_request(&sway, 0, "output HEADLESS-2 enable").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let new = desktop
        .targets()
        .unwrap()
        .into_iter()
        .find(|o| o.label == "HEADLESS-2")
        .unwrap();
    assert_ne!(new.id, targets[1].id);
    assert!(desktop.observe(Some(&targets[1].id), &cancel()).is_err());
    stopped.store(true, Ordering::SeqCst);
    ipc.join().unwrap();
}
