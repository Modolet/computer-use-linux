//! @file dmabuf.rs
//! @brief GBM 截图缓冲区租约与异步 Wayland DMA-BUF 导入
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11

use super::*;
use crate::backend::frame::{DmaImage, DmaPlane, Image};
use gbm::{BufferObject, BufferObjectFlags, Device};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1 as params;

pub(super) struct Pool {
    // Rust drops fields in declaration order: destroy GBM allocations while
    // their device's DRM fd is still open.
    buffers: Vec<Buffer>,
    device: Device<File>,
    formats: Vec<(u32, u64)>,
}
struct Buffer {
    _allocation: BufferObject<()>,
    image: Arc<DmaImage>,
    buffer: wl_buffer::WlBuffer,
    released: Arc<AtomicBool>,
}
impl Buffer {
    fn available(&self) -> bool {
        self.released.load(Ordering::Acquire) && Arc::strong_count(&self.image) == 1
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}
struct Import(params::ZwpLinuxBufferParamsV1);
impl Drop for Import {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

impl Wayland {
    pub fn configure_dma(&mut self, device: Option<&Path>, formats: Vec<(u32, u64)>) {
        self.dma_pool = None;
        let Some(device) = device.filter(|_| !formats.is_empty() && self.state.dma.is_some())
        else {
            return;
        };
        let result = File::options()
            .read(true)
            .write(true)
            .open(device)
            .and_then(Device::new);
        match result {
            Ok(device) => {
                self.dma_pool = Some(Pool {
                    device,
                    formats,
                    buffers: vec![],
                })
            }
            Err(error) => tracing::warn!(%error, "GBM 不可用，预览使用共享内存"),
        }
    }

    pub fn capture_preview(&mut self, output: &Output, cancel: &Cancellation) -> Result<Image> {
        if let Some(mut pool) = self.dma_pool.take() {
            let result = self.capture_dma(&mut pool, output, cancel);
            match result {
                Ok(frame) => {
                    self.dma_pool = Some(pool);
                    return Ok(Image::Dma(frame));
                }
                Err(error) if matches!(error.code, ErrorCode::Busy | ErrorCode::Paused) => {
                    self.dma_pool = Some(pool);
                    return Err(error);
                }
                Err(error) => tracing::warn!(%error, "DMA-BUF 采集不可用，预览回退到共享内存"),
            }
        }
        self.capture_raw(output, true, cancel).map(Image::Memory)
    }

    fn capture_dma(
        &mut self,
        pool: &mut Pool,
        output: &Output,
        cancel: &Cancellation,
    ) -> Result<Arc<DmaImage>> {
        if output.transform != wl_output::Transform::Normal {
            return Err(Fault::unsupported("旋转输出使用共享内存预览"));
        }
        let frame = self.begin_capture(output, true, cancel)?;
        let (fourcc, width, height) = self
            .state
            .dma_dimensions
            .ok_or_else(|| Fault::unsupported("输出不支持 DMA-BUF 截图"))?;
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || u64::from(width) * u64::from(height) > 16_777_216
        {
            return Err(Fault::unsupported("DMA-BUF 尺寸超出预览限制"));
        }
        pool.buffers
            .retain(|b| (b.image.fourcc, b.image.width, b.image.height) == (fourcc, width, height));
        let slot = match pool.buffers.iter().position(Buffer::available) {
            Some(slot) => slot,
            None if pool.buffers.len() < 4 => {
                let modifiers: Vec<_> = self
                    .state
                    .dma_formats
                    .iter()
                    .filter(|pair| {
                        pair.0 == fourcc
                            && pair.1 != u64::from(gbm::Modifier::Invalid)
                            && pool.formats.contains(pair)
                    })
                    .map(|pair| gbm::Modifier::from(pair.1))
                    .collect();
                if modifiers.is_empty() {
                    return Err(Fault::unsupported("GTK 与合成器没有共同 DMA-BUF 格式"));
                }
                let format = gbm::Format::try_from(fourcc)
                    .map_err(|_| Fault::unsupported("未知 DRM 像素格式"))?;
                let allocation = pool
                    .device
                    .create_buffer_object_with_modifiers2::<()>(
                        width,
                        height,
                        format,
                        modifiers.into_iter(),
                        BufferObjectFlags::RENDERING,
                    )
                    .map_err(|e| Fault::unavailable(format!("GBM 分配: {e}")))?;
                let modifier = u64::from(allocation.modifier());
                if !pool.formats.contains(&(fourcc, modifier))
                    || !(1..=4).contains(&allocation.plane_count())
                {
                    return Err(Fault::unsupported("GBM 返回不支持的缓冲区布局"));
                }
                let mut planes = vec![];
                for plane in 0..allocation.plane_count() as i32 {
                    planes.push(DmaPlane {
                        fd: allocation
                            .fd_for_plane(plane)
                            .map_err(|e| Fault::unavailable(format!("导出 DMA-BUF: {e:?}")))?,
                        offset: allocation.offset(plane),
                        stride: allocation.stride_for_plane(plane),
                    });
                }
                let image = Arc::new(DmaImage {
                    width,
                    height,
                    fourcc,
                    modifier,
                    planes,
                });
                self.state.import_id = self.state.import_id.wrapping_add(1);
                self.state.dma_import = None;
                let params = Import(
                    self.state
                        .dma
                        .as_ref()
                        .unwrap()
                        .create_params(&self.queue.handle(), self.state.import_id),
                );
                for (index, plane) in image.planes.iter().enumerate() {
                    params.0.add(
                        plane.fd.as_fd(),
                        index as u32,
                        plane.offset,
                        plane.stride,
                        (modifier >> 32) as u32,
                        modifier as u32,
                    );
                }
                // Asynchronous import reports unsupported modifiers without killing
                // the private Wayland connection (unlike create_immed).
                params
                    .0
                    .create(width as i32, height as i32, fourcc, params::Flags::empty());
                let deadline = Instant::now() + Duration::from_secs(4);
                while self.state.dma_import.is_none() {
                    if let Err(error) = self.step(cancel, deadline) {
                        self.state.import_id = self.state.import_id.wrapping_add(1);
                        if let Some(Ok(buffer)) = self.state.dma_import.take() {
                            buffer.destroy();
                        }
                        return Err(error);
                    }
                }
                let buffer = self
                    .state
                    .dma_import
                    .take()
                    .unwrap()
                    .map_err(|()| Fault::unsupported("合成器无法导入 GPU 缓冲区"))?;
                let released = buffer.data::<Arc<AtomicBool>>().unwrap().clone();
                pool.buffers.push(Buffer {
                    _allocation: allocation,
                    image,
                    buffer,
                    released,
                });
                pool.buffers.len() - 1
            }
            None => return Err(Fault::new(ErrorCode::Busy, "GPU 帧仍被 GTK 持有")),
        };
        let buffer = &pool.buffers[slot];
        buffer.released.store(false, Ordering::Release);
        frame.0.copy_with_damage(&buffer.buffer);
        let deadline = Instant::now() + Duration::from_secs(3600);
        while (!self.state.frame_ready || !buffer.released.load(Ordering::Acquire))
            && !self.state.frame_failed
        {
            self.step(cancel, deadline)?;
        }
        if self.state.frame_failed || self.state.inverted {
            return Err(Fault::unavailable("DMA-BUF 传输失败或需要翻转"));
        }
        // GTK holds these exported fds. The GBM allocation is never reused until
        // both compositor release and the final GTK texture lease are gone.
        Ok(buffer.image.clone())
    }
}

impl Dispatch<linux_dma::ZwpLinuxDmabufV1, ()> for WireState {
    fn event(
        state: &mut Self,
        _: &linux_dma::ZwpLinuxDmabufV1,
        event: linux_dma::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let linux_dma::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            state.dma_formats.push((
                format,
                (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
            ));
        }
    }
}
impl Dispatch<params::ZwpLinuxBufferParamsV1, u64> for WireState {
    fn event(
        state: &mut Self,
        _: &params::ZwpLinuxBufferParamsV1,
        event: params::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            params::Event::Created { buffer } if *id == state.import_id => {
                state.dma_import = Some(Ok(buffer))
            }
            params::Event::Created { buffer } => buffer.destroy(),
            params::Event::Failed if *id == state.import_id => state.dma_import = Some(Err(())),
            _ => {}
        }
    }
    wayland_client::event_created_child!(WireState, params::ZwpLinuxBufferParamsV1, [
        0 => (wl_buffer::WlBuffer, Arc::new(AtomicBool::new(true)))
    ]);
}
