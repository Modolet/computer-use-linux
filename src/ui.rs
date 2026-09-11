//! @file ui.rs
//! @brief 本地 GTK 授权窗口、会话管理与独立应用预览
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use crate::{
    backend::{
        Backend, Cancellation,
        desktop::Desktop,
        frame::{DmaImage, Image, PixelFormat, RawFrame},
        preview::{Stream, Viewport},
        sway::{Application, Isolated},
    },
    ipc::{self, SharedPolicy},
    model::*,
    policy::{BackendHandle, Policy},
};
use base64::Engine;
use gtk4::{self as gtk, gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Prepared {
    backend: Box<dyn Backend>,
    label: String,
    preview: Option<String>,
}
enum Event {
    InputQuiescent(u64, Result<()>),
    AllowedIsolated(String, u64, Result<Prepared>),
    Prepared(String, Result<Prepared>),
    Recovered(Vec<Isolated>),
}
struct Ui {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    rows: gtk::Box,
    banner: gtk::Label,
    policy: SharedPolicy,
    sender: mpsc::Sender<Event>,
    prepared: RefCell<HashMap<String, Prepared>>,
    busy: RefCell<HashSet<String>>,
    detached: RefCell<Vec<(String, BackendHandle)>>,
    signature: RefCell<String>,
}
fn label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l
}
fn button(text: &str, container: &gtk::Box, action: impl Fn() + 'static) {
    let b = gtk::Button::with_label(text);
    b.connect_clicked(move |_| action());
    container.append(&b);
}
fn texture(png: &str) -> Option<gdk::Texture> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(png).ok()?;
    gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
}
fn raw_texture(frame: RawFrame) -> gdk::MemoryTexture {
    let format = match frame.format {
        PixelFormat::Bgra => gdk::MemoryFormat::B8g8r8a8Premultiplied,
        PixelFormat::Bgrx => gdk::MemoryFormat::B8g8r8x8,
        PixelFormat::Rgba => gdk::MemoryFormat::R8g8b8a8Premultiplied,
        PixelFormat::Rgbx => gdk::MemoryFormat::R8g8b8x8,
    };
    gdk::MemoryTexture::new(
        frame.width as i32,
        frame.height as i32,
        format,
        &glib::Bytes::from_owned(frame.pixels),
        frame.stride as usize,
    )
}
fn preview_texture(image: Image) -> Result<gdk::Texture> {
    use glib::translate::*;
    use std::os::fd::AsRawFd;
    let Image::Dma(image) = image else {
        let Image::Memory(frame) = image else {
            unreachable!()
        };
        return Ok(raw_texture(frame).upcast());
    };
    let display =
        gdk::Display::default().ok_or_else(|| Fault::unavailable("GTK display unavailable"))?;
    let mut builder = gdk::DmabufTextureBuilder::new()
        .set_display(&display)
        .set_width(image.width)
        .set_height(image.height)
        .set_fourcc(image.fourcc)
        .set_modifier(image.modifier)
        .set_n_planes(image.planes.len() as u32)
        .set_premultiplied(true);
    for (index, plane) in image.planes.iter().enumerate() {
        // SAFETY: the Arc lease below owns every fd until texture finalization.
        builder = unsafe { builder.set_fd(index as u32, plane.fd.as_raw_fd()) }
            .set_offset(index as u32, plane.offset)
            .set_stride(index as u32, plane.stride);
    }
    unsafe extern "C" fn release(data: glib::ffi::gpointer) {
        // SAFETY: exactly one successful GDK texture owns this boxed lease.
        unsafe {
            drop(Box::<Arc<DmaImage>>::from_raw(data.cast()));
        }
    }
    let lease = Box::into_raw(Box::new(image));
    // SAFETY: valid builder/plane fds. GDK takes the callback on successful
    // construction only; on failure we reclaim it ourselves. Using the FFI here
    // also avoids leaking the closure in gtk-rs build_with_release_func's error path.
    unsafe {
        let mut error = std::ptr::null_mut();
        let texture = gdk::ffi::gdk_dmabuf_texture_builder_build(
            builder.to_glib_none().0,
            Some(release),
            lease.cast(),
            &mut error,
        );
        if texture.is_null() {
            drop(Box::from_raw(lease));
            let message = if error.is_null() {
                "GTK 不支持此 DMA-BUF".into()
            } else {
                let error: glib::Error = from_glib_full(error);
                error.to_string()
            };
            Err(Fault::unavailable(message))
        } else {
            Ok(from_glib_full(texture))
        }
    }
}
#[cfg(test)]
fn uncancelled() -> Cancellation {
    Cancellation::new(Arc::new(AtomicU64::new(0)))
}
fn capabilities_text(capabilities: &[String]) -> String {
    capabilities
        .iter()
        .map(|c| match c.as_str() {
            "screenshot" => "截图",
            "click" => "点击",
            "drag" => "拖动",
            "scroll" => "滚动",
            "key" => "按键",
            "text" => "文本输入",
            "focus_window" => "切换窗口",
            _ => "未知能力",
        })
        .collect::<Vec<_>>()
        .join("、")
}

pub fn run(runtime: &tokio::runtime::Runtime) -> anyhow::Result<()> {
    let _enter = runtime.enter();
    let listener = ipc::Listener::bind()?;
    let policy = Arc::new(Mutex::new(Policy::default()));
    let (show_tx, show_rx) = mpsc::channel();
    let show_rx = Rc::new(RefCell::new(Some(show_rx)));
    let serve_policy = policy.clone();
    runtime.spawn(async move {
        if let Err(e) = ipc::serve(listener, serve_policy, show_tx).await {
            tracing::error!("权限服务退出: {e:#}");
        }
    });
    let app = gtk::Application::builder()
        .application_id("io.github.computer_use_linux.Broker")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let hold = app.hold();
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Computer Use · 本地授权")
            .default_width(720)
            .default_height(720)
            .build();
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
        outer.set_margin_top(18);
        outer.set_margin_bottom(18);
        outer.set_margin_start(18);
        outer.set_margin_end(18);
        outer.append(&label("应用与电脑控制权限"));
        outer.append(&label(
            "此面板打开时，所有 AI 输入均已暂停。应用模式保留应用原有的文件与网络权限。",
        ));
        let banner = label("");
        outer.append(&banner);
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 14);
        outer.append(
            &gtk::ScrolledWindow::builder()
                .vexpand(true)
                .child(&rows)
                .build(),
        );
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        outer.append(&controls);
        window.set_child(Some(&outer));
        let (sender, receiver) = mpsc::channel();
        let ui = Rc::new(Ui {
            app: app.clone(),
            window: window.clone(),
            rows,
            banner,
            policy: policy.clone(),
            sender,
            prepared: RefCell::new(HashMap::new()),
            busy: RefCell::new(HashSet::new()),
            detached: RefCell::new(vec![]),
            signature: RefCell::new(String::new()),
        });
        let weak = Rc::downgrade(&ui);
        button("暂停全部", &controls, move || {
            if let Some(ui) = weak.upgrade() {
                ui.policy.lock().unwrap().pause_all();
                ui.invalidate();
            }
        });
        let weak = Rc::downgrade(&ui);
        button("隐藏面板（保持暂停）", &controls, move || {
            if let Some(ui) = weak.upgrade() {
                ui.hide();
            }
        });
        let weak = Rc::downgrade(&ui);
        window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.hide();
            }
            glib::Propagation::Stop
        });
        let receiver_show = show_rx.borrow_mut().take().unwrap();
        let recover_sender = ui.sender.clone();
        std::thread::spawn(move || {
            let _ = recover_sender.send(Event::Recovered(Isolated::recover()));
        });
        glib::timeout_add_local(Duration::from_millis(150), move || {
            let _keep_alive = &hold;
            if receiver_show.try_iter().last().is_some() {
                let (epoch, handles) = {
                    let p = ui.policy.lock().unwrap();
                    (p.ui_epoch, p.input_handles())
                };
                let sender = ui.sender.clone();
                std::thread::spawn(move || {
                    let result = wait_for_input(handles);
                    let _ = sender.send(Event::InputQuiescent(epoch, result));
                });
            }
            for event in receiver.try_iter() {
                match event {
                    Event::InputQuiescent(epoch, result) => {
                        let ready = {
                            let p = ui.policy.lock().unwrap();
                            p.ui_visible && p.ui_epoch == epoch
                        };
                        if ready {
                            match result {
                                Ok(()) => {
                                    ui.window.present();
                                }
                                Err(e) => tracing::error!("授权界面保持隐藏：{e}"),
                            }
                        }
                    }
                    Event::Prepared(id, result) => {
                        ui.busy.borrow_mut().remove(&id);
                        match result {
                            Ok(ready) => {
                                let pending = ui
                                    .policy
                                    .lock()
                                    .unwrap()
                                    .sessions
                                    .get(&id)
                                    .is_some_and(|s| s.status.state == State::Pending);
                                if pending {
                                    ui.prepared.borrow_mut().insert(id, ready);
                                } else if ready.backend.survives_revoke() {
                                    ui.detached
                                        .borrow_mut()
                                        .push((ready.label, Arc::new(Mutex::new(ready.backend))));
                                }
                            }
                            Err(e) => ui.banner.set_text(&e.message),
                        }
                    }
                    Event::AllowedIsolated(id, epoch, result) => {
                        ui.busy.borrow_mut().remove(&id);
                        match result {
                            Ok(ready) => ui.finish_allowed_isolated(&id, epoch, ready),
                            Err(e) => {
                                ui.policy.lock().unwrap().deny(&id, e.message.clone());
                                ui.banner.set_text(&e.message);
                            }
                        }
                    }
                    Event::Recovered(sessions) => {
                        for isolated in sessions {
                            ui.detached.borrow_mut().push((
                                format!("恢复：{}", isolated.saved.application.label()),
                                Arc::new(Mutex::new(Box::new(isolated))),
                            ));
                        }
                    }
                }
                ui.invalidate();
            }
            let retained = std::mem::take(&mut ui.policy.lock().unwrap().retained);
            if !retained.is_empty() {
                ui.detached.borrow_mut().extend(retained);
                ui.invalidate();
            }
            ui.render();
            glib::ControlFlow::Continue
        });
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

// Cancellation is signalled before this runs. Every input worker retains its
// backend mutex through key release and the Wayland sync, so the authorization
// buttons must not be mapped until all of those critical sections have exited.
fn wait_for_input(handles: Vec<BackendHandle>) -> Result<()> {
    for handle in handles {
        let _guard = handle
            .lock()
            .map_err(|_| Fault::unavailable("输入后端异常，无法确认按键已释放"))?;
    }
    Ok(())
}

impl Ui {
    fn invalidate(&self) {
        self.signature.borrow_mut().clear();
    }
    fn hide(&self) {
        self.window.set_visible(false);
        self.policy.lock().unwrap().ui_visible = false;
    }
    fn render(self: &Rc<Self>) {
        let mut statuses: Vec<_> = self
            .policy
            .lock()
            .unwrap()
            .sessions
            .values()
            .map(|s| s.status.clone())
            .collect();
        statuses.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let signature = serde_json::to_string(&statuses).unwrap();
        if *self.signature.borrow() == signature {
            return;
        }
        *self.signature.borrow_mut() = signature;
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        if statuses.is_empty() {
            self.rows
                .append(&label("等待 MCP 客户端请求。没有活动授权。"));
        }
        for status in statuses {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 8);
            row.append(&label(&format!(
                "{} · {} · {}",
                status.label.as_deref().unwrap_or("新的授权申请"),
                match status.mode {
                    Mode::Isolated => "独立应用",
                    Mode::Desktop => "整个电脑",
                },
                match status.state {
                    State::Pending => "等待授权",
                    State::Active => "AI 控制中",
                    State::Paused => "已暂停",
                    State::Denied => "已拒绝",
                    State::Closed => "已结束",
                }
            )));
            row.append(&label(&format!("会话 {}", status.session_id)));
            if let Some(message) = &status.message {
                row.append(&label(message));
            }
            let id = status.session_id.clone();
            if status.state == State::Pending {
                if self.busy.borrow().contains(&id) {
                    row.append(&label("正在准备，请稍候……"));
                } else if let Some(prepared) = self.prepared.borrow().get(&id) {
                    row.append(&label(&format!("将授权：{}", prepared.label)));
                    row.append(&label(&format!(
                        "能力：{}",
                        capabilities_text(&prepared.backend.capabilities())
                    )));
                    if let Some(png) = &prepared.preview
                        && let Some(t) = texture(png)
                    {
                        let p = gtk::Picture::for_paintable(&t);
                        p.set_height_request(220);
                        p.set_can_shrink(true);
                        row.append(&p);
                    }
                    let weak = Rc::downgrade(self);
                    let grant_id = id.clone();
                    button("确认授权", &row, move || {
                        if let Some(ui) = weak.upgrade() {
                            if let Some(ready) = ui.prepared.borrow_mut().remove(&grant_id)
                                && let Err(e) = ui.policy.lock().unwrap().grant(
                                    &grant_id,
                                    ready.label,
                                    ready.backend,
                                )
                            {
                                ui.banner.set_text(&e.message);
                            }
                            let backend = {
                                let p = ui.policy.lock().unwrap();
                                p.sessions
                                    .get(&grant_id)
                                    .filter(|s| s.status.mode == Mode::Isolated)
                                    .and_then(|s| s.backend.clone())
                            };
                            if let Some(backend) = backend {
                                preview(
                                    &ui.app,
                                    backend,
                                    ui.policy.clone(),
                                    Some(grant_id.clone()),
                                );
                            }
                            ui.invalidate();
                        }
                    });
                } else {
                    self.pending_controls(&row, &id, status.mode);
                }
                let weak = Rc::downgrade(self);
                let deny_id = id.clone();
                button("拒绝", &row, move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.policy
                            .lock()
                            .unwrap()
                            .deny(&deny_id, "用户拒绝授权".into());
                        if let Some(ready) = ui
                            .prepared
                            .borrow_mut()
                            .remove(&deny_id)
                            .filter(|r| r.backend.survives_revoke())
                        {
                            ui.detached
                                .borrow_mut()
                                .push((ready.label, Arc::new(Mutex::new(ready.backend))));
                        }
                        ui.invalidate();
                    }
                });
            } else if matches!(status.state, State::Active | State::Paused) {
                row.append(&label(&format!(
                    "已开放：{}",
                    capabilities_text(&status.capabilities)
                )));
                let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                row.append(&controls);
                let weak = Rc::downgrade(self);
                let resume_id = id.clone();
                button("隐藏面板并恢复 AI", &controls, move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.window.set_visible(false);
                        let policy = ui.policy.clone();
                        let id = resume_id.clone();
                        let epoch = policy.lock().unwrap().ui_epoch;
                        let window = ui.window.clone();
                        glib::timeout_add_local_once(Duration::from_millis(250), move || {
                            let mut p = policy.lock().unwrap();
                            if p.ui_epoch != epoch || window.is_visible() {
                                return;
                            }
                            p.ui_visible = false;
                            if let Err(e) = p.resume(&id) {
                                tracing::warn!("无法恢复：{e}");
                            }
                        });
                    }
                });
                let weak = Rc::downgrade(self);
                let close_id = id.clone();
                button("撤销", &controls, move || {
                    if let Some(ui) = weak.upgrade() {
                        if let Some(handle) = ui.policy.lock().unwrap().close_local(&close_id) {
                            ipc::detach(handle);
                        }
                        ui.invalidate();
                    }
                });
                if status.mode == Mode::Isolated {
                    let weak = Rc::downgrade(self);
                    let preview_id = id.clone();
                    button("打开应用窗口 / 接管", &controls, move || {
                        if let Some(ui) = weak.upgrade() {
                            let backend = {
                                let p = ui.policy.lock().unwrap();

                                p.sessions.get(&preview_id).and_then(|s| s.backend.clone())
                            };
                            if let Some(b) = backend {
                                preview(&ui.app, b, ui.policy.clone(), Some(preview_id.clone()));
                            }
                        }
                    });
                }
            }
            self.rows.append(&row);
            self.rows
                .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        for (name, backend) in self.detached.borrow().iter() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&label(name));
            let app = self.app.clone();
            let b = backend.clone();
            let policy = self.policy.clone();
            button("手动接管（无 AI 授权）", &row, move || {
                preview(&app, b.clone(), policy.clone(), None)
            });
            self.rows.append(&row);
        }
    }
    fn pending_controls(self: &Rc<Self>, row: &gtk::Box, id: &str, mode: Mode) {
        match mode {
            Mode::Isolated => {
                let requested = self
                    .policy
                    .lock()
                    .unwrap()
                    .sessions
                    .get(id)
                    .and_then(|s| s.requested_application.clone())
                    .unwrap_or_default();
                row.append(&label(&format!("请求启动：{requested}")));
                let application = match Application::requested(&requested) {
                    Ok(application) => application,
                    Err(e) => {
                        self.policy.lock().unwrap().deny(id, e.message.clone());
                        row.append(&label(&e.message));
                        return;
                    }
                };
                row.append(&label(&format!(
                    "应用：{}\n启动程序：{}",
                    application.label(),
                    application.executable().display()
                )));
                if let Application::Installed(app) = &application {
                    row.append(&label(&format!(
                        "桌面条目：{}\n预设参数：{:?}",
                        app.id, app.args
                    )));
                }
                row.append(&label("允许 AI 在可见独立窗口中截图、点击、拖动、滚动、按键和输入文本。仅隔离图形会话，使用你的个人配置、登录状态和应用数据，修改会影响日常使用的数据；应用保留文件和网络权限。"));
                let weak = Rc::downgrade(self);
                let id = id.to_string();
                button("允许并启动", row, move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.busy.borrow_mut().insert(id.clone());
                        ui.invalidate();
                        let epoch = ui.policy.lock().unwrap().ui_epoch;
                        let sender = ui.sender.clone();
                        let id = id.clone();
                        let application = application.clone();
                        std::thread::spawn(move || {
                            let name = application.label().to_string();
                            let result = Isolated::launch(application).map(|backend| Prepared {
                                backend: Box::new(backend),
                                label: name,
                                preview: None,
                            });
                            let _ = sender.send(Event::AllowedIsolated(id, epoch, result));
                        });
                    }
                });
            }
            Mode::Desktop => {
                row.append(&label(
                    "将允许 AI 观察整个桌面、切换窗口和使用真实鼠标键盘。可随时暂停。",
                ));
                let weak = Rc::downgrade(self);
                let id = id.to_string();
                button("检查整机控制能力", row, move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.prepare(id.clone(), || {
                            Ok(Prepared {
                                backend: Box::new(Desktop::connect()?),
                                label: "整个电脑".into(),
                                preview: None,
                            })
                        });
                    }
                });
            }
        }
    }
    fn finish_allowed_isolated(&self, id: &str, epoch: u64, ready: Prepared) {
        if !self
            .policy
            .lock()
            .unwrap()
            .sessions
            .get(id)
            .is_some_and(|s| s.status.state == State::Pending)
        {
            self.detached
                .borrow_mut()
                .push((ready.label, Arc::new(Mutex::new(ready.backend))));
            return;
        }
        if let Err(e) = self
            .policy
            .lock()
            .unwrap()
            .grant(id, ready.label, ready.backend)
        {
            self.banner.set_text(&e.message);
            return;
        }
        let backend = self.policy.lock().unwrap().sessions[id]
            .backend
            .clone()
            .unwrap();
        preview(
            &self.app,
            backend,
            self.policy.clone(),
            Some(id.to_string()),
        );
        // A newer authorization request must remain visible and pause this session.
        if self.policy.lock().unwrap().ui_epoch != epoch {
            return;
        }
        self.window.set_visible(false);
        let policy = self.policy.clone();
        let window = self.window.clone();
        let id = id.to_string();
        glib::timeout_add_local_once(Duration::from_millis(250), move || {
            let mut p = policy.lock().unwrap();
            if p.ui_epoch != epoch || window.is_visible() {
                return;
            }
            p.ui_visible = false;
            if let Err(e) = p.resume(&id) {
                tracing::warn!("独立应用保持暂停：{e}");
            }
        });
    }

    fn prepare(&self, id: String, prepare: impl FnOnce() -> Result<Prepared> + Send + 'static) {
        if !self.busy.borrow_mut().insert(id.clone()) {
            return;
        }
        self.invalidate();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let _ = sender.send(Event::Prepared(id, prepare()));
        });
    }
}

fn preview(
    app: &gtk::Application,
    backend: BackendHandle,
    policy: SharedPolicy,
    session_id: Option<String>,
) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("独立应用 · 实时画面")
        .default_width(1000)
        .default_height(720)
        .build();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let takeover = gtk::CheckButton::with_label("手动接管（暂停 AI）");
    outer.append(&takeover);
    let status = label("正在显示应用；只查看画面不会暂停 AI。");
    outer.append(&status);
    let picture = gtk::Picture::new();
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    picture.set_can_shrink(true);
    picture.set_focusable(true);
    outer.append(&picture);
    window.set_child(Some(&outer));
    let current = Rc::new(RefCell::new(None::<Target>));
    let alive = Rc::new(Cell::new(true));
    let manual_generation = Arc::new(AtomicU64::new(0));
    let manual_active = Rc::new(Cell::new(false));
    let active = manual_active.clone();
    let p = policy.clone();
    let gen_toggle = manual_generation.clone();
    takeover.connect_toggled(move |toggle| {
        let mut policy = p.lock().unwrap();
        if toggle.is_active() && !active.replace(true) {
            policy.pause_all();
            policy.manual_previews += 1;
        } else if !toggle.is_active() && active.replace(false) {
            policy.manual_previews = policy.manual_previews.saturating_sub(1);
            gen_toggle.fetch_add(1, Ordering::SeqCst);
        }
    });
    let a = alive.clone();
    let close_policy = policy.clone();
    let generation = manual_generation.clone();
    window.connect_close_request(move |_| {
        a.set(false);
        generation.fetch_add(1, Ordering::SeqCst);
        let mut p = close_policy.lock().unwrap();
        if manual_active.get() {
            p.manual_previews = p.manual_previews.saturating_sub(1);
        }
        glib::Propagation::Proceed
    });
    let (action_sender, action_receiver) =
        mpsc::sync_channel::<(Target, Action, Cancellation)>(128);
    let text_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let entry = gtk::Entry::builder()
        .placeholder_text("接管后可在这里使用中文输入法，再发送到应用")
        .hexpand(true)
        .build();
    text_row.append(&entry);
    let enabled = takeover.clone();
    let text_sender = action_sender.clone();
    let generation = manual_generation.clone();
    let text_target = current.clone();
    button("发送文本", &text_row, move || {
        if enabled.is_active()
            && let Some(target) = text_target.borrow().as_ref()
            && !entry.text().is_empty()
            && text_sender
                .try_send((
                    target.clone(),
                    Action::Text {
                        text: entry.text().to_string(),
                    },
                    Cancellation::new(generation.clone()),
                ))
                .is_ok()
        {
            entry.set_text("");
        }
    });
    outer.append(&text_row);
    let human_backend = backend.lock().unwrap().local_view();
    let human_serial = backend.clone();
    std::thread::spawn(move || {
        let Ok(Some(mut human_backend)) = human_backend else {
            return;
        };
        while let Ok((target, action, cancel)) = action_receiver.recv() {
            // Keep local input inside the same pause/authorization release barrier.
            let _serial = human_serial.lock().unwrap();
            if cancel.check().is_err() {
                continue;
            }
            let _ = human_backend.human_act_at(&target, &action, &cancel);
        }
    });
    let visible_policy = policy.clone();
    let visible_id = session_id.clone();
    picture.connect_map(move |_| {
        if let Some(id) = &visible_id {
            visible_policy.lock().unwrap().view_opened(id);
        }
    });
    let hidden_policy = policy.clone();
    let hidden_id = session_id.clone();
    let hidden_generation = manual_generation.clone();
    picture.connect_unmap(move |_| {
        hidden_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(id) = &hidden_id {
            hidden_policy.lock().unwrap().view_closed(id);
        }
    });
    let source = backend.lock().unwrap().realtime_preview();
    let stream = match source {
        Ok(mut source) => {
            if let Some(display) = gdk::Display::default() {
                let _ = display.prepare_gl();
                let formats = display.dmabuf_formats();
                source.dma_formats(
                    (0..formats.n_formats())
                        .map(|i| formats.format(i))
                        .collect(),
                );
            }
            Rc::new(Stream::start(source))
        }
        Err(error) => {
            status.set_text(&error.message);
            if let Some(id) = &session_id {
                policy.lock().unwrap().pause(id);
            }
            window.present();
            return;
        }
    };
    let mapped_stream = stream.clone();
    picture.connect_map(move |_| mapped_stream.visible(true));
    let hidden_stream = stream.clone();
    picture.connect_unmap(move |_| hidden_stream.visible(false));
    let closing_stream = stream.clone();
    window.connect_close_request(move |_| {
        closing_stream.stop();
        glib::Propagation::Proceed
    });
    let state = current.clone();
    let p = policy.clone();
    let id = session_id;
    let presentation = RefCell::new((
        None,
        Instant::now(),
        Instant::now(),
        0u64,
        String::new(),
        None::<String>,
    ));
    picture.add_tick_callback(move |picture, _clock| {
        let mut presentation = presentation.borrow_mut();
        let (pending_size, size_changed, measured_since, shown_frames, description, failure) =
            &mut *presentation;
        if !alive.get() {
            return glib::ControlFlow::Break;
        }
        let scale = picture
            .native()
            .and_then(|native| native.surface())
            .map(|surface| surface.scale())
            .unwrap_or(f64::from(picture.scale_factor()));
        let viewport = Viewport::new(
            (f64::from(picture.width()) * scale).round() as u32,
            (f64::from(picture.height()) * scale).round() as u32,
            scale,
        );
        if let Ok(viewport) = viewport {
            if *pending_size != Some(viewport) {
                *pending_size = Some(viewport);
                *size_changed = Instant::now();
            }
            // Coalesce window drag allocations; settled sizes render at native
            // physical pixels, including fractional monitor scales.
            if size_changed.elapsed() >= Duration::from_millis(120) {
                stream.configure(viewport);
            }
        }
        if let Some(result) = stream.take() {
            match result {
                Ok(frame) => {
                    // A late frame from a slow consumer must not add visible lag.
                    if frame.ready_at.elapsed() < Duration::from_millis(250) {
                        let (width, height) = frame.image.size();
                        *description = format!(
                            "{width}×{height} · {} · {}",
                            frame.renderer,
                            frame.image.transport()
                        );
                        match preview_texture(frame.image) {
                            Ok(texture) => picture.set_paintable(Some(&texture)),
                            Err(error) => {
                                tracing::warn!(%error, "GTK DMA-BUF 导入失败，切换共享内存");
                                stream.fallback_to_memory();
                                return glib::ControlFlow::Continue;
                            }
                        }
                        *state.borrow_mut() = Some(frame.target);
                        *shown_frames += 1;
                        *failure = None;
                    }
                }
                Err(error) => {
                    *failure = Some(error.message);
                    if let Some(id) = &id {
                        p.lock().unwrap().pause(id);
                    }
                }
            }
        }
        if measured_since.elapsed() >= Duration::from_millis(500) {
            let fps = *shown_frames as f64 / measured_since.elapsed().as_secs_f64();
            let session_text = if let Some(id) = &id {
                match p
                    .lock()
                    .unwrap()
                    .sessions
                    .get(id)
                    .map(|session| session.status.state.clone())
                {
                    Some(State::Active) => "AI 正在控制 · 你可以继续使用其他窗口",
                    Some(State::Paused) => "AI 已暂停 · 可手动接管",
                    _ => "AI 授权已结束 · 应用保留供你使用",
                }
            } else {
                "本地预览 · 可手动接管"
            };
            if let Some(error) = failure.as_ref() {
                status.set_text(error);
            } else {
                let rate = if *shown_frames == 0 {
                    "静止".into()
                } else {
                    format!("{fps:.0} FPS")
                };
                status.set_text(&format!("{session_text} · {description} · {rate}"));
            }
            *shown_frames = 0;
            *measured_since = Instant::now();
        }
        glib::ControlFlow::Continue
    });
    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    let pic = picture.clone();
    let state = current.clone();
    let enable = takeover.clone();
    let sender = action_sender.clone();
    let generation = manual_generation.clone();
    gesture.connect_pressed(move |gesture, _, x, y| {
        if !enable.is_active() || gesture.current_button() == 1 {
            return;
        }
        pic.grab_focus();
        if let Some(o) = state.borrow().as_ref()
            && let Some(at) = picture_point(&pic, o, x, y)
        {
            let button = match gesture.current_button() {
                2 => Button::Middle,
                3 => Button::Right,
                _ => Button::Left,
            };
            let _ = sender.try_send((
                o.clone(),
                Action::Click { at, button },
                Cancellation::new(generation.clone()),
            ));
        }
    });
    picture.add_controller(gesture);
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let start = Rc::new(RefCell::new(None::<(f64, f64, Cancellation)>));
    let begin = start.clone();
    let enabled = takeover.clone();
    let pic = picture.clone();
    let generation = manual_generation.clone();
    drag.connect_drag_begin(move |_, x, y| {
        if enabled.is_active() {
            pic.grab_focus();
            *begin.borrow_mut() = Some((x, y, Cancellation::new(generation.clone())));
        }
    });
    let enabled = takeover.clone();
    let state = current.clone();
    let pic = picture.clone();
    let sender = action_sender.clone();
    drag.connect_drag_end(move |_, dx, dy| {
        let Some((x, y, cancel)) = start.borrow_mut().take() else {
            return;
        };
        if !enabled.is_active() || cancel.check().is_err() {
            return;
        }
        if let Some(o) = state.borrow().as_ref()
            && let Some(from) = picture_point(&pic, o, x, y)
            && let Some(to) = picture_point(&pic, o, x + dx, y + dy)
        {
            let action = if dx.hypot(dy) < 4.0 {
                Action::Click {
                    at: from,
                    button: Button::Left,
                }
            } else {
                Action::Drag { from, to }
            };
            let _ = sender.try_send((o.clone(), action, cancel));
        }
    });
    picture.add_controller(drag);
    let position = Rc::new(Cell::new((0.0, 0.0)));
    let motion = gtk::EventControllerMotion::new();
    let pos = position.clone();
    motion.connect_motion(move |_, x, y| pos.set((x, y)));
    picture.add_controller(motion);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let enabled = takeover.clone();
    let state = current.clone();
    let pic = picture.clone();
    let sender = action_sender.clone();
    let generation = manual_generation.clone();
    scroll.connect_scroll(move |_, dx, dy| {
        if !enabled.is_active() {
            return glib::Propagation::Proceed;
        }
        let (x, y) = position.get();
        if let Some(o) = state.borrow().as_ref()
            && let Some(at) = picture_point(&pic, o, x, y)
        {
            let _ = sender.try_send((
                o.clone(),
                Action::Scroll {
                    at,
                    dx: (dx * 40.0).clamp(-1000.0, 1000.0),
                    dy: (dy * 40.0).clamp(-1000.0, 1000.0),
                },
                Cancellation::new(generation.clone()),
            ));
        }
        glib::Propagation::Stop
    });
    picture.add_controller(scroll);
    let key = gtk::EventControllerKey::new();
    let enable = takeover.clone();
    let generation = manual_generation.clone();
    let key_target = current.clone();
    key.connect_key_pressed(move |_, key, _, state| {
        if !enable.is_active() {
            return glib::Propagation::Proceed;
        }
        let mut modifiers = vec![];
        for (flag, m) in [
            (gdk::ModifierType::CONTROL_MASK, Modifier::Ctrl),
            (gdk::ModifierType::ALT_MASK, Modifier::Alt),
            (gdk::ModifierType::SHIFT_MASK, Modifier::Shift),
            (gdk::ModifierType::SUPER_MASK, Modifier::Super),
        ] {
            if state.contains(flag) {
                modifiers.push(m);
            }
        }
        let name = key
            .to_unicode()
            .filter(|c| !c.is_control())
            .map(|c| c.to_string())
            .or_else(|| key.name().map(|s| s.to_string()));
        if let Some(key) = name
            && let Some(target) = key_target.borrow().as_ref()
        {
            let _ = action_sender.try_send((
                target.clone(),
                Action::Key { key, modifiers },
                Cancellation::new(generation.clone()),
            ));
        }
        glib::Propagation::Stop
    });
    picture.add_controller(key);
    window.present();
}
fn picture_point(picture: &gtk::Picture, o: &Target, x: f64, y: f64) -> Option<Point> {
    let w = f64::from(o.width);
    let h = f64::from(o.height);
    let scale = (f64::from(picture.width()) / w).min(f64::from(picture.height()) / h);
    let left = (f64::from(picture.width()) - w * scale) / 2.0;
    let top = (f64::from(picture.height()) - h * scale) / 2.0;
    let p = Point {
        x: (x - left) / scale,
        y: (y - top) / scale,
    };
    (scale > 0.0 && p.x >= 0.0 && p.y >= 0.0 && p.x < w && p.y < h).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn pump(duration: Duration) {
        let loop_ = glib::MainLoop::new(None, false);
        let quit = loop_.clone();
        glib::timeout_add_local_once(duration, move || quit.quit());
        loop_.run();
    }

    #[test]
    fn authorization_waits_for_input_cleanup() {
        struct PendingInput;
        impl Backend for PendingInput {
            fn capabilities(&self) -> Vec<String> {
                vec![]
            }
            fn targets(&mut self) -> Result<Vec<Target>> {
                Ok(vec![])
            }
            fn observe(&mut self, _: Option<&str>, _: &Cancellation) -> Result<Observation> {
                unreachable!()
            }
            fn act(&mut self, _: &Observation, _: &Action, _: &Cancellation) -> Result<()> {
                unreachable!()
            }
            fn alive(&mut self) -> bool {
                true
            }
        }
        let handle: BackendHandle = Arc::new(Mutex::new(Box::new(PendingInput)));
        let input_cleanup = handle.lock().unwrap();
        let handles = vec![handle.clone()];
        let (ready, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || ready.send(wait_for_input(handles)).unwrap());
        assert!(
            receiver.recv_timeout(Duration::from_millis(50)).is_err(),
            "输入释放完成之前，授权按钮必须保持隐藏"
        );
        drop(input_cleanup);
        receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "只在专用测试 Wayland 桌面中运行 GTK 可见性验收"]
    fn visible_window_allows_ai_and_closing_pauses() {
        assert_eq!(std::env::var("COMPUTER_USE_UI_TEST").as_deref(), Ok("1"));
        assert!(
            std::env::var("WAYLAND_DISPLAY")
                .unwrap()
                .starts_with("/tmp/cv-"),
            "请使用 tests/run-visual.sh 的专用测试桌面"
        );
        gtk::init().unwrap();
        // An unsupported import must release its fd lease and leave the display
        // usable for the following successful DMA-BUF / SHM preview.
        let rejected = Arc::new(DmaImage {
            width: 64,
            height: 64,
            fourcc: 0xdeadbeef,
            modifier: 0,
            planes: vec![crate::backend::frame::DmaPlane {
                fd: tempfile::tempfile().unwrap().into(),
                offset: 0,
                stride: 256,
            }],
        });
        let released = Arc::downgrade(&rejected);
        assert!(preview_texture(Image::Dma(rejected)).is_err());
        assert!(released.upgrade().is_none(), "失败导入必须释放帧租约");
        let app = gtk::Application::builder()
            .application_id("io.github.computer_use_linux.UiTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let mut isolated = Isolated::launch(Application::TextEditor).unwrap();
        let saved = isolated.saved.clone();
        struct Cleanup(crate::backend::sway::SavedSession);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for p in [&self.0.app, &self.0.sway, &self.0.bus] {
                    p.terminate();
                }
            }
        }
        let _cleanup = Cleanup(saved);
        let o = isolated.observe(None, &uncancelled()).unwrap();
        isolated
            .act(
                &o,
                &Action::Text {
                    text: "你好，这个应用始终可见。\nAI 在独立会话中操作；你可以继续使用其他窗口。"
                        .into(),
                },
                &uncancelled(),
            )
            .unwrap();
        let policy = Arc::new(Mutex::new(Policy::default()));
        let owner = uuid::Uuid::new_v4();
        let id = {
            let mut p = policy.lock().unwrap();
            p.register(owner);
            let status = p
                .request(
                    owner,
                    SessionRequest {
                        scope: Scope::Application,
                        mode: Mode::Isolated,
                        application: Some("gnome-text-editor".into()),
                    },
                )
                .unwrap();
            p.grant(
                &status.session_id,
                "GNOME Text Editor".into(),
                Box::new(isolated),
            )
            .unwrap();
            p.ui_visible = false;
            status.session_id
        };
        let backend = policy.lock().unwrap().sessions[&id]
            .backend
            .clone()
            .unwrap();
        preview(&app, backend.clone(), policy.clone(), Some(id.clone()));
        let window = app.windows().into_iter().next().unwrap();
        policy.lock().unwrap().resume(&id).unwrap();
        pump(Duration::from_secs(2));
        assert!(window.is_visible() && window.is_mapped());
        let picture = window
            .child()
            .unwrap()
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Picture>()
            .unwrap();
        let paintable = picture.paintable().unwrap();
        let held_backend = backend.clone();
        let (held, acquired) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = held_backend.lock().unwrap();
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(2));
        });
        acquired.recv().unwrap();
        pump(Duration::from_secs(1));
        assert_ne!(
            picture.paintable().unwrap(),
            paintable,
            "输入占用后端时仍应刷新可见画面"
        );
        worker.join().unwrap();
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Active
        );
        // The passive view refreshes concurrently without consuming AI authority.
        let permit = policy.lock().unwrap().permit(owner, &id, None).unwrap();
        let o = permit
            .backend
            .lock()
            .unwrap()
            .observe(None, &permit.cancel)
            .unwrap();
        policy
            .lock()
            .unwrap()
            .remember(owner, &id, o.clone(), &permit.cancel)
            .unwrap();
        pump(Duration::from_millis(500));
        let request = ActRequest {
            session_id: id.clone(),
            observation_id: o.observation_id.clone(),
            target: o.target.id.clone(),
            action: Action::Text {
                text: "\n实时窗口打开时，AI 仍可继续输入。".into(),
            },
        };
        let permit = policy
            .lock()
            .unwrap()
            .action_permit(owner, &request)
            .unwrap();
        permit
            .backend
            .lock()
            .unwrap()
            .act(&o, &request.action, &permit.cancel)
            .unwrap();
        pump(Duration::from_secs(1));
        if let Ok(path) = std::env::var("COMPUTER_USE_UI_PNG") {
            let mut desktop = crate::backend::wayland::Wayland::connect(
                &crate::backend::wayland::Wayland::host_path().unwrap(),
                &uncancelled(),
            )
            .unwrap();
            let output = desktop.outputs().into_iter().next().unwrap();
            let (png, _, _) = desktop.capture(&output, &uncancelled()).unwrap();
            std::fs::write(
                path,
                base64::engine::general_purpose::STANDARD
                    .decode(png)
                    .unwrap(),
            )
            .unwrap();
        }
        window.set_visible(false);
        pump(Duration::from_millis(100));
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Paused
        );
        window.present();
        pump(Duration::from_millis(100));
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Paused,
            "重新显示不能擅自恢复 AI"
        );
        window.close();
        pump(Duration::from_millis(100));
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Paused
        );
        check_single_allow_flow(&app);
    }
    #[test]
    #[ignore = "需要 tests/run-visual.sh 的独立图形桌面"]
    fn realtime_preview_native_pixels_and_presentation_rate() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        use crate::backend::{
            applications::DesktopApplication,
            sway::{SavedSession, sway_request},
        };
        use std::path::PathBuf;
        let runtime = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap());
        assert!(runtime.to_string_lossy().starts_with("/tmp/cv-"));
        assert_eq!(
            PathBuf::from(std::env::var("HOME").unwrap()),
            runtime.join("home")
        );
        let socket = std::fs::read_dir(&runtime)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy().starts_with("sway-ipc.")
                        && name.to_string_lossy().ends_with(".sock")
                })
            })
            .unwrap();
        gtk::init().unwrap();
        let app = gtk::Application::builder()
            .application_id("io.github.computer_use_linux.PreviewTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let isolated = Isolated::launch(Application::Installed(DesktopApplication {
            id: "preview-probe.desktop".into(),
            name: "Preview probe".into(),
            program: PathBuf::from(std::env::var("COMPUTER_USE_PREVIEW_PROBE").unwrap()),
            args: vec![],
            directory: None,
            desktop_file: runtime.join("probe.desktop"),
        }))
        .unwrap();
        struct Cleanup(SavedSession);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for p in [&self.0.app, &self.0.sway, &self.0.bus] {
                    p.terminate();
                }
            }
        }
        let _cleanup = Cleanup(isolated.saved.clone());
        println!("PREVIEW_RENDERER {}", isolated.saved.renderer);
        let backend: BackendHandle = Arc::new(Mutex::new(Box::new(isolated)));
        preview(&app, backend, Arc::new(Mutex::new(Policy::default())), None);
        let window = app.windows().into_iter().next().unwrap();
        let picture = window
            .child()
            .unwrap()
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Picture>()
            .unwrap();
        let frames = Rc::new(Cell::new(0u32));
        let displayed = frames.clone();
        picture.connect_paintable_notify(move |_| displayed.set(displayed.get() + 1));
        for (mode, scale) in [("1920x1080", 1.0), ("2400x1350", 1.25), ("3840x2160", 2.0)] {
            sway_request(
                &socket,
                0,
                &format!("output HEADLESS-1 mode {mode} scale {scale}"),
            )
            .unwrap();
            pump(Duration::from_secs(2));
            let held = picture
                .paintable()
                .unwrap()
                .downcast::<gdk::Texture>()
                .unwrap();
            let actual_scale = window.surface().unwrap().scale();
            println!("TEXTURE {}", held.type_().name());
            if std::env::var("COMPUTER_USE_EXPECT_DMA").as_deref() == Ok("1") {
                assert!(
                    held.is::<gdk::DmabufTexture>(),
                    "GPU 验收必须实际使用 DMA-BUF"
                );
            }
            assert_eq!(actual_scale, scale);
            assert_eq!(
                held.width(),
                (f64::from(picture.width()) * scale).round() as i32
            );
            assert_eq!(
                held.height(),
                (f64::from(picture.height()) * scale).round() as i32
            );
            let stride = held.width() as usize * 4;
            let mut snapshot = vec![0; stride * held.height() as usize];
            held.download(&mut snapshot, stride);
            let red = 40 * stride + 40 * 4;
            assert_eq!(
                &snapshot[red..red + 4],
                &[0, 0, 255, 255],
                "原生画面颜色或坐标错误"
            );
            frames.set(0);
            let started = Instant::now();
            pump(Duration::from_secs(4));
            let fps = f64::from(frames.get()) / started.elapsed().as_secs_f64();
            println!(
                "PRESENT {}x{} scale={scale}: {fps:.1} FPS",
                held.width(),
                held.height()
            );
            let minimum = std::env::var("COMPUTER_USE_MIN_FPS")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0);
            assert!(fps >= minimum, "GTK 实际呈现帧率不足: {fps:.1} < {minimum}");
            let mut after = vec![0; snapshot.len()];
            held.download(&mut after, stride);
            assert_eq!(snapshot, after, "GTK 仍持有的共享帧被覆盖");
            let target = Target {
                id: "test".into(),
                label: "test".into(),
                width: held.width() as u32,
                height: held.height() as u32,
                scale,
            };
            let point = picture_point(
                &picture,
                &target,
                f64::from(picture.width()) / 2.0,
                f64::from(picture.height()) / 2.0,
            )
            .unwrap();
            assert!((point.x - f64::from(held.width()) / 2.0).abs() < 0.01);
            assert!((point.y - f64::from(held.height()) / 2.0).abs() < 0.01);
        }
        window.set_visible(false);
        pump(Duration::from_millis(200));
        frames.set(0);
        pump(Duration::from_millis(300));
        assert_eq!(frames.get(), 0, "隐藏窗口后不能继续显示帧");
        window.present();
        pump(Duration::from_secs(1));
        assert!(frames.get() > 10, "恢复显示应重新启动采集");
        let retained = picture
            .paintable()
            .unwrap()
            .downcast::<gdk::Texture>()
            .unwrap();
        let stride = retained.width() as usize * 4;
        let mut before = vec![0; stride * retained.height() as usize];
        retained.download(&mut before, stride);
        window.close();
        pump(Duration::from_millis(500));
        let mut after = vec![0; before.len()];
        retained.download(&mut after, stride);
        assert_eq!(before, after, "采集池释放后 GTK 持有的帧仍须有效");
    }

    fn check_single_allow_flow(app: &gtk::Application) {
        let policy = Arc::new(Mutex::new(Policy::default()));
        let owner = uuid::Uuid::new_v4();
        let id = {
            let mut p = policy.lock().unwrap();
            p.register(owner);
            p.request(
                owner,
                SessionRequest {
                    scope: Scope::Application,
                    mode: Mode::Isolated,
                    application: Some("gnome-text-editor".into()),
                },
            )
            .unwrap()
            .session_id
        };
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("授权流程测试")
            .build();
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 8);
        window.set_child(Some(&rows));
        let (sender, receiver) = mpsc::channel();
        let ui = Rc::new(Ui {
            app: app.clone(),
            window: window.clone(),
            rows,
            banner: label(""),
            policy: policy.clone(),
            sender,
            prepared: RefCell::new(HashMap::new()),
            busy: RefCell::new(HashSet::new()),
            detached: RefCell::new(vec![]),
            signature: RefCell::new(String::new()),
        });
        ui.render();
        window.present();
        pump(Duration::from_millis(200));
        fn visit(widget: &gtk::Widget, buttons: &mut Vec<gtk::Button>) {
            assert!(
                !widget.is::<gtk::DropDown>(),
                "独立应用授权不能再要求用户选择应用"
            );
            if let Some(b) = widget.downcast_ref::<gtk::Button>() {
                buttons.push(b.clone());
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                visit(&w, buttons);
                child = w.next_sibling();
            }
        }
        let mut buttons = vec![];
        visit(ui.rows.upcast_ref(), &mut buttons);
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Pending
        );
        assert!(policy.lock().unwrap().sessions[&id].backend.is_none());
        let allow = buttons
            .iter()
            .find(|b| b.label().as_deref() == Some("允许并启动"))
            .unwrap();
        // Local GTK action on the explicitly isolated test desktop; no MCP approval API.
        allow.emit_clicked();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            pump(Duration::from_millis(50));
            if let Ok(Event::AllowedIsolated(id, epoch, result)) = receiver.try_recv() {
                ui.finish_allowed_isolated(&id, epoch, result.unwrap());
                break;
            }
            assert!(Instant::now() < deadline, "允许后应启动独立应用");
        }
        pump(Duration::from_secs(1));
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Active,
            "一次允许应启动可见应用并恢复控制，不再要求二次确认"
        );
        assert!(!window.is_visible());
        assert!(app.windows().iter().any(|w| w.is_mapped()));
        policy.lock().unwrap().close_local(&id);
        for backend in Isolated::recover() {
            backend.saved.app.terminate();
            backend.saved.sway.terminate();
            backend.saved.bus.terminate();
        }
        for window in app.windows() {
            window.close();
        }
    }
}
