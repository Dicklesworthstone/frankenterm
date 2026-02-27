#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/profiling/mux_memory_watch.sh --pid <PID> [--out-dir <DIR>]

Environment variables:
  SAMPLE_SECS        Sample interval in seconds (default: 60)
  MAX_SAMPLES        Max samples before exit; 0 means run until process exits (default: 0)
  VMMAP_EVERY        On macOS, capture `vmmap -summary` every N samples; 0 disables (default: 30)
  CAPTURE_LEAKS_END  On macOS, run `leaks <PID>` once at the end if 1 (default: 1)
  MAX_GROWTH_MB_HR   Optional RSS growth threshold in MB/hour; script exits non-zero if exceeded

Outputs:
  <out-dir>/rss.csv          CSV timeline with RSS/VSZ samples
  <out-dir>/summary.txt      Human-readable growth summary
  <out-dir>/summary.json     Machine-readable growth summary
  <out-dir>/vmmap_*.txt      Optional vmmap snapshots (macOS only)
  <out-dir>/leaks.txt        Optional leaks report (macOS only)
USAGE
}

PID=""
OUT_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --pid)
      PID="${2:-}"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$PID" ]]; then
  echo "Missing required --pid argument" >&2
  usage >&2
  exit 2
fi

if ! [[ "$PID" =~ ^[0-9]+$ ]]; then
  echo "PID must be numeric: $PID" >&2
  exit 2
fi

if ! kill -0 "$PID" 2>/dev/null; then
  echo "Process is not running: PID $PID" >&2
  exit 1
fi

SAMPLE_SECS="${SAMPLE_SECS:-60}"
MAX_SAMPLES="${MAX_SAMPLES:-0}"
VMMAP_EVERY="${VMMAP_EVERY:-30}"
CAPTURE_LEAKS_END="${CAPTURE_LEAKS_END:-1}"
MAX_GROWTH_MB_HR="${MAX_GROWTH_MB_HR:-}"

if [[ -z "$OUT_DIR" ]]; then
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  OUT_DIR="tmp/profiling/mux_memory_${PID}_${stamp}"
fi

mkdir -p "$OUT_DIR"
CSV_PATH="${OUT_DIR}/rss.csv"
SUMMARY_PATH="${OUT_DIR}/summary.txt"
SUMMARY_JSON_PATH="${OUT_DIR}/summary.json"

echo "timestamp_utc,epoch_s,rss_kb,vsz_kb" > "$CSV_PATH"

sample_count=0
platform="$(uname -s)"

while true; do
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "Process exited: PID $PID"
    break
  fi

  timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  epoch_s="$(date +%s)"
  rss_kb="$(ps -o rss= -p "$PID" | awk '{print $1}')"
  vsz_kb="$(ps -o vsz= -p "$PID" | awk '{print $1}')"

  if [[ -z "$rss_kb" || -z "$vsz_kb" ]]; then
    echo "Failed to collect ps metrics for PID $PID" >&2
    break
  fi

  echo "${timestamp},${epoch_s},${rss_kb},${vsz_kb}" >> "$CSV_PATH"
  sample_count=$((sample_count + 1))

  if [[ "$platform" == "Darwin" && "$VMMAP_EVERY" -gt 0 ]]; then
    if (( sample_count % VMMAP_EVERY == 0 )); then
      vmmap -summary "$PID" > "${OUT_DIR}/vmmap_${sample_count}.txt" 2>&1 || true
    fi
  fi

  if [[ "$MAX_SAMPLES" -gt 0 && "$sample_count" -ge "$MAX_SAMPLES" ]]; then
    echo "Reached MAX_SAMPLES=$MAX_SAMPLES"
    break
  fi

  sleep "$SAMPLE_SECS"
done

if [[ "$platform" == "Darwin" && "$CAPTURE_LEAKS_END" == "1" ]]; then
  leaks "$PID" > "${OUT_DIR}/leaks.txt" 2>&1 || true
fi

metrics_csv="$(
  awk -F, '
    NR==2 {
      start_epoch=$2
      start_rss=$3
      min_rss=$3
      max_rss=$3
      next
    }
    NR>2 {
      end_epoch=$2
      end_rss=$3
      if ($3 < min_rss) min_rss=$3
      if ($3 > max_rss) max_rss=$3
    }
    END {
      if (NR < 3) {
        print "ERR,not_enough_samples"
        exit 0
      }
      delta_s = end_epoch - start_epoch
      if (delta_s <= 0) {
        print "ERR,invalid_duration"
        exit 0
      }
      delta_kb = end_rss - start_rss
      growth_mb_per_hour = (delta_kb / 1024.0) * (3600.0 / delta_s)
      printf "OK,%d,%d,%d,%d,%d,%d,%d,%.6f\n", \
        NR - 1, \
        delta_s, \
        start_rss, \
        end_rss, \
        delta_kb, \
        min_rss, \
        max_rss, \
        growth_mb_per_hour
    }
  ' "$CSV_PATH"
)"

IFS=',' read -r -a metrics_fields <<< "$metrics_csv"
metrics_status="${metrics_fields[0]:-ERR}"
metrics_reason="${metrics_fields[1]:-unknown}"
samples="${metrics_fields[1]:-0}"
duration_s="${metrics_fields[2]:-0}"
start_rss="${metrics_fields[3]:-0}"
end_rss="${metrics_fields[4]:-0}"
delta_kb="${metrics_fields[5]:-0}"
min_rss="${metrics_fields[6]:-0}"
max_rss="${metrics_fields[7]:-0}"
growth_mb_per_hour="${metrics_fields[8]:-0}"

exit_code=0

if [[ "$metrics_status" != "OK" ]]; then
  cat > "$SUMMARY_PATH" <<EOF
samples=$sample_count
status=insufficient_data
reason=${metrics_reason:-unknown}
EOF
  cat > "$SUMMARY_JSON_PATH" <<EOF
{
  "status": "insufficient_data",
  "reason": "${metrics_reason:-unknown}",
  "samples": ${sample_count}
}
EOF
  cat "$SUMMARY_PATH"
  exit 0
fi

cat > "$SUMMARY_PATH" <<EOF
status=ok
samples=$samples
duration_seconds=$duration_s
rss_start_kb=$start_rss
rss_end_kb=$end_rss
rss_delta_kb=$delta_kb
rss_min_kb=$min_rss
rss_max_kb=$max_rss
rss_growth_mb_per_hour=$growth_mb_per_hour
EOF

if [[ -n "$MAX_GROWTH_MB_HR" ]]; then
  if awk -v observed="$growth_mb_per_hour" -v threshold="$MAX_GROWTH_MB_HR" 'BEGIN { exit !(observed > threshold) }'; then
    echo "threshold_status=exceeded" >> "$SUMMARY_PATH"
    echo "threshold_mb_per_hour=$MAX_GROWTH_MB_HR" >> "$SUMMARY_PATH"
    exit_code=3
  else
    echo "threshold_status=pass" >> "$SUMMARY_PATH"
    echo "threshold_mb_per_hour=$MAX_GROWTH_MB_HR" >> "$SUMMARY_PATH"
  fi
fi

if [[ -n "$MAX_GROWTH_MB_HR" ]]; then
  threshold_value="$MAX_GROWTH_MB_HR"
else
  threshold_value="null"
fi

if [[ "$exit_code" -eq 0 ]]; then
  threshold_status="not_set"
  if [[ -n "$MAX_GROWTH_MB_HR" ]]; then
    threshold_status="pass"
  fi
else
  threshold_status="exceeded"
fi

cat > "$SUMMARY_JSON_PATH" <<EOF
{
  "status": "ok",
  "samples": $samples,
  "duration_seconds": $duration_s,
  "rss_start_kb": $start_rss,
  "rss_end_kb": $end_rss,
  "rss_delta_kb": $delta_kb,
  "rss_min_kb": $min_rss,
  "rss_max_kb": $max_rss,
  "rss_growth_mb_per_hour": $growth_mb_per_hour,
  "threshold_mb_per_hour": $threshold_value,
  "threshold_status": "$threshold_status"
}
EOF

cat "$SUMMARY_PATH"
exit "$exit_code"
