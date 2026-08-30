#!/usr/bin/env bash
# fm-buzz.sh - thin, safe FirstMate-to-Buzz transport helper.
#
# It owns argument validation and delegates every relay operation to the
# bundled `buzz` CLI. It never reads keys: Buzz injects BUZZ_PRIVATE_KEY and
# BUZZ_RELAY_URL for a managed FirstMate process.
set -u

usage() {
  cat >&2 <<'USAGE'
usage:
  fm-buzz.sh reply --channel <uuid> --event <id> --text-file <file>
  fm-buzz.sh progress|final --channel <uuid> --event <id> --text-file <file>
  fm-buzz.sh mention --channel <uuid> --pubkey <hex-or-npub> --text-file <file> [--event <id>]
  fm-buzz.sh thread --channel <uuid> --event <id>
  fm-buzz.sh resolve --pubkey <hex> | --name <name>
  fm-buzz.sh task-offer --channel <uuid> --pubkey <hex-or-npub> --trace <id> --parent <task-id> --project <key> --delivery <report|branch|pr> --text-file <file> [--event <id>]
USAGE
  exit 2
}

need_file() {
  [ -f "$1" ] || { echo "text file does not exist: $1" >&2; exit 2; }
}

command -v buzz >/dev/null 2>&1 || {
  echo "buzz CLI is required on PATH (provided by the Buzz managed-agent runtime)" >&2
  exit 2
}

operation=${1:-}
[ -n "$operation" ] || usage
shift

channel=''
event=''
pubkey=''
name=''
text_file=''
trace=''
parent=''
project=''
delivery=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --channel|--event|--pubkey|--name|--text-file|--trace|--parent|--project|--delivery)
      [ "$#" -ge 2 ] || usage
      case "$1" in
        --channel) channel=$2 ;;
        --event) event=$2 ;;
        --pubkey) pubkey=$2 ;;
        --name) name=$2 ;;
        --text-file) text_file=$2 ;;
        --trace) trace=$2 ;;
        --parent) parent=$2 ;;
        --project) project=$2 ;;
        --delivery) delivery=$2 ;;
      esac
      shift 2
      ;;
    *) usage ;;
  esac
done

case "$operation" in
  reply|progress|final)
    [ -n "$channel" ] && [ -n "$event" ] && [ -n "$text_file" ] || usage
    need_file "$text_file"
    buzz messages send --channel "$channel" --reply-to "$event" --content - < "$text_file"
    ;;
  mention)
    [ -n "$channel" ] && [ -n "$pubkey" ] && [ -n "$text_file" ] || usage
    need_file "$text_file"
    if [ -n "$event" ]; then
      buzz messages send --channel "$channel" --reply-to "$event" --mention "$pubkey" --content - < "$text_file"
    else
      buzz messages send --channel "$channel" --mention "$pubkey" --content - < "$text_file"
    fi
    ;;
  thread)
    [ -n "$channel" ] && [ -n "$event" ] || usage
    buzz messages thread --channel "$channel" --event "$event"
    ;;
  resolve)
    if [ -n "$pubkey" ] && [ -z "$name" ]; then
      buzz users get --pubkey "$pubkey"
    elif [ -n "$name" ] && [ -z "$pubkey" ]; then
      buzz users get --name "$name"
    else
      usage
    fi
    ;;
  task-offer)
    [ -n "$channel" ] && [ -n "$pubkey" ] && [ -n "$trace" ] && [ -n "$parent" ] && [ -n "$project" ] && [ -n "$delivery" ] && [ -n "$text_file" ] || usage
    case "$delivery" in report|branch|pr) ;; *) usage ;; esac
    need_file "$text_file"
    offer_file=$(mktemp "${TMPDIR:-/tmp}/fm-buzz-offer.XXXXXX") || exit 2
    trap 'rm -f "$offer_file"' EXIT
    {
      printf '%s\n' '[firstmate-task-offer]'
      printf 'trace: %s\nparent: %s\nrequested-by: %s\nproject: %s\ndelivery: %s\n\n' "$trace" "$parent" "${BUZZ_AGENT_PUBKEY:-unknown}" "$project" "$delivery"
      cat "$text_file"
    } > "$offer_file"
    if [ -n "$event" ]; then
      buzz messages send --channel "$channel" --reply-to "$event" --mention "$pubkey" --content - < "$offer_file"
    else
      buzz messages send --channel "$channel" --mention "$pubkey" --content - < "$offer_file"
    fi
    ;;
  *) usage ;;
esac
