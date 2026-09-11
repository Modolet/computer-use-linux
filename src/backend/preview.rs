//! @file preview.rs
//! @brief 独立应用的有界实时帧管线，隐藏和关闭时取消采集
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11

use super::{Cancellation, frame::Image};
use crate::model::*;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}
impl Viewport {
    pub fn new(width: u32, height: u32, scale: f64) -> Result<Self> {
        if width < 64
            || height < 64
            || width > 8192
            || height > 8192
            || u64::from(width) * u64::from(height) > 16_777_216
            || !scale.is_finite()
            || !(0.5..=4.0).contains(&scale)
        {
            return Err(Fault::unsupported(
                "预览尺寸或缩放超出范围（最多 1600 万像素）",
            ));
        }
        Ok(Self {
            width,
            height,
            scale: (scale * 1000.0).round() / 1000.0,
        })
    }
}

pub struct Frame {
    pub image: Image,
    pub target: Target,
    pub renderer: String,
    pub ready_at: Instant,
}

pub trait Source: Send {
    fn dma_formats(&mut self, _formats: Vec<(u32, u64)>) {}
    fn resize(&mut self, viewport: Viewport, cancel: &Cancellation) -> Result<()>;
    fn frame(&mut self, cancel: &Cancellation) -> Result<Frame>;
}

#[derive(Default)]
struct State {
    viewport: Option<Viewport>,
    visible: bool,
    stopped: bool,
    disable_dma: bool,
    latest: Option<Result<Frame>>,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    generation: Arc<AtomicU64>,
}

/// One worker and one latest-frame slot per view. Slow GTK consumers drop old
/// frames instead of creating a growing queue or retaining every capture buffer.
pub struct Stream(Arc<Shared>);
impl Stream {
    pub fn start(mut source: Box<dyn Source>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            generation: Arc::new(AtomicU64::new(0)),
        });
        let worker = shared.clone();
        std::thread::spawn(move || {
            let interval = Duration::from_nanos(1_000_000_000 / 60);
            let mut next = Instant::now();
            loop {
                let mut state = worker.state.lock().unwrap();
                while !state.stopped
                    && (!state.visible || state.viewport.is_none() || Instant::now() < next)
                {
                    state = if state.visible && state.viewport.is_some() {
                        worker
                            .wake
                            .wait_timeout(state, next.saturating_duration_since(Instant::now()))
                            .unwrap()
                            .0
                    } else {
                        worker.wake.wait(state).unwrap()
                    };
                }
                if state.stopped {
                    break;
                }
                let viewport = state.viewport.unwrap();
                let cancel = Cancellation::new(worker.generation.clone());
                let disable_dma = std::mem::take(&mut state.disable_dma);
                drop(state);
                if disable_dma {
                    source.dma_formats(vec![]);
                }
                let started = Instant::now();
                let result = source
                    .resize(viewport, &cancel)
                    .and_then(|()| source.frame(&cancel));
                let mut state = worker.state.lock().unwrap();
                if state.stopped {
                    break;
                }
                if cancel.check().is_err() {
                    next = Instant::now();
                    continue;
                }
                let busy = matches!(&result, Err(e) if e.code == ErrorCode::Busy);
                let failed = result.is_err();
                if !matches!(&result, Err(e) if e.code == ErrorCode::Busy) {
                    state.latest = Some(result);
                }
                next = if busy {
                    Instant::now() + Duration::from_millis(4)
                } else if failed {
                    Instant::now() + Duration::from_millis(100)
                } else {
                    started + interval
                };
            }
        });
        Self(shared)
    }
    pub fn configure(&self, viewport: Viewport) {
        let mut state = self.0.state.lock().unwrap();
        if state.viewport != Some(viewport) {
            state.viewport = Some(viewport);
            state.latest = None;
            self.0.generation.fetch_add(1, Ordering::SeqCst);
            self.0.wake.notify_one();
        }
    }
    pub fn visible(&self, visible: bool) {
        let mut state = self.0.state.lock().unwrap();
        if state.visible != visible {
            state.visible = visible;
            state.latest = None;
            self.0.generation.fetch_add(1, Ordering::SeqCst);
            self.0.wake.notify_one();
        }
    }
    pub fn take(&self) -> Option<Result<Frame>> {
        self.0.state.lock().unwrap().latest.take()
    }
    pub fn fallback_to_memory(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.disable_dma = true;
        state.latest = None;
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        self.0.wake.notify_one();
    }
    pub fn stop(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.stopped = true;
        state.latest = None;
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        self.0.wake.notify_one();
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewport_preserves_physical_pixels_and_fractional_scale() {
        assert_eq!(Viewport::new(2400, 1350, 1.25).unwrap().scale, 1.25);
        assert!(Viewport::new(0, 800, 1.0).is_err());
        assert!(Viewport::new(8192, 8192, 1.0).is_err());
        assert!(Viewport::new(1280, 800, f64::NAN).is_err());
    }

    #[test]
    fn hidden_resize_and_stop_cancel_inflight_capture() {
        use std::sync::mpsc;
        struct Blocking {
            events: mpsc::Sender<&'static str>,
        }
        impl Source for Blocking {
            fn resize(&mut self, _: Viewport, cancel: &Cancellation) -> Result<()> {
                cancel.check()
            }
            fn frame(&mut self, cancel: &Cancellation) -> Result<Frame> {
                self.events.send("capture").unwrap();
                loop {
                    if let Err(error) = cancel.check() {
                        self.events.send("cancelled").unwrap();
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        impl Drop for Blocking {
            fn drop(&mut self) {
                self.events.send("dropped").unwrap();
            }
        }
        let (events, received) = mpsc::channel();
        let stream = Stream::start(Box::new(Blocking { events }));
        let recv = || received.recv_timeout(Duration::from_secs(1)).unwrap();
        stream.configure(Viewport::new(1920, 1080, 1.0).unwrap());
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        stream.visible(true);
        assert_eq!(recv(), "capture");
        stream.configure(Viewport::new(2400, 1350, 1.25).unwrap());
        assert_eq!(recv(), "cancelled");
        assert_eq!(recv(), "capture");
        stream.visible(false);
        assert_eq!(recv(), "cancelled");
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        stream.visible(true);
        assert_eq!(recv(), "capture");
        stream.stop();
        assert_eq!(recv(), "cancelled");
        assert_eq!(recv(), "dropped");
        assert!(stream.take().is_none());
    }
}
