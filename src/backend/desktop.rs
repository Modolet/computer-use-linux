//! @file desktop.rs
//! @brief 经整机授权的 niri 桌面控制
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::{Backend, Cancellation, process::ProcessIdentity, wayland::Wayland};
use crate::model::*;
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use uuid::Uuid;

pub fn niri_request(
    path: &std::path::Path,
    request: niri_ipc::Request,
) -> Result<niri_ipc::Response> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| Fault::unavailable(format!("niri IPC: {e}")))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    serde_json::to_writer(&mut stream, &request).map_err(|e| Fault::unavailable(e.to_string()))?;
    stream
        .write_all(b"\n")
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    let mut reader = BufReader::new(stream);
    let mut data = Vec::new();
    std::io::Read::take(&mut reader, 8 * 1024 * 1024)
        .read_until(b'\n', &mut data)
        .map_err(|e| Fault::unavailable(e.to_string()))?;
    let reply: niri_ipc::Reply =
        serde_json::from_slice(&data).map_err(|e| Fault::unavailable(e.to_string()))?;
    reply.map_err(Fault::unavailable)
}

pub struct Desktop {
    wire: Wayland,
    output_globals: HashMap<String, u32>,
    output_revision: Option<u64>,
    niri: PathBuf,
    compositor: ProcessIdentity,
    output_ids: HashMap<String, String>,
    windows: HashMap<String, u64>,
    geometry: Option<Value>,
    capabilities: Vec<String>,
}
impl Desktop {
    pub fn connect() -> Result<Self> {
        let niri = PathBuf::from(
            std::env::var_os("NIRI_SOCKET")
                .ok_or_else(|| Fault::unavailable("缺少 NIRI_SOCKET"))?,
        );
        let stream = UnixStream::connect(&niri).map_err(|e| Fault::unavailable(e.to_string()))?;
        let peer =
            nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(|e| Fault::unavailable(e.to_string()))?;
        let compositor = ProcessIdentity::read(peer.pid() as u32)?;
        let wayland = Wayland::host_path()?;
        let cancel = Cancellation::new(Arc::new(AtomicU64::new(0)));
        let wire = Wayland::connect(&wayland, &cancel)?;
        if !wire.capture_supported() {
            return Err(Fault::unsupported("当前 niri 没有开放截图协议"));
        }
        let mut capabilities = vec!["screenshot".into(), "focus_window".into()];
        if wire.input_supported() {
            capabilities.extend(
                ["click", "drag", "scroll", "key", "text"]
                    .into_iter()
                    .map(String::from),
            );
        }
        let output_globals = wire
            .outputs()
            .into_iter()
            .map(|o| (o.name, o.global))
            .collect();
        let output_ids = wire
            .outputs()
            .into_iter()
            .map(|o| (o.name, Uuid::new_v4().to_string()))
            .collect();
        Ok(Self {
            wire,
            output_globals,
            output_revision: None,
            niri,
            compositor,
            output_ids,
            windows: HashMap::new(),
            geometry: None,
            capabilities,
        })
    }
    fn refresh_outputs(&mut self, cancel: &Cancellation) -> Result<()> {
        self.wire.refresh(cancel)?;
        let outputs = self.wire.outputs();
        self.output_ids
            .retain(|name, _| outputs.iter().any(|o| &o.name == name));
        for output in outputs {
            if self.output_globals.get(&output.name) != Some(&output.global) {
                self.output_ids
                    .insert(output.name.clone(), Uuid::new_v4().to_string());
            }
            self.output_globals.insert(output.name, output.global);
        }
        Ok(())
    }
    fn topology(&self) -> Result<Value> {
        let outputs = niri_request(&self.niri, niri_ipc::Request::Outputs)?;
        let niri_ipc::Response::Windows(mut windows) =
            niri_request(&self.niri, niri_ipc::Request::Windows)?
        else {
            return Err(Fault::unavailable("无法检查窗口布局"));
        };
        windows.sort_by_key(|w| w.id);
        let geometry: Vec<_> = windows
            .into_iter()
            .map(|w| serde_json::json!([w.id, w.pid, w.layout, w.is_focused]))
            .collect();
        Ok(serde_json::json!({"outputs": outputs, "windows": geometry}))
    }
    fn list_windows(&mut self) -> Result<Vec<Target>> {
        let niri_ipc::Response::Windows(windows) =
            niri_request(&self.niri, niri_ipc::Request::Windows)?
        else {
            return Err(Fault::unavailable("niri 未返回窗口列表"));
        };
        let mut result = vec![];
        let mut next = HashMap::new();
        for window in windows {
            let id = self
                .windows
                .iter()
                .find(|(_, value)| **value == window.id)
                .map(|(key, _)| key.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            next.insert(id.clone(), window.id);
            result.push(Target {
                id,
                label: format!(
                    "{} — {}",
                    window.app_id.unwrap_or_default(),
                    window.title.unwrap_or_default()
                ),
                width: 0,
                height: 0,
                scale: 1.0,
            });
        }
        self.windows = next;
        Ok(result)
    }
}
impl Backend for Desktop {
    fn capabilities(&self) -> Vec<String> {
        self.capabilities.clone()
    }
    fn targets(&mut self) -> Result<Vec<Target>> {
        let cancel = Cancellation::new(Arc::new(AtomicU64::new(0)));
        self.refresh_outputs(&cancel)?;
        let topology = self.topology()?;
        Ok(self
            .wire
            .outputs()
            .into_iter()
            .map(|o| {
                let id = self
                    .output_ids
                    .entry(o.name.clone())
                    .or_insert_with(|| Uuid::new_v4().to_string())
                    .clone();
                let scale = topology["outputs"]["Outputs"][&o.name]["logical"]["scale"]
                    .as_f64()
                    .unwrap_or(f64::from(o.scale));
                Target {
                    id,
                    width: o.image_size().0,
                    height: o.image_size().1,
                    label: o.name,
                    scale,
                }
            })
            .collect())
    }
    fn observe(&mut self, target: Option<&str>, cancel: &Cancellation) -> Result<Observation> {
        if !self.alive() {
            return Err(Fault::stale("niri 会话已经退出"));
        }
        self.refresh_outputs(cancel)?;
        let revision = self.wire.revision();
        let name = match target {
            Some(id) => self
                .output_ids
                .iter()
                .find(|(_, value)| value.as_str() == id)
                .map(|(name, _)| name.clone())
                .ok_or_else(|| Fault::denied("未授权的显示器引用"))?,
            None => {
                let mut names: Vec<_> = self.output_ids.keys().cloned().collect();
                names.sort();
                names
                    .into_iter()
                    .next()
                    .ok_or_else(|| Fault::unavailable("没有显示器"))?
            }
        };
        let before = self.topology()?;
        let output = self.wire.output(&name)?;
        let (png, width, height) = self.wire.capture(&output, cancel)?;
        if before != self.topology()? || revision != self.wire.revision() {
            return Err(Fault::stale("显示器布局变化，请重新观察"));
        }
        let windows = self.list_windows()?;
        let scale = before["outputs"]["Outputs"][&name]["logical"]["scale"]
            .as_f64()
            .unwrap_or(f64::from(output.scale));
        self.geometry = Some(before);
        self.output_revision = Some(revision);
        Ok(Observation {
            observation_id: Uuid::new_v4().to_string(),
            target: Target {
                id: self.output_ids[&name].clone(),
                label: name,
                width,
                height,
                scale,
            },
            png_base64: Some(png),
            nodes: vec![],
            windows,
        })
    }
    fn act(
        &mut self,
        observation: &Observation,
        action: &Action,
        cancel: &Cancellation,
    ) -> Result<()> {
        cancel.check()?;
        if !self.alive() {
            return Err(Fault::stale("niri 会话已经退出"));
        }
        self.refresh_outputs(cancel)?;
        if self.geometry.as_ref() != Some(&self.topology()?)
            || self.output_revision != Some(self.wire.revision())
        {
            return Err(Fault::stale("显示器布局变化，请重新观察"));
        }
        if let Action::FocusWindow { window } = action {
            let id = *self
                .windows
                .get(window)
                .ok_or_else(|| Fault::stale("窗口引用已失效"))?;
            self.list_windows()?;
            if self.windows.get(window) != Some(&id) {
                return Err(Fault::stale("窗口已经退出"));
            }
            cancel.check()?;
            niri_request(
                &self.niri,
                niri_ipc::Request::Action(niri_ipc::Action::FocusWindow { id }),
            )?;
        } else {
            let name = self
                .output_ids
                .iter()
                .find(|(_, id)| **id == observation.target.id)
                .map(|(name, _)| name.clone())
                .ok_or_else(|| Fault::stale("显示器引用失效"))?;
            let output = self.wire.output(&name)?;
            self.wire.input(
                &output,
                (observation.target.width, observation.target.height),
                action,
                cancel,
            )?;
        }
        self.geometry = None;
        Ok(())
    }
    fn alive(&mut self) -> bool {
        self.compositor.alive()
    }
}
