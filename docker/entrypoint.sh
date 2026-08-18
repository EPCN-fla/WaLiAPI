#!/bin/sh
set -eu

# Web 管理面板模式下不再需要 VNC/noVNC；仅保留 Xvfb 虚拟显示，
# 供 Tauri 二进制完成 GTK/WebKit 初始化（窗口默认隐藏，不出现在任何界面）。
DISPLAY_NUM="${DISPLAY_NUM:-99}"
DISPLAY=":${DISPLAY_NUM}"
VNC_RESOLUTION="${VNC_RESOLUTION:-1280x860}"
VNC_DEPTH="${VNC_DEPTH:-24}"
APP_HOME="${APP_HOME:-/usr/local/bin/waliapi}"

export DISPLAY
export WALIAPI_SERVER_HOST="${WALIAPI_SERVER_HOST:-0.0.0.0}"
export WALIAPI_SERVER_PORT="${WALIAPI_SERVER_PORT:-8777}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-/data}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-waliapi}"
export NO_AT_BRIDGE="${NO_AT_BRIDGE:-1}"
export WEBKIT_DISABLE_COMPOSITING_MODE="${WEBKIT_DISABLE_COMPOSITING_MODE:-1}"
export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"

mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

cleanup() {
  if [ "${APP_PID:-}" ]; then
    kill "$APP_PID" 2>/dev/null || true
  fi
  if [ "${XVFB_PID:-}" ]; then
    kill "$XVFB_PID" 2>/dev/null || true
  fi
}

trap cleanup INT TERM EXIT

Xvfb "$DISPLAY" -screen 0 "${VNC_RESOLUTION}x${VNC_DEPTH}" -ac -nolisten tcp >/tmp/xvfb.log 2>&1 &
XVFB_PID=$!

i=0
while [ ! -S "/tmp/.X11-unix/X${DISPLAY_NUM}" ] && [ "$i" -lt 100 ]; do
  sleep 0.1
  i=$((i + 1))
done

if [ ! -S "/tmp/.X11-unix/X${DISPLAY_NUM}" ]; then
  echo "Xvfb did not start on ${DISPLAY}" >&2
  exit 1
fi

if [ "$#" -eq 0 ]; then
  set -- "$APP_HOME"
fi

dbus-run-session -- "$@" 2>&1 &
APP_PID=$!

status=0
wait "$APP_PID" || status=$?
cleanup
trap - INT TERM EXIT
exit "$status"
