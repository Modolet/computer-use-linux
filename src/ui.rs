//! @file ui.rs
//! @brief 本地 GTK 授权窗口、会话管理与独立应用预览
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use crate::{
    backend::{
        Backend, Cancellation,
        accessibility::{self, Candidate},
        desktop::Desktop,
        existing::Existing,
        portal::Portal,
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
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

struct Prepared {
    backend: Box<dyn Backend>,
    label: String,
    preview: Option<String>,
}
enum Event {
    InputQuiescent(u64, Result<()>),
    AllowedIsolated(String, u64, Result<Prepared>),
    Candidates(Result<Vec<Candidate>>),
    Prepared(String, Result<Prepared>),
    Recovered(Vec<Isolated>),
}
struct Ui {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    rows: gtk::Box,
    banner: gtk::Label,
    policy: SharedPolicy,
    runtime: tokio::runtime::Handle,
    sender: mpsc::Sender<Event>,
    candidates: RefCell<Vec<Candidate>>,
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
fn uncancelled() -> Cancellation {
    Cancellation::new(Arc::new(AtomicU64::new(0)))
}
fn capabilities_text(capabilities: &[String]) -> String {
    capabilities
        .iter()
        .map(|c| match c.as_str() {
            "screenshot" => "截图",
            "accessibility_tree" => "读取控件",
            "click" => "点击",
            "drag" => "拖动",
            "scroll" => "滚动",
            "key" => "按键",
            "text" => "文本输入",
            "focus_window" => "切换窗口",
            "set_text" => "编辑控件文本",
            "invoke" => "控件动作",
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
    let rt = runtime.handle().clone();
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
            runtime: rt.clone(),
            sender,
            candidates: RefCell::new(vec![]),
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
        button("刷新应用列表", &controls, move || {
            if let Some(ui) = weak.upgrade() {
                ui.load_candidates();
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
        ui.load_candidates();
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
                                    ui.load_candidates();
                                }
                                Err(e) => tracing::error!("授权界面保持隐藏：{e}"),
                            }
                        }
                    }
                    Event::Candidates(Ok(list)) => *ui.candidates.borrow_mut() = list,
                    Event::Candidates(Err(e)) => ui.banner.set_text(&format!(
                        "已有实例不可用：{}；仍可使用独立实例或整机模式。",
                        e.message
                    )),
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
    fn load_candidates(&self) {
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let _ = sender.send(Event::Candidates(accessibility::candidates()));
        });
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
                    Mode::Existing => "已有应用",
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
                row.append(&label("允许 AI 在可见独立窗口中截图、点击、拖动、滚动、按键和输入文本。使用独立配置，不复制个人登录状态；应用保留文件和网络权限。"));
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
            Mode::Existing => {
                let choices = self.candidates.borrow().clone();
                let names: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
                let dropdown = gtk::DropDown::from_strings(&names);
                row.append(&dropdown);
                row.append(&label(
                    "只列出能核实进程与窗口身份的应用。未验证的后台写操作不会开放。",
                ));
                let weak = Rc::downgrade(self);
                let prepare_id = id.to_string();
                let d = dropdown.clone();
                let list = choices.clone();
                button("选择该窗口的截图权限", row, move || {
                    if let Some(ui) = weak.upgrade()
                        && let Some(candidate) = list.get(d.selected() as usize).cloned()
                    {
                        let sender = ui.sender.clone();
                        let id = prepare_id.clone();
                        ui.busy.borrow_mut().insert(id.clone());
                        ui.invalidate();
                        ui.runtime.spawn(async move {
                            let result = match Portal::select().await {
                                Ok(portal) => tokio::task::spawn_blocking(move || {
                                    let title = candidate.label.clone();
                                    let mut b = Existing::bind(candidate, Some(portal))?;
                                    let preview = b.observe(None, &uncancelled())?.png_base64;
                                    Ok(Prepared {
                                        backend: Box::new(b) as Box<dyn Backend>,
                                        label: title,
                                        preview,
                                    })
                                })
                                .await
                                .unwrap_or_else(|e| Err(Fault::unavailable(e.to_string()))),
                                Err(e) => Err(e),
                            };
                            let _ = sender.send(Event::Prepared(id, result));
                        });
                    }
                });
                let weak = Rc::downgrade(self);
                let id = id.to_string();
                button("准备控件权限（不含截图）", row, move || {
                    if let Some(ui) = weak.upgrade()
                        && let Some(c) = choices.get(dropdown.selected() as usize).cloned()
                    {
                        ui.prepare(id.clone(), move || {
                            let title = c.label.clone();
                            Ok(Prepared {
                                backend: Box::new(Existing::bind(c, None)?),
                                label: title,
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
    if let Some(id) = &session_id {
        policy.lock().unwrap().view_opened(id);
    }
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
    picture.set_can_shrink(true);
    picture.set_focusable(true);
    outer.append(&picture);
    window.set_child(Some(&outer));
    let current = Rc::new(RefCell::new(None::<Observation>));
    let (sender, receiver) = mpsc::channel::<Result<Observation>>();
    let busy = Arc::new(AtomicBool::new(false));
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
    let id = session_id.clone();
    let generation = manual_generation.clone();
    window.connect_close_request(move |_| {
        a.set(false);
        generation.fetch_add(1, Ordering::SeqCst);
        let mut p = close_policy.lock().unwrap();
        if manual_active.get() {
            p.manual_previews = p.manual_previews.saturating_sub(1);
        }
        if let Some(id) = &id {
            p.view_closed(id);
        }
        glib::Propagation::Proceed
    });
    let (action_sender, action_receiver) = mpsc::sync_channel::<(Action, Cancellation)>(128);
    let text_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let entry = gtk::Entry::builder()
        .placeholder_text("接管后可在这里使用中文输入法，再发送到应用")
        .hexpand(true)
        .build();
    text_row.append(&entry);
    let enabled = takeover.clone();
    let text_sender = action_sender.clone();
    let generation = manual_generation.clone();
    button("发送文本", &text_row, move || {
        if enabled.is_active()
            && !entry.text().is_empty()
            && text_sender
                .try_send((
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
    let human_backend = backend.clone();
    std::thread::spawn(move || {
        while let Ok((action, cancel)) = action_receiver.recv() {
            if cancel.check().is_err() {
                continue;
            }
            if let Ok(mut b) = human_backend.lock() {
                let _ = b.human_act(&action, &cancel);
            }
        }
    });
    let pic = picture.clone();
    let state = current.clone();
    let b = backend
        .lock()
        .unwrap()
        .local_view()
        .ok()
        .flatten()
        .map(|view| Arc::new(Mutex::new(view)))
        .unwrap_or_else(|| backend.clone());
    let p = policy.clone();
    let id = session_id;
    glib::timeout_add_local(Duration::from_millis(300), move || {
        if !alive.get() {
            return glib::ControlFlow::Break;
        }
        for result in receiver.try_iter() {
            match result {
                Ok(o) => {
                    if let Some(t) = o.png_base64.as_deref().and_then(texture) {
                        pic.set_paintable(Some(&t));
                    }
                    *state.borrow_mut() = Some(o);
                }
                Err(e) => status.set_text(&e.message),
            }
        }
        if let Some(id) = &id
            && let Some(session) = p.lock().unwrap().sessions.get(id)
        {
            status.set_text(match session.status.state {
                State::Active => "AI 正在控制此应用 · 你可以继续使用其他窗口",
                State::Paused => "AI 已暂停 · 可手动接管，或在权限面板恢复",
                _ => "AI 授权已结束 · 应用保留供你使用",
            });
        }
        if !busy.swap(true, Ordering::SeqCst) {
            let sender = sender.clone();
            let backend = b.clone();
            let busy = busy.clone();
            std::thread::spawn(move || {
                if let Ok(mut b) = backend.try_lock() {
                    let _ = sender.send(b.preview(&uncancelled()));
                }
                busy.store(false, Ordering::SeqCst);
            });
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
            let _ = sender.try_send((action, cancel));
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
        if let Some(key) = name {
            let _ = action_sender.try_send((
                Action::Key { key, modifiers },
                Cancellation::new(generation.clone()),
            ));
        }
        glib::Propagation::Stop
    });
    picture.add_controller(key);
    window.present();
}
fn picture_point(picture: &gtk::Picture, o: &Observation, x: f64, y: f64) -> Option<Point> {
    let w = f64::from(o.target.width);
    let h = f64::from(o.target.height);
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
        let deadline = Instant::now() + duration;
        let context = glib::MainContext::default();
        while Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
        window.close();
        pump(Duration::from_millis(100));
        assert_eq!(
            policy.lock().unwrap().status(owner, &id).unwrap().state,
            State::Paused
        );
        check_single_allow_flow(&app);
    }
    fn check_single_allow_flow(app: &gtk::Application) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
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
            runtime: runtime.handle().clone(),
            sender,
            candidates: RefCell::new(vec![]),
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
