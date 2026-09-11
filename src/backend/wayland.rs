//! @file wayland.rs
//! @brief 有界等待的 Wayland 截图与虚拟输入，不访问宿主剪贴板
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::{
    Cancellation,
    frame::{PixelFormat, Pixels, RawFrame},
};
use crate::model::*;
#[path = "dmabuf.rs"]
mod dmabuf;
use base64::Engine;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool,
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1 as linux_dma;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as keyboard_manager, zwp_virtual_keyboard_v1 as keyboard,
};
use wayland_protocols_wlr::{
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as copy,
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1 as pointer_manager, zwlr_virtual_pointer_v1 as pointer,
    },
};

#[derive(Clone, Debug)]
pub struct Output {
    pub proxy: wl_output::WlOutput,
    pub global: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    pub transform: wl_output::Transform,
}

impl Output {
    pub fn image_size(&self) -> (u32, u32) {
        if matches!(
            self.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        ) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

#[derive(Default)]
struct WireState {
    outputs: BTreeMap<u32, Output>,
    revision: u64,
    seat: Option<wl_seat::WlSeat>,
    shm: Option<wl_shm::WlShm>,
    copy: Option<copy::ZwlrScreencopyManagerV1>,
    pointers: Option<pointer_manager::ZwlrVirtualPointerManagerV1>,
    keyboards: Option<keyboard_manager::ZwpVirtualKeyboardManagerV1>,
    synced: bool,
    dimensions: Option<(wl_shm::Format, u32, u32, u32)>,
    frame_ready: bool,
    formats_done: bool,
    capture_id: u64,
    frame_failed: bool,
    inverted: bool,
    dma: Option<linux_dma::ZwpLinuxDmabufV1>,
    dma_formats: Vec<(u32, u64)>,
    dma_dimensions: Option<(u32, u32, u32)>,
    dma_import: Option<std::result::Result<wl_buffer::WlBuffer, ()>>,
    import_id: u64,
}

pub struct Wayland {
    connection: Connection,
    queue: EventQueue<WireState>,
    state: WireState,
    input_output: Option<u32>,
    capture_buffers: Vec<CaptureBuffer>,
    dma_pool: Option<dmabuf::Pool>,
    input_devices: Option<(
        pointer::ZwlrVirtualPointerV1,
        keyboard::ZwpVirtualKeyboardV1,
    )>,
}

struct CaptureRequest(frame::ZwlrScreencopyFrameV1);
impl Drop for CaptureRequest {
    fn drop(&mut self) {
        self.0.destroy();
    }
}
struct CaptureBuffer {
    key: (wl_shm::Format, u32, u32, u32),
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    mapping: Arc<memmap2::Mmap>,
    released: Arc<AtomicBool>,
}
impl CaptureBuffer {
    fn available(&self) -> bool {
        self.released.load(Ordering::Acquire) && Arc::strong_count(&self.mapping) == 1
    }
}
impl Drop for CaptureBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

impl Wayland {
    pub fn host_path() -> Result<PathBuf> {
        let display = std::env::var_os("WAYLAND_DISPLAY")
            .ok_or_else(|| Fault::unavailable("缺少 WAYLAND_DISPLAY"))?;
        let path = PathBuf::from(display);
        if path.is_absolute() {
            Ok(path)
        } else {
            Ok(PathBuf::from(
                std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or_else(|| Fault::unavailable("缺少 XDG_RUNTIME_DIR"))?,
            )
            .join(path))
        }
    }
    pub fn connect(path: &Path, cancel: &Cancellation) -> Result<Self> {
        let socket = UnixStream::connect(path)
            .map_err(|e| Fault::unavailable(format!("连接 Wayland: {e}")))?;
        let connection =
            Connection::from_socket(socket).map_err(|e| Fault::unavailable(e.to_string()))?;
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut this = Self {
            connection,
            queue,
            state: WireState::default(),
            input_devices: None,
            input_output: None,
            capture_buffers: vec![],
            dma_pool: None,
        };
        this.sync(cancel)?;
        this.sync(cancel)?;
        Ok(this)
    }
    fn step(&mut self, cancel: &Cancellation, deadline: Instant) -> Result<()> {
        cancel.check()?;
        if Instant::now() > deadline {
            return Err(Fault::unavailable("Wayland 响应超时"));
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        self.connection
            .flush()
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        if let Some(guard) = self.queue.prepare_read() {
            let mut fds = [nix::poll::PollFd::new(
                self.connection.as_fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            let count =
                nix::poll::poll(&mut fds, 20u16).map_err(|e| Fault::unavailable(e.to_string()))?;
            if count > 0 {
                guard
                    .read()
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        cancel.check()
    }
    fn sync(&mut self, cancel: &Cancellation) -> Result<()> {
        self.state.synced = false;
        self.connection.display().sync(&self.queue.handle(), ());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.state.synced {
            self.step(cancel, deadline)?;
        }
        Ok(())
    }
    pub fn revision(&self) -> u64 {
        self.state.revision
    }
    pub fn outputs(&self) -> Vec<Output> {
        self.state.outputs.values().cloned().collect()
    }
    pub fn refresh(&mut self, cancel: &Cancellation) -> Result<()> {
        self.sync(cancel)?;
        // Hot-plug registry events create wl_output bindings. Receive their
        // initial geometry/name events before publishing the new target.
        self.sync(cancel)
    }
    pub fn initialize_input(&mut self, output: &Output, cancel: &Cancellation) -> Result<()> {
        if self.input_devices.is_some() && self.input_output == Some(output.global) {
            return Ok(());
        }
        if !self.input_supported() {
            return Err(Fault::unsupported("合成器缺少虚拟输入协议"));
        }
        let qh = self.queue.handle();
        let seat = self.state.seat.as_ref().unwrap();
        let pointer = self
            .state
            .pointers
            .as_ref()
            .unwrap()
            .create_virtual_pointer_with_output(Some(seat), Some(&output.proxy), &qh, ());
        let keyboard = if let Some((old_pointer, keyboard)) = self.input_devices.take() {
            old_pointer.destroy();
            keyboard
        } else {
            self.state
                .keyboards
                .as_ref()
                .unwrap()
                .create_virtual_keyboard(seat, &qh, ())
        };
        let guard = InputGuard {
            pointer: pointer.clone(),
            keyboard: keyboard.clone(),
            connection: self.connection.clone(),
            transform: output.transform,
            buttons: vec![],
            keys: vec![],
        };
        let _map = guard.keymap(&["a".into()])?;
        self.input_devices = Some((pointer, keyboard));
        self.input_output = Some(output.global);
        self.sync(cancel)
    }
    pub fn input_supported(&self) -> bool {
        self.state.seat.is_some()
            && self
                .state
                .pointers
                .as_ref()
                .is_some_and(|p| p.version() >= 2)
            && self.state.keyboards.is_some()
    }
    pub fn capture_supported(&self) -> bool {
        self.state.copy.is_some() && self.state.shm.is_some()
    }
    pub fn output(&self, name: &str) -> Result<Output> {
        self.outputs()
            .into_iter()
            .find(|o| o.name == name)
            .ok_or_else(|| Fault::stale("显示器已移除"))
    }
    pub fn capture(
        &mut self,
        output: &Output,
        cancel: &Cancellation,
    ) -> Result<(String, u32, u32)> {
        let raw = self.capture_raw(output, false, cancel)?;
        let mut png = std::io::Cursor::new(Vec::new());
        raw.rgba()
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        Ok((
            base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
            raw.width,
            raw.height,
        ))
    }

    fn begin_capture(
        &mut self,
        output: &Output,
        damage: bool,
        cancel: &Cancellation,
    ) -> Result<CaptureRequest> {
        let manager = self
            .state
            .copy
            .as_ref()
            .ok_or_else(|| Fault::unsupported("缺少 wlr-screencopy"))?;
        self.state.dimensions = None;
        self.state.dma_dimensions = None;
        self.state.frame_failed = false;
        self.state.frame_ready = false;
        self.state.formats_done = false;
        self.state.inverted = false;
        self.state.capture_id = self.state.capture_id.wrapping_add(1);
        let frame = CaptureRequest(manager.capture_output(
            i32::from(damage),
            &output.proxy,
            &self.queue.handle(),
            self.state.capture_id,
        ));
        let deadline = Instant::now() + Duration::from_secs(4);
        while (self.state.dimensions.is_none()
            || (frame.0.version() >= 3 && !self.state.formats_done))
            && !self.state.frame_failed
        {
            self.step(cancel, deadline)?;
        }
        if self.state.frame_failed {
            return Err(Fault::unavailable("截图被合成器拒绝"));
        }
        Ok(frame)
    }

    /// A bounded pool leases immutable pixels to GTK. No PNG, base64, file read,
    /// or channel swizzle is performed on the normal little-endian preview path.
    pub fn capture_raw(
        &mut self,
        output: &Output,
        damage: bool,
        cancel: &Cancellation,
    ) -> Result<RawFrame> {
        if !damage {
            self.capture_buffers.clear();
        }
        let frame = self.begin_capture(output, damage, cancel)?;
        let shm = self
            .state
            .shm
            .clone()
            .ok_or_else(|| Fault::unsupported("缺少 wl_shm"))?;
        let key = self.state.dimensions.unwrap();
        let (format, width, height, stride) = key;
        let size = u64::from(stride) * u64::from(height);
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || stride < width.saturating_mul(4)
            || size > 256 * 1024 * 1024
        {
            return Err(Fault::unavailable("截图尺寸超出限制"));
        }
        // Obsolete pools can be destroyed while outstanding GTK leases retain
        // their mappings. Never overwrite a buffer still sampled by GTK.
        self.capture_buffers.retain(|buffer| buffer.key == key);
        let slot = self
            .capture_buffers
            .iter()
            .position(CaptureBuffer::available);
        let slot = match slot {
            Some(slot) => slot,
            None if self.capture_buffers.len() < 4 => {
                let file = tempfile::tempfile().map_err(|e| Fault::unavailable(e.to_string()))?;
                file.set_len(size)
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                // SAFETY: the private compositor is the only writer. We expose
                // immutable slices only after screencopy ready and buffer release;
                // a new write is forbidden while any external Arc lease exists.
                let mapping = unsafe { memmap2::MmapOptions::new().len(size as usize).map(&file) }
                    .map_err(|e| Fault::unavailable(e.to_string()))?;
                let pool = shm.create_pool(file.as_fd(), size as i32, &self.queue.handle(), ());
                let released = Arc::new(AtomicBool::new(true));
                let buffer = pool.create_buffer(
                    0,
                    width as i32,
                    height as i32,
                    stride as i32,
                    format,
                    &self.queue.handle(),
                    released.clone(),
                );
                self.capture_buffers.push(CaptureBuffer {
                    key,
                    buffer,
                    pool,
                    mapping: Arc::new(mapping),
                    released,
                });
                self.capture_buffers.len() - 1
            }
            None => {
                return Err(Fault::new(
                    ErrorCode::Busy,
                    "显示缓冲区仍在使用，丢弃过时帧",
                ));
            }
        };
        let buffer = &self.capture_buffers[slot];
        buffer.released.store(false, Ordering::Release);
        if damage && frame.0.version() >= 2 {
            frame.0.copy_with_damage(&buffer.buffer);
        } else {
            frame.0.copy(&buffer.buffer);
        }
        // A static image waits for damage without repeatedly copying pixels.
        // The preview cancellation generation changes on resize/hide/close.
        let deadline = Instant::now() + Duration::from_secs(if damage { 3600 } else { 4 });
        while (!self.state.frame_ready
            || (damage && !self.capture_buffers[slot].released.load(Ordering::Acquire)))
            && !self.state.frame_failed
        {
            self.step(cancel, deadline)?;
        }
        if self.state.frame_failed {
            return Err(Fault::unavailable("截图传输失败"));
        }
        let mapping = self.capture_buffers[slot].mapping.clone();
        if !damage {
            // niri reports completed screenshot contents through frame.ready,
            // without wl_buffer.release. Standalone PNG captures never recycle
            // these buffers; the returned mapping keeps their pixels alive.
            self.capture_buffers.clear();
        }
        if cfg!(target_endian = "little")
            && output.transform == wl_output::Transform::Normal
            && !self.state.inverted
        {
            let format = match format {
                wl_shm::Format::Argb8888 => PixelFormat::Bgra,
                wl_shm::Format::Xrgb8888 => PixelFormat::Bgrx,
                wl_shm::Format::Abgr8888 => PixelFormat::Rgba,
                wl_shm::Format::Xbgr8888 => PixelFormat::Rgbx,
                _ => return Err(Fault::unsupported("不支持的 SHM 像素格式")),
            };
            return Ok(RawFrame {
                pixels: Pixels::Shared(mapping),
                width,
                height,
                stride,
                format,
            });
        }
        let rgba = decode_shm(&mapping, format, width, height, stride, self.state.inverted)?;
        let image = image::RgbaImage::from_raw(width, height, rgba).unwrap();
        let image = orient(image, output.transform);
        let (width, height) = image.dimensions();
        Ok(RawFrame {
            pixels: Pixels::Owned(image.into_raw()),
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Rgba,
        })
    }
    pub fn input(
        &mut self,
        output: &Output,
        size: (u32, u32),
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()> {
        self.input_checked(output, size, action, cancel, &mut || Ok(()))
    }
    pub fn input_checked(
        &mut self,
        output: &Output,
        size: (u32, u32),
        action: &Action,
        cancel: &Cancellation,
        validate: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        cancel.check()?;
        validate()?;
        if !self.input_supported() {
            return Err(Fault::unsupported("合成器缺少虚拟输入协议"));
        }
        let revision = self.revision();
        self.initialize_input(output, cancel)?;
        if revision != self.revision() {
            return Err(Fault::stale("输入设备初始化期间显示器变化"));
        }
        validate()?;
        let (p, k) = self.input_devices.as_ref().unwrap().clone();
        let mut input = InputGuard {
            pointer: p,
            keyboard: k,
            connection: self.connection.clone(),
            transform: output.transform,
            buttons: vec![],
            keys: vec![],
        };
        let result = input.perform(size, action, cancel, &mut || {
            self.sync(cancel)?;
            if self.revision() != revision {
                return Err(Fault::stale("输入期间显示器变化"));
            }
            validate()
        });
        // Drop sends release events even on cancellation or protocol failure.
        drop(input);
        // A cancelled generation must still complete the release round-trip.
        // Disconnecting before the compositor receives it can leave a grab held.
        let released = self.sync(&Cancellation::new(std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(0),
        )));
        result?;
        released
    }
}

fn orient(image: image::RgbaImage, transform: wl_output::Transform) -> image::RgbaImage {
    use image::imageops::{flip_horizontal, rotate90, rotate180, rotate270};
    match transform {
        wl_output::Transform::_90 => rotate90(&image),
        wl_output::Transform::_180 => rotate180(&image),
        wl_output::Transform::_270 => rotate270(&image),
        wl_output::Transform::Flipped => flip_horizontal(&image),
        wl_output::Transform::Flipped90 => rotate90(&flip_horizontal(&image)),
        wl_output::Transform::Flipped180 => rotate180(&flip_horizontal(&image)),
        wl_output::Transform::Flipped270 => rotate270(&flip_horizontal(&image)),
        _ => image,
    }
}

fn decode_shm(
    bytes: &[u8],
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
) -> Result<Vec<u8>> {
    if !matches!(
        format,
        wl_shm::Format::Argb8888
            | wl_shm::Format::Xrgb8888
            | wl_shm::Format::Abgr8888
            | wl_shm::Format::Xbgr8888
    ) {
        return Err(Fault::unsupported("不支持的 SHM 像素格式"));
    }
    let mut result = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let row = if inverted { height - 1 - y } else { y };
        for x in 0..width {
            let at = (row * stride + x * 4) as usize;
            let pixel = bytes
                .get(at..at + 4)
                .ok_or_else(|| Fault::unavailable("截图缓冲区不完整"))?;
            let word = u32::from_ne_bytes(pixel.try_into().unwrap());
            let (r, b) = if matches!(format, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888) {
                ((word & 255) as u8, ((word >> 16) & 255) as u8)
            } else {
                (((word >> 16) & 255) as u8, (word & 255) as u8)
            };
            result.extend_from_slice(&[r, ((word >> 8) & 255) as u8, b, 255]);
        }
    }
    Ok(result)
}

fn input_position(point: Point, size: (u32, u32), transform: wl_output::Transform) -> (u32, u32) {
    let x = point.x / f64::from(size.0);
    let y = point.y / f64::from(size.1);
    let (x, y) = match transform {
        wl_output::Transform::_90 => (y, 1.0 - x),
        wl_output::Transform::_180 => (1.0 - x, 1.0 - y),
        wl_output::Transform::_270 => (1.0 - y, x),
        wl_output::Transform::Flipped => (1.0 - x, y),
        wl_output::Transform::Flipped90 => (1.0 - y, 1.0 - x),
        wl_output::Transform::Flipped180 => (x, 1.0 - y),
        wl_output::Transform::Flipped270 => (y, x),
        _ => (x, y),
    };
    (
        (x * 1_000_000.0).round() as u32,
        (y * 1_000_000.0).round() as u32,
    )
}

fn wheel_steps(value: f64) -> i32 {
    let steps = (value / 15.0).round() as i32;
    if steps == 0 {
        value.signum() as i32
    } else {
        steps
    }
}

fn event_time() -> u32 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    (START.get_or_init(Instant::now).elapsed().as_millis() as u32).wrapping_add(1)
}

struct InputGuard {
    pointer: pointer::ZwlrVirtualPointerV1,
    keyboard: keyboard::ZwpVirtualKeyboardV1,
    connection: Connection,
    transform: wl_output::Transform,
    buttons: Vec<u32>,
    keys: Vec<u32>,
}
impl InputGuard {
    fn flush(&self) -> Result<()> {
        self.connection
            .flush()
            .map_err(|e| Fault::unavailable(e.to_string()))
    }
    fn motion(&self, point: Point, size: (u32, u32)) {
        let (x, y) = input_position(point, size, self.transform);
        self.pointer
            .motion_absolute(event_time(), x, y, 1_000_000, 1_000_000);
        self.pointer.frame();
    }
    fn press(&mut self, button: u32) {
        self.buttons.push(button);
        self.pointer
            .button(event_time(), button, wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
    }
    fn release(&mut self, button: u32) {
        self.pointer
            .button(event_time(), button, wl_pointer::ButtonState::Released);
        self.pointer.frame();
        self.buttons.retain(|b| *b != button);
    }
    fn keymap(&self, symbols: &[String]) -> Result<File> {
        let map = make_keymap(symbols);
        let mut file = tempfile::tempfile().map_err(|e| Fault::unavailable(e.to_string()))?;
        file.write_all(map.as_bytes())
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        self.keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1 as u32,
            file.as_fd(),
            map.len() as u32,
        );
        Ok(file)
    }
    fn perform(
        &mut self,
        size: (u32, u32),
        action: &Action,
        cancel: &Cancellation,
        sync: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        match action {
            Action::Click { at, button } => {
                self.motion(*at, size);
                sync()?;
                cancel.check()?;
                let b = match button {
                    Button::Left => 0x110,
                    Button::Right => 0x111,
                    Button::Middle => 0x112,
                };
                self.press(b);
                sync()?;
                std::thread::sleep(Duration::from_millis(20));
                cancel.check()?;
                self.release(b);
            }
            Action::Drag { from, to } => {
                self.motion(*from, size);
                sync()?;
                self.press(0x110);
                sync()?;
                for i in 1..=30 {
                    cancel.check()?;
                    let t = f64::from(i) / 30.0;
                    self.motion(
                        Point {
                            x: from.x + (to.x - from.x) * t,
                            y: from.y + (to.y - from.y) * t,
                        },
                        size,
                    );
                    sync()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                self.release(0x110);
            }
            Action::Scroll { at, dx, dy } => {
                self.motion(*at, size);
                sync()?;
                cancel.check()?;
                self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                if *dy != 0.0 {
                    self.pointer.axis_discrete(
                        event_time(),
                        wl_pointer::Axis::VerticalScroll,
                        *dy,
                        wheel_steps(*dy),
                    );
                }
                if *dx != 0.0 {
                    self.pointer.axis_discrete(
                        event_time(),
                        wl_pointer::Axis::HorizontalScroll,
                        *dx,
                        wheel_steps(*dx),
                    );
                }
                self.pointer.frame();
            }
            Action::Text { text } => {
                let chars: Vec<_> = text.chars().collect();
                for chunk in chars.chunks(200) {
                    cancel.check()?;
                    let symbols: Vec<_> = chunk
                        .iter()
                        .map(|c| match c {
                            '\n' => "Return".into(),
                            '\t' => "Tab".into(),
                            c => char_symbol(*c),
                        })
                        .collect();
                    let _file = self.keymap(&symbols)?;
                    sync()?;
                    for i in 0..chunk.len() {
                        cancel.check()?;
                        self.keyboard.key(event_time(), i as u32 + 30, 1);
                        self.keys.push(i as u32 + 30);
                        sync()?;
                        std::thread::sleep(Duration::from_millis(2));
                        self.keyboard.key(event_time(), i as u32 + 30, 0);
                        self.keys.clear();
                        sync()?;
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            Action::Key { key, modifiers } => {
                let symbol = key_symbol(key)?;
                let _file = self.keymap(&[symbol])?;
                sync()?;
                let mask = modifiers.iter().fold(0, |mask, m| {
                    mask | match m {
                        Modifier::Shift => 1,
                        Modifier::Ctrl => 4,
                        Modifier::Alt => 8,
                        Modifier::Super => 64,
                    }
                });
                cancel.check()?;
                self.keyboard.modifiers(mask, 0, 0, 0);
                sync()?;
                self.keys.push(30);
                self.keyboard.key(event_time(), 30, 1);
                sync()?;
                std::thread::sleep(Duration::from_millis(2));
                self.keyboard.key(event_time(), 30, 0);
                self.keys.clear();
                self.keyboard.modifiers(0, 0, 0, 0);
            }
            _ => return Err(Fault::unsupported("此操作不是虚拟输入动作")),
        }
        self.flush()
    }
}
impl Drop for InputGuard {
    fn drop(&mut self) {
        for key in &self.keys {
            self.keyboard.key(event_time(), *key, 0);
        }
        self.keyboard.modifiers(0, 0, 0, 0);
        for button in &self.buttons {
            self.pointer
                .button(0, *button, wl_pointer::ButtonState::Released);
        }
        self.pointer.frame();
        let _ = self.connection.flush();
    }
}

fn char_symbol(c: char) -> String {
    if u32::from(c) <= 255 {
        format!("0x{:x}", u32::from(c))
    } else {
        format!("U{:04X}", u32::from(c))
    }
}

fn key_symbol(key: &str) -> Result<String> {
    if key.chars().count() == 1 {
        return Ok(char_symbol(key.chars().next().unwrap()));
    }
    let key = match key {
        "Enter" => "Return",
        "Esc" => "Escape",
        "Space" => "space",
        "Backspace" => "BackSpace",
        key => key,
    };
    if [
        "Return",
        "Escape",
        "Tab",
        "space",
        "BackSpace",
        "Delete",
        "Insert",
        "Home",
        "End",
        "Page_Up",
        "Page_Down",
        "Left",
        "Right",
        "Up",
        "Down",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
    ]
    .contains(&key)
    {
        Ok(key.into())
    } else {
        Err(Fault::unsupported("未知按键名称"))
    }
}

fn make_keymap(symbols: &[String]) -> String {
    let mut codes = String::new();
    let mut keys = String::new();
    for (i, symbol) in symbols.iter().enumerate() {
        codes.push_str(&format!("<K{i:03}> = {};\n", i + 38));
        keys.push_str(&format!("key <K{i:03}> {{ [ {symbol} ] }};\n"));
    }
    format!(
        "xkb_keymap {{ xkb_keycodes \"mcp\" {{ minimum=8; maximum=255; {codes} <LFSH>=250; <LCTL>=251; <LALT>=252; <LWIN>=253; }}; xkb_types \"mcp\" {{ include \"complete\" }}; xkb_compatibility \"mcp\" {{ include \"complete\" }}; xkb_symbols \"mcp\" {{ {keys} key <LFSH> {{ [ Shift_L ] }}; key <LCTL> {{ [ Control_L ] }}; key <LALT> {{ [ Alt_L ] }}; key <LWIN> {{ [ Super_L ] }}; modifier_map Shift {{ <LFSH> }}; modifier_map Control {{ <LCTL> }}; modifier_map Mod1 {{ <LALT> }}; modifier_map Mod4 {{ <LWIN> }}; }}; }};\0"
    )
}

impl Dispatch<wl_registry::WlRegistry, ()> for WireState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_output" => {
                    let proxy = registry.bind(name, version.min(4), qh, name);
                    state.outputs.insert(
                        name,
                        Output {
                            proxy,
                            global: name,
                            name: format!("output-{name}"),
                            width: 0,
                            height: 0,
                            scale: 1,
                            transform: wl_output::Transform::Normal,
                        },
                    );
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                "zwp_linux_dmabuf_v1" if version >= 3 => {
                    state.dma = Some(registry.bind(name, 3, qh, ()));
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => {
                    state.copy = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointers = Some(registry.bind(name, version.min(2), qh, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboards = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } if state.outputs.remove(&name).is_some() => {
                state.revision = state.revision.wrapping_add(1);
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for WireState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(output) = state.outputs.get_mut(id) {
            let before = (
                output.name.clone(),
                output.width,
                output.height,
                output.scale,
                output.transform,
            );
            match event {
                wl_output::Event::Name { name } => output.name = name,
                wl_output::Event::Mode {
                    flags: WEnum::Value(flags),
                    width,
                    height,
                    ..
                } if flags.contains(wl_output::Mode::Current) => {
                    output.width = width.max(0) as u32;
                    output.height = height.max(0) as u32;
                }
                wl_output::Event::Scale { factor } => output.scale = factor,
                wl_output::Event::Geometry {
                    transform: WEnum::Value(transform),
                    ..
                } => output.transform = transform,
                _ => {}
            }
            let after = (
                output.name.clone(),
                output.width,
                output.height,
                output.scale,
                output.transform,
            );
            if before != after {
                state.revision = state.revision.wrapping_add(1);
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for WireState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}
impl Dispatch<frame::ZwlrScreencopyFrameV1, u64> for WireState {
    fn event(
        state: &mut Self,
        _: &frame::ZwlrScreencopyFrameV1,
        event: frame::Event,
        capture_id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if *capture_id != state.capture_id {
            return;
        }
        match event {
            frame::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => state.dimensions = Some((format, width, height, stride)),
            frame::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                state.dma_dimensions = Some((format, width, height));
            }
            frame::Event::BufferDone => state.formats_done = true,
            frame::Event::Ready { .. } => state.frame_ready = true,
            frame::Event::Failed => state.frame_failed = true,
            frame::Event::Flags {
                flags: WEnum::Value(flags),
            } => state.inverted = flags.contains(frame::Flags::YInvert),
            _ => {}
        }
    }
}
delegate_noop!(WireState: ignore wl_seat::WlSeat);
delegate_noop!(WireState: ignore wl_shm::WlShm);
delegate_noop!(WireState: ignore wl_shm_pool::WlShmPool);
impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for WireState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        released: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            released.store(true, Ordering::Release);
        }
    }
}
delegate_noop!(WireState: ignore copy::ZwlrScreencopyManagerV1);
delegate_noop!(WireState: ignore pointer_manager::ZwlrVirtualPointerManagerV1);
delegate_noop!(WireState: ignore pointer::ZwlrVirtualPointerV1);
delegate_noop!(WireState: ignore keyboard_manager::ZwpVirtualKeyboardManagerV1);
delegate_noop!(WireState: ignore keyboard::ZwpVirtualKeyboardV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shm_stride_and_inversion_are_respected() {
        let bytes = [
            0x00112233u32.to_ne_bytes(),
            [0; 4],
            0x00445566u32.to_ne_bytes(),
            [0; 4],
        ]
        .concat();
        assert_eq!(
            decode_shm(&bytes, wl_shm::Format::Xrgb8888, 1, 2, 8, true).unwrap(),
            [0x44, 0x55, 0x66, 255, 0x11, 0x22, 0x33, 255]
        );
        assert!(decode_shm(&[], wl_shm::Format::Xrgb8888, 1, 1, 4, false).is_err());
    }
    #[test]
    fn key_names_cannot_inject_xkb_configuration() {
        assert!(key_symbol("x }; include evil").is_err());
        assert_eq!(key_symbol("猫").unwrap(), "U732B");
    }
}
