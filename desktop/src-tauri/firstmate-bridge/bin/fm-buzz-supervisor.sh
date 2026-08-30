#!/usr/bin/env bash
# fm-buzz-supervisor.sh - broker-attested, home-scoped supervision for a
# Buzz-managed FirstMate.
#
# The broker runs `serve` as one tracked foreground child.  This script never
# backgrounds a shell child: when that child dies the broker observes it and
# starts a higher generation.  `start` and `tick` are deliberately exposed for
# deterministic broker health checks and tests.
#
# Status protocol (strict on purpose):
#   needs-test: <task-id>  -> grant hook -> literal task resume
#   done: <task-id>        -> local merge hook -> release hook
# A task id must match the owning state/<task-id>.status filename.  Optional
# hook overrides are absolute regular executables supplied by the attested
# broker runtime.  Production has deliberate defaults: the supervisor's own
# heavy lease is the test-grant authority, and a finished local task is merged
# then torn down by the FirstMate lifecycle scripts.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FM_ROOT="${FM_ROOT_OVERRIDE:-$(cd "$SCRIPT_DIR/.." && pwd)}"
FM_HOME="${FM_HOME:-${FM_ROOT_OVERRIDE:-$FM_ROOT}}"
STATE="${FM_STATE_OVERRIDE:-$FM_HOME/state}"
ROOT="$STATE/.buzz-supervisor"
LEASE="$ROOT/lease"
CONTROL_LOCK="$ROOT/control.lock"
POLL_SECONDS="${FM_BUZZ_SUPERVISOR_POLL_SECONDS:-1}"

usage() {
  echo "usage: $(basename "$0") start|tick|serve|stop|status" >&2
  exit 2
}

die() { echo "fm-buzz-supervisor: $*" >&2; exit 1; }

canonical_dir() { (cd "$1" 2>/dev/null && pwd -P); }

require_broker() {
  [ -n "${BUZZ_FIRSTMATE_SUPERVISOR_GENERATION:-}" ] || die "missing broker-attested supervisor generation"
  case "$BUZZ_FIRSTMATE_SUPERVISOR_GENERATION" in *[!0-9]*|'') die "invalid broker-attested supervisor generation" ;; esac
  [ "$BUZZ_FIRSTMATE_SUPERVISOR_GENERATION" -gt 0 ] 2>/dev/null || die "invalid broker-attested supervisor generation"
  # shellcheck source=bin/fm-session-lock-lib.sh
  . "$SCRIPT_DIR/fm-session-lock-lib.sh"
  BROKER_PID=$(fm_buzz_harness_pid) || die "Buzz bridge does not attest this exact FM_HOME and cwd"
  BROKER_HOME=$(canonical_dir "$FM_HOME") || die "FM_HOME is not a canonical directory"
  [ "$BROKER_HOME" = "$(canonical_dir "$BUZZ_FIRSTMATE_HOME")" ] || die "Buzz bridge home mismatch"
  BROKER_GENERATION=$BUZZ_FIRSTMATE_SUPERVISOR_GENERATION
}

ensure_root() {
  [ -d "$STATE" ] || mkdir -p "$STATE" || die "cannot create state directory"
  [ -d "$ROOT" ] || mkdir -p "$ROOT" || die "cannot create supervisor directory"
  [ ! -L "$ROOT" ] || die "supervisor directory must not be a symlink"
  mkdir -p "$ROOT/cursors" "$ROOT/receipts" "$ROOT/heavy" || die "cannot initialize supervisor state"
  chmod 700 "$ROOT" "$ROOT/cursors" "$ROOT/receipts" "$ROOT/heavy" 2>/dev/null || true
}

with_control_lock() { # <command...>
  if ! mkdir "$CONTROL_LOCK" 2>/dev/null; then
    die "another supervisor lifecycle operation is in progress for this FM_HOME"
  fi
  # A subshell gives this short critical section an EXIT cleanup even if a
  # fail-closed helper exits early.  RETURN traps are intentionally avoided:
  # they fire for nested shell functions and can release this mutex too soon.
  (
    trap 'rmdir "$CONTROL_LOCK" 2>/dev/null || true' EXIT
    "$@"
  )
}

lease_value() { # <key>
  [ -f "$LEASE" ] && [ ! -L "$LEASE" ] || return 1
  awk -F= -v key="$1" '$1 == key { value=$0; sub(/^[^=]*=/, "", value) } END { if (value != "") print value }' "$LEASE"
}

write_lease() {
  local tmp
  umask 077
  tmp=$(mktemp "$ROOT/.lease.XXXXXX") || die "cannot create lease"
  printf 'version=1\nhome=%s\npid=%s\ngeneration=%s\n' "$BROKER_HOME" "$BROKER_PID" "$BROKER_GENERATION" > "$tmp" || die "cannot write lease"
  mv -f "$tmp" "$LEASE" || die "cannot install lease"
}

lease_matches_broker() {
  [ "$(lease_value home 2>/dev/null || true)" = "$BROKER_HOME" ] \
    && [ "$(lease_value pid 2>/dev/null || true)" = "$BROKER_PID" ] \
    && [ "$(lease_value generation 2>/dev/null || true)" = "$BROKER_GENERATION" ]
}

reap_stale_heavy_leases() {
  local d old
  for d in "$ROOT/heavy"/*.lock; do
    [ -d "$d" ] || continue
    old=$(awk -F= '$1 == "generation" { print $2 }' "$d/owner" 2>/dev/null || true)
    case "$old" in *[!0-9]*|'') continue ;; esac
    [ "$old" -lt "$BROKER_GENERATION" ] 2>/dev/null || continue
    rm -f -- "$d/owner" 2>/dev/null || true
    rmdir "$d" 2>/dev/null || true
  done
}

start_unlocked() {
  local old_generation old_pid old_home
  if [ -e "$LEASE" ]; then
    [ -f "$LEASE" ] && [ ! -L "$LEASE" ] || die "existing lease is not a regular file"
    old_generation=$(lease_value generation || true)
    old_pid=$(lease_value pid || true)
    old_home=$(lease_value home || true)
    case "$old_generation" in *[!0-9]*|'') die "existing lease has invalid generation" ;; esac
    [ "$old_home" = "$BROKER_HOME" ] || die "existing lease belongs to a different home"
    if [ "$old_generation" -gt "$BROKER_GENERATION" ] 2>/dev/null; then
      die "newer broker generation already owns this FM_HOME"
    fi
    if [ "$old_generation" -eq "$BROKER_GENERATION" ] 2>/dev/null; then
      if [ "$old_pid" = "$BROKER_PID" ]; then
        printf 'attached generation=%s\n' "$BROKER_GENERATION"
        return 0
      fi
      die "same-home duplicate supervisor refused for generation $BROKER_GENERATION"
    fi
    write_lease
    reap_stale_heavy_leases
    printf 'recovered generation=%s\n' "$BROKER_GENERATION"
    return 0
  fi
  write_lease
  printf 'started generation=%s\n' "$BROKER_GENERATION"
}

start() { require_broker; ensure_root; with_control_lock start_unlocked; }

require_active_lease() {
  require_broker; ensure_root
  lease_matches_broker || die "this broker generation does not own the FM_HOME supervisor lease"
}

task_id_ok() { [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$ ]]; }

hook_path() { # <env-name> <default-path-or-empty>
  local name fallback path resolved dir base
  name=$1
  fallback=$2
  path=${!name:-$fallback}
  [ -n "$path" ] || die "$name is required for this terminal action"
  case "$path" in /*) ;; *) die "$name must be an absolute executable path" ;; esac
  [ -f "$path" ] && [ ! -L "$path" ] && [ -x "$path" ] || die "$name is not a regular executable file"
  dir=$(canonical_dir "$(dirname "$path")") || die "$name has no canonical directory"
  base=$(basename "$path")
  resolved="$dir/$base"
  [ -f "$resolved" ] && [ ! -L "$resolved" ] && [ -x "$resolved" ] || die "$name resolves unsafely"
  printf '%s\n' "$resolved"
}

grant_path() {
  # FM_BUZZ_TEST_GRANT_BIN remains a fixture compatibility alias.  It is never
  # required in production: serializing this exact operation with
  # with_heavy_lease is the grant, and /usr/bin/true records that authority.
  if [ -n "${FM_BUZZ_GRANT_BIN:-}" ]; then
    hook_path FM_BUZZ_GRANT_BIN ''
  elif [ -n "${FM_BUZZ_TEST_GRANT_BIN:-}" ]; then
    hook_path FM_BUZZ_TEST_GRANT_BIN ''
  else
    hook_path FM_BUZZ_GRANT_BIN /usr/bin/true
  fi
}

receipt() { printf '%s/receipts/%s.%s\n' "$ROOT" "$1" "$2"; }
receipt_is_current() { # <task> <stage>; 0=current, 1=absent, otherwise die
  local dest=$1 stage=$2 path values
  path=$(receipt "$dest" "$stage")
  [ -e "$path" ] || return 1
  [ -f "$path" ] && [ ! -L "$path" ] || die "receipt for $dest/$stage is not a regular file"
  values=$(awk -F= '
    $1 == "generation" || $1 == "pid" || $1 == "stage" { if (seen[$1]++) bad=1; value[$1]=$2; next }
    NF { bad=1 }
    END { if (bad || !("generation" in value) || !("pid" in value) || !("stage" in value)) exit 1; print value["generation"] "\t" value["pid"] "\t" value["stage"] }
  ' "$path") || die "receipt for $dest/$stage is malformed"
  [ "$values" = "$BROKER_GENERATION"$'\t'"$BROKER_PID"$'\t'"$stage" ] \
    || die "receipt for $dest/$stage belongs to a foreign broker lease"
}
write_receipt() { # <task> <stage>
  local dest tmp
  dest=$(receipt "$1" "$2")
  receipt_is_current "$1" "$2" && return 0
  umask 077
  tmp=$(mktemp "$ROOT/receipts/.receipt.XXXXXX") || die "cannot create receipt"
  printf 'generation=%s\npid=%s\nstage=%s\n' "$BROKER_GENERATION" "$BROKER_PID" "$2" > "$tmp" || die "cannot write receipt"
  mv -n "$tmp" "$dest" 2>/dev/null || true
  [ -e "$dest" ] || mv -f "$tmp" "$dest" || die "cannot install receipt"
  receipt_is_current "$1" "$2" || die "cannot verify receipt"
}

append_status() { # <task> <line>
  local status="$STATE/$1.status"
  [ -f "$status" ] && [ ! -L "$status" ] || die "task status is unavailable for $1"
  printf '%s\n' "$2" >> "$status" || die "cannot append task status for $1"
}

with_heavy_lease() { # <task> <command...>
  local task=$1 lock="$ROOT/heavy/$1.lock"; shift
  if ! mkdir "$lock" 2>/dev/null; then
    die "heavy lease already active for task $task"
  fi
  printf 'generation=%s\npid=%s\n' "$BROKER_GENERATION" "$BROKER_PID" > "$lock/owner" || {
    rmdir "$lock" 2>/dev/null || true; die "cannot write heavy lease"; }
  (
    trap 'rm -f -- "$lock/owner" 2>/dev/null || true; rmdir "$lock" 2>/dev/null || true' EXIT
    "$@"
  )
}

grant_and_resume() { # <task>
  local task=$1 grant resume
  if ! receipt_is_current "$task" grant; then
    grant=$(grant_path)
    with_heavy_lease "$task" "$grant" "$task" "$STATE/$task.meta" || return 1
    write_receipt "$task" grant
    append_status "$task" "grant: $task"
  fi
  if ! receipt_is_current "$task" resume; then
    resume=$(hook_path FM_BUZZ_RESUME_BIN "$SCRIPT_DIR/fm-send.sh")
    "$resume" "$task" "FirstMate supervision: tests granted; resume the task and report done: $task when ready." || return 1
    write_receipt "$task" resume
  fi
}

merge_and_release() { # <task>
  local task=$1 merge release
  receipt_is_current "$task" grant || die "done for $task refused before a durable test grant"
  receipt_is_current "$task" resume || die "done for $task refused before a durable resume"
  if ! receipt_is_current "$task" merge; then
    merge=$(hook_path FM_BUZZ_MERGE_BIN "$SCRIPT_DIR/fm-merge-local.sh")
    with_heavy_lease "$task" "$merge" "$task" || return 1
    write_receipt "$task" merge
    append_status "$task" "merged: $task"
  fi
  if ! receipt_is_current "$task" release; then
    release=$(hook_path FM_BUZZ_RELEASE_BIN "$SCRIPT_DIR/fm-teardown.sh")
    with_heavy_lease "$task" "$release" "$task" || return 1
    write_receipt "$task" release
    append_status "$task" "released: $task"
  fi
}

cursor_value() { [ -f "$ROOT/cursors/$1" ] && [ ! -L "$ROOT/cursors/$1" ] && cat "$ROOT/cursors/$1" || printf '0\n'; }
write_cursor() { local tmp; tmp=$(mktemp "$ROOT/cursors/.cursor.XXXXXX") || die "cannot create cursor"; printf '%s\n' "$2" > "$tmp"; mv -f "$tmp" "$ROOT/cursors/$1"; }

tick_unlocked() {
  local status task cursor n line matched=0
  for status in "$STATE"/*.status; do
    [ -f "$status" ] && [ ! -L "$status" ] || continue
    task=$(basename "$status" .status)
    task_id_ok "$task" || continue
    [ -f "$STATE/$task.meta" ] && [ ! -L "$STATE/$task.meta" ] || continue
    cursor=$(cursor_value "$task")
    case "$cursor" in *[!0-9]*|'') die "invalid event cursor for $task" ;; esac
    n=0
    while IFS= read -r line || [ -n "$line" ]; do
      n=$((n + 1))
      [ "$n" -gt "$cursor" ] || continue
      if [[ "$line" =~ ^needs-test:\ ([A-Za-z0-9][A-Za-z0-9_-]{0,63})$ ]]; then
        [ "${BASH_REMATCH[1]}" = "$task" ] || die "cross-task needs-test event refused"
        grant_and_resume "$task" || return 1
        matched=1
      elif [[ "$line" =~ ^done:\ ([A-Za-z0-9][A-Za-z0-9_-]{0,63})$ ]]; then
        [ "${BASH_REMATCH[1]}" = "$task" ] || die "cross-task done event refused"
        merge_and_release "$task" || return 1
        matched=1
      fi
      write_cursor "$task" "$n"
    done < "$status"
  done
  touch "$ROOT/beat" || die "cannot write supervisor beat"
  if [ "$matched" -eq 1 ]; then printf 'processed\n'; else printf 'idle\n'; fi
}

tick() { require_active_lease; with_control_lock tick_unlocked; }

stop_unlocked() {
  lease_matches_broker || die "this broker generation does not own the FM_HOME supervisor lease"
  "$SCRIPT_DIR/fm-buzz-lifecycle.sh" prepare-stop >/dev/null || die "stop refused by FirstMate supervision policy"
  rm -f -- "$LEASE" || die "cannot remove supervisor lease"
  printf 'stopped generation=%s\n' "$BROKER_GENERATION"
}
stop() { require_active_lease; with_control_lock stop_unlocked; }

serve() {
  start
  while :; do
    # ACP owns the persistent log. Avoid emitting one idle line per poll into
    # that bounded operational log; actionable failures still reach stderr and
    # terminate this tracked child for the broker to recover fail-closed.
    tick >/dev/null || exit $?
    sleep "$POLL_SECONDS" || exit 1
  done
}

status() {
  require_broker; ensure_root
  if lease_matches_broker; then printf 'active generation=%s\n' "$BROKER_GENERATION"; else printf 'inactive\n'; fi
}

[ "$#" -eq 1 ] || usage
case "$1" in
  start) start ;;
  tick) tick ;;
  serve) serve ;;
  stop) stop ;;
  status) status ;;
  *) usage ;;
esac
