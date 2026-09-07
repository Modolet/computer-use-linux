#!/usr/bin/env bash
# @file run-visual.sh
# @brief 在独立测试桌面上验证 GTK 应用视图，不操作宿主桌面
# @author modolet <y@xxyx.io>
# @date 2026-09-07
set -euo pipefail

if [[ "${COMPUTER_USE_UI_TEST:-}" != 1 ]]; then
	test_runtime=$(mktemp -d /tmp/cv-XXXXXX)
	exec env XDG_RUNTIME_DIR="$test_runtime" \
		XDG_STATE_HOME="$test_runtime/state" XDG_CONFIG_HOME="$test_runtime/config" \
		XDG_DATA_HOME="$test_runtime/data" COMPUTER_USE_UI_TEST=1 \
		dbus-run-session -- bash "$0"
fi

printf 'output HEADLESS-1 mode 1280x900\nxwayland disable\nfont monospace 10\n' >"$XDG_RUNTIME_DIR/visual.config"
WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_RENDERER=pixman \
	sway --config "$XDG_RUNTIME_DIR/visual.config" >"$XDG_RUNTIME_DIR/visual.log" 2>&1 &
test_compositor=$!
trap 'kill "$test_compositor" 2>/dev/null || true; wait "$test_compositor" 2>/dev/null || true' EXIT

unset WAYLAND_DISPLAY DISPLAY WAYLAND_SOCKET NIRI_SOCKET SWAYSOCK
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
export GDK_BACKEND=wayland GTK_A11Y=none GTK_USE_PORTAL=0 GSK_RENDERER=cairo
unset DISPLAY WAYLAND_SOCKET NIRI_SOCKET SWAYSOCK
cargo test --lib visible_window_allows_ai_and_closing_pauses -- --ignored --nocapture --test-threads=1
