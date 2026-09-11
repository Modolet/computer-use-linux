#!/usr/bin/env bash
# @file run-x11.sh
# @brief 使用临时个人数据验证 XWayland；不连接或控制宿主显示服务
# @author modolet <y@xxyx.io>
# @date 2026-09-11
set -euo pipefail
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
if [[ "${COMPUTER_USE_HEADLESS_TEST:-}" != 1 ]]; then
	cargo build --example input-probe
	export COMPUTER_USE_INPUT_PROBE
	COMPUTER_USE_INPUT_PROBE="$(readlink -f target/debug/examples/input-probe)"
	test_runtime=$(mktemp -d /tmp/cx-XXXXXX)
	mkdir -p "$test_runtime"/{home,config,data,state,cache}
	exec env HOME="$test_runtime/home" XDG_RUNTIME_DIR="$test_runtime" \
		XDG_CONFIG_HOME="$test_runtime/config" XDG_DATA_HOME="$test_runtime/data" \
		XDG_STATE_HOME="$test_runtime/state" XDG_CACHE_HOME="$test_runtime/cache" \
		COMPUTER_USE_HEADLESS_TEST=1 bash "$0" "$@"
fi
export GTK_A11Y=none GSK_RENDERER=cairo
cargo test --test x11 "$@" -- --ignored --nocapture --test-threads=1
