# Linux Computer Use MCP

面向 NixOS、niri 的本地 Rust stdio MCP。权限在本地中文 GTK 窗口中确认，MCP 没有批准授权、任意命令或任意 D-Bus 调用接口。

**独立应用在桌面上有持续更新的可见窗口。** 应用运行于私有 Wayland 和 D-Bus 会话；桌面上的 GTK 窗口显示它的实时画面。查看画面不暂停 AI，不向宿主注入鼠标、键盘或剪贴板操作。勾选“手动接管”会暂停 AI；关闭最后一个应用视图也会暂停 AI。用户接管时可以点击、拖动、滚动、输入中文，并处理独立会话中的额外窗口；这些窗口不会因此获得 AI 授权。内部 Sway 使用 headless 渲染，用户使用的应用窗口并不隐藏。

| 模式 | 当前能力 | 授权和输入边界 |
| --- | --- | --- |
| 单应用：独立实例 | Firefox、GNOME Text Editor 的截图、点击、拖动、滚动、组合键和 Unicode 文本 | 单独图形会话、剪贴板和应用配置；默认显示实时应用窗口 |
| 单应用：已有实例 | AT-SPI 控件树；Portal/PipeWire 单窗口截图 | 绑定进程生命周期、D-Bus 唯一所有者和 niri 窗口；经验证的 GNOME Text Editor 构建支持后台多行文本编辑；其他应用或版本保持只读 |
| 整个电脑 | niri 窗口切换、显示器截图；合成器开放协议时提供虚拟输入 | 使用真实桌面；最多一个整机会话执行输入 |

已有实例不会因为不支持某个动作而改用全局输入。Portal 选择的窗口必须与应用选择一致，并通过 niri 的 cast 记录核实。无法取得 AT-SPI 身份或核实流归属时拒绝授权。不把“存在 EditableText/Action 接口”当作无干扰保证。已有实例禁止坐标输入、全局快捷键、切换焦点和剪贴板写入。

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

已有实例要求应用启用 AT-SPI，并且会话的 AT-SPI D-Bus 服务可用。若环境设置了 `GTK_A11Y=none`，可单独用 `GTK_A11Y=atspi gnome-text-editor --standalone` 启动待授权编辑器；不会替用户修改全局无障碍设置。只有名单中精确匹配的 Nix 构建允许写入：x86_64 的 GNOME Text Editor 50.1、GTK 4.22.4（应用可执行文件和运行时库的 store 路径均需匹配）。输入对象必须是少于 4096 字符、没有用户选区的多行文本框。目标窗口正在使用、出现额外窗口或布局变化时拒绝后台写入。

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
3. 点击“隐藏面板并恢复 AI”。授权面板会先暂停所有自动输入，并等待正在执行的输入释放按键后才显示授权按钮；单纯查看应用窗口不会暂停。
4. 模型通过 `session_status` 获取状态、能力和目标；调用 `observe` 获取图像、控件树和观察编号；每次 `act` 都必须带上最新的 `session_id`、`observation_id` 和 `target`。
5. 随时打开 `manage` 暂停或撤销，或执行 `pause-all`。恢复只能通过本地面板完成。

申请组合：`application/isolated`、`application/existing`、`desktop/desktop`。坐标以观察返回的图像像素为准；返回目标包含像素尺寸、缩放和显示器/应用标签。一次动作消耗一次观察。尺寸、布局或实例变化后必须重新观察。显示器移除、重新连接时，旧显示器引用也会失效。`scroll` 的正 `dy` 向下、正 `dx` 向右；约 15 个滚轮单位对应一个刻度，实际页面滚动距离由应用决定。

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

可见窗口验收使用另一套专用测试桌面：`nix develop --command bash tests/run-visual.sh`。验证 GTK 窗口确实映射、实时画面显示期间 AI 可以继续输入，以及关闭窗口后自动暂停。设置 `COMPUTER_USE_UI_PNG` 为绝对路径可保存该测试桌面的截图。

真实 niri 后端验收：`nix develop --command bash tests/run-niri.sh`。测试在私有 Sway 内运行 niri，不向宿主发送输入；覆盖整机按键、点击、拖动、滚动、分数缩放，以及后台八次中文改写与前台持续键鼠、普通剪贴板和主选择剪贴板操作并发执行。还验证用户选区、未验证版本、窗口尺寸变化、关闭重启和取消后的按键释放。

双显示器验收：`nix develop --command bash tests/run-multi-output.sh`。使用真实的两块 Wayland 输出和输入探针，niri IPC 元数据使用测试夹具。验证不同分辨率、1.25 缩放、输入仅送往指定显示器、断开和重连后的引用失效；这不是物理双显示器硬件测试。独立 Firefox 测试逐一重载页面，并验证全部八种旋转和翻转后的像素点击及滚轮响应。

Portal 实机验收：`nix develop --command cargo run --example portal-check`。在系统选择器中选择 `computer-use-portal-check.txt` 的具体窗口。该命令不会发送鼠标、键盘或剪贴板输入；窗口选择必须由本地用户完成。niri 的嵌套模式没有录屏所需的 GBM 设备，这项测试需要实际图形会话。采集协商线性 32 位 DMA-BUF，按实际分配大小、步长和偏移读取并转换为 PNG；驱动不支持线性映射或格式协商失败时返回后端错误。

已在本机真实 niri 会话通过 Portal 验收：取得 1314×1417 中文窗口 PNG、拒绝错误窗口绑定、持续采集静止画面，以及窗口关闭后拒绝返回缓存图像。

测试结果以各命令的断言和 `PASS` 输出为准。不支持的控件动作（包括通用 `invoke`）、未验证的 Firefox 已有实例写入和其他构建不会因测试不全而放开。

参考协议：[Wayland 独立会话实践](https://github.com/any1/wayvnc/blob/master/FAQ.md)、[Portal ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)、[niri IPC](https://docs.rs/niri-ipc/26.4.0/niri_ipc/)。
