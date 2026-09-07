//! @file mcp_stdio.rs
//! @brief 使用真实 MCP 二进制验证握手、工具 Schema 与连接关闭
//! @author modolet <y@xxyx.io>
//! @date 2026-09-07
use computer_use_linux::{
    ipc::{MAX_REQUEST, read_frame},
    model::*,
    policy::Policy,
};
use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_tools_and_permission_errors() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("computer-use-linux");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = tokio::net::UnixListener::bind(dir.join("broker.sock")).unwrap();
    let broker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut policy = Policy::default();
        let owner = uuid::Uuid::new_v4();
        policy.register(owner);
        loop {
            let bytes = match read_frame(&mut socket, MAX_REQUEST).await {
                Ok(bytes) => bytes,
                Err(_) => break,
            };
            let request: Request = serde_json::from_slice(&bytes).unwrap();
            let response = match request {
                Request::RequestSession(r) => match policy.request(owner, r) {
                    Ok(status) => Response::Status(status),
                    Err(e) => Response::Error(e),
                },
                Request::Observe(r) => {
                    Response::Error(policy.permit(owner, &r.session_id, None).err().unwrap())
                }
                _ => Response::Error(Fault::unsupported("测试不需要该操作")),
            };
            let data = serde_json::to_vec(&response).unwrap();
            socket.write_u32(data.len() as u32).await.unwrap();
            socket.write_all(&data).await.unwrap();
        }
        policy.disconnect(owner);
        assert!(policy.sessions.is_empty());
    });
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_computer-use-linux"))
        .arg("mcp")
        .env("XDG_RUNTIME_DIR", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    async fn send(input: &mut tokio::process::ChildStdin, value: serde_json::Value) {
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn receive(
        output: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> serde_json::Value {
        let line = tokio::time::timeout(Duration::from_secs(5), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).expect("stdout 只能包含 MCP JSON")
    }
    send(&mut input,serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}})).await;
    let result = receive(&mut output).await;
    assert!(
        result["result"]["capabilities"]["tools"].is_object(),
        "{result}"
    );
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let result = receive(&mut output).await;
    let tools = result["result"]["tools"].as_array().unwrap();
    let names: std::collections::HashSet<_> =
        tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "request_session",
            "session_status",
            "observe",
            "act",
            "close_session"
        ]
        .into_iter()
        .collect()
    );
    send(&mut input,serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"request_session","arguments":{"scope":"application","mode":"isolated","application":"gnome-text-editor"}}})).await;
    let result = receive(&mut output).await;
    let status: serde_json::Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["value"]["state"], "pending");
    assert_eq!(status["value"]["targets"], serde_json::json!([]));
    let id = status["value"]["session_id"].as_str().unwrap();
    send(&mut input,serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"observe","arguments":{"session_id":id}}})).await;
    let result = receive(&mut output).await;
    assert_eq!(result["result"]["isError"], true);
    send(&mut input, serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"request_session","arguments":{"scope":"application","mode":"isolated"}}})).await;
    assert_eq!(receive(&mut output).await["result"]["isError"], true);
    send(&mut input, serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"request_session","arguments":{"scope":"application","mode":"isolated","application":"kitty.desktop"}}})).await;
    let result = receive(&mut output).await;
    assert_ne!(result["result"]["isError"], true);
    let status: serde_json::Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["value"]["state"], "pending");
    assert_eq!(status["value"]["targets"], serde_json::json!([]));
    // The removed mode must neither be advertised nor reach the permission broker.
    let request_tool = tools
        .iter()
        .find(|t| t["name"] == "request_session")
        .unwrap();
    let schema = request_tool["inputSchema"].to_string();
    assert!(!schema.contains("existing"));
    send(&mut input, serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"request_session","arguments":{"scope":"application","mode":"existing"}}})).await;
    let removed = receive(&mut output).await;
    assert!(removed["error"].is_object() || removed["result"]["isError"] == true);
    input.shutdown().await.unwrap();
    drop(input);
    tokio::time::timeout(Duration::from_secs(3), broker)
        .await
        .unwrap()
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}
