//! @file accessibility.rs
//! @brief AT-SPI 实例身份绑定与有界控件树读取
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use super::{Cancellation, process::ProcessIdentity};
use crate::model::*;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use uuid::Uuid;
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::OwnedObjectPath,
};

type Object = (String, OwnedObjectPath);
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";

fn fault(error: impl std::fmt::Display) -> Fault {
    Fault::unavailable(format!("AT-SPI: {error}"))
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub bus: String,
    pub root: OwnedObjectPath,
    pub process: ProcessIdentity,
    pub label: String,
    pub toolkit: String,
    pub version: String,
    pub window_id: u64,
}

pub struct Accessibility {
    connection: Connection,
    pub candidate: Candidate,
    nodes: HashMap<String, OwnedObjectPath>,
}

pub fn connect_bus() -> Result<Connection> {
    let session = zbus::blocking::connection::Builder::session()
        .map_err(fault)?
        .method_timeout(Duration::from_secs(2))
        .build()
        .map_err(fault)?;
    let proxy =
        Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus").map_err(fault)?;
    let address: String = proxy.call("GetAddress", &()).map_err(fault)?;
    zbus::blocking::connection::Builder::address(address.as_str())
        .map_err(fault)?
        .method_timeout(Duration::from_millis(800))
        .build()
        .map_err(fault)
}

fn pid(connection: &Connection, name: &str) -> Result<u32> {
    let proxy = Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(fault)?;
    proxy
        .call("GetConnectionUnixProcessID", &(name,))
        .map_err(fault)
}

pub fn candidates() -> Result<Vec<Candidate>> {
    let connection = connect_bus()?;
    let path =
        std::env::var_os("NIRI_SOCKET").ok_or_else(|| Fault::unavailable("缺少 NIRI_SOCKET"))?;
    let niri_ipc::Response::Windows(niri_windows) =
        super::desktop::niri_request(std::path::Path::new(&path), niri_ipc::Request::Windows)?
    else {
        return Err(Fault::unavailable("niri 窗口列表不可用"));
    };
    let registry = Proxy::new(
        &connection,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
        ACCESSIBLE,
    )
    .map_err(fault)?;
    let apps: Vec<Object> = registry.call("GetChildren", &()).map_err(fault)?;
    let mut result = vec![];
    for (bus, root) in apps.into_iter().take(64) {
        if !bus.starts_with(':') {
            continue;
        }
        let Ok(process) = pid(&connection, &bus).and_then(ProcessIdentity::read) else {
            continue;
        };
        if process.pid == std::process::id() {
            continue;
        }
        let app =
            Proxy::new(&connection, bus.as_str(), root.as_str(), ACCESSIBLE).map_err(fault)?;
        let app_name: String = app.get_property("Name").unwrap_or_default();
        let app_info = Proxy::new(
            &connection,
            bus.as_str(),
            root.as_str(),
            "org.a11y.atspi.Application",
        )
        .map_err(fault)?;
        let toolkit: String = app_info.get_property("ToolkitName").unwrap_or_default();
        let version: String = app_info
            .get_property("ToolkitVersion")
            .or_else(|_| app_info.get_property("Version"))
            .unwrap_or_default();
        let Ok(children) = app.call::<_, _, Vec<Object>>("GetChildren", &()) else {
            continue;
        };
        for (child_bus, child_root) in children.into_iter().take(32) {
            if child_bus != bus {
                continue;
            }
            let window = Proxy::new(&connection, bus.as_str(), child_root.as_str(), ACCESSIBLE)
                .map_err(fault)?;
            let title: String = window.get_property("Name").unwrap_or_default();
            drop(window);
            let matching: Vec<_> = niri_windows
                .iter()
                .filter(|w| {
                    w.pid == Some(process.pid as i32) && w.title.as_deref() == Some(title.as_str())
                })
                .collect();
            if matching.len() != 1 {
                continue;
            }
            result.push(Candidate {
                bus: bus.clone(),
                root: child_root,
                process: process.clone(),
                label: format!("{app_name} — {title}"),
                toolkit: toolkit.clone(),
                version: version.clone(),
                window_id: matching[0].id,
            });
        }
    }
    Ok(result)
}

impl Accessibility {
    pub fn bind(candidate: Candidate) -> Result<Self> {
        let connection = connect_bus()?;
        if !candidate.process.alive() || pid(&connection, &candidate.bus)? != candidate.process.pid
        {
            return Err(Fault::stale("应用身份已改变"));
        }
        Ok(Self {
            connection,
            candidate,
            nodes: HashMap::new(),
        })
    }
    fn proxy<'a>(&'a self, path: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
        Proxy::new(
            &self.connection,
            self.candidate.bus.as_str(),
            path,
            interface,
        )
        .map_err(fault)
    }
    pub fn alive(&self) -> bool {
        self.candidate.process.alive()
            && pid(&self.connection, &self.candidate.bus)
                .is_ok_and(|pid| pid == self.candidate.process.pid)
            && self
                .proxy(self.candidate.root.as_str(), ACCESSIBLE)
                .and_then(|p| p.get_property::<String>("Name").map_err(fault))
                .is_ok()
    }
    pub fn read(&mut self, cancel: &Cancellation) -> Result<Vec<Node>> {
        if !self.alive() {
            return Err(Fault::stale("应用窗口已退出或 D-Bus 所有者变化"));
        }
        self.nodes.clear();
        let mut visited = HashSet::new();
        let mut remaining = 256;
        let root = self.candidate.root.clone();
        let deadline = Instant::now() + Duration::from_secs(4);
        Ok(vec![self.walk(
            &root,
            0,
            &mut remaining,
            &mut visited,
            deadline,
            cancel,
        )?])
    }
    fn walk(
        &mut self,
        path: &OwnedObjectPath,
        depth: usize,
        remaining: &mut usize,
        visited: &mut HashSet<String>,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Node> {
        cancel.check()?;
        if depth > 12 || *remaining == 0 || Instant::now() > deadline {
            return Err(Fault::unavailable("控件树读取达到限制"));
        }
        *remaining -= 1;
        visited.insert(path.to_string());
        let proxy = self.proxy(path.as_str(), ACCESSIBLE)?;
        let name: String = proxy.get_property("Name").map_err(fault)?;
        let role: String = proxy.call("GetRoleName", &()).map_err(fault)?;
        let interfaces: Vec<String> = proxy.call("GetInterfaces", &()).unwrap_or_default();
        let children: Vec<Object> = proxy.call("GetChildren", &()).unwrap_or_default();
        let role_number: u32 = proxy.call("GetRole", &()).unwrap_or_default();
        drop(proxy);
        // ATSPI_ROLE_PASSWORD_TEXT = 40. Never expose password fields.
        let text = if role_number != 40 && interfaces.iter().any(|i| i == "org.a11y.atspi.Text") {
            let p = self.proxy(path.as_str(), "org.a11y.atspi.Text")?;
            let count: i32 = p.get_property("CharacterCount").unwrap_or(0);
            p.call::<_, _, String>("GetText", &(0i32, count.clamp(0, 4096)))
                .ok()
        } else {
            None
        };
        let id = Uuid::new_v4().to_string();
        self.nodes.insert(id.clone(), path.clone());
        let mut nodes = vec![];
        for (bus, child) in children {
            if role_number == 40
                || bus != self.candidate.bus
                || visited.contains(child.as_str())
                || *remaining == 0
                || depth >= 12
                || Instant::now() > deadline
            {
                continue;
            }
            if let Ok(node) = self.walk(&child, depth + 1, remaining, visited, deadline, cancel) {
                nodes.push(node);
            }
        }
        cancel.check()?;
        // Only verified mutation adapters may advertise actions.
        Ok(Node {
            id,
            name: if role_number == 40 {
                "密码输入框".into()
            } else {
                name
            },
            role,
            text,
            actions: vec![],
            children: nodes,
        })
    }
    pub fn focused(&self) -> Result<bool> {
        let root = self.proxy(self.candidate.root.as_str(), ACCESSIBLE)?;
        let states: Vec<u32> = root.call("GetState", &()).map_err(fault)?;
        let state = states
            .first()
            .copied()
            .ok_or_else(|| Fault::unavailable("缺少窗口焦点状态"))?;
        Ok(state & ((1 << 1) | (1 << 12)) != 0)
    }
    /// Deliberately closed by default. A toolkit interface is not evidence that
    /// an application action is safe to invoke in the background.
    pub fn mutation_capabilities(&self) -> Vec<String> {
        vec![]
    }
    pub fn act(&mut self, _action: &Action, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        if self.focused()? {
            return Err(Fault::new(ErrorCode::Paused, "用户正在使用目标窗口"));
        }
        Err(Fault::unsupported(
            "此应用及版本尚无通过无干扰验证的后台写操作；可选择独立实例",
        ))
    }
}
