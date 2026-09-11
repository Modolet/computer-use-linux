//! @file preview.rs
//! @brief 实时采集吞吐、帧租约、静止帧与分辨率变化的真实 Wayland 验收
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11

use computer_use_linux::{
    backend::{
        Backend, Cancellation,
        applications::DesktopApplication,
        preview::{Source, Viewport},
        sway::{Application, Isolated, SavedSession},
    },
    model::*,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

struct Cleanup(SavedSession);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in [&self.0.app, &self.0.sway, &self.0.bus] {
            p.terminate();
        }
    }
}
fn cancel() -> Cancellation {
    Cancellation::new(Arc::new(AtomicU64::new(0)))
}
fn frame(source: &mut dyn Source) -> computer_use_linux::backend::preview::Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match source.frame(&cancel()) {
            Ok(frame) => return frame,
            Err(e) if e.code == ErrorCode::Busy && Instant::now() < deadline => {}
            Err(e) => panic!("{e:?}"),
        }
    }
}
fn raw(
    frame: &computer_use_linux::backend::preview::Frame,
) -> &computer_use_linux::backend::frame::RawFrame {
    let computer_use_linux::backend::frame::Image::Memory(raw) = &frame.image else {
        panic!("expected SHM without negotiated GTK formats")
    };
    raw
}
#[test]
#[ignore = "需要 tests/run-preview.sh 的私有图形与应用数据环境"]
fn realtime_pixels_resize_leases_and_idle_cancellation() {
    let runtime = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap());
    assert!(runtime.to_string_lossy().starts_with("/tmp/cp-"));
    assert_eq!(
        PathBuf::from(std::env::var("HOME").unwrap()),
        runtime.join("home")
    );
    let probe = PathBuf::from(std::env::var("COMPUTER_USE_PREVIEW_PROBE").unwrap());
    let mut app = Isolated::launch(Application::Installed(DesktopApplication {
        id: "preview-probe.desktop".into(),
        name: "Preview probe".into(),
        program: probe,
        args: vec![],
        directory: None,
        desktop_file: runtime.join("probe.desktop"),
    }))
    .unwrap();
    let _cleanup = Cleanup(app.saved.clone());
    println!("RENDERER {}", app.saved.renderer);
    if std::env::var("COMPUTER_USE_RENDERER").as_deref() == Ok("gles2") {
        assert!(app.saved.renderer.starts_with("GPU"));
    }
    let mut source = app.realtime_preview().unwrap();
    for (width, height, scale) in [(1920, 1080, 1.0), (2400, 1350, 1.25), (3840, 2160, 2.0)] {
        let old = app.observe(None, &cancel()).unwrap();
        source
            .resize(Viewport::new(width, height, scale).unwrap(), &cancel())
            .unwrap();
        assert!(
            app.act(
                &old,
                &Action::Key {
                    key: "a".into(),
                    modifiers: vec![]
                },
                &cancel()
            )
            .is_err()
        );
        // Keep one full frame alive while the pool cycles; it must be immutable.
        let held = frame(&mut *source);
        assert_eq!(
            (raw(&held).width, raw(&held).height, held.target.scale),
            (width, height, scale)
        );
        let snapshot = raw(&held).pixels.as_ref().to_vec();
        let rgba = raw(&held).rgba();
        assert_eq!(rgba.get_pixel(40, 40).0, [255, 0, 0, 255]);
        let mut count = 0;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            let image = frame(&mut *source);
            assert_eq!((raw(&image).width, raw(&image).height), (width, height));
            count += 1;
        }
        let fps = f64::from(count) / started.elapsed().as_secs_f64();
        println!("CAPTURE {width}x{height} scale={scale}: {fps:.1} FPS");
        let minimum = std::env::var("COMPUTER_USE_MIN_FPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(25.0);
        assert!(
            fps >= minimum,
            "实时采集未达到验收帧率: {fps:.1} < {minimum}"
        );
        assert_eq!(
            raw(&held).pixels.as_ref(),
            snapshot,
            "仍被显示的帧不能被采集线程覆盖"
        );
        app.human_act_at(
            &old.target,
            &Action::Click {
                at: Point { x: 30.0, y: 30.0 },
                button: Button::Left,
            },
            &cancel(),
        )
        .unwrap_err();
    }
    // All leases outstanding: reject instead of allocating an unbounded queue.
    let leases: Vec<_> = (0..4).map(|_| frame(&mut *source)).collect();
    assert!(matches!(source.frame(&cancel()), Err(e) if e.code == ErrorCode::Busy));
    drop(leases);
    drop(frame(&mut *source));
    app.human_act(
        &Action::Key {
            key: "space".into(),
            modifiers: vec![],
        },
        &cancel(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // Drain the last damage, then a static screen must sleep until cancelled.
    let generation = Arc::new(AtomicU64::new(0));
    let stopping = generation.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let cancel = Cancellation::new(generation);
        let mut count = 0;
        loop {
            match source.frame(&cancel) {
                Ok(_) => count += 1,
                Err(e) if e.code == ErrorCode::Paused => break,
                Err(e) if e.code == ErrorCode::Busy => {}
                Err(e) => panic!("{e:?}"),
            }
        }
        sent.send(count).unwrap();
    });
    assert!(received.recv_timeout(Duration::from_millis(400)).is_err());
    stopping.fetch_add(1, Ordering::SeqCst);
    let remaining = received.recv_timeout(Duration::from_millis(500)).unwrap();
    assert!(remaining <= 2, "静止画面不应重复抓帧: {remaining}");
    worker.join().unwrap();
}
