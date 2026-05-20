#!/usr/bin/env bash
# eli gateway daemon — runs `eli gateway` inside a detached tmux session and
# tees the pane to ~/.eli/logs/gateway.log so the listener survives shell exits.
set -euo pipefail

SESSION="${ELI_GATEWAY_SESSION:-eli-gateway}"
LOG_DIR="${ELI_LOG_DIR:-$HOME/.eli/logs}"
LOG_FILE="$LOG_DIR/gateway.log"
ELI_BIN="${ELI_BIN:-eli}"

mkdir -p "$LOG_DIR"

usage() {
  cat <<EOF
Usage: $(basename "$0") <command>

Commands:
  start     Start the gateway in a detached tmux session (idempotent)
  stop      Kill the tmux session
  restart   Stop then start
  status    Show whether the session is alive
  attach    Attach to the running session (Ctrl-b d to detach)
  logs      Tail the gateway log
EOF
}

session_alive() {
  tmux has-session -t "$SESSION" 2>/dev/null
}

cmd_start() {
  if session_alive; then
    echo "gateway already running (tmux session: $SESSION)"
    return 0
  fi

  if ! command -v "$ELI_BIN" >/dev/null 2>&1; then
    echo "error: '$ELI_BIN' not found in PATH" >&2
    return 1
  fi

  tmux new-session -d -s "$SESSION" -n gateway \
    "exec '$ELI_BIN' gateway"
  tmux pipe-pane -o -t "$SESSION:gateway" "cat >> '$LOG_FILE'"

  sleep 1
  if session_alive; then
    echo "gateway started (tmux: $SESSION, log: $LOG_FILE)"
  else
    echo "gateway failed to start; check $LOG_FILE" >&2
    return 1
  fi
}

cmd_stop() {
  if session_alive; then
    tmux kill-session -t "$SESSION"
    echo "gateway stopped"
  else
    echo "gateway not running"
  fi
}

cmd_status() {
  if session_alive; then
    echo "running (tmux: $SESSION)"
    tmux list-panes -t "$SESSION" -F '#{pane_pid} #{pane_current_command}'
  else
    echo "stopped"
    return 1
  fi
}

cmd_attach() {
  if ! session_alive; then
    echo "gateway not running" >&2
    return 1
  fi
  tmux attach-session -t "$SESSION"
}

cmd_logs() {
  if [ ! -f "$LOG_FILE" ]; then
    echo "no log file yet at $LOG_FILE" >&2
    return 1
  fi
  tail -n 200 -f "$LOG_FILE"
}

case "${1:-}" in
  start)   cmd_start ;;
  stop)    cmd_stop ;;
  restart) cmd_stop || true; cmd_start ;;
  status)  cmd_status ;;
  attach)  cmd_attach ;;
  logs)    cmd_logs ;;
  -h|--help|"") usage ;;
  *)       usage; exit 1 ;;
esac
