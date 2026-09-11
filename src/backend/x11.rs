//! @file x11.rs
//! @brief 私有 XWayland 端点与 XRes 客户端身份核验，不信任窗口 PID 属性
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11

use super::process::ProcessIdentity;
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::{
        fd::AsFd,
        unix::{fs::MetadataExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        res::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
    utils::RawFdContainer,
};

/// Written by a fixed Sway startup command before any application is launched.
/// No host environment or service manager import is performed.
pub const REPORT_DISPLAY: &str =
    "exec sh -c 'umask 077; printf \"%s\" \"$DISPLAY\" > \"$XDG_RUNTIME_DIR/x11-display\"'\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoint {
    pub display: String,
    device: u64,
    inode: u64,
    peer_pid: i32,
}
#[derive(Debug, Serialize)]
pub struct Window {
    pub id: u32,
    pub process: ProcessIdentity,
    geometry: (i16, i16, u16, u16),
    mapped: bool,
    override_redirect: bool,
}
fn socket_path(display: &str) -> Result<PathBuf> {
    let number = display
        .strip_prefix(':')
        .filter(|s| !s.is_empty() && s.len() <= 6 && s.bytes().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| Fault::denied("独立 X11 display 无效，不连接宿主或网络显示服务"))?;
    Ok(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
}
impl Endpoint {
    pub fn discover(runtime: &Path, sway: &ProcessIdentity) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if !sway.alive() {
                return Err(Fault::stale("独立合成器已退出"));
            }
            if let Ok(display) = fs::read_to_string(runtime.join("x11-display"))
                && !display.is_empty()
            {
                let path = socket_path(&display)?;
                let metadata =
                    fs::metadata(&path).map_err(|e| Fault::unavailable(e.to_string()))?;
                let socket =
                    UnixStream::connect(&path).map_err(|e| Fault::unavailable(e.to_string()))?;
                let peer = nix::sys::socket::getsockopt(
                    &socket,
                    nix::sys::socket::sockopt::PeerCredentials,
                )
                .map_err(|e| Fault::unavailable(e.to_string()))?;
                let connected =
                    fs::metadata(&path).map_err(|e| Fault::unavailable(e.to_string()))?;
                if (connected.dev(), connected.ino()) != (metadata.dev(), metadata.ino()) {
                    return Err(Fault::stale("独立 X11 服务在连接时已变化"));
                }
                if peer.uid() != nix::unistd::getuid().as_raw() {
                    return Err(Fault::denied("独立 X11 服务用户不匹配"));
                }
                return Ok(Self {
                    display,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    peer_pid: peer.pid(),
                });
            }
            if Instant::now() >= deadline {
                return Err(Fault::unavailable(
                    "无法获取独立 XWayland display；查看 sway.log",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    pub fn connect(&self) -> Result<RustConnection<BoundedStream>> {
        let path = socket_path(&self.display)?;
        let metadata = fs::metadata(&path).map_err(|_| Fault::stale("独立 X11 服务已退出"))?;
        if (metadata.dev(), metadata.ino()) != (self.device, self.inode) {
            return Err(Fault::stale("独立 X11 服务已更换，旧授权失效"));
        }
        let socket = UnixStream::connect(&path).map_err(|e| Fault::unavailable(e.to_string()))?;
        let peer =
            nix::sys::socket::getsockopt(&socket, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(|e| Fault::unavailable(e.to_string()))?;
        let connected = fs::metadata(&path).map_err(|_| Fault::stale("独立 X11 服务已退出"))?;
        if (connected.dev(), connected.ino()) != (self.device, self.inode)
            || peer.uid() != nix::unistd::getuid().as_raw()
            || peer.pid() != self.peer_pid
        {
            return Err(Fault::stale("独立 X11 服务连接身份已变化"));
        }
        let (inner, _) = DefaultStream::from_unix_stream(socket)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        RustConnection::connect_to_stream(
            BoundedStream {
                inner,
                deadline: Instant::now() + Duration::from_secs(2),
            },
            0,
        )
        .map_err(|e| Fault::unavailable(format!("连接独立 X11: {e}")))
    }
    /// Includes override-redirect popups missing from the Sway managed tree.
    pub fn windows(&self, managed: &[u32]) -> Result<BTreeMap<u32, Window>> {
        let conn = self.connect()?;
        snapshot(&conn, managed)
            .map_err(|e| Fault::stale(format!("X11 窗口身份不可验证，请重新观察: {e}")))
    }
}
fn snapshot(
    conn: &RustConnection<BoundedStream>,
    managed: &[u32],
) -> anyhow::Result<BTreeMap<u32, Window>> {
    let version = conn.res_query_version(1, 2)?.reply()?;
    anyhow::ensure!(
        (version.server_major, version.server_minor) >= (1, 2),
        "XRes 1.2 不可用"
    );
    let root = conn.setup().roots[0].root;
    let children = conn.query_tree(root)?.reply()?.children;
    let managed: BTreeSet<_> = managed.iter().copied().collect();
    let mut result = BTreeMap::new();
    for id in children
        .into_iter()
        .collect::<BTreeSet<_>>()
        .union(&managed)
        .copied()
    {
        let attrs = conn.get_window_attributes(id)?.reply()?;
        let mapped = attrs.map_state == xproto::MapState::VIEWABLE;
        if attrs.class != xproto::WindowClass::INPUT_OUTPUT || (!mapped && !managed.contains(&id)) {
            continue;
        }
        let ids = conn
            .res_query_client_ids(&[res::ClientIdSpec {
                client: id,
                mask: res::ClientIdMask::LOCAL_CLIENT_PID,
            }])?
            .reply()?;
        let pid = ids
            .ids
            .iter()
            .find(|value| {
                value
                    .spec
                    .mask
                    .contains(res::ClientIdMask::LOCAL_CLIENT_PID)
            })
            .and_then(|value| value.value.first())
            .copied()
            .filter(|pid| *pid > 1)
            .ok_or_else(|| anyhow::anyhow!("XRes 没有返回本地客户端 PID"))?;
        let process = ProcessIdentity::read(pid)?;
        let geometry = conn.get_geometry(id)?.reply()?;
        result.insert(
            id,
            Window {
                id,
                process,
                geometry: (geometry.x, geometry.y, geometry.width, geometry.height),
                mapped,
                override_redirect: attrs.override_redirect,
            },
        );
    }
    Ok(result)
}

/// One bounded connection per identity snapshot, including handshake and all
/// replies. A wedged X server must not indefinitely block cancellation barriers.
pub struct BoundedStream {
    inner: DefaultStream,
    deadline: Instant,
}
impl BoundedStream {
    fn check(&self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            Err(io::Error::new(io::ErrorKind::TimedOut, "X11 响应超时"))
        } else {
            Ok(())
        }
    }
}
impl Stream for BoundedStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        loop {
            self.check()?;
            let mut flags = nix::poll::PollFlags::empty();
            if mode.readable() {
                flags |= nix::poll::PollFlags::POLLIN;
            }
            if mode.writable() {
                flags |= nix::poll::PollFlags::POLLOUT;
            }
            let mut fd = [nix::poll::PollFd::new(self.inner.as_fd(), flags)];
            match nix::poll::poll(&mut fd, 20u16) {
                Ok(0) | Err(nix::errno::Errno::EINTR) => {}
                Ok(_) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn read(&self, buffer: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buffer, fds)
    }
    fn write(&self, buffer: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        self.inner.write(buffer, fds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unresponsive_server_handshake_is_bounded() {
        let (client, _server) = UnixStream::pair().unwrap();
        let (inner, _) = DefaultStream::from_unix_stream(client).unwrap();
        let start = Instant::now();
        let result = RustConnection::connect_to_stream(
            BoundedStream {
                inner,
                deadline: start + Duration::from_millis(80),
            },
            0,
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn only_private_local_display_names_are_accepted() {
        assert_eq!(
            socket_path(":12").unwrap(),
            PathBuf::from("/tmp/.X11-unix/X12")
        );
        for bad in [
            "",
            ":",
            "localhost:0",
            "host:1",
            ":1/../../tmp",
            ":1.0",
            ":-1",
        ] {
            assert!(socket_path(bad).is_err());
        }
    }
}
