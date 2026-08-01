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

# Leftovers from a previous run poison a new one twice over: a stale nested
# compositor still owns the socket name (kwin then falls back to WL-0 and the
# new Scully can't find it), and a stale sim Scully still owns the D-Bus app
# id (GApplication hands the launch off to it, and its window pops up on the
# HOST session instead of in the phone frame). Sweep both.
pkill -f "kwin_wayland --width ${W}" 2>/dev/null || true
for pid in $(pgrep -f "scully" || true); do
  if tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -q "^SCULLY_APP_ID=io.jawsh.Scully.Sim"; then
    kill "$pid" 2>/dev/null || true
  fi
done

# A dev instance beside your real session, never inside it: separate app id
# AND an isolated profile, so the sim cannot silently reuse the real saved
# token. `--real` opts into the normal profile deliberately. The id is
# per-run (PID suffix) so even a survivor of the sweep can't capture us.
SOCKET="scully-sim-$$"
export SCULLY_APP_ID=${SCULLY_APP_ID:-io.jawsh.Scully.Sim.P$$}

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
  echo ">>   or: WAYLAND_DISPLAY=$SOCKET $BIN"
  exec phoc -C "$ini" -S "$SOCKET" -E phosh
fi

# Fallback: kwin nested. No phone shell, but the output is phone-sized, so
# the narrow layout, back navigation and touch targets are all exercised.
# kwin is started bare and Scully launched at its socket ourselves — kwin's
# positional-application launching silently does nothing in a plain nested
# start (no session manager), which is how the first version of this script
# produced an empty grey window.
echo ">> nested kwin at ${W}x${H} (install phosh+phoc for the full shell)"
kwin_wayland --width "$W" --height "$H" --socket "$SOCKET" --no-lockscreen &
KWIN=$!
# Not exec: the compositor must die WITH the app, or every quit strands a
# black phone-frame window on the desktop.
trap 'kill "$KWIN" 2>/dev/null' EXIT
for _ in $(seq 1 50); do
  [ -S "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/$SOCKET" ] && break
  kill -0 "$KWIN" 2>/dev/null || { echo "kwin died"; exit 1; }
  sleep 0.2
done
WAYLAND_DISPLAY="$SOCKET" "$BIN"
