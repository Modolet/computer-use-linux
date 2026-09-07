//! @file portal-check.rs
//! @brief 由本地用户选择测试窗口的真实 Portal 验收，不注入桌面输入
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use computer_use_linux::backend::{
    Cancellation, desktop::niri_request, portal::Portal, process::ProcessIdentity,
};
use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

struct Fixture(std::process::Child);
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(identity) = ProcessIdentity::read(self.0.id()) {
            identity.terminate();
        }
        let _ = self.0.wait();
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("computer-use-portal-check.txt");
    std::fs::write(
        &path,
        "Linux Computer Use MCP\nPortal 中文窗口采集测试\n仅共享这个测试窗口。",
    )?;
    let fixture = Fixture(
        std::process::Command::new("gnome-text-editor")
            .arg("--standalone")
            .arg(&path)
            .spawn()?,
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let identity = ProcessIdentity::read(fixture.0.id())?;
    eprintln!(
        "请在本地共享选择器中选择 computer-use-portal-check.txt；本测试不会发送任何鼠标或键盘输入。"
    );
    let mut portal = Portal::select().await?;
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        anyhow::ensure!(identity.alive(), "测试进程已退出");
        let socket = std::path::PathBuf::from(
            std::env::var_os("NIRI_SOCKET").ok_or_else(|| anyhow::anyhow!("缺少 NIRI_SOCKET"))?,
        );
        let niri_ipc::Response::Windows(windows) =
            niri_request(&socket, niri_ipc::Request::Windows)?
        else {
            anyhow::bail!("窗口列表无效")
        };
        let target = windows
            .into_iter()
            .find(|w| w.pid == Some(identity.pid as i32))
            .ok_or_else(|| anyhow::anyhow!("找不到测试进程窗口"))?;
        portal.bind_window(target.id)?;
        let cancel = Cancellation::new(Arc::new(AtomicU64::new(0)));
        let (png, width, height) = portal.capture(&cancel)?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
        let image = image::load_from_memory(&bytes)?.to_rgb8();
        let min = image.pixels().flat_map(|p| p.0).min().unwrap_or(0);
        let max = image.pixels().flat_map(|p| p.0).max().unwrap_or(0);
        anyhow::ensure!(
            max.saturating_sub(min) > 60 && width > 100 && height > 100,
            "图像没有有效窗口内容"
        );
        if let Some(path) = std::env::var_os("COMPUTER_USE_PORTAL_PNG") {
            std::fs::write(path, bytes)?;
        }
        println!("PASS: Portal/PipeWire 窗口采集 {width}×{height}，进程和窗口身份匹配");
        assert!(portal.bind_window(target.id.wrapping_add(1000000)).is_err());
        println!("PASS: 拒绝将窗口流重新绑定到其他窗口");
        std::thread::sleep(Duration::from_secs(1));
        portal.capture(&cancel)?;
        println!("PASS: 静止窗口持续提供图像");
        identity.terminate();
        std::thread::sleep(Duration::from_millis(250));
        anyhow::ensure!(
            portal.capture(&cancel).is_err(),
            "窗口关闭后不能返回缓存图像"
        );
        println!("PASS: 窗口关闭后采集引用立即失效");
        Ok(())
    })
    .await??;
    Ok(())
}
