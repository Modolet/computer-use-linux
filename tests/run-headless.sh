#!/usr/bin/env bash
# @file run-headless.sh
# @brief 用临时个人数据验证共享数据与独立图形会话，不修改真实个人配置
# @author modolet <y@xxyx.io>
# @date 2026-09-07
set -euo pipefail
# Keep build caches outside the temporary application HOME.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"

if [[ "${COMPUTER_USE_HEADLESS_TEST:-}" != 1 ]]; then
	test_runtime=$(mktemp -d /tmp/ch-XXXXXX)
	mkdir -p "$test_runtime"/{home,config,data,state,cache}
	# This is fixture configuration, never written by the application launcher.
	mkdir -p "$test_runtime/home/.mozilla/firefox/default"
	printf '[General]\nStartWithLastProfile=1\n[Profile0]\nName=default\nIsRelative=1\nPath=default\nDefault=1\n' >"$test_runtime/home/.mozilla/firefox/profiles.ini"
	printf 'user_pref("browser.shell.checkDefaultBrowser", false);\nuser_pref("browser.aboutwelcome.enabled", false);\nuser_pref("browser.startup.homepage_override.mstone", "ignore");\n' >"$test_runtime/home/.mozilla/firefox/default/user.js"
	exec env HOME="$test_runtime/home" XDG_RUNTIME_DIR="$test_runtime" \
		XDG_CONFIG_HOME="$test_runtime/config" XDG_DATA_HOME="$test_runtime/data" \
		XDG_STATE_HOME="$test_runtime/state" XDG_CACHE_HOME="$test_runtime/cache" \
		COMPUTER_USE_HEADLESS_TEST=1 bash "$0" "$@"
fi

test "$HOME" = "$XDG_RUNTIME_DIR/home"
export GDK_BACKEND=wayland GTK_A11Y=none GSK_RENDERER=cairo
if [[ -n "${COMPUTER_USE_GENERIC_TEST_DESKTOP:-}" ]]; then
	cargo test --test headless "$@" -- --ignored --nocapture --test-threads=1
else
	cargo test --test headless "$@" -- --ignored --nocapture --test-threads=1 \
		--skip installed_desktop_application_shares_data_with_private_graphics
fi
