# shellcheck shell=bash
# Shared "supervision missing" predicate.
# Usage: . bin/fm-supervision-lib.sh
#
# Reports whether a firstmate home needs supervision because it has in-flight
# work (a state/<id>.meta exists) or an X-mode relay poll
# (state/x-watch.check.sh), and whether its watcher has a fresh liveness beacon
# (state/.last-watcher-beat, touched every poll cycle, within the grace window).
# bin/fm-turnend-guard.sh uses the PID-strict fm_watcher_healthy from
# bin/fm-wake-lib.sh for its block decision. bin/fm-guard.sh uses the model-aware
# fm_watcher_supervision_verdict (also in bin/fm-wake-lib.sh), which owns what a
# live watcher process means per supervision model. The status fields here retain
# the beacon-age details used in their messages.

# Portable mtime; Linux stat lacks -f, macOS stat lacks -c.
fm_sup_stat_mtime() {
  if [ "$(uname)" = Darwin ]; then
    stat -f %m "$1" 2>/dev/null
  else
    stat -c %Y "$1" 2>/dev/null
  fi
}

# fm_supervision_status <state-dir> [grace-seconds]
# Populates, for the state dir at $1:
#   FM_SUP_IN_FLIGHT      count of state/*.meta files (INCLUDING parked ones)
#   FM_SUP_LIVE           count of those metas with a live endpoint (window=)
#   FM_SUP_SOURCES        count of registered process-to-event sources
#   FM_SUP_NEEDED         true/false - a LIVE agent, an X-mode relay poll, or a
#                         registered event source (a source is a wait on an
#                         external process, not a task, so it has no metadata)
#   FM_SUP_WATCHER_FRESH  true/false - a watcher beacon within the grace window
#   FM_SUP_BEACON_DESC    human-readable beacon age, for banners ("never" if absent)
#   FM_SUP_QUEUE_PENDING  true/false - state/.wake-queue has unread records
# grace-seconds defaults to $FM_GUARD_GRACE, then 300, matching fm-guard.sh.
# Always returns 0; callers read the vars, or use fm_supervision_unhealthy below.
fm_supervision_status() {
  local state=$1 grace=${2:-${FM_GUARD_GRACE:-300}} meta source beat m age
  FM_SUP_IN_FLIGHT=0
  FM_SUP_NEEDED=false
  FM_SUP_WATCHER_FRESH=false
  FM_SUP_BEACON_DESC=never
  FM_SUP_QUEUE_PENDING=false

  FM_SUP_LIVE=0
  for meta in "$state"/*.meta; do
    [ -e "$meta" ] || continue
    FM_SUP_IN_FLIGHT=$((FM_SUP_IN_FLIGHT + 1))
    # FM_SUP_IN_FLIGHT conta ARQUIVO de metadados, inclusive tarefa ja encerrada
    # cujo registro foi estacionado (window= vira window_parked=). Pra saber se
    # existe alguem REALMENTE trabalhando, so vale meta com endpoint ativo.
    grep -q '^window=' "$meta" 2>/dev/null && FM_SUP_LIVE=$((FM_SUP_LIVE + 1))
  done
  FM_SUP_SOURCES=0
  for source in "$state"/procevent/*.source; do
    [ -e "$source" ] || continue
    FM_SUP_SOURCES=$((FM_SUP_SOURCES + 1))
  done
  # Supervisao existe pra vigiar AGENTE, nao arquivo. Contar metadados fazia a
  # frota parecer cheia com a maquina parada (52 arquivos, zero agentes em
  # 23/08) e mantinha FM_SUP_NEEDED=true pra sempre. So endpoint ativo pesa.
  if [ "$FM_SUP_LIVE" -gt 0 ] \
    || [ -f "$state/x-watch.check.sh" ] \
    || [ "$FM_SUP_SOURCES" -gt 0 ]; then
    FM_SUP_NEEDED=true
  fi

  beat="$state/.last-watcher-beat"
  if [ -e "$beat" ]; then
    m=$(fm_sup_stat_mtime "$beat")
    if [ -n "$m" ]; then
      age=$(( $(date +%s) - m ))
      FM_SUP_BEACON_DESC="${age}s ago"
      [ "$age" -lt "$grace" ] && FM_SUP_WATCHER_FRESH=true
    else
      # shellcheck disable=SC2034 # Read by callers (fm-guard.sh) after sourcing.
      FM_SUP_BEACON_DESC=unknown
    fi
  fi

  # shellcheck disable=SC2034 # Read by callers (fm-guard.sh) after sourcing.
  [ -s "$state/.wake-queue" ] && FM_SUP_QUEUE_PENDING=true
  return 0
}

# fm_supervision_needed <state-dir> [grace-seconds]
# Exit 0 (true) exactly when the home needs a watcher.
fm_supervision_needed() {
  fm_supervision_status "$@"
  [ "$FM_SUP_NEEDED" = true ]
}

# fm_supervision_unhealthy <state-dir> [grace-seconds]
# Exit 0 (true) exactly when supervision is needed and no watcher has a fresh
# beacon. Exit 1 (false) otherwise.
fm_supervision_unhealthy() {
  fm_supervision_status "$@"
  [ "$FM_SUP_NEEDED" = true ] && [ "$FM_SUP_WATCHER_FRESH" = false ]
}

# fm_plan_is_validated <arquivo> - 0 quando a linha "> STATUS: validado" existe.
# Plano em rascunho nao dirige a trava: o capitao ainda esta lendo o recorte.
fm_plan_is_validated() {
  local st
  st="$(sed -n 's/^>[[:space:]]*STATUS:[[:space:]]*//p' "$1" 2>/dev/null | head -1)"
  case "$st" in validado|VALIDADO) return 0 ;; *) return 1 ;; esac
}

# fm_plan_pending <fm-home> - imprime quantas tarefas de PLANO seguem pendentes.
#
# O firstmate trabalha com backlog de PLANOS (bin/fm-plan.sh e o dono do formato),
# nao com tarefa solta. Esta funcao existe pra trava de fim de turno poder
# perguntar "sobrou trabalho combinado?" sem depender de agente vivo.
#
# So conta "- [ ]" (pendente). "[x]" feita e "[~]" esperando o capitao nao contam:
# o segundo e o que impede a trava de virar armadilha quando o que sobra depende
# da palavra dele. Diretorio ausente = 0, entao nada muda pra quem nao usa planos.
fm_plan_pending() {
  local home=${1:-${FM_HOME:-.}} dir f total=0 n
  dir="${FM_PLAN_DIR:-$home/data/planos}"
  [ -d "$dir" ] || { echo 0; return 0; }
  for f in "$dir"/*.md; do
    [ -e "$f" ] || continue
    case "$(basename "$f")" in README.md) continue ;; esac
    # plano em rascunho nao prende: o capitao ainda esta validando o recorte
    fm_plan_is_validated "$f" || continue
    n=$(grep -c '^- \[ \]' "$f" 2>/dev/null || echo 0)
    total=$((total + n))
  done
  echo "$total"
}

# fm_plan_next <fm-home> [n] - imprime ate n tarefas pendentes, uma por linha,
# prefixadas pelo slug do plano. Usada so pra montar banner legivel.
fm_plan_next() {
  local home=${1:-${FM_HOME:-.}} limit=${2:-3} dir f slug shown=0 line
  dir="${FM_PLAN_DIR:-$home/data/planos}"
  [ -d "$dir" ] || return 0
  for f in "$dir"/*.md; do
    [ -e "$f" ] || continue
    case "$(basename "$f")" in README.md) continue ;; esac
    fm_plan_is_validated "$f" || continue
    slug="$(basename "$f" .md)"
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      [ "$shown" -lt "$limit" ] || return 0
      printf '%s · %s\n' "$slug" "${line#- \[ \] }"
      shown=$((shown + 1))
    done <<< "$(grep '^- \[ \]' "$f" 2>/dev/null)"
  done
}
