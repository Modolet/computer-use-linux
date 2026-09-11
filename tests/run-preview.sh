#!/usr/bin/env bash
# @file run-preview.sh
# @brief 在私有桌面测量高清实时采集，不修改宿主桌面或个人应用数据
# @author modolet <y@xxyx.io>
# @date 2026-09-11
set -euo pipefail
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
cargo build --example preview-probe
COMPUTER_USE_PREVIEW_PROBE="$(readlink -f target/debug/examples/preview-probe)"
export COMPUTER_USE_PREVIEW_PROBE
test_runtime=$(mktemp -d /tmp/cp-XXXXXX)
mkdir -p "$test_runtime"/{home,config,data,state,cache}
exec env HOME="$test_runtime/home" XDG_RUNTIME_DIR="$test_runtime" \
	XDG_CONFIG_HOME="$test_runtime/config" XDG_DATA_HOME="$test_runtime/data" \
	XDG_STATE_HOME="$test_runtime/state" XDG_CACHE_HOME="$test_runtime/cache" \
	GDK_BACKEND=wayland GTK_A11Y=none \
	cargo test --test preview -- --ignored --nocapture --test-threads=1
