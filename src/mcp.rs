//! @file mcp.rs
//! @brief 标准 stdio MCP 工具入口
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07

use crate::{
    ipc::Client,
    model::{ActRequest, Fault, ObserveRequest, Request, Response, SessionRef, SessionRequest},
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock as Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};

#[derive(Clone)]
pub struct Mcp {
    client: Client,
    #[expect(dead_code, reason = "由 tool_handler 宏使用")]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Mcp {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }
    async fn call(&self, request: Request) -> CallToolResult {
        match self.client.request(request).await {
            Ok(Response::Error(error)) => {
                CallToolResult::error(vec![Content::text(serde_json::to_string(&error).unwrap())])
            }
            Ok(Response::Observation(mut observation)) => {
                let png = observation.png_base64.take();
                let mut content = vec![Content::text(serde_json::to_string(&observation).unwrap())];
                if let Some(png) = png {
                    content.push(Content::image(png, "image/png"));
                }
                CallToolResult::success(content)
            }
            Ok(other) => {
                CallToolResult::success(vec![Content::text(serde_json::to_string(&other).unwrap())])
            }
            Err(error) => CallToolResult::error(vec![Content::text(
                serde_json::to_string(&Fault::unavailable(error.to_string())).unwrap(),
            )]),
        }
    }
    #[tool(
        description = "申请桌面授权。scope=application 配合 isolated/existing；scope=desktop 配合 desktop。isolated 必须用 application 指定已安装应用名称或 .desktop ID（如 firefox、kitty.desktop），用户只需在本地允许或拒绝，不接受启动命令或参数。existing 仍由本地选择已有实例。返回申请编号；授权前不会返回桌面内容。"
    )]
    async fn request_session(
        &self,
        Parameters(params): Parameters<SessionRequest>,
    ) -> CallToolResult {
        self.call(Request::RequestSession(params)).await
    }
    #[tool(description = "查询当前连接申请的授权状态、可用能力与目标；只在 active 状态执行动作。")]
    async fn session_status(&self, Parameters(params): Parameters<SessionRef>) -> CallToolResult {
        self.call(Request::SessionStatus(params)).await
    }
    #[tool(
        description = "观察获准目标，返回 PNG、控件树和新的 observation_id。坐标使用返回图像的像素，不能使用未经观察的目标。"
    )]
    async fn observe(&self, Parameters(params): Parameters<ObserveRequest>) -> CallToolResult {
        self.call(Request::Observe(params)).await
    }
    #[tool(
        description = "在有效观察上执行一个动作。只使用会话公布的能力；不支持时停止或请求用户选择其他模式，不得绕过授权。scroll 的 dx/dy 为像素距离；key 支持单字符、Enter、Tab、方向键和 F1-F12。"
    )]
    async fn act(&self, Parameters(params): Parameters<ActRequest>) -> CallToolResult {
        self.call(Request::Act(params)).await
    }
    #[tool(description = "撤销当前连接的会话，停止控制但不强制关闭有未保存内容的应用。")]
    async fn close_session(&self, Parameters(params): Parameters<SessionRef>) -> CallToolResult {
        self.call(Request::CloseSession(params)).await
    }
}

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("先申请本地用户授权，再检查状态和能力。已有实例只支持经过验证的后台操作，永不回退到全局输入。单应用授权不等于宿主机文件沙箱。")
    }
}

pub async fn run() -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let input = DisconnectAware {
        inner: tokio::io::stdin(),
        client: client.clone(),
    };
    let service = Mcp::new(client).serve((input, tokio::io::stdout())).await?;
    service.waiting().await?;
    Ok(())
}

struct DisconnectAware {
    inner: tokio::io::Stdin,
    client: Client,
}
impl tokio::io::AsyncRead for DisconnectAware {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, std::task::Poll::Ready(Err(_)))
            || matches!(result, std::task::Poll::Ready(Ok(()))) && buf.filled().len() == before
        {
            self.client.shutdown();
        }
        result
    }
}
