//! @file process.rs
//! @brief 进程生命周期身份，防止 PID 重用
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
    pub executable: PathBuf,
}
impl ProcessIdentity {
    pub fn read(pid: u32) -> Result<Self> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| Fault::stale("进程已经退出"))?;
        let rest = stat
            .rsplit_once(')')
            .ok_or_else(|| Fault::stale("进程状态无效"))?
            .1;
        let fields: Vec<_> = rest.split_whitespace().collect();
        let start_time = fields
            .get(19)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Fault::stale("进程生命周期无效"))?;
        let executable = fs::read_link(format!("/proc/{pid}/exe"))
            .map_err(|_| Fault::stale("无法验证进程身份"))?;
        Ok(Self {
            pid,
            start_time,
            executable,
        })
    }
    pub fn alive(&self) -> bool {
        Self::read(self.pid).is_ok_and(|current| current == *self)
    }
    /// Cleanup uses a pidfd so a recycled PID can never receive the signal.
    pub fn terminate(&self) -> bool {
        // SAFETY: pidfd_open takes a numeric PID and flags, returning an owned fd.
        let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, self.pid, 0) };
        if fd < 0 {
            return false;
        }
        // SAFETY: a successful pidfd_open returned this uniquely owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if !self.alive() {
            return false;
        }
        // SAFETY: the fd is live; a null siginfo requests a normal SIGTERM.
        unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                nix::libc::SIGTERM,
                std::ptr::null::<nix::libc::siginfo_t>(),
                0,
            ) == 0
        }
    }
}

pub fn descendant(mut pid: u32, parent: u32) -> bool {
    for _ in 0..64 {
        if pid == parent {
            return true;
        }
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        let Some(next) = rest
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return false;
        };
        if next == pid || next < 2 {
            return false;
        }
        pid = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_process_identity_and_changed_start_time() {
        let mut identity = ProcessIdentity::read(std::process::id()).unwrap();
        assert!(identity.alive());
        identity.start_time += 1;
        assert!(!identity.alive());
    }
    #[test]
    fn cleanup_does_not_signal_a_different_lifetime() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = ProcessIdentity::read(child.id()).unwrap();
        let mut wrong = identity.clone();
        wrong.start_time += 1;
        assert!(!wrong.terminate());
        assert!(child.try_wait().unwrap().is_none());
        assert!(identity.terminate());
        assert!(!child.wait().unwrap().success());
    }
}
