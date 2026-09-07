//! @file cast_watch.rs
//! @brief 监测窗口采集目标变化，禁止短暂切换后复用旧授权
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use crate::model::*;
use niri_ipc::{Cast, Event};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

struct Binding {
    expected: Cast,
    initialized: bool,
    valid: bool,
}
impl Binding {
    fn matches(&self, cast: &Cast) -> bool {
        cast.stream_id == self.expected.stream_id
            && cast.session_id == self.expected.session_id
            && cast.pw_node_id == self.expected.pw_node_id
            && cast.target == self.expected.target
            && !cast.is_dynamic_target
            && cast.kind == niri_ipc::CastKind::PipeWire
    }
    fn update(&mut self, event: Event) {
        match event {
            Event::CastsChanged { casts } => {
                self.initialized = true;
                self.valid &= casts.iter().any(|c| self.matches(c));
            }
            Event::CastStartedOrChanged { cast } if cast.stream_id == self.expected.stream_id => {
                self.valid &= self.matches(&cast);
            }
            Event::CastStopped { stream_id } if stream_id == self.expected.stream_id => {
                self.valid = false
            }
            _ => {}
        }
    }
}

pub struct CastWatch {
    reader: BufReader<UnixStream>,
    pending: Vec<u8>,
    binding: Binding,
}
impl CastWatch {
    pub fn connect(path: &Path, expected: Cast) -> Result<Self> {
        let mut stream =
            UnixStream::connect(path).map_err(|e| Fault::unavailable(e.to_string()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        serde_json::to_writer(&mut stream, &niri_ipc::Request::EventStream)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        stream
            .write_all(b"\n")
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let mut reader = BufReader::new(stream);
        let mut reply = Vec::new();
        (&mut reader)
            .take(8 * 1024 * 1024)
            .read_until(b'\n', &mut reply)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let reply: niri_ipc::Reply =
            serde_json::from_slice(&reply).map_err(|e| Fault::unavailable(e.to_string()))?;
        reply.map_err(Fault::unavailable)?;
        reader
            .get_ref()
            .set_nonblocking(true)
            .map_err(|e| Fault::unavailable(e.to_string()))?;
        let mut watch = Self {
            reader,
            pending: vec![],
            binding: Binding {
                expected,
                initialized: false,
                valid: true,
            },
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !watch.binding.initialized {
            watch.drain()?;
            if Instant::now() > deadline {
                return Err(Fault::unavailable("niri 未发送采集流初始状态"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(watch)
    }
    pub fn drain(&mut self) -> Result<()> {
        let mut bytes = [0; 8192];
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            match self.reader.read(&mut bytes) {
                Ok(0) => {
                    self.binding.valid = false;
                    break;
                }
                Ok(n) => self.pending.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.binding.valid = false;
                    break;
                }
            }
            if self.pending.len() > 8 * 1024 * 1024 || Instant::now() > deadline {
                self.binding.valid = false;
                break;
            }
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let event: Event = serde_json::from_slice(&self.pending[..end]).map_err(|_| {
                    self.binding.valid = false;
                    Fault::stale("采集流监视协议错误")
                })?;
                self.binding.update(event);
                self.pending.drain(..=end);
            }
        }
        if !self.binding.valid {
            return Err(Fault::stale("窗口采集目标或监视连接已变化；必须重新授权"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cast() -> Cast {
        Cast {
            stream_id: 1,
            session_id: 2,
            kind: niri_ipc::CastKind::PipeWire,
            target: niri_ipc::CastTarget::Window { id: 3 },
            is_dynamic_target: false,
            is_active: true,
            pid: None,
            pw_node_id: Some(4),
        }
    }
    #[test]
    fn target_switch_and_restore_never_revalidates_authority() {
        let expected = cast();
        let mut binding = Binding {
            expected: expected.clone(),
            valid: true,
            initialized: false,
        };
        binding.update(Event::CastsChanged {
            casts: vec![expected.clone()],
        });
        assert!(binding.valid && binding.initialized);
        let mut other = expected.clone();
        other.target = niri_ipc::CastTarget::Window { id: 99 };
        binding.update(Event::CastStartedOrChanged { cast: other });
        binding.update(Event::CastStartedOrChanged { cast: expected });
        assert!(!binding.valid);
    }
    #[test]
    fn missing_stream_in_snapshot_invalidates_binding() {
        let mut binding = Binding {
            expected: cast(),
            valid: true,
            initialized: false,
        };
        binding.update(Event::CastsChanged { casts: vec![] });
        assert!(!binding.valid);
    }
}
