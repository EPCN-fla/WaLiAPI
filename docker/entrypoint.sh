#!/bin/sh
set -eu

DISPLAY_NUM="${DISPLAY_NUM:-99}"
DISPLAY=":${DISPLAY_NUM}"
VNC_RESOLUTION="${VNC_RESOLUTION:-1280x860}"
VNC_DEPTH="${VNC_DEPTH:-24}"
VNC_PORT="${VNC_PORT:-5900}"
NOVNC_PORT="${NOVNC_PORT:-6080}"
NOVNC_WEB="${NOVNC_WEB:-/usr/share/novnc}"
APP_HOME="${APP_HOME:-/usr/local/bin/waliapi}"
WALIAPI_ENABLE_UI="${WALIAPI_ENABLE_UI:-0}"

export DISPLAY
export WALIAPI_SERVER_HOST="${WALIAPI_SERVER_HOST:-0.0.0.0}"
export WALIAPI_SERVER_PORT="${WALIAPI_SERVER_PORT:-8777}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-/data}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-waliapi}"
export GDK_BACKEND="${GDK_BACKEND:-x11}"
export NO_AT_BRIDGE="${NO_AT_BRIDGE:-1}"
export WEBKIT_DISABLE_COMPOSITING_MODE="${WEBKIT_DISABLE_COMPOSITING_MODE:-1}"
export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"

is_true() {
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes|on) return 0 ;;
    *) return 1 ;;
  esac
}

mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

cleanup() {
  if [ "${APP_PID:-}" ]; then
    kill "$APP_PID" 2>/dev/null || true
  fi
  if [ "${WEB_PID:-}" ]; then
    kill "$WEB_PID" 2>/dev/null || true
  fi
  if [ "${VNC_PID:-}" ]; then
    kill "$VNC_PID" 2>/dev/null || true
  fi
  if [ "${WM_PID:-}" ]; then
    kill "$WM_PID" 2>/dev/null || true
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

if is_true "$WALIAPI_ENABLE_UI"; then
  if command -v fluxbox >/dev/null 2>&1; then
    fluxbox >/tmp/fluxbox.log 2>&1 &
    WM_PID=$!
  fi

  x11vnc \
    -display "$DISPLAY" \
    -rfbport "$VNC_PORT" \
    -listen 0.0.0.0 \
    -forever \
    -shared \
    -nopw \
    -noxdamage \
    -repeat \
    -nap \
    -wait 80 \
    -defer 15 \
    >/tmp/x11vnc.log 2>&1 &
  VNC_PID=$!

  websockify --web="$NOVNC_WEB" "$NOVNC_PORT" "127.0.0.1:${VNC_PORT}" >/tmp/websockify.log 2>&1 &
  WEB_PID=$!
fi

if [ "$#" -eq 0 ]; then
  set -- "$APP_HOME"
fi

dbus-run-session -- "$@" >/tmp/waliapi.log 2>&1 &
APP_PID=$!

status=0
wait "$APP_PID" || status=$?
cleanup
trap - INT TERM EXIT
exit "$status"
