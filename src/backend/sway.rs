//! @file sway.rs
//! @brief 独立 Sway 会话及窗口归属校验
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::{
    Backend, Cancellation,
    preview::{self, Viewport},
    process::{ProcessIdentity, descendant},
    wayland::Wayland,
    x11,
};
use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Application {
    Firefox,
    TextEditor,
    Installed(super::applications::DesktopApplication),
}
impl Application {
    pub fn requested(name: &str) -> Result<Self> {
        match super::applications::DesktopApplication::resolve(name) {
            Ok(app) => Ok(Self::Installed(app)),
            Err(_)
                if name == "firefox" && gtk4::glib::find_program_in_path("firefox").is_some() =>
            {
                Ok(Self::Firefox)
            }
            Err(_)
                if name == "gnome-text-editor"
                    && gtk4::glib::find_program_in_path("gnome-text-editor").is_some() =>
            {
                Ok(Self::TextEditor)
            }
            Err(e) => Err(e),
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Firefox => "Firefox",
            Self::TextEditor => "GNOME Text Editor",
            Self::Installed(app) => &app.name,
        }
    }
    pub fn executable(&self) -> &Path {
        match self {
            Self::Firefox => Path::new("firefox"),
            Self::TextEditor => Path::new("gnome-text-editor"),
            Self::Installed(app) => &app.program,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    pub application: Application,
    pub runtime: PathBuf,
    pub sway: ProcessIdentity,
    pub app: ProcessIdentity,
    pub bus: ProcessIdentity,
    pub socket: PathBuf,
    pub wayland: PathBuf,
    #[serde(default = "legacy_renderer")]
    pub renderer: String,
    #[serde(default)]
    pub x11: Option<x11::Endpoint>,
    #[serde(default)]
    pub x11_application: bool,
}

fn legacy_renderer() -> String {
    "旧实例 · 渲染方式未知".into()
}

struct StartupGuard(Vec<ProcessIdentity>);
impl Drop for StartupGuard {
    fn drop(&mut self) {
        for process in self.0.iter().rev() {
            process.terminate();
        }
    }
}

fn reap_session(saved: SavedSession) {
    std::thread::spawn(move || {
        while saved.app.alive() {
            std::thread::sleep(Duration::from_secs(1));
        }
        // Never terminate a still-running application with unsaved work.
        if !saved.app.alive() {
            saved.sway.terminate();
            saved.bus.terminate();
        }
    });
}

pub struct Isolated {
    pub saved: SavedSession,
    target_id: String,
    geometry: Option<(String, u64)>,
    input: Wayland,
    local_only: bool,
}

pub fn state_dir() -> Result<PathBuf> {
    let root = match std::env::var_os("XDG_STATE_HOME") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| Fault::unavailable("缺少用户目录"))?,
        )
        .join(".local/state"),
    }
    .join("computer-use-linux");
    fs::create_dir_all(&root).map_err(|e| Fault::unavailable(e.to_string()))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    Ok(root)
}

fn command(program: impl AsRef<std::ffi::OsStr>, runtime: &Path, bus: &str) -> Command {
    let mut command = Command::new(program);
    for name in [
        "DISPLAY",
        "XAUTHORITY",
        "SESSION_MANAGER",
        "WAYLAND_DISPLAY",
        "WAYLAND_SOCKET",
        "NIRI_SOCKET",
        "SWAYSOCK",
        "DBUS_SESSION_BUS_ADDRESS",
        "DBUS_STARTER_ADDRESS",
        "DBUS_STARTER_BUS_TYPE",
        "XDG_ACTIVATION_TOKEN",
        "DESKTOP_STARTUP_ID",
        "GTK_IM_MODULE",
        "QT_IM_MODULE",
        "XMODIFIERS",
        "AT_SPI_BUS_ADDRESS",
    ] {
        command.env_remove(name);
    }
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .env("XDG_CURRENT_DESKTOP", "sway")
        .env("XDG_SESSION_TYPE", "wayland")
        .env("GTK_USE_PORTAL", "0")
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland;xcb")
        .env("NO_AT_BRIDGE", "1")
        .stdin(Stdio::null())
        .process_group(0);
    command
}

pub fn sway_request(socket: &Path, kind: u32, payload: &str) -> Result<Value> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| Fault::unavailable(format!("Sway IPC: {e}")))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    let mut request = b"i3-ipc".to_vec();
    request.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    request.extend_from_slice(&kind.to_le_bytes());
    request.extend_from_slice(payload.as_bytes());
    stream
        .write_all(&request)
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    let mut header = [0; 14];
    stream
        .read_exact(&mut header)
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    if &header[..6] != b"i3-ipc" || u32::from_le_bytes(header[10..14].try_into().unwrap()) != kind {
        return Err(Fault::unavailable("Sway IPC 响应类型错误"));
    }
    let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    if len > 8 * 1024 * 1024 {
        return Err(Fault::unavailable("Sway IPC 响应过大"));
    }
    let mut bytes = vec![0; len];
    stream
        .read_exact(&mut bytes)
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| Fault::unavailable(e.to_string()))
}

fn windows<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
    if node.get("window").and_then(Value::as_u64).is_some()
        || (node.get("pid").and_then(Value::as_u64).is_some()
            && node.get("app_id").is_some_and(|id| !id.is_null()))
    {
        out.push(node);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(nodes) = node[key].as_array() {
            for child in nodes {
                windows(child, out);
            }
        }
    }
}

impl Isolated {
    pub fn launch(application: Application) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let root = crate::ipc::runtime_dir().map_err(Fault::from)?;
        // Unix-domain socket paths are limited to 107 bytes on Linux; Sway also
        // creates an initial IPC name before reading its configuration.
        let runtime = root.join(format!("s-{}", &id[..12]));
        fs::create_dir(&runtime).map_err(|e| Fault::unavailable(e.to_string()))?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let bus = format!("unix:path={}/bus", runtime.display());
        let bus_config = runtime.join("dbus.conf");
        // Activate settings services (such as dconf) against the same personal
        // data, but keep service activation inside this graphical session.
        fs::write(&bus_config,format!("<busconfig><type>session</type><listen>{bus}</listen><auth>EXTERNAL</auth><standard_session_servicedirs/><policy context=\"default\"><allow user=\"{}\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",nix::unistd::getuid())).map_err(|e| Fault::unavailable(e.to_string()))?;
        let bus_output = command("dbus-daemon", &runtime, &bus)
            .arg(format!("--config-file={}", bus_config.display()))
            .args(["--fork", "--print-pid=1"])
            .output()
            .map_err(|e| Fault::unavailable(format!("启动独立 D-Bus: {e}")))?;
        if !bus_output.status.success() {
            return Err(Fault::unavailable(
                String::from_utf8_lossy(&bus_output.stderr).to_string(),
            ));
        }
        let bus_pid: u32 = String::from_utf8_lossy(&bus_output.stdout)
            .trim()
            .parse()
            .map_err(|_| Fault::unavailable("D-Bus 未返回进程号"))?;
        let bus_identity = ProcessIdentity::read(bus_pid)?;
        let mut startup = StartupGuard(vec![bus_identity.clone()]);
        let config = runtime.join("sway.config");
        let contents = format!(
            "output HEADLESS-1 mode 1280x800\nseat seat0 fallback true\nfocus_on_window_activation none\nfocus_follows_mouse no\nfont monospace 10\nxwayland force\ndefault_border none\n{}",
            x11::REPORT_DISPLAY
        );
        fs::write(&config, contents).map_err(|e| Fault::unavailable(e.to_string()))?;
        let (mut sway, wayland, socket, renderer) = start_compositor(&runtime, &bus, &config)?;
        startup.0.push(ProcessIdentity::read(sway.id())?);
        let sway_identity = ProcessIdentity::read(sway.id())?;
        let x11 = x11::Endpoint::discover(&runtime, &sway_identity)?;
        x11.windows(&[])?;
        let activation = command("dbus-update-activation-environment", &runtime, &bus)
            .arg(format!("WAYLAND_DISPLAY={}", wayland.display()))
            .arg(format!("DISPLAY={}", x11.display))
            .output()
            .map_err(|e| Fault::unavailable(format!("设置图形会话服务环境: {e}")))?;
        if !activation.status.success() {
            return Err(Fault::unavailable("无法设置独立图形会话的服务环境"));
        }
        // Create the keyboard before launching GTK/Firefox. Removing the only
        // keyboard between actions makes clients rebind wl_keyboard and can
        // discard the first key while seat capabilities are being negotiated.
        let mut input = Wayland::connect(&wayland, &Self::cancel())?;
        let output = input
            .outputs()
            .into_iter()
            .next()
            .ok_or_else(|| Fault::unavailable("没有虚拟显示器"))?;
        input.initialize_input(&output, &Self::cancel())?;
        std::thread::spawn(move || {
            let _ = sway.wait();
        });
        let app_log = fs::File::create(runtime.join("application.log"))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let mut cmd = command(application.executable(), &runtime, &bus);
        // Keep the user's HOME and XDG data/config/cache/state environment intact.
        // Only the graphical session and its runtime endpoints are private.
        if let Some(home) = std::env::var_os("HOME") {
            cmd.current_dir(home);
        }
        if let Application::Installed(app) = &application {
            if let Some(directory) = &app.directory {
                cmd.current_dir(directory);
            }
            cmd.args(&app.args);
        }
        cmd.env("WAYLAND_DISPLAY", &wayland)
            .env("DISPLAY", &x11.display)
            .env("MOZ_ENABLE_WAYLAND", "1")
            .stdout(
                app_log
                    .try_clone()
                    .map_err(|e| Fault::unavailable(e.to_string()))?,
            )
            .stderr(app_log);
        if application
            .executable()
            .file_name()
            .is_some_and(|n| n == "firefox" || n == "firefox-esr")
        {
            // Prevent forwarding to the user's other graphical session while using
            // Firefox's normal profile selection and profile lock unchanged.
            cmd.args(["--no-remote", "--new-instance", "about:blank"]);
        } else if application
            .executable()
            .file_name()
            .is_some_and(|n| n == "gnome-text-editor")
        {
            cmd.arg("--standalone");
        }
        let mut app = cmd
            .spawn()
            .map_err(|e| Fault::unavailable(format!("启动应用: {e}")))?;
        startup.0.push(ProcessIdentity::read(app.id())?);
        let deadline = Instant::now() + Duration::from_secs(20);
        let (identity, x11_application) = loop {
            if let Some(status) = app
                .try_wait()
                .map_err(|e| Fault::unavailable(e.to_string()))?
            {
                return Err(Fault::unavailable(format!(
                    "应用启动器已退出（{status}），未创建可验证窗口；可能不兼容当前图形会话或配置被占用。查看 {}；日志为空时，启动器可能屏蔽了子进程输出",
                    runtime.join("application.log").display()
                )));
            }
            let tree = sway_request(&socket, 4, "")?;
            let mut list = vec![];
            windows(&tree, &mut list);
            let managed: Vec<_> = list
                .iter()
                .filter_map(|node| node["window"].as_u64().map(|id| id as u32))
                .collect();
            if let Ok(xwindows) = x11.windows(&managed) {
                let found = list.iter().find_map(|node| {
                    let is_x11 = node["window"].as_u64().is_some();
                    let pid = if let Some(id) = node["window"].as_u64() {
                        xwindows.get(&(id as u32))?.process.pid
                    } else {
                        node["pid"].as_u64()? as u32
                    };
                    descendant(pid, app.id()).then_some((pid, is_x11))
                });
                if let Some((pid, is_x11)) = found {
                    break (ProcessIdentity::read(pid)?, is_x11);
                }
            }
            if Instant::now() > deadline {
                return Err(Fault::unavailable(
                    "应用未创建可验证窗口；若个人配置被其他实例占用，请先关闭该应用再重试",
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        std::thread::spawn(move || {
            let _ = app.wait();
        });
        let saved = SavedSession {
            id: id.clone(),
            application,
            renderer,
            x11: Some(x11),
            x11_application,
            runtime,
            sway: sway_identity,
            app: identity,
            bus: bus_identity,
            socket,
            wayland,
        };
        fs::write(
            state_dir()?.join(format!("{id}.json")),
            serde_json::to_vec(&saved).map_err(|e| Fault::unavailable(e.to_string()))?,
        )
        .map_err(|e| Fault::unavailable(e.to_string()))?;
        let result = Self {
            saved,
            target_id: Uuid::new_v4().to_string(),
            geometry: None,
            input,
            local_only: false,
        };
        result.check_windows()?;
        startup.0.clear();
        reap_session(result.saved.clone());
        Ok(result)
    }
    pub fn recover() -> Vec<Self> {
        let Ok(root) = state_dir() else {
            return vec![];
        };
        fs::read_dir(root)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|e| fs::read(e.path()).ok())
            .filter_map(|s| serde_json::from_slice::<SavedSession>(&s).ok())
            .filter(|s| s.sway.alive() && s.app.alive())
            .filter_map(|saved| {
                let mut input = Wayland::connect(&saved.wayland, &Self::cancel()).ok()?;
                let output = input.outputs().into_iter().next()?;
                input.initialize_input(&output, &Self::cancel()).ok()?;
                reap_session(saved.clone());
                Some(Self {
                    saved,
                    target_id: Uuid::new_v4().to_string(),
                    geometry: None,
                    input,
                    local_only: false,
                })
            })
            .collect()
    }
    fn check_windows(&self) -> Result<String> {
        Self::check_saved_windows(&self.saved, self.local_only)
    }
    fn check_saved_windows(saved: &SavedSession, local_only: bool) -> Result<String> {
        if !saved.sway.alive() || !saved.app.alive() {
            return Err(Fault::stale("独立应用或合成器已退出"));
        }
        let tree = sway_request(&saved.socket, 4, "")?;
        let mut list = vec![];
        windows(&tree, &mut list);
        if list.is_empty() {
            return Err(Fault::stale("应用没有窗口"));
        }
        let managed: Vec<_> = list
            .iter()
            .filter_map(|node| node["window"].as_u64().map(|id| id as u32))
            .collect();
        let xwindows = match &saved.x11 {
            Some(endpoint) => endpoint.windows(&managed)?,
            None if managed.is_empty() => Default::default(),
            None => return Err(Fault::denied("此旧会话没有获准的 X11 端点")),
        };
        let authorized = |identity: &ProcessIdentity| -> Result<()> {
            if !local_only
                && (!descendant(identity.pid, saved.app.pid)
                    || identity.executable != saved.app.executable)
            {
                return Err(Fault::denied(
                    "独立会话出现未授权应用窗口；暂停观察和输入，请在本地接管处理",
                ));
            }
            Ok(())
        };
        // X11 override-redirect menus never enter the Sway managed tree. Their
        // real XRes client identities must pass the same authorization check.
        for window in xwindows.values() {
            // The private compositor owns an off-screen XWM selection window.
            // Only its exact kernel-backed process lifetime is trusted here.
            if window.process != saved.sway {
                authorized(&window.process)?;
            }
        }
        let mut signature = vec![];
        for window in list {
            let identity = if let Some(xid) = window["window"].as_u64() {
                xwindows
                    .get(&(xid as u32))
                    .ok_or_else(|| Fault::stale("X11 窗口已关闭"))?
                    .process
                    .clone()
            } else {
                ProcessIdentity::read(
                    window["pid"]
                        .as_u64()
                        .ok_or_else(|| Fault::denied("窗口没有可验证身份"))?
                        as u32,
                )?
            };
            authorized(&identity)?;
            signature.push(serde_json::json!([
                window["id"],
                window["rect"],
                window["focused"],
                identity
            ]));
        }
        signature.push(serde_json::to_value(&xwindows).unwrap());
        let outputs = sway_request(&saved.socket, 3, "")?;
        let outputs: Vec<_> = outputs
            .as_array()
            .ok_or_else(|| Fault::stale("虚拟输出列表无效"))?
            .iter()
            .map(|o| {
                serde_json::json!([
                    o["name"],
                    o["rect"],
                    o["scale"],
                    o["transform"],
                    o["current_mode"],
                    o["active"]
                ])
            })
            .collect();
        signature.push(serde_json::json!(outputs));
        Ok(serde_json::to_string(&signature).unwrap())
    }
    fn human_input(
        &mut self,
        expected: Option<&Target>,
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()> {
        // A local input connection has no AI observation authority and need not
        // capture/encode the entire screen before every key or click.
        if !self.local_only {
            let mut view = self
                .local_view()?
                .ok_or_else(|| Fault::unavailable("本地视图不可用"))?;
            return match expected {
                Some(target) => view.human_act_at(target, action, cancel),
                None => view.human_act(action, cancel),
            };
        }
        self.input.refresh(cancel)?;
        let output = self
            .input
            .outputs()
            .into_iter()
            .next()
            .ok_or_else(|| Fault::stale("虚拟输出已关闭"))?;
        let (width, height) = output.image_size();
        let target = Target {
            id: self.target_id.clone(),
            label: self.saved.application.label().into(),
            width,
            height,
            scale: self.scale(&output.name)?,
        };
        if expected.is_some_and(|old| {
            old.id != target.id
                || old.width != width
                || old.height != height
                || old.scale != target.scale
        }) {
            return Err(Fault::stale("预览尺寸已变化，请等待新画面后操作"));
        }
        self.geometry = Some((self.check_windows()?, self.input.revision()));
        action.validate(width, height)?;
        self.act(
            &Observation {
                observation_id: String::new(),
                target,
                png_base64: None,
                nodes: vec![],
                windows: vec![],
            },
            action,
            cancel,
        )
    }
    fn cancel() -> Cancellation {
        Cancellation::new(Arc::new(AtomicU64::new(0)))
    }
    fn scale(&self, output_name: &str) -> Result<f64> {
        let outputs = sway_request(&self.saved.socket, 3, "")?;
        outputs
            .as_array()
            .and_then(|outputs| {
                outputs
                    .iter()
                    .find(|o| o["name"].as_str() == Some(output_name))
            })
            .and_then(|o| o["scale"].as_f64())
            .filter(|s| s.is_finite() && *s > 0.0)
            .ok_or_else(|| Fault::stale("虚拟显示器缩放信息无效"))
    }
}

impl Backend for Isolated {
    fn local_view(&self) -> Result<Option<Box<dyn Backend>>> {
        Ok(Some(Box::new(Self {
            saved: self.saved.clone(),
            target_id: self.target_id.clone(),
            geometry: None,
            input: Wayland::connect(&self.saved.wayland, &Self::cancel())?,
            local_only: true,
        })))
    }
    fn realtime_preview(&self) -> Result<Box<dyn preview::Source>> {
        Ok(Box::new(IsolatedPreview {
            saved: self.saved.clone(),
            target_id: self.target_id.clone(),
            viewport: None,
            revision: 0,
            scale: 1.0,
            wire: Wayland::connect(&self.saved.wayland, &Self::cancel())?,
        }))
    }
    fn human_act(&mut self, action: &Action, cancel: &Cancellation) -> Result<()> {
        self.human_input(None, action, cancel)
    }
    fn human_act_at(
        &mut self,
        target: &Target,
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()> {
        self.human_input(Some(target), action, cancel)
    }
    fn survives_revoke(&self) -> bool {
        true
    }
    fn preview(&mut self, cancel: &Cancellation) -> Result<Observation> {
        let saved = self.geometry.clone();
        let result = self.observe(None, cancel);
        self.geometry = saved;
        result
    }
    fn capabilities(&self) -> Vec<String> {
        ["screenshot", "click", "drag", "scroll", "key", "text"]
            .into_iter()
            .map(String::from)
            .collect()
    }
    fn targets(&mut self) -> Result<Vec<Target>> {
        self.check_windows()?;
        let wire = Wayland::connect(&self.saved.wayland, &Self::cancel())?;
        if !wire.input_supported() || !wire.capture_supported() {
            return Err(Fault::unsupported("独立合成器缺少必要协议"));
        }
        let output = wire
            .outputs()
            .into_iter()
            .next()
            .ok_or_else(|| Fault::unavailable("没有虚拟显示器"))?;
        Ok(vec![Target {
            id: self.target_id.clone(),
            label: self.saved.application.label().into(),
            width: output.image_size().0,
            height: output.image_size().1,
            scale: self.scale(&output.name)?,
        }])
    }
    fn observe(&mut self, target: Option<&str>, cancel: &Cancellation) -> Result<Observation> {
        if target.is_some_and(|t| t != self.target_id) {
            return Err(Fault::denied("目标不属于此应用会话"));
        }
        let before = self.check_windows()?;
        self.input.refresh(cancel)?;
        let output = self
            .input
            .outputs()
            .into_iter()
            .next()
            .ok_or_else(|| Fault::unavailable("没有虚拟显示器"))?;
        let (png, width, height) = self.input.capture(&output, cancel)?;
        let after = self.check_windows()?;
        if before != after {
            return Err(Fault::stale("截图期间窗口布局改变，请重新观察"));
        }
        self.geometry = Some((after, self.input.revision()));
        Ok(Observation {
            observation_id: Uuid::new_v4().to_string(),
            target: Target {
                id: self.target_id.clone(),
                label: self.saved.application.label().into(),
                width,
                height,
                scale: self.scale(&output.name)?,
            },
            png_base64: Some(png),
            nodes: vec![],
            windows: vec![],
        })
    }
    fn act(
        &mut self,
        observation: &Observation,
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()> {
        cancel.check()?;
        self.input.refresh(cancel)?;
        if observation.target.id != self.target_id
            || self.geometry.as_ref() != Some(&(self.check_windows()?, self.input.revision()))
        {
            return Err(Fault::stale("窗口身份或布局变化，请重新观察"));
        }
        self.input.refresh(cancel)?;
        let output = self
            .input
            .outputs()
            .into_iter()
            .next()
            .ok_or_else(|| Fault::stale("虚拟显示器消失"))?;
        if output.image_size() != (observation.target.width, observation.target.height) {
            return Err(Fault::stale("虚拟显示器尺寸变化"));
        }
        if self.scale(&output.name)? != observation.target.scale {
            return Err(Fault::stale("虚拟显示器缩放变化"));
        }
        let saved = self.saved.clone();
        let local_only = self.local_only;
        self.input.input_checked(
            &output,
            (observation.target.width, observation.target.height),
            action,
            cancel,
            &mut || Self::check_saved_windows(&saved, local_only).map(|_| ()),
        )?;
        self.geometry = None;
        Ok(())
    }
    fn alive(&mut self) -> bool {
        self.saved.sway.alive() && self.saved.app.alive()
    }
}

struct IsolatedPreview {
    saved: SavedSession,
    target_id: String,
    viewport: Option<Viewport>,
    revision: u64,
    scale: f64,
    wire: Wayland,
}
impl preview::Source for IsolatedPreview {
    fn dma_formats(&mut self, formats: Vec<(u32, u64)>) {
        // Resolve the render node actually opened by our compositor, including
        // multi-GPU systems. Validate its process lifetime before reading fds.
        let device = self
            .saved
            .sway
            .alive()
            .then(|| {
                fs::read_dir(format!("/proc/{}/fd", self.saved.sway.pid))
                    .ok()?
                    .filter_map(|entry| fs::read_link(entry.ok()?.path()).ok())
                    .find(|path| {
                        path.parent() == Some(Path::new("/dev/dri"))
                            && path
                                .file_name()
                                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
                    })
            })
            .flatten();
        self.wire.configure_dma(device.as_deref(), formats);
    }
    fn resize(&mut self, viewport: Viewport, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        Viewport::new(viewport.width, viewport.height, viewport.scale)?;
        if self.viewport == Some(viewport) {
            return Ok(());
        }
        if !self.saved.sway.alive() || !self.saved.app.alive() {
            return Err(Fault::stale("独立应用已退出"));
        }
        let response = sway_request(
            &self.saved.socket,
            0,
            &format!(
                "output HEADLESS-1 mode {}x{}@60Hz scale {:.3}",
                viewport.width,
                viewport.height,
                if self.saved.x11_application {
                    1.0
                } else {
                    viewport.scale
                }
            ),
        )?;
        if !response.as_array().is_some_and(|items| {
            !items.is_empty() && items.iter().all(|item| item["success"] == true)
        }) {
            return Err(Fault::unavailable("无法调整独立输出尺寸"));
        }
        self.wire.refresh(cancel)?;
        self.viewport = Some(viewport);
        Ok(())
    }
    fn frame(&mut self, cancel: &Cancellation) -> Result<preview::Frame> {
        if !self.saved.sway.alive() || !self.saved.app.alive() {
            return Err(Fault::stale("独立应用已退出"));
        }
        self.wire.refresh(cancel)?;
        if self.revision != self.wire.revision() {
            let outputs = sway_request(&self.saved.socket, 3, "")?;
            self.scale = outputs
                .as_array()
                .and_then(|outputs| outputs.iter().find(|o| o["name"] == "HEADLESS-1"))
                .and_then(|output| output["scale"].as_f64())
                .ok_or_else(|| Fault::stale("虚拟输出缩放无效"))?;
            self.revision = self.wire.revision();
        }
        let output = self.wire.output("HEADLESS-1")?;
        let image = self.wire.capture_preview(&output, cancel)?;
        let (width, height) = image.size();
        cancel.check()?;
        if self.wire.revision() != self.revision {
            return Err(Fault::new(ErrorCode::Busy, "输出尺寸变化，等待下一帧"));
        }
        let target = Target {
            id: self.target_id.clone(),
            label: self.saved.application.label().into(),
            width,
            height,
            scale: self.scale,
        };
        Ok(preview::Frame {
            image,
            target,
            renderer: self.saved.renderer.clone(),
            ready_at: Instant::now(),
        })
    }
}

fn start_compositor(
    runtime: &Path,
    bus: &str,
    config: &Path,
) -> Result<(std::process::Child, PathBuf, PathBuf, String)> {
    let requested = std::env::var("COMPUTER_USE_RENDERER").unwrap_or_else(|_| "auto".into());
    if !matches!(requested.as_str(), "auto" | "gles2" | "pixman") {
        return Err(Fault::unavailable(
            "COMPUTER_USE_RENDERER 必须为 auto、gles2 或 pixman",
        ));
    }
    let mut devices: Vec<PathBuf> =
        if let Some(device) = std::env::var_os("COMPUTER_USE_RENDER_DRM_DEVICE") {
            vec![PathBuf::from(device)]
        } else {
            fs::read_dir("/dev/dri")
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
                .map(|entry| entry.path())
                .collect()
        };
    devices.sort();
    let mut candidates: Vec<Option<PathBuf>> = if requested == "pixman" {
        vec![]
    } else {
        devices
            .into_iter()
            .filter(|device| {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(device)
                    .is_ok()
            })
            .map(Some)
            .collect()
    };
    if requested != "gles2" {
        candidates.push(None);
    }
    let mut errors = vec![];
    for device in candidates {
        let renderer = if device.is_some() { "gles2" } else { "pixman" };
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(runtime.join("sway.log"))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let mut cmd = command("sway", runtime, bus);
        cmd.arg("--config")
            .arg(config)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", renderer)
            .env_remove("WLR_RENDERER_FORCE_SOFTWARE")
            .env_remove("WLR_RENDERER_ALLOW_SOFTWARE")
            .env_remove("WLR_RENDER_DRM_DEVICE")
            .stdout(Stdio::null())
            .stderr(log);
        if let Some(device) = &device {
            cmd.env("WLR_RENDER_DRM_DEVICE", device);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| Fault::unavailable(format!("启动 Sway: {e}")))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if child
                .try_wait()
                .map_err(|e| Fault::unavailable(e.to_string()))?
                .is_some()
            {
                break;
            }
            let paths: Vec<_> = fs::read_dir(runtime)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            let wayland = paths.iter().find(|p| {
                p.file_name().is_some_and(|n| {
                    n.to_string_lossy().starts_with("wayland-")
                        && !n.to_string_lossy().ends_with(".lock")
                })
            });
            let socket = paths.iter().find(|p| {
                p.file_name().is_some_and(|n| {
                    n.to_string_lossy().starts_with("sway-ipc.")
                        && n.to_string_lossy().ends_with(".sock")
                })
            });
            if let (Some(wayland), Some(socket)) = (wayland, socket)
                && sway_request(socket, 3, "").is_ok()
            {
                let label = match &device {
                    Some(device) => format!(
                        "GPU · GLES2 ({})",
                        device.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    None => "CPU · Pixman（兼容模式）".into(),
                };
                tracing::info!(%label, "独立桌面渲染器");
                return Ok((child, wayland.clone(), socket.clone(), label));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.wait();
        errors.push(format!("{renderer} {:?}", device));
        // No application has been started. Remove only this failed compositor's
        // sockets, never the private D-Bus endpoint or any user application data.
        for entry in fs::read_dir(runtime).ok().into_iter().flatten().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("wayland-")
                || name.starts_with("sway-ipc.")
                || name == "x11-display"
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        tracing::warn!(%renderer, "渲染器启动失败，尝试下一个本地渲染器；详情见 sway.log");
    }
    Err(Fault::unavailable(format!(
        "无法启动独立渲染器（{}）；请检查 GPU 权限及 {}",
        errors.join(", "),
        runtime.join("sway.log").display()
    )))
}
