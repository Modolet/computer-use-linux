#!/usr/bin/env bash
# @file run-niri.sh
# @brief 在私有 D-Bus 与嵌套 niri 中执行整机及后台应用验收
# @author modolet <y@xxyx.io>
# @date 2026-09-07
set -euo pipefail
if [[ "${1:-}" == inner ]]; then
	exec >"$XDG_RUNTIME_DIR/test.log" 2>&1
	export COMPUTER_USE_NIRI_TEST=1 GTK_A11Y=atspi GDK_BACKEND=wayland GSK_RENDERER=cairo
	export XDG_CURRENT_DESKTOP=niri
	dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP NIRI_SOCKET GSK_RENDERER GTK_A11Y
	unset NO_AT_BRIDGE
	"$TEST_ATSPI_LAUNCHER" --launch-immediately &
	a11y_pid=$!
	trap 'kill "$a11y_pid" 2>/dev/null || true' EXIT
	sleep 0.3
	"$TEST_ATSPI_REGISTRY" &
	set +e
	cargo build --example input-probe || exit $?
	cargo test --test niri "${TEST_FILTER:-}" -- --ignored --nocapture --test-threads=1
	status=$?
	printf '%s' "$status" >"$XDG_RUNTIME_DIR/test-status"
	exit "$status"
fi
if [[ "${1:-}" != outer ]]; then
	test_runtime=$(mktemp -d /tmp/cn-XXXXXX)
	exec env -u WAYLAND_DISPLAY -u DISPLAY -u NIRI_SOCKET -u AT_SPI_BUS_ADDRESS XDG_RUNTIME_DIR="$test_runtime" XDG_STATE_HOME="$test_runtime/state" \
		XDG_CONFIG_HOME="$test_runtime/config" XDG_DATA_HOME="$test_runtime/data" \
		dbus-run-session -- bash "$0" outer
fi
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET NIRI_SOCKET SWAYSOCK AT_SPI_BUS_ADDRESS DBUS_STARTER_ADDRESS DBUS_STARTER_BUS_TYPE XDG_ACTIVATION_TOKEN DESKTOP_STARTUP_ID
printf 'output HEADLESS-1 mode 1600x1000\nxwayland disable\n' >"$XDG_RUNTIME_DIR/sway.config"
WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_RENDERER="${TEST_RENDERER:-pixman}" sway --config "$XDG_RUNTIME_DIR/sway.config" >"$XDG_RUNTIME_DIR/sway.log" 2>&1 &
outer_pid=$!
inner_pid=''
export PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR"
unset PIPEWIRE_REMOTE
pipewire >"$XDG_RUNTIME_DIR/pipewire.log" 2>&1 &
pipewire_pid=$!
trap '[[ -z "$inner_pid" ]] || kill "$inner_pid" 2>/dev/null || true; kill "$outer_pid" "$pipewire_pid" 2>/dev/null || true; wait 2>/dev/null || true' EXIT
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
printf 'animations { off; }\nhotkey-overlay { skip-at-startup; }\ndebug { dbus-interfaces-in-non-session-instances; }\n' >"$XDG_RUNTIME_DIR/niri.kdl"
LIBGL_ALWAYS_SOFTWARE="${TEST_SOFTWARE_GL:-1}" niri --config "$XDG_RUNTIME_DIR/niri.kdl" -- bash "$0" inner >"$XDG_RUNTIME_DIR/niri.log" 2>&1 &
inner_pid=$!
for _ in {1..6000}; do
	if [[ -f "$XDG_RUNTIME_DIR/test-status" ]]; then
		cat "$XDG_RUNTIME_DIR/test.log"
		exit "$(cat "$XDG_RUNTIME_DIR/test-status")"
	fi
	if ! kill -0 "$inner_pid" 2>/dev/null; then
		cat "$XDG_RUNTIME_DIR/test.log" 2>/dev/null || true
		cat "$XDG_RUNTIME_DIR/niri.log"
		exit 1
	fi
	sleep 0.1
done
cat "$XDG_RUNTIME_DIR/niri.log"
exit 1
