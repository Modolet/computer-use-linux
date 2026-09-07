//! @file portal.rs
//! @brief Portal 授权窗口流与 PipeWire 图像采集
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::Cancellation;
use crate::model::*;
use ashpd::desktop::{
    Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use base64::Engine;
use gstreamer::{self as gst, prelude::*};
use gstreamer_video::{VideoFrameExt, VideoFrameRef, VideoInfo};
use std::{
    os::fd::{AsRawFd, OwnedFd},
    time::{Duration, Instant},
};

pub struct Portal {
    _session: PortalSession,
    _fd: OwnedFd,
    pipeline: gst::Pipeline,
    sink: gstreamer_app::AppSink,
    pub source_id: Option<String>,
    node_id: u32,
    bound: Option<(std::path::PathBuf, u64, u64)>,
    watch: Option<std::sync::Mutex<super::cast_watch::CastWatch>>,
}

struct PortalSession(Option<Session<Screencast>>);
impl Drop for PortalSession {
    fn drop(&mut self) {
        let Some(session) = self.0.take() else {
            return;
        };
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                let _ = runtime.block_on(session.close());
            }
        });
    }
}

impl Portal {
    pub async fn select() -> Result<Self> {
        let proxy = Screencast::new()
            .await
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let session_guard = PortalSession(Some(session));
        let session = session_guard.0.as_ref().unwrap();
        proxy
            .select_sources(
                session,
                SelectSourcesOptions::default()
                    .set_sources(Some(SourceType::Window.into()))
                    .set_multiple(false)
                    .set_cursor_mode(CursorMode::Hidden),
            )
            .await
            .map_err(|e| Fault::unavailable(e.to_string()))?
            .response()
            .map_err(|e| Fault::denied(e.to_string()))?;
        let response = proxy
            .start(session, None, Default::default())
            .await
            .map_err(|e| Fault::unavailable(e.to_string()))?
            .response()
            .map_err(|e| Fault::denied(e.to_string()))?;
        let streams = response.streams();
        if streams.len() != 1 || streams[0].source_type() != Some(SourceType::Window) {
            return Err(Fault::denied("必须选择一个应用窗口，不能选择整块显示器"));
        }
        let stream = &streams[0];
        let fd = proxy
            .open_pipe_wire_remote(session, Default::default())
            .await
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        gst::init().map_err(|e| Fault::unavailable(e.to_string()))?;
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("pipewiresrc")
            .property("fd", fd.as_raw_fd())
            .property("path", stream.pipe_wire_node_id().to_string())
            .property("do-timestamp", true)
            .build()
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let sink = gstreamer_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .build(),
            )
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .build();
        pipeline
            .add_many([&source, &convert, sink.upcast_ref()])
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        gst::Element::link_many([&source, &convert, sink.upcast_ref()])
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        Ok(Self {
            _session: session_guard,
            _fd: fd,
            pipeline,
            sink,
            source_id: stream.id().map(String::from),
            node_id: stream.pipe_wire_node_id(),
            bound: None,
            watch: None,
        })
    }
    pub fn bind_window(&mut self, window_id: u64) -> Result<()> {
        let path = std::path::PathBuf::from(
            std::env::var_os("NIRI_SOCKET")
                .ok_or_else(|| Fault::unavailable("缺少 NIRI_SOCKET"))?,
        );
        let niri_ipc::Response::Casts(casts) =
            super::desktop::niri_request(&path, niri_ipc::Request::Casts)?
        else {
            return Err(Fault::unavailable("无法核实窗口流归属"));
        };
        let matching: Vec<_> = casts
            .iter()
            .filter(|cast| cast.pw_node_id == Some(self.node_id))
            .collect();
        if matching.len() != 1
            || matching[0].is_dynamic_target
            || matching[0].target != (niri_ipc::CastTarget::Window { id: window_id })
        {
            return Err(Fault::denied(
                "选择的截图窗口与授权应用不一致，或使用了动态共享目标",
            ));
        }
        self.watch = Some(std::sync::Mutex::new(
            super::cast_watch::CastWatch::connect(&path, matching[0].clone())?,
        ));
        self.bound = Some((path, matching[0].stream_id, window_id));
        Ok(())
    }
    fn validate_binding(&self) -> Result<()> {
        let (path, stream_id, window_id) = self
            .bound
            .as_ref()
            .ok_or_else(|| Fault::denied("截图流尚未绑定获准窗口"))?;
        let niri_ipc::Response::Casts(casts) =
            super::desktop::niri_request(path, niri_ipc::Request::Casts)?
        else {
            return Err(Fault::stale("窗口流已失效"));
        };
        if !casts.iter().any(|c| {
            c.stream_id == *stream_id
                && c.pw_node_id == Some(self.node_id)
                && !c.is_dynamic_target
                && c.target == (niri_ipc::CastTarget::Window { id: *window_id })
        }) {
            return Err(Fault::stale("窗口采集目标发生变化；必须重新授权"));
        }
        self.watch
            .as_ref()
            .ok_or_else(|| Fault::denied("窗口流没有生命周期监视"))?
            .lock()
            .map_err(|_| Fault::stale("窗口流监视已退出"))?
            .drain()?;
        Ok(())
    }
    pub fn capture(&self, cancel: &Cancellation) -> Result<(String, u32, u32)> {
        self.validate_binding()?;
        // Drain queued frames: never return a cached image after the stream stops.
        while self.sink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            cancel.check()?;
            if let Some(bus) = self.pipeline.bus()
                && let Some(message) =
                    bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
            {
                return Err(Fault::stale(format!(
                    "窗口采集流已结束：{:?}",
                    message.type_()
                )));
            }
            if let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::from_mseconds(30)) {
                let caps = sample
                    .caps()
                    .ok_or_else(|| Fault::unavailable("图像格式缺失"))?;
                let format = caps
                    .structure(0)
                    .ok_or_else(|| Fault::unavailable("图像格式无效"))?;
                let width = format
                    .get::<i32>("width")
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let height = format
                    .get::<i32>("height")
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                if width <= 0 || height <= 0 || width > 16384 || height > 16384 {
                    return Err(Fault::unavailable("窗口图像尺寸无效"));
                }
                let buffer = sample
                    .buffer()
                    .ok_or_else(|| Fault::unavailable("图像缓冲区缺失"))?;
                let info =
                    VideoInfo::from_caps(caps).map_err(|e| Fault::unavailable(e.to_string()))?;
                let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let stride = frame.plane_stride()[0];
                if stride < width * 4
                    || u64::from(width as u32) * u64::from(height as u32) > 64 * 1024 * 1024
                {
                    return Err(Fault::unavailable("窗口图像步长或大小无效"));
                }
                let plane = frame
                    .plane_data(0)
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
                for row in plane.chunks(stride as usize).take(height as usize) {
                    let rgba = row
                        .get(..width as usize * 4)
                        .ok_or_else(|| Fault::unavailable("窗口图像行不完整"))?;
                    pixels.extend_from_slice(rgba);
                }
                let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                    .ok_or_else(|| Fault::unavailable("窗口图像缓冲区长度无效"))?;
                let mut png = std::io::Cursor::new(Vec::new());
                image
                    .write_to(&mut png, image::ImageFormat::Png)
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                self.validate_binding()?;
                return Ok((
                    base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
                    width as u32,
                    height as u32,
                ));
            }
            if Instant::now() > deadline {
                return Err(Fault::unavailable("窗口图像采集超时"));
            }
        }
    }
}
impl Drop for Portal {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
