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

Outputs:
  <out-dir>/rss.csv          CSV timeline with RSS/VSZ samples
  <out-dir>/summary.txt      Human-readable growth summary
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

if [[ -z "$OUT_DIR" ]]; then
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  OUT_DIR="tmp/profiling/mux_memory_${PID}_${stamp}"
fi

mkdir -p "$OUT_DIR"
CSV_PATH="${OUT_DIR}/rss.csv"
SUMMARY_PATH="${OUT_DIR}/summary.txt"

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

awk -F, '
  NR==2 { start_epoch=$2; start_rss=$3; next }
  NR>2  { end_epoch=$2; end_rss=$3 }
  END {
    if (NR < 3) {
      print "Not enough samples to compute growth rate."
      exit 0
    }
    delta_s = end_epoch - start_epoch
    delta_kb = end_rss - start_rss
    if (delta_s <= 0) {
      print "Invalid sample duration."
      exit 0
    }
    growth_mb_per_hour = (delta_kb / 1024.0) * (3600.0 / delta_s)
    printf("samples=%d\n", NR - 1)
    printf("duration_seconds=%d\n", delta_s)
    printf("rss_start_kb=%d\n", start_rss)
    printf("rss_end_kb=%d\n", end_rss)
    printf("rss_delta_kb=%d\n", delta_kb)
    printf("rss_growth_mb_per_hour=%.4f\n", growth_mb_per_hour)
  }
' "$CSV_PATH" > "$SUMMARY_PATH"

cat "$SUMMARY_PATH"
