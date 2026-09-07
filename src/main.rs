//! @file main.rs
//! @brief MCP 服务、权限守护进程与本地管理命令
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use clap::{Parser, Subcommand};
use computer_use_linux::{
    ipc::Client,
    model::{Request, Response},
};
#[derive(Parser)]
#[command(version, about = "面向 niri 的 Linux Computer Use MCP")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// 启动标准 stdio MCP（默认）
    Mcp,
    /// 启动本地权限服务及 GTK 授权界面
    Daemon,
    /// 立即暂停所有自动输入，可绑定 niri 紧急快捷键
    PauseAll,
    /// 打开本地授权与会话管理面板
    Manage,
}
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match args.command.unwrap_or(Command::Mcp) {
        Command::Daemon => computer_use_linux::ui::run(&runtime),
        Command::Mcp => runtime.block_on(computer_use_linux::mcp::run()),
        command => runtime.block_on(async {
            let response = Client::connect()
                .await?
                .request(match command {
                    Command::PauseAll => Request::PauseAll,
                    _ => Request::ShowUi,
                })
                .await?;
            match response {
                Response::Error(e) => Err(e.into()),
                _ => Ok(()),
            }
        }),
    }
}
