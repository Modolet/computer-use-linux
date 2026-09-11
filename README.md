# Linux Computer Use MCP

面向 NixOS、niri 的本地 Rust stdio MCP。权限在本地中文 GTK 窗口中确认，MCP 没有批准授权、任意命令或任意 D-Bus 调用接口。

**独立应用在桌面上有持续更新的可见窗口。** 应用运行于私有 Wayland / XWayland 和 D-Bus 会话；桌面上的 GTK 窗口显示它的实时画面。查看画面不暂停 AI，不向宿主注入鼠标、键盘或剪贴板操作。勾选“手动接管”会暂停 AI；关闭最后一个应用视图也会暂停 AI。用户接管时可以点击、拖动、滚动、输入中文，并处理独立会话中的额外窗口；这些窗口不会因此获得 AI 授权。内部 Sway 使用 headless 渲染，用户使用的应用窗口并不隐藏。

| 模式 | 当前能力 | 授权和输入边界 |
| --- | --- | --- |
| 单应用：独立实例 | 已安装 Wayland / X11 图形应用的截图、点击、拖动、滚动、组合键和 Unicode 文本 | 申请中指定应用，本地只需允许或拒绝；仅隔离图形会话，沿用个人 HOME、配置、登录状态和应用数据，默认显示实时窗口 |
| 整个电脑 | niri 窗口切换、显示器截图；合成器开放协议时提供虚拟输入 | 使用真实桌面；最多一个整机会话执行输入 |

## 实时预览

新启动的独立会话默认优先使用 GPU GLES2 渲染。预览通过 GBM 分配独立 GPU 缓冲区，使用 Wayland screencopy 写入，再由 GTK 导入 DMA-BUF 显示；正常路径不经过 PNG、Base64、CPU 像素读回或重新上传。MCP 的 `observe` 仍按需返回 PNG，与持续预览各用独立连接。

窗口调整大小后，虚拟输出会匹配视图的实际物理像素和显示器缩放，支持 1.25 等分数缩放。尺寸变化合并 120 ms，避免拖动边框时反复分配；每个视图最多保留 4 个采集缓冲区和一个待显示帧。画面无变化时等待 damage；窗口隐藏或关闭立即取消采集，最后一个视图隐藏时暂停 AI，重新显示不会擅自恢复授权。旧观察和手动输入坐标在尺寸变化后失效。

状态栏显示物理分辨率、渲染器、传输方式（`DMA-BUF` / `SHM`）和更新帧率。目标刷新率为 60 FPS，静止画面显示“静止”。画质不做有损压缩，最高支持约 1600 万像素；实际帧率取决于 GPU、驱动、应用负载和宿主显示器刷新率。缺少兼容 DMA-BUF 格式时回退到有界共享内存帧，GPU 渲染不可用时回退到 CPU Pixman，并在界面标明。

可通过守护进程环境变量配置：

| 变量 | 默认与用途 |
| --- | --- |
| `COMPUTER_USE_RENDERER` | `auto`：优先 GPU，失败回退；`gles2`：要求 GPU，失败报错；`pixman`：CPU 兼容模式 |
| `COMPUTER_USE_RENDER_DRM_DEVICE` | 自动探测可访问的 `/dev/dri/renderD*`；可指定某个渲染节点 |

升级不会改变已运行实例的渲染器；旧实例仍可接管，新启动的实例使用新的 GPU 路径。Home Manager 服务重启只停止守护进程，保留应用及未保存内容，且不恢复 AI 授权。

## 安装和启动

```sh
nix build
./result/bin/computer-use-linux daemon
```

守护进程启动后等待请求；打开管理面板：

```sh
./result/bin/computer-use-linux manage
```

守护进程需要从当前图形会话继承 `XDG_RUNTIME_DIR`、`WAYLAND_DISPLAY`、`DBUS_SESSION_BUS_ADDRESS` 和 `NIRI_SOCKET`。整机模式需要 niri 的窗口与显示器 IPC 接口。

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

1. 模型调用 `request_session`，例如 `{"scope":"application","mode":"isolated","application":"kitty.desktop"}`。立即返回申请编号和 pending 状态，不返回桌面信息，也不启动应用。
2. 本地面板显示申请的应用名称、启动程序、桌面条目预设参数和控制能力。用户只需点击“允许并启动”或“拒绝”，不用再选择应用。允许后自动启动独立实例、打开实时窗口并恢复 AI；没有二次授权按钮。
3. 如果准备期间出现新的授权申请，应用先保持暂停，避免隐藏新的请求。整机模式仍在本地确认范围后，通过“隐藏面板并恢复 AI”开始控制。授权面板会先暂停所有自动输入，并等待正在执行的输入释放按键后才显示授权按钮；单纯查看应用窗口不会暂停。
4. 模型通过 `session_status` 获取状态、能力和目标；调用 `observe` 获取图像和观察编号；每次 `act` 都必须带上最新的 `session_id`、`observation_id` 和 `target`。
5. 随时打开 `manage` 暂停或撤销，或执行 `pause-all`。恢复只能通过本地面板完成。

独立实例的 `application` 必填，可使用已安装 `.desktop` ID（推荐，例如 `kitty.desktop`）、不带后缀的 ID，或能唯一匹配的应用名称。服务从当前桌面的 GIO 应用数据库读取启动定义；MCP 不接收可执行路径、启动参数或任意 shell 命令，也不会向模型返回已安装应用列表。名字不明确、没有可执行启动条目时拒绝请求，不要求用户改选应用。Firefox 和 GNOME Text Editor 保留专用启动适配，软件包提供的这两个程序也可直接用 `firefox` 和 `gnome-text-editor` 申请。

通用入口通过桌面条目的 `Exec` 直接启动进程，不调用宿主 D-Bus 激活；文件和 URL 占位符在空启动时移除。应用继承守护进程的 HOME 和 XDG 配置、数据、状态及缓存目录，与日常应用使用同一份数据。每个会话启动私有 XWayland，兼容 WPS 等 X11 应用；Qt 优先使用 Wayland，缺少该插件时使用私有 X11。X11 窗口使用 XRes 返回的真实客户端进程身份核验，不信任可伪造的窗口 PID 属性；绕过窗口管理器的弹出窗口也必须通过归属检查。只提供 D-Bus 激活、需要外部终端的条目，或启动器退出后无法核实窗口归属的应用会报不支持/启动失败，不回退到宿主桌面。外部应用窗口仍不自动获得 AI 权限。通用入口不等于每个应用都已经验证兼容。

申请组合：`application/isolated`、`desktop/desktop`。坐标以观察返回的图像像素为准；返回目标包含像素尺寸、缩放和显示器/应用标签。一次动作消耗一次观察。尺寸、布局或实例变化后必须重新观察。显示器移除、重新连接时，旧显示器引用也会失效。`scroll` 的正 `dy` 向下、正 `dx` 向右；约 15 个滚轮单位对应一个刻度，实际页面滚动距离由应用决定。

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

会话元数据位于 `$XDG_STATE_HOME/computer-use-linux`，未设置时使用 `~/.local/state/computer-use-linux`。应用使用原有 HOME、XDG 配置、数据、状态和缓存目录，不再为新会话创建个人数据副本；应用所做的修改会写回日常使用的同一份数据。Wayland、X11、图形会话 D-Bus 和运行时端点独立，焦点、键鼠输入和图形会话剪贴板互不干扰。这不是文件系统沙箱。

Firefox 沿用正常的个人 profile 选择，不再生成 profile 或改写 `user.js`。为避免把窗口转交到宿主图形会话，仍使用 `--no-remote --new-instance`。若 Firefox 或其他应用锁定个人配置，只能在占用实例退出后重试；不会复制配置、移除锁或回退到控制宿主窗口。数据共享不意味着所有应用支持并发打开或即时刷新。

旧版本保留的实例仍使用启动时的旧配置，管理面板可重新打开它们供本地接管；新行为适用于升级后新启动的实例。旧 `profiles/` 数据不会自动迁移或删除。

撤销只停止控制，不强制关闭有未保存内容的独立应用；应用保留供本地接管。独立实例限制桌面观察和输入范围，仍然拥有原来的文件、网络权限和同一 Unix 用户权限，不构成针对恶意应用的文件系统或进程安全沙箱。

## 验证

```sh
nix develop --command cargo fmt --check
nix develop --command cargo check
nix develop --command cargo test
nix develop --command cargo clippy --all-targets -- -D warnings
```

真实独立会话测试默认忽略，显式运行：

```sh
nix develop --command bash tests/run-headless.sh
```

测试脚本为整个测试进程设置临时 HOME 和 XDG 数据目录，模拟个人环境，防止验收修改真实个人数据。被测应用必须沿用这套环境，仅图形会话另建。脚本仅启动测试桌面，不向宿主桌面注入输入。

测试覆盖连接隔离、授权前拒绝、暂停与撤销、过期引用、可见视图门控、真实 stdio MCP、Firefox 和编辑器的文本与快捷键、独立会话剪贴板隔离、尺寸变化和取消。

可见窗口验收使用另一套专用测试桌面：`nix develop --command bash tests/run-visual.sh`。验证 GTK 窗口确实映射、实时画面显示期间 AI 可以继续输入，以及关闭窗口后自动暂停；同时验证独立实例申请没有应用选择器，且一次本地允许即可启动可见窗口并恢复控制。设置 `COMPUTER_USE_UI_PNG` 为绝对路径可保存该测试桌面的截图。

实时预览验收（需要可访问的 GPU，运行于私有测试桌面）：

```sh
nix develop --command env COMPUTER_USE_RENDERER=gles2 COMPUTER_USE_MIN_FPS=40 bash tests/run-preview.sh
nix develop --command env COMPUTER_USE_RENDERER=gles2 COMPUTER_USE_TEST_RENDERER=gles2 COMPUTER_USE_TEST_GSK_RENDERER=gl COMPUTER_USE_EXPECT_DMA=1 COMPUTER_USE_MIN_FPS=40 bash tests/run-visual.sh
```

测试覆盖 1080p、2400×1350 / 1.25 倍缩放、4K / 2 倍缩放，明确验证显示对象为 `GdkDmabufTexture`，检查像素颜色、旧帧租约不被覆盖、旧坐标失效、静止画面取消、隐藏与恢复。GPU 测试设置最低 40 FPS；CPU 兼容测试只检查画面与生命周期正确性。帧率统计表示 GTK 每秒接受的新画面数量，不等同于物理面板扫描或端到端延迟测量。

真实 niri 后端验收：`nix develop --command bash tests/run-niri.sh`。测试在私有 Sway 内运行 niri，不向宿主发送输入；覆盖整机按键、点击、拖动、滚动、分数缩放、过期观察拒绝以及取消后的按键释放。

双显示器验收：`nix develop --command bash tests/run-multi-output.sh`。使用真实的两块 Wayland 输出和输入探针，niri IPC 元数据使用测试夹具。验证不同分辨率、1.25 缩放、输入仅送往指定显示器、断开和重连后的引用失效；这不是物理双显示器硬件测试。独立 Firefox 测试逐一重载页面，并验证全部八种旋转和翻转后的像素点击及滚轮响应。

第三方独立应用验收：设置 `COMPUTER_USE_GENERIC_TEST_DESKTOP` 为本机 `kitty.desktop` 的绝对路径，运行 `nix develop --command bash tests/run-headless.sh installed_desktop_application_shares_data_with_private_graphics`。验证 HOME 和所有 XDG 数据目录与调用者一致、原有数据可读写、Wayland/D-Bus 端点独立，以及会话元数据恢复。默认测试脚本跳过这项需要本机桌面条目的验收。

X11 兼容验收：`nix develop --command bash tests/run-x11.sh`。使用临时个人数据启动本机 WPS 和 X11 模式的 GNOME Text Editor，检查截图、真实键盘输入、中文与剪贴板隔离、伪造 `_NET_WM_PID` 的普通窗口和绕过窗口管理器的弹窗被拒绝、尺寸变化及显示服务退出。WPS 测试需要本机已安装 `wps-office-et.desktop`，初次启动停留在许可窗口，不修改个人许可或账户状态。

X11 预览按物理像素调整独立输出，合成器缩放固定为 1，避免将低分辨率 X11 图像放大而模糊。高 DPI 下应用控件可能较小，可通过应用自身的缩放设置调整；原生 Wayland 应用仍使用预览窗口的缩放比例。

测试结果以各命令的断言和 `PASS` 输出为准。

参考协议：[Wayland 独立会话实践](https://github.com/any1/wayvnc/blob/master/FAQ.md)、[niri IPC](https://docs.rs/niri-ipc/26.4.0/niri_ipc/)。
