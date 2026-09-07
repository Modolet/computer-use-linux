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
use gstreamer_allocators::{DmaBufAllocator, DmaBufAllocatorExtManual, DmaBufMemory};
use gstreamer_video::VideoMeta;
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
            .property("keepalive-time", 250i32)
            .property("use-bufferpool", true)
            .build()
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        // Request CPU-readable linear 32-bit buffers. Tiled DRM modifiers are
        // deliberately excluded: interpreting their bytes as rows corrupts images.
        // Mapping through GstDmaBufAllocator performs DMA_BUF_IOCTL_SYNC for us.
        let caps = gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("format", "DMA_DRM")
            .field(
                "drm-format",
                gst::List::new(["XR24", "AR24", "XB24", "AB24"]),
            )
            .build();
        let sink = gstreamer_app::AppSink::builder()
            .caps(&caps)
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .build();
        pipeline
            .add_many([&source, sink.upcast_ref()])
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        source
            .link(&sink)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        if let Err(error) = pipeline.set_state(gst::State::Playing) {
            let detail = pipeline
                .bus()
                .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
                .map(|message| match message.view() {
                    gst::MessageView::Error(e) => format!("{}: {:?}", e.error(), e.debug()),
                    _ => error.to_string(),
                })
                .unwrap_or_else(|| error.to_string());
            let _ = pipeline.set_state(gst::State::Null);
            return Err(Fault::unavailable(format!(
                "PipeWire 图像流启动失败：{detail}"
            )));
        }
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
    pub(crate) fn validate_binding(&self) -> Result<()> {
        let (path, stream_id, window_id) = self
            .bound
            .as_ref()
            .ok_or_else(|| Fault::denied("截图流尚未绑定获准窗口"))?;
        let niri_ipc::Response::Casts(casts) =
            super::desktop::niri_request(path, niri_ipc::Request::Casts)?
        else {
            return Err(Fault::stale("窗口流已失效"));
        };
        let current = casts.iter().find(|c| {
            c.stream_id == *stream_id
                && c.pw_node_id == Some(self.node_id)
                && !c.is_dynamic_target
                && c.target == (niri_ipc::CastTarget::Window { id: *window_id })
        });
        if current.is_none() {
            return Err(Fault::stale("窗口采集目标发生变化；必须重新授权"));
        }
        if !current.unwrap().is_active {
            return Err(Fault::new(ErrorCode::Paused, "窗口共享已暂停或尚未就绪"));
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
        // Keepalive repeats static frames; lifecycle and active-cast checks before
        // and after capture prevent a stopped/rebound stream from revealing cached pixels.
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
                let drm = format
                    .get::<&str>("drm-format")
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let blue_first = match drm {
                    "XR24" | "AR24" => true,
                    "XB24" | "AB24" => false,
                    _ => return Err(Fault::unsupported("窗口流不是受支持的线性 32 位图像")),
                };
                let meta = buffer
                    .meta::<VideoMeta>()
                    .ok_or_else(|| Fault::unavailable("窗口图像缺少布局元数据"))?;
                if meta.width() != width as u32
                    || meta.height() != height as u32
                    || meta.stride().len() != 1
                    || meta.offset().len() != 1
                {
                    return Err(Fault::unavailable("窗口图像布局不一致"));
                }
                if buffer.n_memory() != 1 {
                    return Err(Fault::unsupported("只支持单平面线性 DMA-BUF"));
                }
                let memory = buffer
                    .peek_memory(0)
                    .downcast_memory_ref::<DmaBufMemory>()
                    .ok_or_else(|| Fault::unavailable("窗口流丢失 DMA-BUF 文件描述符"))?;
                let offset = memory
                    .offset()
                    .checked_add(meta.offset()[0])
                    .ok_or_else(|| Fault::unavailable("窗口图像偏移溢出"))?;
                let required = linear_end(width as u32, height as u32, meta.stride()[0], offset)?;
                // niri correctly sets spa_data.maxsize / chunk.size to 1: those
                // fields are not a DMA-BUF's byte length (PipeWire DMA-BUF docs).
                // Validate against the kernel's allocation size, then map our own
                // duplicated fd. Keep the sample alive so the producer cannot reuse it.
                // SAFETY: GstDmaBufMemory owns this live fd for the sample's lifetime.
                let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(memory.fd()) }
                    .try_clone_to_owned()
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let allocated = nix::unistd::lseek(&fd, 0, nix::unistd::Whence::SeekEnd)
                    .map_err(|e| Fault::unavailable(format!("无法核实 DMA-BUF 大小：{e}")))?;
                if allocated < 0 || required as u64 > allocated as u64 {
                    return Err(Fault::unavailable("DMA-BUF 分配大小小于窗口图像布局"));
                }
                // SAFETY: the duplicated descriptor is a real DMA-BUF; required
                // is bounded and checked against the allocation reported by the kernel.
                let owned = unsafe { DmaBufAllocator::new().alloc_dmabuf(fd, required) }
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let mapped = owned
                    .map_readable()
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let pixels = linear_rgba(
                    mapped.as_slice(),
                    width as u32,
                    height as u32,
                    meta.stride()[0],
                    offset,
                    blue_first,
                )?;
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

// PipeWire buffers may contain padding and an offset; validate the complete last
// row before indexing. Output is opaque because the portal composites a window.
fn linear_rgba(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: i32,
    offset: usize,
    blue_first: bool,
) -> Result<Vec<u8>> {
    let row = width as usize * 4;
    let end = linear_end(width, height, stride, offset)?;
    if end > bytes.len() {
        return Err(Fault::unavailable("窗口图像缓冲区不完整"));
    }
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = offset + y * stride as usize;
        for pixel in bytes[start..start + row].chunks_exact(4) {
            let (r, b) = if blue_first {
                (pixel[2], pixel[0])
            } else {
                (pixel[0], pixel[2])
            };
            pixels.extend_from_slice(&[r, pixel[1], b, 255]);
        }
    }
    Ok(pixels)
}

fn linear_end(width: u32, height: u32, stride: i32, offset: usize) -> Result<usize> {
    let row = width as usize * 4;
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 64 * 1024 * 1024
        || stride < 0
        || (stride as usize) < row
    {
        return Err(Fault::unavailable("窗口图像步长或大小无效"));
    }
    let end = (stride as usize)
        .checked_mul(height as usize - 1)
        .and_then(|n| n.checked_add(offset))
        .and_then(|n| n.checked_add(row))
        .ok_or_else(|| Fault::unavailable("窗口图像布局溢出"))?;
    if end > 512 * 1024 * 1024 {
        return Err(Fault::unavailable("窗口图像内存布局过大"));
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linear_buffers_respect_stride_offset_and_format() {
        let bytes = [99, 3, 2, 1, 0, 88, 6, 5, 4, 0];
        assert_eq!(
            linear_rgba(&bytes, 1, 2, 5, 1, true).unwrap(),
            [1, 2, 3, 255, 4, 5, 6, 255]
        );
        assert_eq!(
            linear_rgba(&bytes, 1, 1, 4, 1, false).unwrap(),
            [3, 2, 1, 255]
        );
        assert!(linear_rgba(&bytes[..9], 1, 2, 5, 1, true).is_err());
        assert!(linear_rgba(&bytes, 1, 1, -4, 0, true).is_err());
        assert!(linear_rgba(&bytes, 1, 2, 4, usize::MAX, true).is_err());
    }
}
