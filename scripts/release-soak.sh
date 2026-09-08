#!/usr/bin/env bash
# Run only with a known read-only PR and rules already configured in DATA_DIR.
set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/release-soak.sh --data-dir PATH [--binary PATH] [--hours N] [--interval DURATION] [--evidence-dir PATH]

Runs a real release binary in the foreground for the requested duration (24 by
default). It stores only timestamps, exit codes, output hashes, alert counts,
lease results, and RSS samples. It never writes command output or environment
variables, so do not put tokens in command arguments.
EOF
}

binary="./target/release/airborne"
data_dir=""
hours=24
interval="5m"
evidence_dir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary) binary="$2"; shift 2 ;;
    --data-dir) data_dir="$2"; shift 2 ;;
    --hours) hours="$2"; shift 2 ;;
    --interval) interval="$2"; shift 2 ;;
    --evidence-dir) evidence_dir="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
[[ -n "$data_dir" ]] || { echo "--data-dir is required" >&2; exit 2; }
[[ "$hours" =~ ^[1-9][0-9]*$ ]] || { echo "--hours must be a positive integer" >&2; exit 2; }
[[ -x "$binary" ]] || { echo "release binary is not executable: $binary" >&2; exit 2; }
[[ -d "$data_dir" ]] || { echo "data directory must already exist: $data_dir" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required to count alerts safely" >&2; exit 2; }

if [[ -z "$evidence_dir" ]]; then
  evidence_dir="$data_dir/soak-evidence-$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$evidence_dir"
chmod 700 "$evidence_dir"
summary="$evidence_dir/summary.tsv"
memory="$evidence_dir/memory.tsv"
printf 'timestamp\tevent\texit\tvalue\n' > "$summary"
printf 'timestamp\trss_kb\n' > "$memory"

record() { printf '%s\t%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$1" "$2" "$3" >> "$summary"; }
run_hashed() {
  local label="$1"; shift
  local tmp exit_code digest
  tmp="$(mktemp)"
  set +e
  "$@" >"$tmp" 2>&1
  exit_code=$?
  set -e
  digest="$(shasum -a 256 "$tmp" | awk '{print $1}')"
  rm -f "$tmp"
  record "$label" "$exit_code" "$digest"
  return "$exit_code"
}
alert_count() {
  local tmp count
  tmp="$(mktemp)"
  "$binary" --data-dir "$data_dir" --json alerts list --all >"$tmp"
  count="$(jq -er '.data.alerts | length' "$tmp")"
  rm -f "$tmp"
  printf '%s' "$count"
}

before="$(alert_count)"
run_hashed refresh_before "$binary" --data-dir "$data_dir" refresh || true
after_first="$(alert_count)"
run_hashed refresh_repeat "$binary" --data-dir "$data_dir" refresh || true
after_second="$(alert_count)"
duplicate_exit=0
if [[ "$after_first" != "$after_second" ]]; then
  duplicate_exit=1
fi
record duplicate_check "$duplicate_exit" "before=$before,after_first=$after_first,after_second=$after_second"

"$binary" --data-dir "$data_dir" run --interval "$interval" >/dev/null 2>&1 &
runner_pid=$!
cleanup() {
  if kill -0 "$runner_pid" 2>/dev/null; then
    kill -TERM "$runner_pid" 2>/dev/null || true
    wait "$runner_pid" || true
  fi
}
trap cleanup EXIT INT TERM
sleep 2
set +e
run_hashed second_runner_lease "$binary" --data-dir "$data_dir" run --interval "$interval"
lease_exit=$?
set -e
record second_runner_lease_expected "$([[ "$lease_exit" -eq 4 ]] && echo 0 || echo 1)" "expected=4,actual=$lease_exit"

end_epoch=$(( $(date +%s) + hours * 3600 ))
while kill -0 "$runner_pid" 2>/dev/null && [[ $(date +%s) -lt $end_epoch ]]; do
  rss="$(ps -o rss= -p "$runner_pid" | tr -d ' ')"
  printf '%s\t%s\n' "$(date -u +%FT%TZ)" "${rss:-0}" >> "$memory"
  sleep 60
done

if kill -0 "$runner_pid" 2>/dev/null; then
  kill -TERM "$runner_pid"
fi
set +e
wait "$runner_pid"
runner_exit=$?
set -e
record runner_exit "$runner_exit" "signal=TERM"
trap - EXIT INT TERM

echo "Soak evidence: $evidence_dir"
