#!/usr/bin/env bash
# fm-buzz-lifecycle.sh - FirstMate-owned handoff before Buzz stops its primary.
#
# Usage: fm-buzz-lifecycle.sh prepare-stop|start|tick|serve|stop|status
#
# This command deliberately has a tiny, machine-readable contract for the
# Desktop parent process. It never kills a worker, removes a lock, or claims a
# worker was cancelled. It asks the existing supervision predicate whether
# work still needs a watcher and reports exactly one of:
#
#   safe                     no live work or registered external source
#   supervision-transferred  work remains and the watcher has a fresh beacon
#   refused: <reason>        work remains without healthy supervision
#
# The script is the only FirstMate lifecycle policy owner. Buzz may use the
# answer to decide whether stopping the ACP primary is safe, but must not
# recreate the predicate or mutate FirstMate state itself.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FM_ROOT="${FM_ROOT_OVERRIDE:-$(cd "$SCRIPT_DIR/.." && pwd)}"
FM_HOME="${FM_HOME:-${FM_ROOT_OVERRIDE:-$FM_ROOT}}"
STATE="${FM_STATE_OVERRIDE:-$FM_HOME/state}"

usage() {
  echo "usage: $(basename "$0") prepare-stop|start|tick|serve|stop|status" >&2
  exit 2
}

[ "$#" -eq 1 ] || usage

# The native ACP broker starts `serve` as its one tracked foreground child.
# Keep the public lifecycle endpoint here so the broker has no reason to
# replicate FirstMate's home/lease policy or create a shell background job.
case "$1" in
  start|tick|serve|stop|status)
    exec "$SCRIPT_DIR/fm-buzz-supervisor.sh" "$1"
    ;;
  prepare-stop) ;;
  *) usage ;;
esac

# shellcheck source=bin/fm-supervision-lib.sh
. "$SCRIPT_DIR/fm-supervision-lib.sh"

fm_supervision_status "$STATE"
if [ "$FM_SUP_NEEDED" != true ]; then
  printf 'safe\n'
  exit 0
fi

if [ "$FM_SUP_WATCHER_FRESH" = true ]; then
  printf 'supervision-transferred\n'
  exit 0
fi

# A Buzz supervisor can carry the watcher responsibility while the primary is
# stopped. Accept its beat only when the same broker identity currently owns the
# home-scoped supervisor lease; a plain touched file must never authorize a
# shutdown of live work.
buzz_supervisor_transfer_fresh() {
  local root lease beat pid home generation m age grace=${FM_GUARD_GRACE:-300} values
  root="$STATE/.buzz-supervisor"
  lease="$root/lease"
  beat="$root/beat"
  [ -f "$beat" ] && [ ! -L "$beat" ] || return 1
  [ -f "$lease" ] && [ ! -L "$lease" ] || return 1
  [ -n "${BUZZ_FIRSTMATE_SUPERVISOR_GENERATION:-}" ] || return 1
  case "$BUZZ_FIRSTMATE_SUPERVISOR_GENERATION" in ''|*[!0-9]*) return 1 ;; esac
  # shellcheck source=bin/fm-session-lock-lib.sh
  . "$SCRIPT_DIR/fm-session-lock-lib.sh"
  pid=$(fm_buzz_harness_pid) || return 1
  home=$(cd "$FM_HOME" 2>/dev/null && pwd -P) || return 1
  generation=$BUZZ_FIRSTMATE_SUPERVISOR_GENERATION
  # Do not accept a partially overwritten or duplicate-field lease.  The
  # transfer decision is an authority boundary, so it requires one exact
  # home/pid/generation tuple rather than independently finding a matching
  # value somewhere in an attacker-controlled file.
  values=$(awk -F= '
    $1 == "home" || $1 == "pid" || $1 == "generation" { if (seen[$1]++) bad=1; value[$1]=$2; next }
    $1 == "version" { if (seen[$1]++) bad=1; next }
    NF { bad=1 }
    END { if (bad || !("home" in value) || !("pid" in value) || !("generation" in value)) exit 1; print value["home"] "\t" value["pid"] "\t" value["generation"] }
  ' "$lease") || return 1
  [ "$values" = "$home"$'\t'"$pid"$'\t'"$generation" ] || return 1
  m=$(fm_sup_stat_mtime "$beat") || return 1
  age=$(( $(date +%s) - m ))
  [ "$age" -ge 0 ] && [ "$age" -lt "$grace" ]
}

if buzz_supervisor_transfer_fresh; then
  printf 'supervision-transferred\n'
  exit 0
fi

printf 'refused: active work needs supervision; watcher beacon is %s\n' "$FM_SUP_BEACON_DESC"
