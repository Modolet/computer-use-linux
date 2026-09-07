//! @file sway.rs
//! @brief 独立 Sway 会话及窗口归属校验
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::{
    Backend, Cancellation,
    process::{ProcessIdentity, descendant},
    wayland::Wayland,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Application {
    Firefox,
    TextEditor,
}
impl Application {
    pub fn label(self) -> &'static str {
        match self {
            Self::Firefox => "Firefox",
            Self::TextEditor => "GNOME Text Editor",
        }
    }
    pub fn executable(self) -> &'static str {
        match self {
            Self::Firefox => "firefox",
            Self::TextEditor => "gnome-text-editor",
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
    pub bus_pid: u32,
    pub socket: PathBuf,
    pub wayland: PathBuf,
}

pub struct Isolated {
    pub saved: SavedSession,
    target_id: String,
    geometry: Option<String>,
    input: Wayland,
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

fn command(program: &str, runtime: &Path, bus: &str) -> Command {
    let mut command = Command::new(program);
    for name in [
        "DISPLAY",
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
        .env("QT_QPA_PLATFORM", "wayland")
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
    if node.get("pid").and_then(Value::as_u64).is_some()
        && node.get("app_id").is_some_and(|id| !id.is_null())
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
        fs::write(&bus_config,format!("<busconfig><type>session</type><listen>{bus}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow user=\"{}\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",nix::unistd::getuid())).map_err(|e| Fault::unavailable(e.to_string()))?;
        let bus_output = Command::new("dbus-daemon")
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
        let config = runtime.join("sway.config");
        fs::write(&config, "output HEADLESS-1 mode 1280x800\nseat seat0 fallback true\nfocus_on_window_activation none\nfocus_follows_mouse no\nfont monospace 10\nxwayland disable\ndefault_border none\n").map_err(|e| Fault::unavailable(e.to_string()))?;
        let log = fs::File::create(runtime.join("sway.log"))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let mut sway = command("sway", &runtime, &bus)
            .args(["--config"])
            .arg(&config)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| Fault::unavailable(format!("启动 Sway: {e}")))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        let (wayland, socket) = loop {
            if sway
                .try_wait()
                .map_err(|e| Fault::unavailable(e.to_string()))?
                .is_some()
            {
                return Err(Fault::unavailable(format!(
                    "Sway 已退出；查看 {}",
                    runtime.join("sway.log").display()
                )));
            }
            let path = fs::read_dir(&runtime)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| {
                    p.file_name().is_some_and(|n| {
                        n.to_string_lossy().starts_with("wayland-")
                            && !n.to_string_lossy().ends_with(".lock")
                    })
                });
            let socket = fs::read_dir(&runtime)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| {
                    p.file_name().is_some_and(|n| {
                        n.to_string_lossy().starts_with("sway-ipc.")
                            && n.to_string_lossy().ends_with(".sock")
                    })
                });
            if let (Some(path), Some(socket)) = (path, socket) {
                break (path, socket);
            }
            if Instant::now() > deadline {
                let _ = sway.kill();
                return Err(Fault::unavailable("独立 Sway 启动超时"));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let sway_identity = ProcessIdentity::read(sway.id())?;
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
        let profile_root = state_dir()?.join("profiles").join(&id);
        for (variable, folder) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let path = profile_root.join(folder);
            fs::create_dir_all(&path).map_err(|e| Fault::unavailable(e.to_string()))?;
            cmd.env(variable, path);
        }
        cmd.env("WAYLAND_DISPLAY", &wayland)
            .env("MOZ_ENABLE_WAYLAND", "1")
            .stdout(Stdio::null())
            .stderr(app_log);
        if matches!(application, Application::Firefox) {
            let profile = profile_root.join("firefox");
            fs::create_dir_all(&profile).map_err(|e| Fault::unavailable(e.to_string()))?;
            fs::write(profile.join("user.js"),"user_pref(\"browser.shell.checkDefaultBrowser\", false);\nuser_pref(\"browser.aboutwelcome.enabled\", false);\nuser_pref(\"browser.startup.homepage_override.mstone\", \"ignore\");\n").map_err(|e|Fault::unavailable(e.to_string()))?;
            cmd.args(["--no-remote", "--new-instance", "--profile"])
                .arg(profile)
                .arg("about:blank");
        } else {
            cmd.arg("--standalone");
        }
        let mut app = cmd
            .spawn()
            .map_err(|e| Fault::unavailable(format!("启动应用: {e}")))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let identity = loop {
            if app
                .try_wait()
                .map_err(|e| Fault::unavailable(e.to_string()))?
                .is_some()
            {
                return Err(Fault::unavailable(format!(
                    "应用启动失败；查看 {}",
                    runtime.join("application.log").display()
                )));
            }
            let tree = sway_request(&socket, 4, "")?;
            let mut list = vec![];
            windows(&tree, &mut list);
            if let Some(pid) = list
                .iter()
                .filter_map(|w| w["pid"].as_u64())
                .find(|pid| descendant(*pid as u32, app.id()))
            {
                break ProcessIdentity::read(pid as u32)?;
            }
            if Instant::now() > deadline {
                return Err(Fault::unavailable("应用未创建可验证窗口；保留进程以便检查"));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        std::thread::spawn(move || {
            let _ = app.wait();
        });
        let saved = SavedSession {
            id: id.clone(),
            application,
            runtime,
            sway: sway_identity,
            app: identity,
            bus_pid,
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
        };
        result.check_windows()?;
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
                Some(Self {
                    saved,
                    target_id: Uuid::new_v4().to_string(),
                    geometry: None,
                    input,
                })
            })
            .collect()
    }
    fn check_windows(&self) -> Result<String> {
        if !self.saved.sway.alive() || !self.saved.app.alive() {
            return Err(Fault::stale("独立应用或合成器已退出"));
        }
        let tree = sway_request(&self.saved.socket, 4, "")?;
        let mut list = vec![];
        windows(&tree, &mut list);
        if list.is_empty() {
            return Err(Fault::stale("应用没有窗口"));
        }
        let mut signature = vec![];
        for window in list {
            let pid = window["pid"].as_u64().unwrap() as u32;
            let identity = ProcessIdentity::read(pid)?;
            if !descendant(pid, self.saved.app.pid)
                || identity.executable != self.saved.app.executable
            {
                return Err(Fault::denied(
                    "独立会话出现未授权应用窗口；暂停观察和输入，请在本地接管处理",
                ));
            }
            signature.push(serde_json::json!([
                window["id"],
                window["rect"],
                window["focused"],
                identity
            ]));
        }
        Ok(serde_json::to_string(&signature).unwrap())
    }
    fn cancel() -> Cancellation {
        Cancellation::new(Arc::new(AtomicU64::new(0)))
    }
}

impl Backend for Isolated {
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
            width: output.width,
            height: output.height,
            scale: f64::from(output.scale),
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
        self.geometry = Some(after);
        Ok(Observation {
            observation_id: Uuid::new_v4().to_string(),
            target: Target {
                id: self.target_id.clone(),
                label: self.saved.application.label().into(),
                width,
                height,
                scale: f64::from(output.scale),
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
        if observation.target.id != self.target_id
            || self.geometry.as_ref() != Some(&self.check_windows()?)
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
        if (output.width, output.height) != (observation.target.width, observation.target.height) {
            return Err(Fault::stale("虚拟显示器尺寸变化"));
        }
        self.input.input(
            &output,
            (observation.target.width, observation.target.height),
            action,
            cancel,
        )?;
        self.geometry = None;
        Ok(())
    }
    fn alive(&mut self) -> bool {
        self.saved.sway.alive() && self.saved.app.alive()
    }
}
