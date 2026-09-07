# Linux Computer Use MCP

面向 NixOS、niri 的本地 Rust stdio MCP。权限在本地中文 GTK 窗口中确认，MCP 没有批准授权、任意命令或任意 D-Bus 调用接口。

**独立应用在桌面上有持续更新的可见窗口。** 应用运行于私有 Wayland 和 D-Bus 会话；桌面上的 GTK 窗口显示它的实时画面。查看画面不暂停 AI，不向宿主注入鼠标、键盘或剪贴板操作。勾选“手动接管”会暂停 AI；关闭最后一个应用视图也会暂停 AI。内部 Sway 使用 headless 渲染，用户使用的应用窗口并不隐藏。

| 模式 | 当前能力 | 授权和输入边界 |
| --- | --- | --- |
| 单应用：独立实例 | Firefox、GNOME Text Editor 的截图、点击、拖动、滚动、组合键和 Unicode 文本 | 单独图形会话、剪贴板和应用配置；默认显示实时应用窗口 |
| 单应用：已有实例 | AT-SPI 控件树；Portal/PipeWire 单窗口截图 | 绑定进程生命周期、D-Bus 唯一所有者和 niri 窗口；后台写动作尚无验证名单，当前全部拒绝 |
| 整个电脑 | niri 窗口切换、显示器截图；合成器开放协议时提供虚拟输入 | 使用真实桌面；最多一个整机会话执行输入 |

已有实例不会因为不支持某个动作而改用全局输入。Portal 选择的窗口必须与应用选择一致，并通过 niri 的 cast 记录核实。无法取得 AT-SPI 身份或核实流归属时拒绝授权。当前不把“存在 EditableText/Action 接口”当作无干扰保证。

## 安装和启动

```sh
nix build
./result/bin/computer-use-linux daemon
```

守护进程启动后等待请求；打开管理面板：

```sh
./result/bin/computer-use-linux manage
```

守护进程需要从当前图形会话继承 `XDG_RUNTIME_DIR`、`WAYLAND_DISPLAY`、`DBUS_SESSION_BUS_ADDRESS` 和 `NIRI_SOCKET`。需要 niri 26.04 的窗口 PID、cast IPC 接口。已有实例截图还需要当前桌面的 xdg-desktop-portal、niri Portal 后端与 PipeWire 正常运行。

开发环境：

```sh
nix develop
cargo run -- daemon
# 在另一个终端中：
cargo run -- mcp
```

通用 MCP 客户端配置，将 command 替换成构建产物的绝对路径：

```json
{
  "mcpServers": {
    "linux-computer-use": {
      "command": "/absolute/path/to/result/bin/computer-use-linux",
      "args": ["mcp"]
    }
  }
}
```

`mcp` 通过 stdio 使用官方 rmcp SDK；日志只写 stderr。需要预先运行本地 daemon。没有 HTTP 服务。

## 使用流程

1. 模型调用 `request_session`，例如 `{"scope":"application","mode":"isolated"}`。立即返回申请编号和 pending 状态，不返回桌面信息。
2. 在本地面板选择 Firefox 或 GNOME Text Editor，启动并查看预览，然后确认授权。独立应用会自动打开实时窗口。
3. 点击“隐藏面板并恢复 AI”。授权面板出现期间会暂停所有自动输入；单纯查看应用窗口不会暂停。
4. 模型通过 `session_status` 获取状态、能力和目标；调用 `observe` 获取图像、控件树和观察编号；每次 `act` 都必须带上最新的 `session_id`、`observation_id` 和 `target`。
5. 随时打开 `manage` 暂停或撤销，或执行 `pause-all`。恢复只能通过本地面板完成。

申请组合：`application/isolated`、`application/existing`、`desktop/desktop`。坐标以观察返回的图像像素为准；返回目标包含像素尺寸、缩放和显示器/应用标签。一次动作消耗一次观察。尺寸、布局或实例变化后必须重新观察。

```json
{
  "session_id": "会话编号",
  "observation_id": "最新观察编号",
  "target": "观察返回的目标编号",
  "action": { "kind": "text", "text": "你好，Linux" }
}
```

工具只有 `request_session`、`session_status`、`observe`、`act`、`close_session`。统一错误码：`permission_denied`、`paused`、`unsupported`、`stale_target`、`backend_unavailable`、`invalid_request`、`busy`。忙碌或目标失效时重新观察，不应自动重复已经执行的动作。

## 可选 Home Manager 服务

将本 flake 添加为配置输入后：

```nix
{
  imports = [ inputs.computer-use-linux.homeManagerModules.default ];
  services.computer-use-linux.enable = true;
}
```

模块安装程序并提供图形会话用户服务，同时生成 `~/.config/computer-use-linux/niri.kdl`。可按需在 niri 配置中 include 该文件，或手动加入：

```kdl
binds {
    Mod+Shift+Escape { spawn "computer-use-linux" "pause-all"; }
}
```

模块不会修改现有 niri 配置。用户服务依赖图形会话环境已导入 systemd 用户管理器。

## 数据与生命周期

Socket 位于 `$XDG_RUNTIME_DIR/computer-use-linux/broker.sock`，目录 0700、socket 0600，并检查连接 UID。授权与每条客户端连接绑定；断线、撤销、应用退出都会取消当前输入。暂停会使排队动作失效并释放虚拟按键。守护进程重启不恢复 AI 授权。

独立应用状态位于 `$XDG_STATE_HOME/computer-use-linux`，未设置时使用 `~/.local/state/computer-use-linux`。每个实例使用单独配置目录，Firefox 禁止复用个人实例。登录由用户在可见窗口中接管完成；登录状态留在该实例的持久化配置中。当前新建会话会创建新配置，保留实例可在管理面板中重新打开。

撤销只停止控制，不强制关闭有未保存内容的独立应用；应用保留供本地接管。已有应用继续运行。独立实例限制桌面观察和输入范围，仍然拥有原来的文件、网络权限和同一 Unix 用户权限，不构成针对恶意应用的文件系统或进程安全沙箱。

## 验证

```sh
nix develop --command cargo fmt --check
nix develop --command cargo check
nix develop --command cargo test
nix develop --command cargo clippy --all-targets -- -D warnings
```

真实独立会话测试默认忽略，显式运行会启动测试用应用和私有 Sway，不向当前桌面注入输入：

```sh
nix develop
test_runtime=$(mktemp -d /tmp/cu-XXXXXX)
export XDG_RUNTIME_DIR="$test_runtime"
export XDG_STATE_HOME="$test_runtime/state"
export XDG_CONFIG_HOME="$test_runtime/config"
export XDG_DATA_HOME="$test_runtime/data"
export COMPUTER_USE_HEADLESS_TEST=1
cargo test --test headless -- --ignored --test-threads=1
```

测试覆盖连接隔离、授权前拒绝、暂停与撤销、过期引用、可见视图门控、真实 stdio MCP、Firefox 和编辑器的文本与快捷键、独立会话剪贴板隔离、尺寸变化和取消。

宿主 niri 的 Portal 采集、整机虚拟输入、分数缩放、多显示器和持续人工操作验收仍需在目标桌面上验证。双独立会话测试不能代替真实宿主的人工无干扰验收。已有实例的后台写操作在通过具体应用构建和控件动作测试前保持禁用。

参考协议：[Wayland 独立会话实践](https://github.com/any1/wayvnc/blob/master/FAQ.md)、[Portal ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)、[niri IPC](https://docs.rs/niri-ipc/26.4.0/niri_ipc/)。
