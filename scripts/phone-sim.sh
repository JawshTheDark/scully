#!/usr/bin/env bash
# Run Scully at FuriPhone geometry in a nested compositor — a FuriOS
# simulator in the only sense that matters for this app: the same binary, a
# 720-logical-pixel-wide Wayland output, and (when phosh is installed) the
# same shell the phone runs.
#
# A real FuriOS image cannot boot on x86: it is a Halium kernel built for the
# FLX1's SoC. What bit us on the actual phone was never the architecture —
# it was Phosh behaviour (maximise policy, app_id → icon lookup) and the
# narrow layout, all of which reproduce nested.
#
#   scripts/phone-sim.sh            # phosh if installed, else kwin nested
#   scripts/phone-sim.sh --kwin     # force the kwin fallback
#
# FLX1 logical geometry: 720x1520 (2460x1080 physical at ~1.5x, minus bars).
set -eu
cd "$(dirname "$0")/.."

# Not sudo. A nested compositor attaches to YOUR session's Wayland socket via
# XDG_RUNTIME_DIR, and root has neither — kwin segfaults after failing to
# bind. Nothing here needs privileges.
if [ "$(id -u)" = 0 ]; then
  echo "phone-sim: run this as your normal user, not with sudo" >&2
  exit 1
fi

W=720 H=1440
BIN=${SCULLY_BIN:-target/debug/scully}
[ -x "$BIN" ] || cargo build -p scully

# A dev instance beside your real session, never inside it: separate app id
# AND an isolated profile, so the sim cannot silently reuse the real saved
# token. `--real` opts into the normal profile deliberately.
export SCULLY_APP_ID=${SCULLY_APP_ID:-io.jawsh.Scully.Sim}

mode=auto real=0
for arg in "$@"; do
  case "$arg" in
    --kwin) mode=kwin ;;
    --real) real=1 ;;
  esac
done
if [ "$real" = 0 ]; then
  export XDG_CONFIG_HOME="$PWD/.sim-profile/config"
  export XDG_DATA_HOME="$PWD/.sim-profile/data"
  mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"
  echo ">> isolated profile at .sim-profile/ (pass --real for your session)"
fi

if [ "$mode" != "kwin" ] && command -v phosh >/dev/null 2>&1; then
  # Nested phosh: phoc drives a windowed wlroots output; phosh is the shell.
  # This is the phone's actual UX — app drawer, top bar, maximise policy.
  ini=$(mktemp --suffix=.phoc.ini)
  trap 'rm -f "$ini"' EXIT
  cat > "$ini" <<EOF
[output:WL-1]
mode = ${W}x${H}
EOF
  echo ">> nested phosh at ${W}x${H} — launch Scully from the app drawer,"
  echo ">>   or: WAYLAND_DISPLAY=wayland-99 $BIN"
  exec phoc -C "$ini" -S wayland-99 -E phosh
fi

# Fallback: kwin nested. No phone shell, but the output is phone-sized, so
# the narrow layout, back navigation and touch targets are all exercised.
# kwin is started bare and Scully launched at its socket ourselves — kwin's
# positional-application launching silently does nothing in a plain nested
# start (no session manager), which is how the first version of this script
# produced an empty grey window.
echo ">> nested kwin at ${W}x${H} (install phosh+phoc for the full shell)"
kwin_wayland --width "$W" --height "$H" --socket scully-sim --no-lockscreen &
KWIN=$!
trap 'kill "$KWIN" 2>/dev/null' EXIT
for _ in $(seq 1 50); do
  [ -S "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/scully-sim" ] && break
  kill -0 "$KWIN" 2>/dev/null || { echo "kwin died"; exit 1; }
  sleep 0.2
done
WAYLAND_DISPLAY=scully-sim exec "$BIN"
