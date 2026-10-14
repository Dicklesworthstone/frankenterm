#!/bin/sh
# Profile shares of the two kill switches' targets on the full T0 file
# (~715 MB, several seconds per run): default arm vs each switch off.
set -u
R=/private/tmp/claude-501/-Users-jemanuel-projects/62f910a5-5c5c-4f7b-925b-c76d31c3007c/scratchpad/killswitch-ab
T0=$HOME/color-emoji-random.bin
prof() { # label env...
  label=$1; shift
  env "$@" "$R/ingest" --lane term --from-file "$T0" > "$R/prof-$label.json" 2> "$R/prof-$label.err" &
  pid=$!
  sleep 0.5
  /usr/bin/sample "$pid" 2 -file "$R/sample-$label.txt" > /dev/null 2>&1
  wait "$pid"
  python3 - "$R/sample-$label.txt" "$R/prof-$label.json" "$label" <<'EOF'
import json, re, sys
text = open(sys.argv[1], errors='replace').read()
d = json.load(open(sys.argv[2]))
# "Sort by top of stack" section: "<symbol>  (in <lib>)  <count>"
top = text.split('Sort by top of stack', 1)[-1]
rows = [(m.group(1).strip(), int(m.group(2))) for m in re.finditer(r'^\s+(.+?)\s+\(in [^)]*\)\s+(\d+)\s*$', top, re.M)]
total = sum(c for _, c in rows) or 1
def share(pat):
    return 100 * sum(c for s, c in rows if re.search(pat, s)) / total
print(f"{sys.argv[3]}: {d['mib_per_s']:.1f} MiB/s fp={d['state_fingerprint']} samples={total} "
      f"compress={share(r'compress_for_scrollback|ClusteredLine::from|slice_builder'):.1f}% "
      f"for_grapheme={share(r'for_grapheme|VARIATION_MAP|phf'):.1f}%")
EOF
}
prof default FT_BENCH_ARM=default
prof vs-skip-off FT_PRESENTATION_VS_SKIP=0
prof slice-off FT_LINE_COMPRESS_SLICE=0
echo DONE
