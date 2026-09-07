//! @file ipc.rs
//! @brief 用户级 Unix socket 与连接断开即时撤销
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use crate::{
    model::*,
    policy::{BackendHandle, Policy},
};
use anyhow::Context;
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex as AsyncMutex, Semaphore},
};
use uuid::Uuid;

pub type SharedPolicy = Arc<Mutex<Policy>>;
pub const MAX_REQUEST: usize = 128 * 1024;
pub const MAX_RESPONSE: usize = 64 * 1024 * 1024;

pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    let root = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("缺少 XDG_RUNTIME_DIR")?);
    let root_meta = fs::symlink_metadata(&root)?;
    anyhow::ensure!(
        root_meta.is_dir() && root_meta.uid() == nix::unistd::getuid().as_raw(),
        "运行目录不属于当前用户"
    );
    let dir = root.join("computer-use-linux");
    match fs::create_dir(&dir) {
        Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let metadata = fs::symlink_metadata(&dir)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == nix::unistd::getuid().as_raw()
            && metadata.mode() & 0o077 == 0,
        "MCP 运行目录权限必须为 0700，且不能是符号链接"
    );
    Ok(dir)
}

pub struct Listener {
    pub socket: UnixListener,
    _lock: File,
    path: PathBuf,
}
impl Listener {
    pub fn bind() -> anyhow::Result<Self> {
        let dir = runtime_dir()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(dir.join("daemon.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("权限服务已经运行")?;
        let path = dir.join("broker.sock");
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let socket = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            socket,
            _lock: lock,
            path,
        })
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    let len = reader.read_u32().await? as usize;
    anyhow::ensure!(len > 0 && len <= limit, "IPC 消息大小超出限制");
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl serde::Serialize,
) -> anyhow::Result<()> {
    let data = serde_json::to_vec(value)?;
    anyhow::ensure!(data.len() <= MAX_RESPONSE, "IPC 响应过大");
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(&data).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn serve(
    listener: Listener,
    policy: SharedPolicy,
    show: std::sync::mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let watch_policy = policy.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let sessions: Vec<_> = watch_policy
                .lock()
                .unwrap()
                .sessions
                .iter()
                .filter_map(|(id, s)| s.backend.clone().map(|b| (id.clone(), b)))
                .collect();
            let p = watch_policy.clone();
            let _ = tokio::task::spawn_blocking(move || {
                for (id, backend) in sessions {
                    let alive = backend.try_lock().ok().map(|mut b| b.alive());
                    if alive == Some(false)
                        && let Some(handle) = p.lock().unwrap().close_local(&id)
                    {
                        detach(handle);
                    }
                }
            })
            .await;
        }
    });
    loop {
        let (socket, _) = listener.socket.accept().await?;
        if socket.peer_cred()?.uid() != nix::unistd::getuid().as_raw() {
            continue;
        }
        let policy = policy.clone();
        let show = show.clone();
        tokio::spawn(async move {
            connection(socket, policy, show).await;
        });
    }
}

async fn connection(socket: UnixStream, policy: SharedPolicy, show: std::sync::mpsc::Sender<()>) {
    let owner = Uuid::new_v4();
    policy.lock().unwrap().register(owner);
    let (mut reader, writer) = socket.into_split();
    let writer = Arc::new(AsyncMutex::new(writer));
    let serial = Arc::new(Semaphore::new(1));
    while let Ok(bytes) = read_frame(&mut reader, MAX_REQUEST).await {
        let request: Request = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(_) => {
                let _ = write_frame(
                    &mut *writer.lock().await,
                    &Response::Error(Fault::invalid("无效请求")),
                )
                .await;
                continue;
            }
        };
        let Ok(guard) = serial.clone().try_acquire_owned() else {
            break;
        };
        let (policy, show, writer) = (policy.clone(), show.clone(), writer.clone());
        tokio::spawn(async move {
            let _guard = guard;
            let response = dispatch(policy, owner, request, show)
                .await
                .unwrap_or_else(Response::Error);
            let _ = write_frame(&mut *writer.lock().await, &response).await;
        });
    }
    // Reader remains independent of a long capture/action, so EOF cancels it immediately.
    let handles = policy.lock().unwrap().disconnect(owner);
    for handle in handles {
        detach(handle);
    }
}

pub fn detach(handle: BackendHandle) {
    std::thread::spawn(move || {
        if let Ok(mut backend) = handle.lock() {
            backend.detach();
        }
    });
}

async fn dispatch(
    policy: SharedPolicy,
    owner: Uuid,
    request: Request,
    show: std::sync::mpsc::Sender<()>,
) -> Result<Response> {
    match request {
        Request::RequestSession(r) => {
            let status = policy.lock().unwrap().request(owner, r)?;
            if show.send(()).is_err() {
                policy
                    .lock()
                    .unwrap()
                    .deny(&status.session_id, "授权界面不可用".into());
                return Err(Fault::unavailable("授权界面不可用"));
            }
            Ok(Response::Status(status))
        }
        Request::SessionStatus(r) => Ok(Response::Status(
            policy.lock().unwrap().status(owner, &r.session_id)?,
        )),
        Request::CloseSession(r) => {
            if let Some(backend) = policy.lock().unwrap().close(owner, &r.session_id)? {
                detach(backend);
            }
            Ok(Response::Ok)
        }
        Request::PauseAll => {
            policy.lock().unwrap().pause_all();
            Ok(Response::Ok)
        }
        Request::ShowUi => {
            {
                let mut p = policy.lock().unwrap();
                p.open_ui();
            }
            show.send(())
                .map_err(|_| Fault::unavailable("授权界面不可用"))?;
            Ok(Response::Ok)
        }
        Request::Observe(r) => {
            let permit = policy.lock().unwrap().permit(owner, &r.session_id, None)?;
            let cancel = permit.cancel.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut backend = permit
                    .backend
                    .try_lock()
                    .map_err(|_| Fault::new(ErrorCode::Busy, "后端正在执行其他操作"))?;
                if !backend.alive() {
                    return Err(Fault::stale("目标实例已退出"));
                }
                backend.observe(r.target.as_deref(), &permit.cancel)
            })
            .await
            .map_err(|_| Fault::unavailable("后端任务意外结束"))??;
            policy
                .lock()
                .unwrap()
                .remember(owner, &r.session_id, result.clone(), &cancel)?;
            Ok(Response::Observation(result))
        }
        Request::Act(r) => {
            let permit = policy.lock().unwrap().action_permit(owner, &r)?;
            tokio::task::spawn_blocking(move || {
                let mut backend = permit
                    .backend
                    .try_lock()
                    .map_err(|_| Fault::new(ErrorCode::Busy, "后端正在执行其他操作"))?;
                permit.cancel.check()?;
                if !backend.alive() {
                    return Err(Fault::stale("目标实例已退出"));
                }
                backend.act(
                    permit.observation.as_ref().unwrap(),
                    &r.action,
                    &permit.cancel,
                )
            })
            .await
            .map_err(|_| Fault::unavailable("后端任务意外结束"))??;
            Ok(Response::Ok)
        }
    }
}

#[derive(Clone)]
pub struct Client {
    stream: Arc<AsyncMutex<Option<UnixStream>>>,
    shutdown: Arc<std::os::unix::net::UnixStream>,
}
struct ExchangeGuard {
    socket: Arc<std::os::unix::net::UnixStream>,
    armed: bool,
}
impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
impl Client {
    pub async fn connect() -> anyhow::Result<Self> {
        let stream = UnixStream::connect(runtime_dir()?.join("broker.sock"))
            .await
            .context("请先启动 computer-use-linux daemon 或启用 Home Manager 服务")?;
        anyhow::ensure!(
            stream.peer_cred()?.uid() == nix::unistd::getuid().as_raw(),
            "服务用户不匹配"
        );
        let stream = stream.into_std()?;
        let shutdown = Arc::new(stream.try_clone()?);
        Ok(Self {
            stream: Arc::new(AsyncMutex::new(Some(UnixStream::from_std(stream)?))),
            shutdown,
        })
    }
    pub fn shutdown(&self) {
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }
    pub async fn request(&self, request: Request) -> anyhow::Result<Response> {
        let mut slot = self.stream.lock().await;
        // Own the socket across await: cancelling this future closes the
        // connection instead of leaving a response for the next request.
        let mut stream = slot.take().context("MCP 连接已结束；请重新连接")?;
        let mut guard = ExchangeGuard {
            socket: self.shutdown.clone(),
            armed: true,
        };
        write_frame(&mut stream, &request).await?;
        let bytes = read_frame(&mut stream, MAX_RESPONSE).await?;
        let response = serde_json::from_slice(&bytes)?;
        *slot = Some(stream);
        guard.armed = false;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, Cancellation};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Slow(Arc<AtomicBool>);
    impl Backend for Slow {
        fn capabilities(&self) -> Vec<String> {
            vec![]
        }
        fn targets(&mut self) -> Result<Vec<Target>> {
            Ok(vec![])
        }
        fn observe(&mut self, _: Option<&str>, cancel: &Cancellation) -> Result<Observation> {
            for _ in 0..100 {
                if let Err(e) = cancel.check() {
                    self.0.store(true, Ordering::SeqCst);
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(Fault::unavailable("测试操作未被取消"))
        }
        fn act(&mut self, _: &Observation, _: &Action, _: &Cancellation) -> Result<()> {
            unreachable!()
        }
        fn alive(&mut self) -> bool {
            true
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socket_eof_revokes_an_inflight_capture() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let policy = Arc::new(Mutex::new(Policy::default()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(connection(a, policy.clone(), tx));
        write_frame(
            &mut b,
            &Request::RequestSession(SessionRequest {
                scope: Scope::Application,
                mode: Mode::Existing,
                application: None,
            }),
        )
        .await
        .unwrap();
        let response: Response =
            serde_json::from_slice(&read_frame(&mut b, MAX_RESPONSE).await.unwrap()).unwrap();
        let Response::Status(status) = response else {
            panic!("申请应处于 pending")
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        policy
            .lock()
            .unwrap()
            .grant(
                &status.session_id,
                "test".into(),
                Box::new(Slow(cancelled.clone())),
            )
            .unwrap();
        write_frame(
            &mut b,
            &Request::Observe(ObserveRequest {
                session_id: status.session_id,
                target: None,
            }),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        b.shutdown().await.unwrap();
        task.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(policy.lock().unwrap().sessions.is_empty());
        assert!(cancelled.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn oversize_frame_rejected_before_allocation() {
        let mut data: &[u8] = &[0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut data, MAX_REQUEST).await.is_err());
    }
    #[test]
    fn approval_and_resume_are_not_ipc_methods() {
        for method in ["grant", "resume", "approve", "launch", "shell"] {
            assert!(
                serde_json::from_value::<Request>(serde_json::json!({"method":method,"params":{}}))
                    .is_err()
            );
        }
    }
}
