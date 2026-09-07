#!/usr/bin/env bash
# @file run-multi-output.sh
# @brief 私有双显示器 Wayland 与 niri IPC 数据夹具验收
# @author modolet <y@xxyx.io>
# @date 2026-09-07
set -euo pipefail
if [[ "${COMPUTER_USE_MULTI_TEST:-}" != 1 ]]; then
	test_runtime=$(mktemp -d /tmp/cm-XXXXXX)
	exec env -u WAYLAND_DISPLAY -u DISPLAY -u NIRI_SOCKET -u AT_SPI_BUS_ADDRESS \
		XDG_RUNTIME_DIR="$test_runtime" XDG_STATE_HOME="$test_runtime/state" \
		XDG_CONFIG_HOME="$test_runtime/config" XDG_DATA_HOME="$test_runtime/data" \
		COMPUTER_USE_MULTI_TEST=1 dbus-run-session -- bash "$0"
fi
unset WAYLAND_DISPLAY DISPLAY WAYLAND_SOCKET NIRI_SOCKET SWAYSOCK
printf 'output HEADLESS-1 mode 1280x800 position 0 0\noutput HEADLESS-2 mode 1000x900 position 1280 0 scale 1.25\nxwayland disable\n' >"$XDG_RUNTIME_DIR/sway.config"
WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=2 WLR_RENDERER=pixman sway --config "$XDG_RUNTIME_DIR/sway.config" >"$XDG_RUNTIME_DIR/sway.log" 2>&1 &
test_compositor=$!
trap 'kill "$test_compositor" 2>/dev/null || true; wait "$test_compositor" 2>/dev/null || true' EXIT
for _ in {1..100}; do
	for socket in "$XDG_RUNTIME_DIR"/wayland-*; do
		if [[ -S "$socket" ]]; then
			export WAYLAND_DISPLAY="$socket"
			break 2
		fi
	done
	sleep 0.05
done
test -n "${WAYLAND_DISPLAY:-}"
export SWAYSOCK="$XDG_RUNTIME_DIR/sway-ipc.$UID.$test_compositor.sock"
export NIRI_SOCKET="$XDG_RUNTIME_DIR/niri-fixture.sock"
export GTK_A11Y=none GDK_BACKEND=wayland GSK_RENDERER=cairo
cargo build --example input-probe
cargo test --test multi_output -- --ignored --nocapture --test-threads=1
